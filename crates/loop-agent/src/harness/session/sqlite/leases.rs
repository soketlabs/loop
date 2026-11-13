//! Session lease identity, liveness and transactional ownership checks.

use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use super::store::{begin_immediate, open, SqliteSessionStore};
use crate::harness::types::SessionError;

pub(super) use crate::harness::session::lease::SystemLeaseEnvironment;
use crate::harness::session::lease::{
    check_claimant, check_write_rule, conflict, judge_claim, lost, same_database, sweepable, Claim,
};
pub use crate::harness::session::lease::{
    LeaseEnvironment, LeaseIdentity, PidLiveness, SessionLease, LEASE_RENEW_INTERVAL_MS,
    LEASE_STALE_AFTER_MS,
};

fn storage(error: rusqlite::Error) -> SessionError {
    SessionError::Storage(error.to_string())
}

pub(super) fn read_lease(
    conn: &Connection,
    id: &str,
) -> Result<Option<SessionLease>, SessionError> {
    conn.query_row(
        "SELECT owner, pid, generation, heartbeat_ms FROM session_leases WHERE session_id=?1",
        [id],
        |row| {
            Ok(SessionLease {
                identity: LeaseIdentity {
                    owner: row.get(0)?,
                    pid: row.get(1)?,
                    generation: row.get(2)?,
                },
                heartbeat_ms: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(storage)
}
pub(super) fn check_write(
    conn: &Connection,
    id: &str,
    identity: Option<&LeaseIdentity>,
) -> Result<(), SessionError> {
    check_write_rule(read_lease(conn, id)?.as_ref(), id, identity)
}

impl SqliteSessionStore {
    /// Bind writes to this identity. Every append/delete rechecks the full identity
    /// inside its IMMEDIATE transaction. A missing or replaced lease fences writes.
    pub fn for_lease(&self, identity: LeaseIdentity) -> Self {
        Self {
            path: self.path.clone(),
            environment: Arc::clone(&self.environment),
            identity: Some(identity),
            resume_guard: None,
            gate: Arc::clone(&self.gate),
        }
    }

    /// Read ownership without acquiring a lease (listing remains read-only).
    pub async fn lease(&self, id: &str) -> Result<Option<SessionLease>, SessionError> {
        let path = self.path.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || read_lease(&open(&path)?, &id))
            .await
            .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    /// Refuse standalone resume while the Session is owned by another writer.
    /// Stale owners must first be replaced through `takeover_lease`.
    pub async fn check_resume(&self, id: &str) -> Result<(), SessionError> {
        if let Some(lease) = self.lease(id).await? {
            return Err(conflict(id, &lease));
        }
        Ok(())
    }

    /// Acquire an absent lease. Re-acquiring the same identity is idempotent.
    pub async fn acquire_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
    ) -> Result<(), SessionError> {
        self.claim_lease(id, identity, false, false).await
    }

    /// Replace a stale lease only when its pid is dead. The Runtime exception is
    /// explicit: the caller must hold the associated runtime.lock for THIS store's
    /// ownership domain. Only runtime-to-runtime reclamation bypasses age and pid liveness: a
    /// live pid may have been recycled since the recorded Runtime died.
    pub async fn takeover_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        runtime_lock_held: bool,
    ) -> Result<(), SessionError> {
        self.claim_lease(id, identity, true, runtime_lock_held)
            .await
    }

    async fn claim_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        takeover: bool,
        runtime_lock_held: bool,
    ) -> Result<(), SessionError> {
        check_claimant(identity)?;
        let path = self.path.clone();
        let id = id.to_owned();
        let identity = identity.clone();
        let env = Arc::clone(&self.environment);
        let guard = self.resume_guard.clone();
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx = begin_immediate(&conn, &gate).map_err(storage)?;
            // Checked inside the transaction: release deletes in its own
            // IMMEDIATE transaction after closing the writer, so a claim
            // either commits before that delete or sees the writer closed.
            if let Some(guard) = guard.as_ref().filter(|guard| guard.failed()) {
                return Err(guard.refusal());
            }
            let now = env.now_ms();
            if !tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)", [&id], |row| row.get::<_, bool>(0)).map_err(storage)? {
                return Err(SessionError::NotFound(id));
            }
            let present = read_lease(&tx, &id)?;
            if judge_claim(&id, present.as_ref(), &identity, takeover, runtime_lock_held, now, env.as_ref())? == Claim::Held {
                return Ok(());
            }
            tx.execute("INSERT INTO session_leases(session_id, owner, pid, generation, heartbeat_ms) VALUES (?1,?2,?3,?4,?5)
                ON CONFLICT(session_id) DO UPDATE SET owner=excluded.owner, pid=excluded.pid, generation=excluded.generation, heartbeat_ms=excluded.heartbeat_ms",
                params![id, identity.owner, identity.pid, identity.generation, now]).map_err(storage)?;
            tx.commit().map_err(storage)
        }).await.map_err(|e| SessionError::Storage(e.to_string()))?
    }

    /// Renew only the full matching identity. Failure means the caller must stop
    /// new writes and cancel its work. Schedule this every LEASE_RENEW_INTERVAL_MS.
    pub async fn renew_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
    ) -> Result<(), SessionError> {
        self.update_lease(id, identity, false).await
    }

    /// Release only the full matching identity; an old owner cannot release its successor.
    pub async fn release_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
    ) -> Result<(), SessionError> {
        self.update_lease(id, identity, true).await
    }

    async fn update_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        release: bool,
    ) -> Result<(), SessionError> {
        let path = self.path.clone();
        let id = id.to_owned();
        let identity = identity.clone();
        let env = Arc::clone(&self.environment);
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx = begin_immediate(&conn, &gate).map_err(storage)?;
            let changed = if release {
                tx.execute("DELETE FROM session_leases WHERE session_id=?1 AND owner=?2 AND pid=?3 AND generation=?4",
                    params![id, identity.owner, identity.pid, identity.generation]).map_err(storage)?
            } else {
                tx.execute("UPDATE session_leases SET heartbeat_ms=?5 WHERE session_id=?1 AND owner=?2 AND pid=?3 AND generation=?4",
                    params![id, identity.owner, identity.pid, identity.generation, env.now_ms()]).map_err(storage)?
            };
            if changed != 1 { return Err(lost(&id)); }
            tx.commit().map_err(storage)
        }).await.map_err(|e| SessionError::Storage(e.to_string()))?
    }
}

