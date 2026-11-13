//! In-memory session store.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::harness::session::fork::{entries_for_fork_selection, SessionForkSelection};
use crate::harness::session::lease::{
    check_claimant, check_write_rule, judge_claim, lost, same_database, sweepable, Claim,
    DestinationCreation, LeaseEnvironment, LeaseIdentity, SessionControl, SessionControlState,
    SessionLease, SessionLeases, SystemLeaseEnvironment,
};
use crate::harness::session::types::{
    create_session_id, PendingSessionWrite, SessionMetadata, SessionReader, SessionStore,
    SessionTreeEntry,
};
use crate::harness::session::SessionFile;
use crate::harness::types::SessionError;
use loop_ai::now_ms;

struct SessionData {
    meta: SessionMetadata,
    entries: Vec<SessionTreeEntry>,
    leaf_id: Option<String>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, SessionData>,
    /// Session leases by Session id. Only the Runtime store writes them.
    leases: HashMap<String, SessionLease>,
    /// Branch revision and pending marker by Session id.
    control: HashMap<String, SessionControlState>,
    /// Destination receipts: request key to (fingerprint, Session id, result).
    destinations: HashMap<String, (String, String, String)>,
}

/// Create an in-memory session store.
pub fn create_in_memory_session_store() -> Arc<dyn SessionStore> {
    Arc::new(InMemorySessionStore {
        inner: Arc::new(Mutex::new(Inner::default())),
    })
}

struct InMemorySessionStore {
    inner: Arc<Mutex<Inner>>,
}

struct MemoryReader {
    meta: SessionMetadata,
    inner: Arc<Mutex<Inner>>,
}

#[async_trait]
impl SessionReader for MemoryReader {
    fn metadata(&self) -> &SessionMetadata {
        &self.meta
    }

    async fn read_head(&self) -> Result<Option<String>, SessionError> {
        Ok(self
            .inner
            .lock()
            .sessions
            .get(&self.meta.id)
            .and_then(|s| s.leaf_id.clone()))
    }

    async fn read_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
        Ok(self
            .inner
            .lock()
            .sessions
            .get(&self.meta.id)
            .and_then(|s| s.entries.iter().find(|e| e.id() == id).cloned()))
    }

    async fn read_entries(
        &self,
        _after_seq: Option<u64>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        Ok(self
            .inner
            .lock()
            .sessions
            .get(&self.meta.id)
            .map(|s| s.entries.clone())
            .unwrap_or_default())
    }

    async fn read_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let g = self.inner.lock();
        let Some(session) = g.sessions.get(&self.meta.id) else {
            return Ok(vec![]);
        };
        let leaf = leaf_id
            .map(|s| s.to_string())
            .or_else(|| session.leaf_id.clone());
        let Some(leaf) = leaf else {
            return Ok(vec![]);
        };
        let by_id: HashMap<_, _> = session
            .entries
            .iter()
            .map(|e| (e.id().to_string(), e.clone()))
            .collect();
        let mut path = Vec::new();
        let mut cur = Some(leaf);
        while let Some(id) = cur {
            let Some(entry) = by_id.get(&id) else {
                break;
            };
            path.push(entry.clone());
            if let SessionTreeEntry::Compaction {
                first_kept_entry_id,
                ..
            } = entry
            {
                // Include the retained tail (compaction parent back to the
                // first kept entry) so recent context survives.
                if let Some(first_kept) = first_kept_entry_id.clone() {
                    let mut kept_cur = entry.parent_id().map(|s| s.to_string());
                    while let Some(kid) = kept_cur {
                        let Some(kept_entry) = by_id.get(&kid) else {
                            break;
                        };
                        path.push(kept_entry.clone());
                        if kid == first_kept {
                            break;
                        }
                        kept_cur = kept_entry.parent_id().map(|s| s.to_string());
                    }
                }
                break;
            }
            cur = entry.parent_id().map(|s| s.to_string());
        }
        path.reverse();
        Ok(path)
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let id = create_session_id();
        let meta = SessionMetadata {
            id: id.clone(),
            cwd,
            name,
            parent_session_id: None,
            created_at: now_ms(),
            path: None,
        };
        self.inner.lock().sessions.insert(
            id,
            SessionData {
                meta: meta.clone(),
                entries: Vec::new(),
                leaf_id: None,
            },
        );
        Ok(Arc::new(MemoryReader {
            meta,
            inner: Arc::clone(&self.inner),
        }))
    }

    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        let meta = self
            .inner
            .lock()
            .sessions
            .get(id)
            .map(|s| s.meta.clone())
            .ok_or_else(|| SessionError::NotFound(id.into()))?;
        Ok(Arc::new(MemoryReader {
            meta,
            inner: Arc::clone(&self.inner),
        }))
    }

    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        Ok(self
            .inner
            .lock()
            .sessions
            .values()
            .filter(|s| {
                cwd.map(|c| s.meta.cwd.as_deref() == Some(c))
                    .unwrap_or(true)
            })
            .map(|s| s.meta.clone())
            .collect())
    }

    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        append_locked(&mut self.inner.lock(), session_id, pending)
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        delete_locked(&mut self.inner.lock(), id);
        Ok(())
    }

    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let (cwd, parent_id, entries) = {
            let g = self.inner.lock();
            let src = g
                .sessions
                .get(source_id)
                .ok_or_else(|| SessionError::NotFound(source_id.into()))?;
            let selected = entries_for_fork_selection(
                &src.entries,
                src.leaf_id.as_deref(),
                selection,
                through_entry_id,
            )?;
            (src.meta.cwd.clone(), src.meta.id.clone(), selected)
        };
        let reader = self.create(cwd, name).await?;
        let new_id = reader.metadata().id.clone();
        {
            let mut g = self.inner.lock();
            if let Some(session) = g.sessions.get_mut(&new_id) {
                session.meta.parent_session_id = Some(parent_id);
                let leaf = entries.last().map(|e| e.id().to_string());
                session.entries = entries;
                session.leaf_id = leaf;
            }
        }
        self.load(&new_id).await
    }
}

