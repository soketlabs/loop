//! The store traits after ADR-0007 (§8).
//!
//! Defined for later steps. No type implements them yet, and nothing calls
//! them. Steps B1 to B7 add the implementations; each method notes its step.

use async_trait::async_trait;
use loop_protocol::SessionKey;

use crate::append::Append;
use crate::error::Result;
use crate::event::Committed;
use crate::identity::{SystemScope, Tenant};
use crate::lease::{Fence, Generation, OwnerInfo};
use crate::page::Page;
use crate::tree::{SessionMetadata, SessionTreeEntry};

/// Small per-Session summary that the projection keeps (step B2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The Session.
    pub session: SessionKey,
    /// The last revision of `config_changed` or `branch_reset`.
    pub last_revision: u64,
    /// Newest record timestamp (unix ms).
    pub last_activity_ms: i64,
    /// Prompts sent to the Session.
    pub prompts_sent: u64,
    /// Distinct turns of its updates.
    pub turn_count: u64,
    /// The parent Session of a Subagent.
    pub parent: Option<SessionKey>,
}

/// What a projection rebuild did (step B6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Records applied.
    pub records_applied: u64,
    /// Sessions in the rebuilt projection.
    pub sessions: u64,
}

/// What a projection verify found (step B5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Sessions compared.
    pub sessions_checked: u64,
    /// One line per difference between the rebuilt and the live rows.
    pub differences: Vec<String>,
}

impl VerifyReport {
    /// No difference was found.
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty()
    }
}

/// What a store implementation supports. During the migration a store can
/// declare less than the full contract, and the Runtime and the conformance
/// kit can read it. The full contract is [`StoreCapabilities::FULL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StoreCapabilities {
    /// `read_session` and the other reads return bounded pages (B2).
    pub paged_reads: bool,
    /// Reads use reader connections, never the writer (B2).
    pub reader_pool: bool,
    /// Each read is scoped by `&Tenant` (B3).
    pub tenant_scoped: bool,
    /// `append` checks a `Fence` in the same transaction (B4).
    pub fenced_append: bool,
    /// The projection is applied in the transaction of the append (B5).
    pub same_transaction_projection: bool,
    /// Recovery of Session records is scoped by lease epoch (B8).
    pub session_scoped_recovery: bool,
}

impl StoreCapabilities {
    /// The present `EventLog` stores: none of the new behaviour.
    pub const EVENT_LOG_V1: Self = Self {
        paged_reads: false,
        reader_pool: false,
        tenant_scoped: false,
        fenced_append: false,
        same_transaction_projection: false,
        session_scoped_recovery: false,
    };
    /// Everything that ADR-0007 asks of a store.
    pub const FULL: Self = Self {
        paged_reads: true,
        reader_pool: true,
        tenant_scoped: true,
        fenced_append: true,
        same_transaction_projection: true,
        session_scoped_recovery: true,
    };
}

/// The write side.
#[async_trait]
pub trait EventWriter: Send + Sync {
    /// Returns after commit. Gives `global_seq` and `session_seq`.
    /// Applies the projection in the same transaction.
    async fn append(&self, req: Append) -> Result<Vec<Committed>>;
    /// Allocate a generation for an owner of any kind.
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        started_ms: i64,
    ) -> Result<Generation>;
    /// Record a clean stop.
    async fn finish_generation(&self, g: Generation, stopped_ms: i64) -> Result<()>;
    /// Remember that this generation's Runtime-level recovery committed.
    async fn mark_recovered(&self, g: Generation, at_ms: i64) -> Result<()>;
}

