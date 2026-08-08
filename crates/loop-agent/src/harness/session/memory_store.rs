//! In-memory session store.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::harness::session::fork::{entries_for_fork_selection, SessionForkSelection};
use crate::harness::session::types::{
    create_session_id, PendingSessionWrite, SessionMetadata, SessionReader, SessionStore,
    SessionTreeEntry,
};
use crate::harness::types::SessionError;
use loop_ai::now_ms;

struct SessionData {
    meta: SessionMetadata,
    entries: Vec<SessionTreeEntry>,
    leaf_id: Option<String>,
}

struct Inner {
    sessions: HashMap<String, SessionData>,
}

/// Create an in-memory session store.
pub fn create_in_memory_session_store() -> Arc<dyn SessionStore> {
    Arc::new(InMemorySessionStore {
        inner: Arc::new(Mutex::new(Inner {
            sessions: HashMap::new(),
        })),
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
            .filter(|s| cwd.map(|c| s.meta.cwd.as_deref() == Some(c)).unwrap_or(true))
            .map(|s| s.meta.clone())
            .collect())
    }

    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        let mut g = self.inner.lock();
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

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.lock().sessions.remove(id);
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
    }
}
