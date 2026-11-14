//! The present Event Log contract. Live: the Runtime and recovery use it.
//!
//! It moved from `loop_runtime::log` with no change. ADR-0007 §8 replaces it
//! step by step with the traits in [`crate::store`]. Step B2 added the
//! paged reads and the Session summary as provided methods, so a store that
//! does not implement them keeps working.
//!
//! Step B3 (ADR-0007 §7.2): every read takes the tenant first. A read of one
//! Session takes `&Tenant`; there is no default, so a read without a tenant
//! does not compile. A read that is not about one Session takes a
//! [`Scope`]: one tenant, or every tenant with a [`crate::SystemScope`],
//! which only the Runtime's startup code makes. A Session key is unique
//! only within its tenant: `(tenant, session_key)` names a Session, and
//! each tenant has its own `session_seq` space for a key.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use loop_protocol::{SessionKey, TurnId};

use crate::error::Result;
use crate::event::{is_recovery_record, Event, EventKind, LoggedEvent};
use crate::identity::{Identity, Scope, SystemScope, Tenant};
use crate::page::{cut, Cursor, Page, PageLimit};
use crate::store::{summarize, SessionSummary, StoreCapabilities, TenantSession};

/// Payload size of a record for a page's byte limit, in the default
/// implementations: the length of its JSON form.
pub fn record_bytes(record: &LoggedEvent) -> usize {
    serde_json::to_string(&record.event.kind).map_or(0, |text| text.len())
}

/// Whether `record` is one of `tenant`'s records of `session`.
fn of_session(record: &LoggedEvent, tenant: &Tenant, session: &SessionKey) -> bool {
    record.event.session_key.as_ref() == Some(session) && record.event.tenant() == tenant
}