impl SqliteSessionStore {
    /// Delete lease rows whose owner is gone, and return how many.
    ///
    /// A row goes only when its heartbeat is older than
    /// [`LEASE_STALE_AFTER_MS`] and its pid is proven dead. Unknown liveness,
    /// a live pid, and any platform without a pid probe keep the row. Rows
    /// of a Runtime also need `runtime_lock_for` to name this database: the
    /// caller then holds the Runtime lock of this ownership domain.
    ///
    /// Candidates are read, judged and deleted in one IMMEDIATE transaction,
    /// and each delete names the exact row it judged, so a concurrent renewal
    /// or a new owner is never removed.
    pub async fn sweep_stale_leases(
        &self,
        runtime_lock_for: Option<&std::path::Path>,
    ) -> Result<usize, SessionError> {
        let proof = runtime_lock_for.is_some_and(|held| same_database(held, &self.path));
        let path = self.path.clone();
        let env = Arc::clone(&self.environment);
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx = begin_immediate(&conn, &gate)
                .map_err(storage)?;
            let now = env.now_ms();
            let rows: Vec<(String, SessionLease)> = {
                let mut statement = tx
                    .prepare(
                        "SELECT session_id, owner, pid, generation, heartbeat_ms FROM session_leases",
                    )
                    .map_err(storage)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            SessionLease {
                                identity: LeaseIdentity {
                                    owner: row.get(1)?,
                                    pid: row.get(2)?,
                                    generation: row.get(3)?,
                                },
                                heartbeat_ms: row.get(4)?,
                            },
                        ))
                    })
                    .map_err(storage)?;
                rows.collect::<Result<_, _>>().map_err(storage)?
            };
            let mut swept = 0;
            for (id, lease) in rows {
                if !sweepable(&lease, proof, now, env.as_ref()) {
                    continue;
                }
                swept += tx
                    .execute(
                        "DELETE FROM session_leases WHERE session_id=?1 AND owner=?2 AND pid=?3 AND generation=?4 AND heartbeat_ms=?5",
                        params![
                            id,
                            lease.identity.owner,
                            lease.identity.pid,
                            lease.identity.generation,
                            lease.heartbeat_ms
                        ],
                    )
                    .map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
            Ok(swept)
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }
}

/// Open the standalone bootstrap store and claim its resumed Session, if any.
/// Stale rows of dead owners are swept first, and ownership is renewed every
/// 5 s and released when the final store handle drops.
pub async fn create_sqlite_session_store_for_resume(
    path: impl AsRef<std::path::Path>,
    id: Option<&str>,
) -> Result<Arc<dyn crate::harness::session::SessionStore>, SessionError> {
    let store = SqliteSessionStore::open(path)?;
    // Rows of `loop` processes that died without releasing would block
    // their Sessions until someone takes them over.
    match store.sweep_stale_leases(None).await {
        Ok(0) => {}
        Ok(swept) => tracing::info!(swept, "removed Session leases of exited processes"),
        Err(error) => tracing::warn!(%error, "could not sweep stale Session leases"),
    }
    Ok(Arc::new(match id {
        Some(id) => store.standalone_resume(id).await?,
        None => store.standalone_writer(),
    }))
}