fn append_locked(
    g: &mut Inner,
    session_id: &str,
    pending: PendingSessionWrite,
) -> Result<SessionTreeEntry, SessionError> {
    let session = g
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| SessionError::NotFound(session_id.into()))?;
    let parent_id = session.leaf_id.clone();
    let id = loop_ai::new_id();
    let ts = now_ms();
    let entry = pending_to_entry(pending, id, parent_id, ts);
    if let SessionTreeEntry::Leaf { target_id, .. } = &entry {
        session.leaf_id = target_id.clone();
    } else {
        session.leaf_id = Some(entry.id().to_string());
    }
    session.entries.push(entry.clone());
    Ok(entry)
}

/// Remove a Session with its lease, control row and receipts, as the
/// SQLite store's foreign keys do.
fn delete_locked(g: &mut Inner, id: &str) {
    g.sessions.remove(id);
    g.leases.remove(id);
    g.control.remove(id);
    g.destinations.retain(|_, (_, session, _)| session != id);
}

fn pending_to_entry(
    pending: PendingSessionWrite,
    id: String,
    parent_id: Option<String>,
    timestamp: i64,
) -> SessionTreeEntry {
    match pending {
        PendingSessionWrite::Message { message } => SessionTreeEntry::Message {
            id,
            parent_id,
            timestamp,
            message,
        },
        PendingSessionWrite::ThinkingLevelChange { thinking_level } => {
            SessionTreeEntry::ThinkingLevelChange {
                id,
                parent_id,
                timestamp,
                thinking_level,
            }
        }
        PendingSessionWrite::ModelChange { provider, model_id } => SessionTreeEntry::ModelChange {
            id,
            parent_id,
            timestamp,
            provider,
            model_id,
        },
        PendingSessionWrite::ActiveToolsChange { tool_names } => {
            SessionTreeEntry::ActiveToolsChange {
                id,
                parent_id,
                timestamp,
                tool_names,
            }
        }
        PendingSessionWrite::Compaction {
            summary,
            first_kept_entry_id,
            details,
        } => SessionTreeEntry::Compaction {
            id,
            parent_id,
            timestamp,
            summary,
            first_kept_entry_id,
            details,
        },
        PendingSessionWrite::BranchSummary { summary } => SessionTreeEntry::BranchSummary {
            id,
            parent_id,
            timestamp,
            summary,
        },
        PendingSessionWrite::Leaf { target_id } => SessionTreeEntry::Leaf {
            id,
            parent_id,
            timestamp,
            target_id,
        },
        PendingSessionWrite::Label { label } => SessionTreeEntry::Label {
            id,
            parent_id,
            timestamp,
            label,
        },
        PendingSessionWrite::SessionInfo { name } => SessionTreeEntry::SessionInfo {
            id,
            parent_id,
            timestamp,
            name,
        },
        PendingSessionWrite::Custom { custom_type, data } => SessionTreeEntry::Custom {
            id,
            parent_id,
            timestamp,
            custom_type,
            data,
        },
    }
}

/// Create an in-memory store with everything the Runtime needs: leases,
/// control markers and destinations. It uses the system clock and pid
/// probe, and has no database file (so no Runtime lock proves its leases).
pub fn create_in_memory_runtime_store() -> Arc<dyn crate::harness::session::RuntimeSessionStore> {
    Arc::new(InMemoryRuntimeStore::new(
        Arc::new(SystemLeaseEnvironment),
        None,
    ))
}