/// Durable Session replay and Runtime audit boundary. Missing cursors start at 0.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Atomically append at most MAX_BATCH_EVENTS; an empty batch is a no-op.
    /// Returned records have committed cursors and re-stamped Update envelopes.
    /// Each record keeps its [`crate::Identity`]; an Update takes the next
    /// `session_seq` of its `(tenant, session)`.
    async fn append_batch(&self, events: Vec<Event>) -> Result<Vec<LoggedEvent>>;
    /// Updates of `tenant`'s Session with session_seq strictly greater than after_seq.
    /// A store with paged reads may read it in pages; the result is one
    /// contiguous run of `session_seq`, through the latest seq at the start.
    ///
    /// Pages are separate read transactions. They join into one consistent
    /// read only because the log is append-only: a committed record never
    /// changes or goes away. A future purge (ADR-0007 B6) must pin the read
    /// or check a tombstone on each page.
    async fn read_after(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
    ) -> Result<Vec<LoggedEvent>>;
    /// All activity in `scope` with global_seq strictly greater than the
    /// supplied cursor.
    ///
    /// Cost: the SQLite store walks the primary key from the cursor and
    /// filters by tenant, so a tenant scope reads every tenant's rows after
    /// the cursor. Production reads use a system scope today. This must be
    /// fixed (an index or a partition per tenant) before shared-cell use.
    async fn read_runtime_after(
        &self,
        scope: Scope<'_>,
        global_seq: u64,
    ) -> Result<Vec<LoggedEvent>>;
    /// Last committed Update sequence of `tenant`'s Session, or zero.
    async fn latest_seq(&self, tenant: &Tenant, session: &SessionKey) -> Result<u64>;
    /// Wait until every append that was queued before this call has
    /// committed or failed, even an append whose caller stopped waiting.
    ///
    /// Reads see every append that returned. They need not see one that
    /// was queued and then abandoned (its future was dropped while the
    /// writer still held it). A caller that must see such appends, for
    /// example before it reads the turn ids of a reloaded Session, calls
    /// this first. The default does nothing: it is right for a store whose
    /// reads already see queued appends, or whose appends are never queued.
    async fn sync(&self) -> Result<()> {
        Ok(())
    }
    /// What this store supports. The default declares none of the ADR-0007
    /// behaviour, so its paged reads below return pages cut from whole reads.
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities::EVENT_LOG_V1
    }
    /// One page of `tenant`'s Session's Updates in `(after_seq, through_seq]`, in
    /// `session_seq` order (ADR-0007 §8 `read_session`). `through_seq: None`
    /// reads to the end. See [`Page`] for `next` and [`PageLimit`] for the
    /// size rule. A read that starts after an append returned sees it.
    /// Pages read one after another rely on the log being append-only, as
    /// [`EventLog::read_after`] says.
    ///
    /// The default cuts the page from [`EventLog::read_after`], so each page
    /// costs a whole read. Stores with `paged_reads` read only the page.
    async fn read_session_page(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        let records = self.read_after(tenant, session, after_seq).await?;
        let through = through_seq.unwrap_or(u64::MAX);
        Ok(cut(
            records
                .into_iter()
                .take_while(|r| r.session_seq.is_some_and(|seq| seq <= through)),
            limit,
            record_bytes,
            |r| Cursor(r.session_seq.unwrap_or(0)),
        ))
    }
    /// `tenant`'s Session's summary (ADR-0007 §8), or `None` when it has no record.
    /// It equals [`summarize`] over the Session's records. The default folds
    /// the whole log; stores keep it as a projection.
    async fn session_summary(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
    ) -> Result<Option<SessionSummary>> {
        let records = self.read_runtime_after(Scope::Tenant(tenant), 0).await?;
        Ok(
            summarize(records.iter().filter(|r| of_session(r, tenant, session)))
                .remove(&(tenant.clone(), session.clone()))
                .map(|(summary, _)| summary),
        )
    }
    /// The owner of record of `tenant`'s Session (step B3): the identity of
    /// the `session_opened` that created it, from the summary. `None` when
    /// the log has no creating record (a Session made before B3 or outside
    /// the Runtime). Groundwork for access checks (I3); it decides nothing.
    async fn session_owner(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
    ) -> Result<Option<Identity>> {
        Ok(self
            .session_summary(tenant, session)
            .await?
            .and_then(|summary| summary.owner))
    }
    /// One page of [`EventLog::recovery_records`] with `global_seq` strictly
    /// greater than `after_global`. `next` is a `global_seq`. A tenant scope
    /// has the cost of [`EventLog::read_runtime_after`]: it must be fixed
    /// before shared-cell use.
    ///
    /// The default cuts the page from a whole `recovery_records` read.
    async fn recovery_page(
        &self,
        scope: Scope<'_>,
        from_generation: u64,
        after_global: u64,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        let records = self.recovery_records(scope, from_generation).await?;
        Ok(cut(
            records.into_iter().filter(|r| r.global_seq > after_global),
            limit,
            record_bytes,
            |r| Cursor(r.global_seq),
        ))
    }
    /// Transactionally allocate a persistent Runtime generation, starting at one.
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> Result<u64>;
    /// Record a clean stop for a previously allocated generation.
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> Result<()>;
    /// Turn boundaries (`turn_started`, `turn_ended` Updates), prompts sent,
    /// clean stops and Escalation records in `scope`, of generations at or
    /// after `from_generation`, in `global_seq` order. Runtime start-up
    /// recovery reads with a system scope. Implementations should filter in
    /// storage; the default scans the log.
    async fn recovery_records(
        &self,
        scope: Scope<'_>,
        from_generation: u64,
    ) -> Result<Vec<LoggedEvent>> {
        Ok(self
            .read_runtime_after(scope, 0)
            .await?
            .into_iter()
            .filter(|r| r.event.generation >= from_generation && is_recovery_record(&r.event.kind))
            .collect())
    }
    /// Newest generation whose crash recovery committed, or 0. Records of older
    /// generations are already settled. The default never remembers one.
    /// Generations belong to the Runtime, so only a system scope reads them.
    async fn last_recovered_generation(&self, _scope: &SystemScope) -> Result<u64> {
        Ok(0)
    }
    /// Remember that this generation's recovery committed.
    async fn mark_recovered(&self, _generation: u64, _recovered_ms: i64) -> Result<()> {
        Ok(())
    }
    /// Sessions in `scope` whose opening recorded a parent: the Runtime's
    /// Subagents, each with its tenant.
    /// Implementations should filter in storage; the default scans the log.
    async fn subagent_sessions(&self, scope: Scope<'_>) -> Result<HashSet<TenantSession>> {
        Ok(self
            .read_runtime_after(scope, 0)
            .await?
            .into_iter()
            .filter_map(|record| match &record.event.kind {
                EventKind::SessionOpened(payload) if !payload["parent"].is_null() => record
                    .event
                    .session_key
                    .map(|key| (record.event.identity.tenant, key)),
                _ => None,
            })
            .collect())
    }
    /// How many prompts `tenant`'s Session was sent. Each turn starts from one, so
    /// this bounds the turns begun even when a turn left no Update.
    /// Implementations should count in storage; the default scans the log.
    async fn prompts_sent(&self, tenant: &Tenant, session: &SessionKey) -> Result<u64> {
        Ok(self
            .read_runtime_after(Scope::Tenant(tenant), 0)
            .await?
            .iter()
            .filter(|r| {
                of_session(r, tenant, session) && matches!(r.event.kind, EventKind::PromptSent(_))
            })
            .count() as u64)
    }
    /// Distinct turn ids of `tenant`'s Session's records (all kinds), in first-seen
    /// order. Implementations should answer without decoding payloads; the
    /// default scans the whole log.
    async fn turn_ids(&self, tenant: &Tenant, session: &SessionKey) -> Result<Vec<TurnId>> {
        let records = self.read_runtime_after(Scope::Tenant(tenant), 0).await?;
        Ok(
            summarize(records.iter().filter(|r| of_session(r, tenant, session)))
                .remove(&(tenant.clone(), session.clone()))
                .map(|(_, turns)| turns)
                .unwrap_or_default(),
        )
    }
    /// Newest event timestamp per Session in `scope`. Implementations should
    /// answer with an aggregate query; the default scans the whole log.
    async fn last_activity_ms(&self, scope: Scope<'_>) -> Result<HashMap<TenantSession, i64>> {
        let mut activity = HashMap::new();
        for record in self.read_runtime_after(scope, 0).await? {
            if let Some(key) = record.event.session_key {
                let ts = activity
                    .entry((record.event.identity.tenant, key))
                    .or_insert(record.event.ts_ms);
                *ts = (*ts).max(record.event.ts_ms);
            }
        }
        Ok(activity)
    }
}
