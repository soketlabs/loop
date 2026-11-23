//! Session leases, control markers and destinations, as the Runtime uses them.
//!
//! The Runtime owns its Sessions through these traits, not through a concrete
//! store. [`SessionLeases`] holds the lease functions, [`SessionControl`]
//! holds the control markers, the child Sessions and the destinations, and
//! [`RuntimeSessionStore`] is both of them plus [`SessionStore`]. The SQLite
//! store and the in-memory store implement them.
//!
//! The lease rules (stale age, pid proof, the Runtime-to-Runtime reclaim, the
//! fenced write check) and the error texts live here once, so that every
//! store applies the same rules.
//!
//! These are the ADR-0007 step B1 traits. Steps B4 and D1 replace the lease
//! half with `loop_store::LeaseStore` (fences with an epoch, `StoreError`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::harness::session::types::{
    PendingSessionWrite, SessionReader, SessionStore, SessionTreeEntry,
};
use crate::harness::session::{SessionFile, TOOL_APPROVAL_LABEL_NAMESPACE};
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

/// The system clock and the conservative process-liveness probe.
pub(crate) struct SystemLeaseEnvironment;
impl LeaseEnvironment for SystemLeaseEnvironment {
    fn now_ms(&self) -> i64 {
        loop_ai::now_ms()
    }
    fn pid_liveness(&self, pid: u32) -> PidLiveness {
        pid_liveness(pid)
    }
}

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

/// Another owner holds the Session.
pub(crate) fn conflict(id: &str, lease: &SessionLease) -> SessionError {
    let owner = if lease.identity.owner == "runtime" {
        "Runtime"
    } else {
        &lease.identity.owner
    };
    SessionError::InUse(format!(
        "Session {id} is in use by {owner} {}. Use `loop session attach {id}` instead.",
        lease.identity.pid
    ))
}

/// The caller's lease is gone or was replaced.
pub(crate) fn lost(id: &str) -> SessionError {
    SessionError::Invalid(format!(
        "Session {id} lease ownership was lost; stop writing and cancel work."
    ))
}

/// The fenced write rule: an unleased writer writes only an unleased
/// Session, and a leased writer only a Session whose lease is its own.
pub(crate) fn check_write_rule(
    lease: Option<&SessionLease>,
    id: &str,
    identity: Option<&LeaseIdentity>,
) -> Result<(), SessionError> {
    match (lease, identity) {
        (None, None) => Ok(()),
        (Some(lease), Some(identity)) if &lease.identity == identity => Ok(()),
        (Some(lease), None) => Err(conflict(id, lease)),
        _ => Err(lost(id)),
    }
}

/// A claimant must name an owner and a positive pid that fits pid_t.
pub(crate) fn check_claimant(identity: &LeaseIdentity) -> Result<(), SessionError> {
    if identity.owner.is_empty() || identity.pid == 0 || identity.pid > i32::MAX as u32 {
        return Err(SessionError::Invalid(
            "lease requires an owner and a positive process id".into(),
        ));
    }
    Ok(())
}

/// What a claim does with the present lease row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claim {
    /// The claimant holds it already: change nothing.
    Held,
    /// Write the claimant's row (insert or replace).
    Write,
}

/// Judge a claim against the present lease. A plain acquire takes only a
/// free Session. A takeover replaces a lease whose heartbeat is stale and
/// whose pid is proven dead. Only Runtime-to-Runtime reclaim under the held
/// Runtime lock bypasses age and pid liveness.
pub(crate) fn judge_claim(
    id: &str,
    present: Option<&SessionLease>,
    identity: &LeaseIdentity,
    takeover: bool,
    runtime_lock_held: bool,
    now: i64,
    env: &dyn LeaseEnvironment,
) -> Result<Claim, SessionError> {
    let Some(lease) = present else {
        return Ok(Claim::Write);
    };
    if &lease.identity == identity {
        return Ok(Claim::Held);
    }
    let reclaim =
        runtime_lock_held && identity.owner == "runtime" && lease.identity.owner == "runtime";
    if !takeover
        || (!reclaim
            && (now.saturating_sub(lease.heartbeat_ms) < LEASE_STALE_AFTER_MS
                || env.pid_liveness(lease.identity.pid) != PidLiveness::Dead))
    {
        return Err(conflict(id, lease));
    }
    Ok(Claim::Write)
}

/// Whether a sweep removes this lease. `proof` is true when the caller holds
/// the Runtime lock of the store's ownership domain.
pub(crate) fn sweepable(
    lease: &SessionLease,
    proof: bool,
    now: i64,
    env: &dyn LeaseEnvironment,
) -> bool {
    let stale = now.saturating_sub(lease.heartbeat_ms) >= LEASE_STALE_AFTER_MS;
    if !stale || (lease.identity.owner == "runtime" && !proof) {
        return false;
    }
    env.pid_liveness(lease.identity.pid) == PidLiveness::Dead
}