/// In-memory Session store with leases, control markers and destinations.
///
/// It applies the same lease rules and error texts as the SQLite store
/// (`harness::session::lease`). Every operation runs under one lock, so each
/// is atomic like a SQLite transaction. For tests and tools without a
/// database.
#[derive(Clone)]
pub struct InMemoryRuntimeStore {
    inner: Arc<Mutex<Inner>>,
    environment: Arc<dyn LeaseEnvironment>,
    database: Option<Arc<std::path::PathBuf>>,
    identity: Option<LeaseIdentity>,
}

impl InMemoryRuntimeStore {
    /// An empty store. `database` is the path whose Runtime lock proves
    /// ownership of its leases (a lock file is all it needs; nothing is
    /// written there). `None` means no lock can prove it.
    pub fn new(
        environment: Arc<dyn LeaseEnvironment>,
        database: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            environment,
            database: database.map(Arc::new),
            identity: None,
        }
    }

    fn plain(&self) -> InMemorySessionStore {
        InMemorySessionStore {
            inner: Arc::clone(&self.inner),
        }
    }

    fn claim(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        takeover: bool,
        runtime_lock_held: bool,
    ) -> Result<(), SessionError> {
        check_claimant(identity)?;
        let mut g = self.inner.lock();
        let now = self.environment.now_ms();
        if !g.sessions.contains_key(id) {
            return Err(SessionError::NotFound(id.to_owned()));
        }
        let present = g.leases.get(id);
        if judge_claim(
            id,
            present,
            identity,
            takeover,
            runtime_lock_held,
            now,
            self.environment.as_ref(),
        )? == Claim::Held
        {
            return Ok(());
        }
        g.leases.insert(
            id.to_owned(),
            SessionLease {
                identity: identity.clone(),
                heartbeat_ms: now,
            },
        );
        Ok(())
    }

    fn update(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        release: bool,
    ) -> Result<(), SessionError> {
        let mut g = self.inner.lock();
        match g.leases.get_mut(id) {
            Some(lease) if &lease.identity == identity => {
                if release {
                    g.leases.remove(id);
                } else {
                    lease.heartbeat_ms = self.environment.now_ms();
                }
                Ok(())
            }
            _ => Err(lost(id)),
        }
    }
}

#[async_trait]
impl SessionStore for InMemoryRuntimeStore {
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.plain().create(cwd, name).await
    }

    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.plain().load(id).await
    }

    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        self.plain().list(cwd).await
    }

    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        let mut g = self.inner.lock();
        check_write_rule(g.leases.get(session_id), session_id, self.identity.as_ref())?;
        append_locked(&mut g, session_id, pending)
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        let mut g = self.inner.lock();
        check_write_rule(g.leases.get(id), id, self.identity.as_ref())?;
        delete_locked(&mut g, id);
        Ok(())
    }

    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.plain()
            .fork(source_id, selection, through_entry_id, name)
            .await
    }
}

#[async_trait]
impl SessionLeases for InMemoryRuntimeStore {
    fn database_path(&self) -> Option<&std::path::Path> {
        self.database.as_deref().map(|path| path.as_path())
    }

    fn for_lease(&self, identity: LeaseIdentity) -> Arc<dyn SessionStore> {
        Arc::new(Self {
            identity: Some(identity),
            ..self.clone()
        })
    }

    async fn lease(&self, id: &str) -> Result<Option<SessionLease>, SessionError> {
        Ok(self.inner.lock().leases.get(id).cloned())
    }

    async fn acquire_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError> {
        self.claim(id, identity, false, false)
    }

    async fn takeover_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        runtime_lock_held: bool,
    ) -> Result<(), SessionError> {
        self.claim(id, identity, true, runtime_lock_held)
    }

    async fn renew_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError> {
        self.update(id, identity, false)
    }

    async fn release_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError> {
        self.update(id, identity, true)
    }

    async fn sweep_stale_leases(
        &self,
        runtime_lock_for: Option<&std::path::Path>,
    ) -> Result<usize, SessionError> {
        let proof = runtime_lock_for
            .zip(self.database.as_deref())
            .is_some_and(|(held, ours)| same_database(held, ours));
        let mut g = self.inner.lock();
        let now = self.environment.now_ms();
        let before = g.leases.len();
        let environment = self.environment.as_ref();
        g.leases
            .retain(|_, lease| !sweepable(lease, proof, now, environment));
        Ok(before - g.leases.len())
    }
}

