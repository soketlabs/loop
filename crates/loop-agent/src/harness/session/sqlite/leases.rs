//! Session lease identity, liveness and transactional ownership checks.

use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::store::{open, SqliteSessionStore};
use crate::harness::types::SessionError;

/// Owners should renew a live Session lease at this interval.
pub const LEASE_RENEW_INTERVAL_MS: i64 = 5_000;
/// Ordinary takeover requires both this age and proof that the pid is dead.
pub const LEASE_STALE_AFTER_MS: i64 = 30_000;

/// Full fencing identity. Generations must change when an owner restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseIdentity {
    /// Owner kind, such as `runtime` or `standalone`.
    pub owner: String,
    /// Owning process id.
    pub pid: u32,
    /// Fencing generation, distinct across owner restarts.
    pub generation: i64,
}

/// Persisted Session lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLease {
    /// Full identity that must match on every write.
    pub identity: LeaseIdentity,
    /// Last renewal in Unix epoch milliseconds.
    pub heartbeat_ms: i64,
}

/// Unknown liveness never permits ordinary takeover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidLiveness {
    /// The pid is alive.
    Alive,
    /// The probe proved the pid no longer exists.
    Dead,
    /// Liveness could not be established (including permission errors).
    Unknown,
}

/// Injectable clock and pid probe. Probes run inside the ownership transaction.
pub trait LeaseEnvironment: Send + Sync {
    /// Current Unix epoch time in milliseconds.
    fn now_ms(&self) -> i64;
    /// Probe the recorded pid without sending it a signal.
    fn pid_liveness(&self, pid: u32) -> PidLiveness;
}

pub(super) struct SystemLeaseEnvironment;
impl LeaseEnvironment for SystemLeaseEnvironment {
    fn now_ms(&self) -> i64 {
        loop_ai::now_ms()
    }
    fn pid_liveness(&self, pid: u32) -> PidLiveness {
        pid_liveness(pid)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pid_liveness(pid: u32) -> PidLiveness {
    // Reject process-group semantics and values that cannot fit pid_t.
    let Ok(pid) = i32::try_from(pid) else {
        return PidLiveness::Unknown;
    };
    if pid <= 0 {
        return PidLiveness::Unknown;
    }
    unsafe extern "C" {
        fn kill(pid: std::ffi::c_int, sig: std::ffi::c_int) -> std::ffi::c_int;
    }
    // SAFETY: signal 0 only probes a positive pid and does not send a signal.
    if unsafe { kill(pid, 0) } == 0 {
        return PidLiveness::Alive;
    }
    // ESRCH is 3 on Linux and macOS. EPERM and all other failures are unknown.
    if std::io::Error::last_os_error().raw_os_error() == Some(3) {
        PidLiveness::Dead
    } else {
        PidLiveness::Unknown
    }
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pid_liveness(_: u32) -> PidLiveness {
    PidLiveness::Unknown
}

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
fn conflict(id: &str, lease: &SessionLease) -> SessionError {
    let owner = if lease.identity.owner == "runtime" {
        "Runtime"
    } else {
        &lease.identity.owner
    };
    SessionError::Invalid(format!(
        "Session {id} is in use by {owner} {}. Use `loop attach {id}` instead.",
        lease.identity.pid
    ))
}
fn lost(id: &str) -> SessionError {
    SessionError::Invalid(format!(
        "Session {id} lease ownership was lost; stop writing and cancel work."
    ))
}

pub(super) fn check_write(
    conn: &Connection,
    id: &str,
    identity: Option<&LeaseIdentity>,
) -> Result<(), SessionError> {
    match (read_lease(conn, id)?, identity) {
        (None, None) => Ok(()),
        (Some(lease), Some(identity)) if &lease.identity == identity => Ok(()),
        (Some(lease), None) => Err(conflict(id, &lease)),
        _ => Err(lost(id)),
    }
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
        if identity.owner.is_empty() || identity.pid == 0 || identity.pid > i32::MAX as u32 {
            return Err(SessionError::Invalid(
                "lease requires an owner and a positive process id".into(),
            ));
        }
        let path = self.path.clone();
        let id = id.to_owned();
        let identity = identity.clone();
        let env = Arc::clone(&self.environment);
        tokio::task::spawn_blocking(move || {
            let mut conn = open(&path)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let now = env.now_ms();
            if !tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)", [&id], |row| row.get::<_, bool>(0)).map_err(storage)? {
                return Err(SessionError::NotFound(id));
            }
            if let Some(lease) = read_lease(&tx, &id)? {
                if lease.identity == identity { return Ok(()); }
                let reclaim = runtime_lock_held && identity.owner == "runtime" && lease.identity.owner == "runtime";
                if !takeover || (!reclaim && (
                    now.saturating_sub(lease.heartbeat_ms) < LEASE_STALE_AFTER_MS
                    || env.pid_liveness(lease.identity.pid) != PidLiveness::Dead
                )) { return Err(conflict(&id, &lease)); }
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
        tokio::task::spawn_blocking(move || {
            let mut conn = open(&path)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
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

/// Open the standalone bootstrap store and claim its resumed Session, if any.
/// On Unix, ownership is renewed every 5 s and released when the final store
/// handle drops. Other platforms retain standalone behavior without leases;
/// the explicit lease API remains available with injectable process liveness.
pub async fn create_sqlite_session_store_for_resume(
    path: impl AsRef<std::path::Path>,
    id: Option<&str>,
) -> Result<Arc<dyn crate::harness::session::SessionStore>, SessionError> {
    let store = SqliteSessionStore::open(path)?;
    #[cfg(unix)]
    {
        Ok(Arc::new(match id {
            Some(id) => store.standalone_resume(id).await?,
            None => store.standalone_writer(),
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = id;
        Ok(Arc::new(store))
    }
}

pub(super) struct ResumeLeaseGuard {
    store: SqliteSessionStore,
    ids: parking_lot::Mutex<std::collections::HashSet<String>>,
    identity: LeaseIdentity,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
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
    }
}
impl Drop for ResumeLeaseGuard {
    fn drop(&mut self) {
        // Drop cannot await. Release on the blocking pool, fenced against any successor.
        let path = self.store.path.clone();
        let identity = self.identity.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                if let Ok(conn) = open(&path) {
                    let _ = conn.execute(
                        "DELETE FROM session_leases WHERE owner=?1 AND pid=?2 AND generation=?3",
                        params![identity.owner, identity.pid, identity.generation],
                    );
                }
            });
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
            },
            ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
            identity: identity.clone(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        });
        let weak = Arc::downgrade(&guard);
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