/// Whether two paths name the same database file.
pub(crate) fn same_database(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => std::path::absolute(a).ok() == std::path::absolute(b).ok(),
    }
}

/// The write that copies one entry of a captured branch into a destination,
/// or `None` for an entry that a copy never carries. `remap` maps source
/// entry ids to the ids already written.
pub(crate) fn destination_write(
    entry: SessionTreeEntry,
    remap: &HashMap<String, String>,
) -> Option<PendingSessionWrite> {
    Some(match entry {
        SessionTreeEntry::Message { message, .. } => PendingSessionWrite::Message { message },
        SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
            PendingSessionWrite::ThinkingLevelChange { thinking_level }
        }
        SessionTreeEntry::ModelChange {
            provider, model_id, ..
        } => PendingSessionWrite::ModelChange { provider, model_id },
        SessionTreeEntry::ActiveToolsChange { tool_names, .. } => {
            PendingSessionWrite::ActiveToolsChange { tool_names }
        }
        // Approval grants belong to the source Session, never to a copy.
        SessionTreeEntry::Label { label, .. }
            if label.starts_with(TOOL_APPROVAL_LABEL_NAMESPACE) =>
        {
            return None
        }
        SessionTreeEntry::Label { label, .. } => PendingSessionWrite::Label { label },
        SessionTreeEntry::SessionInfo { .. } => return None,
        SessionTreeEntry::Compaction {
            summary,
            first_kept_entry_id,
            details,
            ..
        } => PendingSessionWrite::Compaction {
            summary,
            first_kept_entry_id: first_kept_entry_id.and_then(|id| remap.get(&id).cloned()),
            // Only the shortened tool outputs (by new entry id), so the copy builds the same context.
            details: crate::harness::compaction::remap_details(
                details.map(|d| {
                    serde_json::json!({"shortened": d.get("shortened").cloned().unwrap_or_default()})
                }),
                |old| remap.get(old).cloned(),
            ),
        },
        SessionTreeEntry::BranchSummary { summary, .. } => {
            PendingSessionWrite::BranchSummary { summary }
        }
        // Imported paths, custom data and grants never confer authority.
        _ => return None,
    })
}

/// Persisted branch revision and an unannounced mutation. The Session
/// revision is counted from the tree itself.
#[derive(Debug, Clone, Default)]
pub struct SessionControlState {
    /// Committed active-branch revision.
    pub branch_revision: u64,
    /// Marker of a committed mutation not yet announced in the Event Log.
    pub pending: Option<String>,
}

/// Session leases, as the Runtime and standalone owners use them.
///
/// Every method is fenced by the full [`LeaseIdentity`]. Errors are the
/// store's `SessionError`: `InUse` when another owner holds the Session,
/// `Invalid` when the caller's lease was lost, `NotFound` for an unknown
/// Session and `Storage` for a transient failure that a renewal retries.
#[async_trait]
pub trait SessionLeases: Send + Sync {
    /// The database file whose Runtime lock (`SessionDatabaseLock`) proves
    /// ownership of this store's leases, or `None` when the store has none.
    ///
    /// This is a SQLite-specific proof. `Runtime::recover(Some(lock))`
    /// compares the lock's path with it, and `sweep_stale_leases` gets the
    /// lock's path; so `recover` with a lock fails for a store that returns
    /// `None`. ADR-0007 steps B4 and D1 replace it with a proof on the store
    /// side: `LeaseStore::sweep_stale(&SystemScope)` and the `Fence` epoch.
    fn database_path(&self) -> Option<&Path>;

    /// A writer bound to this identity. Each append and delete through it
    /// rechecks the full identity atomically; a missing or replaced lease
    /// fences the write.
    fn for_lease(&self, identity: LeaseIdentity) -> Arc<dyn SessionStore>;

    /// Read ownership without acquiring a lease (listing remains read-only).
    async fn lease(&self, id: &str) -> Result<Option<SessionLease>, SessionError>;

    /// Refuse standalone resume while the Session is owned by another writer.
    /// Stale owners must first be replaced through `takeover_lease`.
    async fn check_resume(&self, id: &str) -> Result<(), SessionError> {
        if let Some(lease) = self.lease(id).await? {
            return Err(conflict(id, &lease));
        }
        Ok(())
    }

    /// Acquire an absent lease. Re-acquiring the same identity is idempotent.
    async fn acquire_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError>;

    /// Replace a stale lease only when its pid is dead. With
    /// `runtime_lock_held` (the caller holds the Runtime lock of this
    /// store's ownership domain), a Runtime reclaims another Runtime's lease
    /// without the age and pid checks.
    async fn takeover_lease(
        &self,
        id: &str,
        identity: &LeaseIdentity,
        runtime_lock_held: bool,
    ) -> Result<(), SessionError>;

