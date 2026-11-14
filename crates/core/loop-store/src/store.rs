//! The store traits after ADR-0007 (§8).
//!
//! The traits are defined for later steps. No type implements them yet, and
//! nothing calls them. Steps B1 to B7 add the implementations; each method
//! notes its step.
//!
//! Live from step B2: [`SessionSummary`] and its fold ([`SummaryDelta`],
//! [`summarize`]). The SQLite and memory Event Logs keep the summary in the
//! transaction of each append, and `EventLog::session_summary` reads it.

use std::collections::BTreeMap;

use async_trait::async_trait;
use loop_protocol::{SessionKey, SessionUpdate, TurnId, UpdateEnvelope};

use crate::append::Append;
use crate::error::Result;
use crate::event::{opened_creates, Committed, EventKind, LoggedEvent};
use crate::identity::{Identity, SystemScope, Tenant};
use crate::lease::{Fence, Generation, OwnerInfo};
use crate::page::{Page, PageLimit};
use crate::tree::{SessionMetadata, SessionTreeEntry};

/// Small per-Session summary that the projection keeps (step B2).
///
/// Every field is a fold over the Session's records in `global_seq` order
/// ([`SessionSummary::apply`]). A store keeps it in the transaction of the
/// append, so a read after an append sees the append in the summary too.
///
/// Step B3: a summary is keyed by `(tenant, session)`. The same Session key
/// in two tenants is two Sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The Session's tenant.
    pub tenant: Tenant,
    /// The Session.
    pub session: SessionKey,
    /// The last `session_seq` (Updates only), or 0.
    pub last_seq: u64,
    /// The revision of the last `config_changed` or `branch_reset` Update,
    /// in log order (not the largest), or 0.
    pub last_revision: u64,
    /// Newest timestamp (unix ms) of any record of the Session.
    pub last_activity_ms: i64,
    /// `prompt_sent` records of the Session.
    pub prompts_sent: u64,
    /// Distinct turn ids of the Session's records (all kinds, not only
    /// Updates).
    pub turn_count: u64,
    /// The parent of a Subagent: set by the first `session_opened` whose
    /// `parent` is not null, and never cleared. A parent that is not a
    /// string is kept as its JSON text.
    pub parent: Option<SessionKey>,
    /// The owner of record (step B3): the identity of the `session_opened`
    /// that created the Session ([`crate::event::SESSION_CREATED_KEY`]),
    /// set once and never changed. `None` when no creating record is in the
    /// log: a Session created before step B3, or outside the Runtime. This
    /// is not an access check; the Runtime decides access (task I3).
    pub owner: Option<Identity>,
}

/// What one record changes in its Session's summary. Records without a
/// Session change nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryDelta {
    /// The record's tenant.
    pub tenant: Tenant,
    /// The record's Session.
    pub session: SessionKey,
    /// Its `session_seq` (Updates).
    pub seq: Option<u64>,
    /// Its timestamp.
    pub ts_ms: i64,
    /// The revision of a `config_changed` or `branch_reset` Update.
    pub revision: Option<u64>,
    /// It is a `prompt_sent`.
    pub prompt: bool,
    /// Its turn.
    pub turn: Option<TurnId>,
    /// The parent of a `session_opened` with a parent.
    pub parent: Option<SessionKey>,
    /// The record's identity, when it is the `session_opened` that created
    /// the Session.
    pub owner: Option<Identity>,
}

impl SummaryDelta {
    /// The change of one committed record, or `None` without a Session.
    pub fn of(record: &LoggedEvent) -> Option<Self> {
        let event = &record.event;
        let session = event.session_key.clone()?;
        let revision = match &event.kind {
            EventKind::Update(UpdateEnvelope {
                update: SessionUpdate::ConfigChanged(changed),
                ..
            }) => Some(changed.revision),
            EventKind::Update(UpdateEnvelope {
                update: SessionUpdate::BranchReset { revision, .. },
                ..
            }) => Some(*revision),
            _ => None,
        };
        let parent = match &event.kind {
            EventKind::SessionOpened(payload) => match payload.get("parent") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(key)) => Some(SessionKey::from(key.as_str())),
                Some(other) => Some(SessionKey::from(other.to_string())),
            },
            _ => None,
        };
        let owner = match &event.kind {
            EventKind::SessionOpened(payload) if opened_creates(payload) => {
                Some(event.identity.clone())
            }
            _ => None,
        };
        Some(Self {
            tenant: event.identity.tenant.clone(),
            session,
            seq: record.session_seq,
            ts_ms: event.ts_ms,
            revision,
            prompt: matches!(event.kind, EventKind::PromptSent(_)),
            turn: event.turn.clone(),
            parent,
            owner,
        })
    }
}

