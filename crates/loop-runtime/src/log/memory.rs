use super::*;
use async_trait::async_trait;
use loop_protocol::{SessionKey, TurnId};
use loop_store::page::cut;
use loop_store::{
    record_bytes, Cursor, Page, PageLimit, Scope, SessionSummary, StoreCapabilities, SystemScope,
    Tenant, TenantSession,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

/// One Session's index and summary.
#[derive(Default)]
struct SessionState {
    /// Positions in `State::events` of the Session's Updates; the Update
    /// with `session_seq` n is at index n - 1.
    updates: Vec<usize>,
    summary: Option<SessionSummary>,
    turns: Vec<TurnId>,
}

#[derive(Default)]
struct State {
    /// Each record with its payload size, for the byte limit of a page.
    events: Vec<(LoggedEvent, usize)>,
    /// Keyed by `(tenant, session)`: the same key in two tenants is two
    /// Sessions.
    sessions: HashMap<TenantSession, SessionState>,
    generations: Vec<Option<i64>>,
    recovered: u64,
}

impl State {
    /// Index the record and fold it into its Session's summary.
    fn apply(&mut self, record: &LoggedEvent, index: usize) {
        let Some(delta) = loop_store::SummaryDelta::of(record) else {
            return;
        };
        let session = self
            .sessions
            .entry((delta.tenant.clone(), delta.session.clone()))
            .or_default();
        if record.session_seq.is_some() {
            session.updates.push(index);
        }
        let new_turn = match &delta.turn {
            Some(turn) if !session.turns.contains(turn) => {
                session.turns.push(turn.clone());
                true
            }
            _ => false,
        };
        session
            .summary
            .get_or_insert_with(|| {
                SessionSummary::empty(delta.tenant.clone(), delta.session.clone())
            })
            .apply(&delta, new_turn);
    }

    fn session(&self, tenant: &Tenant, session: &SessionKey) -> Option<&SessionState> {
        self.sessions.get(&(tenant.clone(), session.clone()))
    }

    fn latest(&self, tenant: &Tenant, session: &SessionKey) -> u64 {
        self.session(tenant, session)
            .map_or(0, |s| s.updates.len() as u64)
    }

    fn session_page(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
        through_seq: u64,
        limit: PageLimit,
    ) -> Page<LoggedEvent> {
        let Some(state) = self.session(tenant, session) else {
            return Page::last(Vec::new());
        };
        let start = usize::try_from(after_seq).unwrap_or(usize::MAX);
        let end = usize::try_from(through_seq)
            .unwrap_or(usize::MAX)
            .min(state.updates.len());
        let indices = state.updates.get(start..end).unwrap_or(&[]);
        let page = cut(
            indices.iter().map(|&i| &self.events[i]),
            limit,
            |(_, bytes)| *bytes,
            |(record, _)| Cursor(record.session_seq.unwrap_or(0)),
        );
        Page {
            items: page.items.into_iter().map(|(r, _)| r.clone()).collect(),
            next: page.next,
        }
    }
}

/// In-memory implementation with the same atomic append, cursor, page and
/// summary semantics. A long `read_after` takes the lock for one page at a
/// time, so appends run between its pages.
#[derive(Default)]
pub struct MemoryEventLog {
    state: Mutex<State>,
}
impl MemoryEventLog {
    pub fn new() -> Self {
        Self::default()
    }
}
#[async_trait]
impl EventLog for MemoryEventLog {
    async fn append_batch(&self, events: Vec<Event>) -> Result<Vec<LoggedEvent>> {
        validate(&events)?;
        let mut state = self.state.lock().await;
        // Sequences of this batch; the state changes only after this loop.
        let mut next: HashMap<TenantSession, u64> = HashMap::new();
        let mut seqs = Vec::with_capacity(events.len());
        for event in &events {
            seqs.push(match (&event.kind, &event.session_key) {
                (EventKind::Update(_), Some(key)) => {
                    let tenant = event.tenant();
                    let seq = next
                        .entry((tenant.clone(), key.clone()))
                        .or_insert_with(|| state.latest(tenant, key));
                    *seq += 1;
                    Some(*seq)
                }
                (EventKind::Update(_), None) => {
                    return Err(EventLogError::Invalid("Update without a Session".into()))
                }
                _ => None,
            });
        }
        let mut result = Vec::with_capacity(events.len());
        for (event, seq) in events.into_iter().zip(seqs) {
            let index = state.events.len();
            let record = stamp(event, index as u64 + 1, seq);
            state.apply(&record, index);
            let bytes = record_bytes(&record);
            state.events.push((record.clone(), bytes));
            result.push(record);
        }
        Ok(result)
    }
    async fn read_after(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
    ) -> Result<Vec<LoggedEvent>> {
        let through = self.latest_seq(tenant, session).await?;
        let mut records = Vec::new();
        let mut after = after_seq;
        loop {
            let page = self.state.lock().await.session_page(
                tenant,
                session,
                after,
                through,
                PageLimit::DEFAULT,
            );
            records.extend(page.items);
            match page.next {
                Some(Cursor(next)) => after = next,
                None => return Ok(records),
            }
        }
    }
    async fn read_runtime_after(
        &self,
        scope: Scope<'_>,
        global_seq: u64,
    ) -> Result<Vec<LoggedEvent>> {
        let state = self.state.lock().await;
        let start = usize::try_from(global_seq)
            .unwrap_or(usize::MAX)
            .min(state.events.len());
        Ok(state.events[start..]
            .iter()
            .filter(|(record, _)| scope.admits(record.event.tenant()))
            .map(|(record, _)| record.clone())
            .collect())
    }
    async fn latest_seq(&self, tenant: &Tenant, session: &SessionKey) -> Result<u64> {
        Ok(self.state.lock().await.latest(tenant, session))
    }
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            paged_reads: true,
            tenant_scoped: true,
            ..StoreCapabilities::EVENT_LOG_V1
        }
    }
    async fn read_session_page(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        Ok(self.state.lock().await.session_page(
            tenant,
            session,
            after_seq,
            through_seq.unwrap_or(u64::MAX),
            limit,
        ))
    }
    async fn session_summary(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
    ) -> Result<Option<SessionSummary>> {
        Ok(self
            .state
            .lock()
            .await
            .session(tenant, session)
            .and_then(|s| s.summary.clone()))
    }
    async fn recovery_page(
        &self,
        scope: Scope<'_>,
        from_generation: u64,
        after_global: u64,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        let state = self.state.lock().await;
        let start = usize::try_from(after_global)
            .unwrap_or(usize::MAX)
            .min(state.events.len());
        let page = cut(
            state.events[start..].iter().filter(|(r, _)| {
                r.event.generation >= from_generation
                    && is_recovery_record(&r.event.kind)
                    && scope.admits(r.event.tenant())
            }),
            limit,
            |(_, bytes)| *bytes,
            |(record, _)| Cursor(record.global_seq),
        );
        Ok(Page {
            items: page.items.into_iter().map(|(r, _)| r.clone()).collect(),
            next: page.next,
        })
    }
    async fn subagent_sessions(&self, scope: Scope<'_>) -> Result<HashSet<TenantSession>> {
        Ok(self
            .state
            .lock()
            .await
            .sessions
            .iter()
            .filter(|((tenant, _), s)| {
                scope.admits(tenant) && s.summary.as_ref().is_some_and(|s| s.parent.is_some())
            })
            .map(|(key, _)| key.clone())
            .collect())
    }
    async fn prompts_sent(&self, tenant: &Tenant, session: &SessionKey) -> Result<u64> {
        Ok(self
            .session_summary(tenant, session)
            .await?
            .map_or(0, |s| s.prompts_sent))
    }
    async fn turn_ids(&self, tenant: &Tenant, session: &SessionKey) -> Result<Vec<TurnId>> {
        Ok(self
            .state
            .lock()
            .await
            .session(tenant, session)
            .map(|s| s.turns.clone())
            .unwrap_or_default())
    }
    async fn last_activity_ms(&self, scope: Scope<'_>) -> Result<HashMap<TenantSession, i64>> {
        Ok(self
            .state
            .lock()
            .await
            .sessions
            .iter()
            .filter(|((tenant, _), _)| scope.admits(tenant))
            .filter_map(|(key, s)| Some((key.clone(), s.summary.as_ref()?.last_activity_ms)))
            .collect())
    }
    async fn recovery_records(
        &self,
        scope: Scope<'_>,
        from_generation: u64,
    ) -> Result<Vec<LoggedEvent>> {
        Ok(self
            .state
            .lock()
            .await
            .events
            .iter()
            .filter(|(r, _)| {
                r.event.generation >= from_generation
                    && is_recovery_record(&r.event.kind)
                    && scope.admits(r.event.tenant())
            })
            .map(|(r, _)| r.clone())
            .collect())
    }
    async fn allocate_generation(&self, _owner: &str, _pid: u32, _started_ms: i64) -> Result<u64> {
        let mut state = self.state.lock().await;
        state.generations.push(None);
        Ok(state.generations.len() as u64)
    }
    async fn last_recovered_generation(&self, _scope: &SystemScope) -> Result<u64> {
        Ok(self.state.lock().await.recovered)
    }
    async fn mark_recovered(&self, generation: u64, _recovered_ms: i64) -> Result<()> {
        let mut state = self.state.lock().await;
        if generation == 0 || generation > state.generations.len() as u64 {
            return Err(EventLogError::Invalid("unknown Runtime generation".into()));
        }
        state.recovered = state.recovered.max(generation);
        Ok(())
    }
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> Result<()> {
        let mut state = self.state.lock().await;
        let entry = generation
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| state.generations.get_mut(i));
        match entry {
            Some(entry) => {
                *entry = Some(stopped_ms);
                Ok(())
            }
            None => Err(EventLogError::Invalid("unknown Runtime generation".into())),
        }
    }
}