pub(super) struct ResumeLeaseGuard {
    store: SqliteSessionStore,
    ids: parking_lot::Mutex<std::collections::HashSet<String>>,
    identity: LeaseIdentity,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    /// Set when release begins: from then on the writer is closed.
    releasing: std::sync::atomic::AtomicBool,
    /// Set once the rows are gone; `Drop` then has nothing to do.
    released: std::sync::atomic::AtomicBool,
}

/// Every standalone writer of this process, for [`close_standalone_writers`].
static WRITERS: parking_lot::Mutex<Vec<std::sync::Weak<ResumeLeaseGuard>>> =
    parking_lot::Mutex::new(Vec::new());
/// Set by [`close_standalone_writers`]: no writer of this process claims again.
static WRITERS_CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Close every standalone writer of this process and release its leases,
/// for a process that is about to exit before anyone owns a shutdown
/// handle (a signal during startup). Terminal for the whole process: any
/// writer, including one created later, refuses to claim or write.
pub async fn close_standalone_writers() -> Result<(), SessionError> {
    WRITERS_CLOSED.store(true, std::sync::atomic::Ordering::Release);
    let writers: Vec<_> = WRITERS
        .lock()
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect();
    let mut result = Ok(());
    for writer in writers {
        if let Err(error) = writer.release().await {
            result = Err(error);
        }
    }
    result
}

/// How long an explicit release waits for a locked database. Short, like
/// the `Drop` fallback: it runs while the process exits.
const RELEASE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// How long the `Drop` fallback waits for a locked database. Short: it may
/// run while the process exits, with nobody to retry.
const DROP_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// Delete every lease row of this exact identity, fenced against successors.
fn delete_identity_rows(conn: &Connection, identity: &LeaseIdentity) -> Result<(), SessionError> {
    conn.execute(
        "DELETE FROM session_leases WHERE owner=?1 AND pid=?2 AND generation=?3",
        params![identity.owner, identity.pid, identity.generation],
    )
    .map_err(storage)?;
    Ok(())
}
impl ResumeLeaseGuard {
    pub(super) fn track(&self, id: &str) {
        self.ids.lock().insert(id.to_owned());
    }
    pub(super) fn forget(&self, id: &str) {
        self.ids.lock().remove(id);
    }
    pub(super) fn claim_created(&self, tx: &Connection, id: &str) -> Result<(), SessionError> {
        if self.failed() {
            return Err(lost(id));
        }
        tx.execute("INSERT INTO session_leases(session_id, owner, pid, generation, heartbeat_ms) VALUES (?1,?2,?3,?4,?5)",
            params![id, self.identity.owner, self.identity.pid, self.identity.generation, self.store.environment.now_ms()]).map_err(storage)?;
        Ok(())
    }

    pub(super) fn failed(&self) -> bool {
        self.cancellation.is_cancelled()
            || WRITERS_CLOSED.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Why a write through this writer is refused.
    pub(super) fn refusal(&self) -> SessionError {
        if self.releasing.load(std::sync::atomic::Ordering::Acquire)
            || WRITERS_CLOSED.load(std::sync::atomic::Ordering::Acquire)
        {
            SessionError::Invalid("Session leases were released; this writer is closed.".into())
        } else {
            SessionError::Invalid(
                "Session lease renewal failed; stop writing and cancel work.".into(),
            )
        }
    }

    /// Close the writer, then delete its lease rows. Writes already inside
    /// their transaction finish first; any later one is refused.
    pub(super) async fn release(&self) -> Result<(), SessionError> {
        self.releasing
            .store(true, std::sync::atomic::Ordering::Release);
        self.cancellation.cancel();
        let path = self.store.path.clone();
        let identity = self.identity.clone();
        // Wait only briefly for this process's other writes too.
        let permit = self.store.gate.enter_within(RELEASE_BUSY_TIMEOUT).await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // One short attempt, no retries: this runs on the way out, and a
            // locked database must not hold up exit. The writer is closed
            // already; a row left behind expires and the startup sweep
            // removes it.
            let conn = open(&path)?;
            conn.busy_timeout(RELEASE_BUSY_TIMEOUT).map_err(storage)?;
            let tx = rusqlite::Transaction::new_unchecked(
                &conn,
                rusqlite::TransactionBehavior::Immediate,
            )
            .map_err(storage)?;
            delete_identity_rows(&tx, &identity)?;
            tx.commit().map_err(storage)
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))??;
        self.ids.lock().clear();
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}
impl Drop for ResumeLeaseGuard {
    fn drop(&mut self) {
        if self.released.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // A fallback for owners that never called `release`. Drop cannot
        // await: inside a Tokio runtime, delete on its blocking pool; outside
        // one, delete now with a short busy timeout.
        let path = self.store.path.clone();
        let identity = self.identity.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || {
                    if let Ok(conn) = open(&path) {
                        let _ = delete_identity_rows(&conn, &identity);
                    }
                });
            }
            Err(_) => {
                if let Ok(conn) = Connection::open(&path) {
                    let _ = conn.busy_timeout(DROP_BUSY_TIMEOUT);
                    let _ = delete_identity_rows(&conn, &identity);
                }
            }
        }
    }
}
impl SqliteSessionStore {
    /// Acquire or recover a standalone resume lease before a harness can write.
    /// The returned store owns renewal and release; failure fences subsequent writes.
    pub async fn standalone_resume(&self, id: &str) -> Result<Self, SessionError> {
        let store = self.standalone_writer();
        let identity = store
            .identity
            .as_ref()
            .expect("standalone writer has identity");
        store.takeover_lease(id, identity, false).await?;
        store
            .resume_guard
            .as_ref()
            .expect("standalone writer has guard")
            .track(id);
        Ok(store)
    }