#[async_trait]
impl SessionControl for InMemoryRuntimeStore {
    async fn create_child(
        &self,
        cwd: Option<String>,
        name: Option<String>,
        parent: &str,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let id = create_session_id();
        let meta = SessionMetadata {
            id: id.clone(),
            cwd,
            name,
            parent_session_id: Some(parent.to_owned()),
            created_at: now_ms(),
            path: None,
        };
        self.inner.lock().sessions.insert(
            id,
            SessionData {
                meta: meta.clone(),
                entries: Vec::new(),
                leaf_id: None,
            },
        );
        Ok(Arc::new(MemoryReader {
            meta,
            inner: Arc::clone(&self.inner),
        }))
    }

    async fn control_state(&self, id: &str) -> Result<SessionControlState, SessionError> {
        Ok(self
            .inner
            .lock()
            .control
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    async fn control_mutation(
        &self,
        id: &str,
        identity: LeaseIdentity,
        branch: bool,
        write: PendingSessionWrite,
        marker: String,
    ) -> Result<SessionControlState, SessionError> {
        let mut g = self.inner.lock();
        check_write_rule(g.leases.get(id), id, Some(&identity))?;
        let present = g.control.get(id).cloned().unwrap_or_default();
        if present.pending.is_some() {
            return Err(SessionError::Invalid(
                "Session has an unannounced mutation; reopen it".into(),
            ));
        }
        if let PendingSessionWrite::Leaf {
            target_id: Some(target),
        } = &write
        {
            let known = g.sessions.get(id).is_some_and(|session| {
                session.entries.iter().any(|entry| {
                    entry.id() == target && !matches!(entry, SessionTreeEntry::Leaf { .. })
                })
            });
            if !known {
                return Err(SessionError::Invalid("unknown branch target".into()));
            }
        }
        let marker_value: serde_json::Value = serde_json::from_str(&marker)
            .map_err(|error| SessionError::Invalid(error.to_string()))?;
        // Check the Session before the first write, so a failure writes nothing.
        if !g.sessions.contains_key(id) {
            return Err(SessionError::NotFound(id.to_owned()));
        }
        if let Some(summary) = marker_value["summary"].as_str() {
            append_locked(
                &mut g,
                id,
                PendingSessionWrite::BranchSummary {
                    summary: summary.into(),
                },
            )?;
        }
        append_locked(&mut g, id, write)?;
        let state = SessionControlState {
            branch_revision: present.branch_revision + u64::from(branch),
            pending: Some(marker),
        };
        g.control.insert(id.to_owned(), state.clone());
        Ok(state)
    }

    async fn acknowledge_control(&self, id: &str, marker: &str) -> Result<(), SessionError> {
        if let Some(state) = self.inner.lock().control.get_mut(id) {
            if state.pending.as_deref() == Some(marker) {
                state.pending = None;
            }
        }
        Ok(())
    }

    async fn destination_receipt(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<(String, String)>, SessionError> {
        self.inner
            .lock()
            .destinations
            .get(key)
            .map(|(old, id, result)| {
                if old != fingerprint {
                    Err(SessionError::Invalid(
                        "request id already used with different parameters".into(),
                    ))
                } else {
                    Ok((id.clone(), result.clone()))
                }
            })
            .transpose()
    }

    async fn create_destination(
        &self,
        file: SessionFile,
        cwd: String,
        parent: Option<String>,
        request_key: String,
        fingerprint: String,
        result: String,
        identity: LeaseIdentity,
    ) -> Result<DestinationCreation, SessionError> {
        let store: Arc<dyn crate::harness::session::RuntimeSessionStore> = Arc::new(self.clone());
        let mut g = self.inner.lock();
        if let Some((old, id, result)) = g.destinations.get(&request_key).cloned() {
            if old != fingerprint {
                return Err(SessionError::Invalid(
                    "request id already used with different parameters".into(),
                ));
            }
            return Ok(DestinationCreation::new(id, result, store, identity, false));
        }
        let id = create_session_id();
        g.sessions.insert(
            id.clone(),
            SessionData {
                meta: SessionMetadata {
                    id: id.clone(),
                    cwd: Some(cwd),
                    name: file.name,
                    parent_session_id: parent,
                    created_at: now_ms(),
                    path: None,
                },
                entries: Vec::new(),
                leaf_id: None,
            },
        );
        let mut remap = HashMap::new();
        for entry in file.entries {
            let old_id = entry.id().to_owned();
            let Some(pending) = crate::harness::session::lease::destination_write(entry, &remap)
            else {
                continue;
            };
            let written = append_locked(&mut g, &id, pending)?;
            remap.insert(old_id, written.id().to_owned());
        }
        g.leases.insert(
            id.clone(),
            SessionLease {
                identity: identity.clone(),
                heartbeat_ms: now_ms(),
            },
        );
        g.destinations
            .insert(request_key, (fingerprint, id.clone(), result.clone()));
        Ok(DestinationCreation::new(id, result, store, identity, true))
    }
}