/// The read side. Reader connections, never the writer.
#[async_trait]
pub trait EventReader: Send + Sync {
    /// Durable updates of one Session in `(after_seq, through_seq]`, paged.
    async fn read_session(
        &self,
        t: &Tenant,
        s: &SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: u32,
    ) -> Result<Page<Committed>>;
    /// Last committed `session_seq`, or 0. Reads the `events_session_seq` index.
    async fn latest_seq(&self, t: &Tenant, s: &SessionKey) -> Result<u64>;
    /// Small per-Session summary that the projection keeps:
    /// last revision, last activity, prompts sent, turn count, parent.
    async fn session_summary(&self, t: &Tenant, s: &SessionKey) -> Result<Option<SessionSummary>>;
    /// Summaries of a tenant's Sessions, in key order, paged.
    async fn summaries(
        &self,
        t: &Tenant,
        after: Option<SessionKey>,
        limit: u32,
    ) -> Result<Page<SessionSummary>>;
    /// Session-scoped recovery (§2.5): records of earlier lease epochs
    /// after the Session's recovery mark.
    async fn session_recovery_records(
        &self,
        t: &Tenant,
        s: &SessionKey,
        below_epoch: u64,
        after_global: u64,
        limit: u32,
    ) -> Result<Page<Committed>>;
    /// The Session's recovery mark (a `global_seq`), or 0.
    async fn recovered_through(&self, t: &Tenant, s: &SessionKey) -> Result<u64>;
    /// All records across tenants, in `global_seq` order, paged.
    async fn read_global(
        &self,
        scope: &SystemScope,
        after_global: u64,
        limit: u32,
    ) -> Result<Page<Committed>>;
    /// Runtime-level recovery: generations with owner = 'runtime' only.
    async fn recovery_records(
        &self,
        scope: &SystemScope,
        from: Generation,
        after_global: u64,
        limit: u32,
    ) -> Result<Page<Committed>>;
    /// Newest generation whose Runtime-level recovery committed.
    async fn last_recovered_generation(&self, scope: &SystemScope) -> Result<Generation>;
}

/// Reads of the Session tree projection.
#[async_trait]
pub trait TreeReader: Send + Sync {
    /// One Session's metadata.
    async fn session(&self, t: &Tenant, id: &str) -> Result<Option<SessionMetadata>>;
    /// A tenant's Sessions, optionally of one cwd, paged by Session id.
    async fn list(
        &self,
        t: &Tenant,
        cwd: Option<&str>,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Page<SessionMetadata>>;
    /// The head entry id.
    async fn head(&self, t: &Tenant, id: &str) -> Result<Option<String>>;
    /// One entry.
    async fn entry(&self, t: &Tenant, id: &str, entry: &str) -> Result<Option<SessionTreeEntry>>;
    /// Entries after an `entry_seq`, paged.
    async fn entries(
        &self,
        t: &Tenant,
        id: &str,
        after_entry_seq: u64,
        limit: u32,
    ) -> Result<Page<SessionTreeEntry>>;
    /// The path from `leaf` (or the head) to the root or the last compaction.
    async fn path_to_root_or_compaction(
        &self,
        t: &Tenant,
        id: &str,
        leaf: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>>;
}

/// Session leases. Chain D owns the details.
#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// Take a free or stale lease.
    async fn acquire(&self, t: &Tenant, s: &SessionKey, owner: &OwnerInfo) -> Result<Fence>;
    /// Take over a lease whose owner is proven dead.
    async fn takeover(&self, t: &Tenant, s: &SessionKey, owner: &OwnerInfo) -> Result<Fence>;
    /// Renew a held lease.
    async fn renew(&self, f: &Fence) -> Result<()>;
    /// Release a held lease.
    async fn release(&self, f: &Fence) -> Result<()>;
    /// Remove stale leases. Returns how many.
    async fn sweep_stale(&self, scope: &SystemScope) -> Result<usize>;
}

/// Projection maintenance.
#[async_trait]
pub trait ProjectionAdmin: Send + Sync {
    /// Rebuild the projection of one tenant, or of all.
    async fn rebuild(&self, scope: &SystemScope, t: Option<&Tenant>) -> Result<RebuildReport>;
    /// Rebuild into temporary tables and compare with the live rows.
    async fn verify(&self, scope: &SystemScope) -> Result<VerifyReport>;
}

/// The whole store, as `Runtime::new` will take it.
pub trait Store: EventWriter + EventReader + TreeReader + LeaseStore + ProjectionAdmin {}

impl<T> Store for T where T: EventWriter + EventReader + TreeReader + LeaseStore + ProjectionAdmin {}