    /// Create a standalone writer that acquires each Session before creating or loading it.
    pub fn standalone_writer(&self) -> Self {
        static GENERATION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        let now = self.environment.now_ms();
        // A plain compare-exchange loop: `fetch_update` is deprecated on newer toolchains.
        let mut previous = GENERATION.load(std::sync::atomic::Ordering::SeqCst);
        while let Err(actual) = GENERATION.compare_exchange_weak(
            previous,
            now.max(previous.saturating_add(1)),
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        ) {
            previous = actual;
        }
        let identity = LeaseIdentity {
            owner: "standalone".into(),
            pid: std::process::id(),
            generation: now.max(previous.saturating_add(1)),
        };
        let guard = Arc::new(ResumeLeaseGuard {
            store: Self {
                path: self.path.clone(),
                environment: self.environment.clone(),
                identity: None,
                resume_guard: None,
                gate: Arc::clone(&self.gate),
            },
            ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
            identity: identity.clone(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            releasing: std::sync::atomic::AtomicBool::new(false),
            released: std::sync::atomic::AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&guard);
        {
            let mut writers = WRITERS.lock();
            writers.retain(|w| w.strong_count() > 0);
            writers.push(weak.clone());
        }
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(
                    LEASE_RENEW_INTERVAL_MS as u64,
                ))
                .await;
                let Some(guard) = weak.upgrade() else {
                    break;
                };
                let store = guard.store.clone();
                let ids: Vec<_> = guard.ids.lock().iter().cloned().collect();
                let identity = guard.identity.clone();
                drop(guard);
                for id in ids {
                    if store.renew_lease(&id, &identity).await.is_err() {
                        if let Some(guard) = weak.upgrade() {
                            if guard.ids.lock().contains(&id) {
                                guard.cancellation.cancel();
                            } else {
                                continue;
                            }
                        }
                        return;
                    }
                }
            }
        });
        let mut store = self.for_lease(identity);
        store.resume_guard = Some(guard);
        store
    }
}

/// The lease trait of the Runtime. Each method is the inherent method of the
/// same name: same SQL, same transactions, same errors.
#[async_trait::async_trait]
impl crate::harness::session::lease::SessionLeases for SqliteSessionStore {
    fn database_path(&self) -> Option<&std::path::Path> {
        Some(&self.path)
    }
    fn for_lease(&self, identity: LeaseIdentity) -> Arc<dyn crate::harness::session::SessionStore> {
        Arc::new(SqliteSessionStore::for_lease(self, identity))
    }
    async fn lease(&self, id: &str) -> Result<Option<SessionLease>, SessionError> {
        SqliteSessionStore::lease(self, id).await
    }
    async fn check_resume(&self, id: &str) -> Result<(), SessionError> {
        SqliteSessionStore::check_resume(self, id).await
    }
    async fn acquire_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError> {
        SqliteSessionStore::acquire_lease(self, id, identity).await
    }
    async fn takeover_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        runtime_lock_held: bool,
    ) -> Result<(), SessionError> {
        SqliteSessionStore::takeover_lease(self, id, identity, runtime_lock_held).await
    }
    async fn renew_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError> {
        SqliteSessionStore::renew_lease(self, id, identity).await
    }
    async fn release_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError> {
        SqliteSessionStore::release_lease(self, id, identity).await
    }
    async fn sweep_stale_leases(
        &self,
        runtime_lock_for: Option<&std::path::Path>,
    ) -> Result<usize, SessionError> {
        SqliteSessionStore::sweep_stale_leases(self, runtime_lock_for).await
    }
}