    /// Renew only the full matching identity. Failure means the caller must
    /// stop new writes and cancel its work. Schedule this every
    /// [`LEASE_RENEW_INTERVAL_MS`].
    async fn renew_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError>;

    /// Release only the full matching identity; an old owner cannot release
    /// its successor.
    async fn release_lease(&self, id: &str, identity: &LeaseIdentity) -> Result<(), SessionError>;

    /// Delete leases whose owner is gone, and return how many. A lease goes
    /// only when it is stale and its pid is proven dead; a Runtime's lease
    /// also needs `runtime_lock_for` to name [`Self::database_path`].
    async fn sweep_stale_leases(
        &self,
        runtime_lock_for: Option<&Path>,
    ) -> Result<usize, SessionError>;
}

/// Control markers, child Sessions and destinations of the Runtime.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Create a Session whose parent is recorded with it, as for a Subagent.
    /// Unlike `fork`, no parent entries are copied.
    async fn create_child(
        &self,
        cwd: Option<String>,
        name: Option<String>,
        parent: &str,
    ) -> Result<Arc<dyn SessionReader>, SessionError>;

    /// Read revisions without acquiring a writer lease.
    async fn control_state(&self, id: &str) -> Result<SessionControlState, SessionError>;

    /// Commit a tree or metadata write and its recovery marker atomically,
    /// fenced by `identity`. The caller has checked the Session revision
    /// under its own lock.
    async fn control_mutation(
        &self,
        id: &str,
        identity: LeaseIdentity,
        branch: bool,
        write: PendingSessionWrite,
        marker: String,
    ) -> Result<SessionControlState, SessionError>;

    /// Clear only the marker that was durably announced in the Event Log.
    async fn acknowledge_control(&self, id: &str, marker: &str) -> Result<(), SessionError>;

    /// Read a destination retry receipt without changing ownership or capacity.
    async fn destination_receipt(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<(String, String)>, SessionError>;

    /// Atomically create a validated branch and its retry receipt, with its
    /// own lease for `identity`.
    #[allow(clippy::too_many_arguments)]
    async fn create_destination(
        &self,
        file: SessionFile,
        cwd: String,
        parent: Option<String>,
        request_key: String,
        fingerprint: String,
        result: String,
        identity: LeaseIdentity,
    ) -> Result<DestinationCreation, SessionError>;
}

/// Everything the Runtime needs of its Session store.
pub trait RuntimeSessionStore: SessionStore + SessionLeases + SessionControl {}

impl<T> RuntimeSessionStore for T where T: SessionStore + SessionLeases + SessionControl + ?Sized {}

/// Owns an unadmitted destination; cancellation removes its row and lease.
pub struct DestinationCreation {
    /// The destination Session id.
    pub id: String,
    /// The retry receipt committed with it.
    pub receipt: String,
    store: Arc<dyn RuntimeSessionStore>,
    identity: LeaseIdentity,
    armed: bool,
}
impl DestinationCreation {
    /// For the stores of this crate. `armed` is true for a destination that
    /// this call created, false for one found by its receipt. An armed value
    /// deletes the Session when it is dropped before `admitted`.
    pub(crate) fn new(
        id: String,
        receipt: String,
        store: Arc<dyn RuntimeSessionStore>,
        identity: LeaseIdentity,
        armed: bool,
    ) -> Self {
        Self {
            id,
            receipt,
            store,
            identity,
            armed,
        }
    }
    /// Transfer responsibility to the admitted Runtime Session.
    pub fn admitted(&mut self) {
        self.armed = false;
    }
    /// Remove an unadmitted destination, whether admission released its lease or not.
    pub async fn rollback(&mut self) -> Result<(), SessionError> {
        if !self.armed {
            return Ok(());
        }
        rollback_destination(self.store.as_ref(), &self.id, &self.identity).await?;
        self.armed = false;
        Ok(())
    }
}
async fn rollback_destination(
    store: &dyn RuntimeSessionStore,
    id: &str,
    identity: &LeaseIdentity,
) -> Result<(), SessionError> {
    match store.lease(id).await? {
        Some(lease) if &lease.identity == identity => store.release_lease(id, identity).await?,
        Some(_) => {
            return Err(SessionError::Invalid(
                "destination has another writer; refusing rollback".into(),
            ))
        }
        None => {}
    }
    store.delete(id).await
}
impl Drop for DestinationCreation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let store = Arc::clone(&self.store);
        let identity = self.identity.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            if let Err(error) = rollback_destination(store.as_ref(), &id, &identity).await {
                eprintln!("could not remove unadmitted Session {id}: {error}");
            }
        });
    }
}