impl SessionSummary {
    /// The summary of a Session with no records.
    pub fn empty(tenant: Tenant, session: SessionKey) -> Self {
        Self {
            tenant,
            session,
            last_seq: 0,
            last_revision: 0,
            last_activity_ms: i64::MIN,
            prompts_sent: 0,
            turn_count: 0,
            parent: None,
            owner: None,
        }
    }

    /// Apply the next record of the Session. `new_turn` is true when the
    /// delta's turn was not seen before in this Session; the caller keeps
    /// the set of turns.
    pub fn apply(&mut self, delta: &SummaryDelta, new_turn: bool) {
        if let Some(seq) = delta.seq {
            self.last_seq = self.last_seq.max(seq);
        }
        self.last_activity_ms = self.last_activity_ms.max(delta.ts_ms);
        if let Some(revision) = delta.revision {
            self.last_revision = revision;
        }
        if delta.prompt {
            self.prompts_sent += 1;
        }
        if new_turn && delta.turn.is_some() {
            self.turn_count += 1;
        }
        if self.parent.is_none() {
            self.parent.clone_from(&delta.parent);
        }
        if self.owner.is_none() {
            self.owner.clone_from(&delta.owner);
        }
    }
}

/// A Session in its tenant: the key of every per-Session projection row.
pub type TenantSession = (Tenant, SessionKey);

/// The summary of each Session and its turn ids (first-seen order), folded
/// from records in `global_seq` order, keyed by `(tenant, session)`. It is
/// the reference that a store's projection must equal.
pub fn summarize<'a>(
    records: impl IntoIterator<Item = &'a LoggedEvent>,
) -> BTreeMap<TenantSession, (SessionSummary, Vec<TurnId>)> {
    let mut sessions: BTreeMap<TenantSession, (SessionSummary, Vec<TurnId>)> = BTreeMap::new();
    for record in records {
        let Some(delta) = SummaryDelta::of(record) else {
            continue;
        };
        let (summary, turns) = sessions
            .entry((delta.tenant.clone(), delta.session.clone()))
            .or_insert_with(|| {
                (
                    SessionSummary::empty(delta.tenant.clone(), delta.session.clone()),
                    Vec::new(),
                )
            });
        let new_turn = match &delta.turn {
            Some(turn) if !turns.contains(turn) => {
                turns.push(turn.clone());
                true
            }
            _ => false,
        };
        summary.apply(&delta, new_turn);
    }
    sessions
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
        limit: PageLimit,
    ) -> Result<Page<Committed>>;
    /// Last committed `session_seq`, or 0. Reads the `events_session_seq` index.
    async fn latest_seq(&self, t: &Tenant, s: &SessionKey) -> Result<u64>;
    /// Small per-Session summary that the projection keeps:
    /// last revision, last activity, prompts sent, turn count, parent.
    async fn session_summary(&self, t: &Tenant, s: &SessionKey) -> Result<Option<SessionSummary>>;
    /// Summaries of a tenant's Sessions, in key order, paged by key.
    async fn summaries(
        &self,
        t: &Tenant,
        after: Option<SessionKey>,
        limit: u32,
    ) -> Result<Page<SessionSummary, SessionKey>>;
    /// Session-scoped recovery (§2.5): records of earlier lease epochs
    /// after the Session's recovery mark.
    async fn session_recovery_records(
        &self,
        t: &Tenant,
        s: &SessionKey,
        below_epoch: u64,
        after_global: u64,
        limit: PageLimit,
    ) -> Result<Page<Committed>>;
    /// The Session's recovery mark (a `global_seq`), or 0.
    async fn recovered_through(&self, t: &Tenant, s: &SessionKey) -> Result<u64>;
    /// All records across tenants, in `global_seq` order, paged.
    async fn read_global(
        &self,
        scope: &SystemScope,
        after_global: u64,
        limit: PageLimit,
    ) -> Result<Page<Committed>>;
    /// Runtime-level recovery: generations with owner = 'runtime' only.
    async fn recovery_records(
        &self,
        scope: &SystemScope,
        from: Generation,
        after_global: u64,
        limit: PageLimit,
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
    ) -> Result<Page<SessionMetadata, String>>;
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
