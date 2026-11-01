use super::*;
use async_trait::async_trait;
use loop_protocol::SessionKey;
use std::collections::HashMap;
use tokio::sync::Mutex;

#[derive(Default)]
struct State {
    events: Vec<LoggedEvent>,
    latest: HashMap<SessionKey, u64>,
    generations: Vec<Option<i64>>,
    recovered: u64,
}

/// In-memory implementation with the same atomic append and cursor semantics.
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
        let mut result = Vec::with_capacity(events.len());
        for event in events {
            let seq = if matches!(event.kind, EventKind::Update(_)) {
                let latest = state
                    .latest
                    .entry(event.session_key.clone().expect("validated Update"))
                    .or_default();
                *latest += 1;
                Some(*latest)
            } else {
                None
            };
            let record = stamp(event, state.events.len() as u64 + 1, seq);
            state.events.push(record.clone());
            result.push(record);
        }
        Ok(result)
    }
    async fn read_after(&self, session: &SessionKey, after_seq: u64) -> Result<Vec<LoggedEvent>> {
        Ok(self
            .state
            .lock()
            .await
            .events
            .iter()
            .filter(|e| {
                e.event.session_key.as_ref() == Some(session)
                    && e.session_seq.is_some_and(|s| s > after_seq)
            })
            .cloned()
            .collect())
    }
    async fn read_runtime_after(&self, global_seq: u64) -> Result<Vec<LoggedEvent>> {
        Ok(self
            .state
            .lock()
            .await
            .events
            .iter()
            .filter(|e| e.global_seq > global_seq)
            .cloned()
            .collect())
    }
    async fn latest_seq(&self, session: &SessionKey) -> Result<u64> {
        Ok(*self.state.lock().await.latest.get(session).unwrap_or(&0))
    }
    async fn allocate_generation(&self, _owner: &str, _pid: u32, _started_ms: i64) -> Result<u64> {
        let mut state = self.state.lock().await;
        state.generations.push(None);
        Ok(state.generations.len() as u64)
    }
    async fn last_recovered_generation(&self) -> Result<u64> {
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
