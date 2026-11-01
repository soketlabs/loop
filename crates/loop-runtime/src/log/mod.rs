//! Append-only Event Log shared by Runtime pumps and recovery.
//!
//! Appends return only after commit, with durable Update sequences and generation
//! stamped into the envelopes. Audit events never consume a Session cursor.

mod memory;
mod sqlite;
pub use memory::MemoryEventLog;
pub use sqlite::SqliteEventLog;

use async_trait::async_trait;
use loop_protocol::{Origin, SessionKey, TurnId, UpdateEnvelope};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum events in one atomic append (and one SQLite writer transaction).
pub const MAX_BATCH_EVENTS: usize = 256;
/// Maximum queued writer requests; senders wait when the queue is full.
pub const WRITER_QUEUE_CAPACITY: usize = 64;

/// A typed activity. Audit payloads retain their callers' JSON without loss.
// Updates dominate the stream; keep envelopes inline to avoid an allocation per chunk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EventKind {
    Update(UpdateEnvelope),
    PromptSent(Value),
    Cancelled(Value),
    EscalationRaised(Value),
    EscalationAnswered(Value),
    EscalationWithdrawn(Value),
    SessionOpened(Value),
    SessionClosed(Value),
    /// A Session's `subagent` call opened a child (logged on the parent).
    SubagentStarted(Value),
    /// A child's result reached its parent (logged on the parent).
    SubagentEnded(Value),
    RuntimeStarted(Value),
    RuntimeStopped(Value),
    /// Unrecognized activity, including its original kind and payload.
    #[serde(untagged)]
    Unknown(Value),
}

/// Activity before allocation of durable cursors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub generation: u64,
    pub session_key: Option<SessionKey>,
    pub turn: Option<TurnId>,
    pub ts_ms: i64,
    pub kind: EventKind,
}

impl Event {
    /// Build an Update using the envelope's Session and turn correlation.
    /// Its transient sequence and generation are replaced during append.
    pub fn update(generation: u64, ts_ms: i64, envelope: UpdateEnvelope) -> Self {
        Self {
            generation,
            session_key: Some(envelope.session.clone()),
            turn: envelope.turn.clone(),
            ts_ms,
            kind: EventKind::Update(envelope),
        }
    }
}

/// A committed activity, ordered globally and (for Updates) within a Session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggedEvent {
    pub global_seq: u64,
    pub session_seq: Option<u64>,
    pub event: Event,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EventLogError {
    #[error("invalid Event Log append: {0}")]
    Invalid(String),
    #[error("Event Log storage: {0}")]
    Storage(String),
    #[error("Event Log writer stopped")]
    WriterStopped,
}

impl From<rusqlite::Error> for EventLogError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}
impl From<serde_json::Error> for EventLogError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, EventLogError>;

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
    /// Turn boundaries (`turn_started`, `turn_ended` Updates) and Escalation
    /// records of generations at or after `from_generation`, in `global_seq`
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
    async fn last_activity_ms(&self) -> Result<std::collections::HashMap<SessionKey, i64>> {
        let mut activity = std::collections::HashMap::new();
        for record in self.read_runtime_after(0).await? {
            if let Some(key) = record.event.session_key {
                let ts = activity.entry(key).or_insert(record.event.ts_ms);
                *ts = (*ts).max(record.event.ts_ms);
            }
        }
        Ok(activity)
    }
}

/// Whether crash recovery needs this record.
pub fn is_recovery_record(kind: &EventKind) -> bool {
    match kind {
        EventKind::Update(envelope) => matches!(
            envelope.update,
            loop_protocol::SessionUpdate::TurnStarted { .. }
                | loop_protocol::SessionUpdate::TurnEnded { .. }
        ),
        EventKind::EscalationRaised(_)
        | EventKind::EscalationAnswered(_)
        | EventKind::EscalationWithdrawn(_) => true,
        _ => false,
    }
}

fn validate(events: &[Event]) -> Result<()> {
    if events.len() > MAX_BATCH_EVENTS {
        return Err(EventLogError::Invalid(format!(
            "batch exceeds {MAX_BATCH_EVENTS} events"
        )));
    }
    for event in events {
        sql_seq(event.generation)?;
        if matches!(
            event.kind,
            EventKind::RuntimeStarted(_) | EventKind::RuntimeStopped(_)
        ) && (event.session_key.is_some() || event.turn.is_some())
        {
            return Err(EventLogError::Invalid(
                "Runtime activity must not have Session correlation".into(),
            ));
        }
        if let EventKind::Unknown(raw) = &event.kind {
            if raw.get("kind").and_then(Value::as_str).is_none()
                || serde_json::from_value::<EventKind>(raw.clone())? != event.kind
            {
                return Err(EventLogError::Invalid(
                    "Unknown must retain an unrecognized kind and lossless payload".into(),
                ));
            }
        }
        if let EventKind::Update(envelope) = &event.kind {
            if event.session_key.as_ref() != Some(&envelope.session) || event.turn != envelope.turn
            {
                return Err(EventLogError::Invalid(
                    "Update correlation differs from event".into(),
                ));
            }
            if envelope.origin == Origin::Replay {
                return Err(EventLogError::Invalid(
                    "Replay must not become new activity".into(),
                ));
            }
        }
    }
    Ok(())
}

fn sql_seq(seq: u64) -> Result<i64> {
    i64::try_from(seq)
        .map_err(|_| EventLogError::Invalid("sequence exceeds SQLite integer range".into()))
}

fn stamp(mut event: Event, global_seq: u64, session_seq: Option<u64>) -> LoggedEvent {
    if let EventKind::Update(envelope) = &mut event.kind {
        envelope.seq = session_seq.expect("Update has a Session sequence");
        envelope.generation = event.generation;
    }
    LoggedEvent {
        global_seq,
        session_seq,
        event,
    }
}
