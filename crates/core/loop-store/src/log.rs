//! The present Event Log contract. Live: the Runtime and recovery use it.
//!
//! It moved from `loop_runtime::log` with no change. ADR-0007 §8 replaces it
//! step by step with the traits in [`crate::store`].

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use loop_protocol::{SessionKey, TurnId};

use crate::error::Result;
use crate::event::{is_recovery_record, Event, EventKind, LoggedEvent};

/// Durable Session replay and Runtime audit boundary. Missing cursors start at 0.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Atomically append at most MAX_BATCH_EVENTS; an empty batch is a no-op.
    /// Returned records have committed cursors and re-stamped Update envelopes.
    async fn append_batch(&self, events: Vec<Event>) -> Result<Vec<LoggedEvent>>;
    /// Updates for this Session with session_seq strictly greater than after_seq.
    async fn read_after(&self, session: &SessionKey, after_seq: u64) -> Result<Vec<LoggedEvent>>;
    /// All activity with global_seq strictly greater than the supplied cursor.
    async fn read_runtime_after(&self, global_seq: u64) -> Result<Vec<LoggedEvent>>;
    /// Last committed Update sequence for this Session, or zero.
    async fn latest_seq(&self, session: &SessionKey) -> Result<u64>;
    /// Transactionally allocate a persistent Runtime generation, starting at one.
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> Result<u64>;
    /// Record a clean stop for a previously allocated generation.
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> Result<()>;
    /// Turn boundaries (`turn_started`, `turn_ended` Updates), prompts sent,
    /// clean stops and Escalation records of generations at or after `from_generation`, in `global_seq`
    /// order. Implementations should filter in storage; the default scans the log.
    async fn recovery_records(&self, from_generation: u64) -> Result<Vec<LoggedEvent>> {
        Ok(self
            .read_runtime_after(0)
            .await?
            .into_iter()
            .filter(|r| r.event.generation >= from_generation && is_recovery_record(&r.event.kind))
            .collect())
    }
    /// Newest generation whose crash recovery committed, or 0. Records of older
    /// generations are already settled. The default never remembers one.
    async fn last_recovered_generation(&self) -> Result<u64> {
        Ok(0)
    }
    /// Remember that this generation's recovery committed.
    async fn mark_recovered(&self, _generation: u64, _recovered_ms: i64) -> Result<()> {
        Ok(())
    }
    /// Sessions whose opening recorded a parent: the Runtime's Subagents.
    /// Implementations should filter in storage; the default scans the log.
    async fn subagent_sessions(&self) -> Result<HashSet<SessionKey>> {
        Ok(self
            .read_runtime_after(0)
            .await?
            .into_iter()
            .filter_map(|record| match &record.event.kind {
                EventKind::SessionOpened(payload) if !payload["parent"].is_null() => {
                    record.event.session_key
                }
                _ => None,
            })
            .collect())
    }
    /// How many prompts this Session was sent. Each turn starts from one, so
    /// this bounds the turns begun even when a turn left no Update.
    /// Implementations should count in storage; the default scans the log.
    async fn prompts_sent(&self, session: &SessionKey) -> Result<u64> {
        Ok(self
            .read_runtime_after(0)
            .await?
            .iter()
            .filter(|r| {
                r.event.session_key.as_ref() == Some(session)
                    && matches!(r.event.kind, EventKind::PromptSent(_))
            })
            .count() as u64)
    }
    /// Distinct turn ids this Session's Updates belong to. Implementations
    /// should answer without decoding payloads; the default reads every Update.
    async fn turn_ids(&self, session: &SessionKey) -> Result<Vec<TurnId>> {
        let mut turns = Vec::new();
        for record in self.read_after(session, 0).await? {
            if let Some(turn) = record.event.turn {
                if !turns.contains(&turn) {
                    turns.push(turn);
                }
            }
        }
        Ok(turns)
    }
    /// Newest event timestamp per Session. Implementations should answer with an
    /// aggregate query; the default scans the whole log.
    async fn last_activity_ms(&self) -> Result<HashMap<SessionKey, i64>> {
        let mut activity = HashMap::new();
        for record in self.read_runtime_after(0).await? {
            if let Some(key) = record.event.session_key {
                let ts = activity.entry(key).or_insert(record.event.ts_ms);
                *ts = (*ts).max(record.event.ts_ms);
            }
        }
        Ok(activity)
    }
}
