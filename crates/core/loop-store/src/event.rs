//! Event Log records: what a store appends and what it gives back.
//!
//! All items here are live. They moved from `loop_runtime::log` with no
//! change of shape or serde form.

use loop_protocol::{Origin, SessionKey, TurnId, UpdateEnvelope};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, StoreError};

/// Maximum events in one atomic append (and one SQLite writer transaction).
pub const MAX_BATCH_EVENTS: usize = 256;
/// Maximum queued writer requests; senders wait when the queue is full.
pub const WRITER_QUEUE_CAPACITY: usize = 64;

/// A typed activity. Audit payloads retain their callers' JSON without loss.
///
/// The ADR-0007 kinds `policy_decision`, `tree` and `session_deleted` are not
/// variants yet: a stored row of those kinds still reads as
/// [`EventKind::Unknown`]. Their payload types are in [`crate::records`].
// Updates dominate the stream; keep envelopes inline to avoid an allocation per chunk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EventKind {
    Update(UpdateEnvelope),
    PromptSent(Value),
    /// A prompt ended without starting a turn (cancelled while queued,
    /// refused as busy, closing). Log-only: recovery reads it so the prompt
    /// is not taken for an interrupted turn. Never sent on the wire.
    PromptSettled(Value),
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
///
/// The ADR-0007 §7 identity fields are not here yet (step B3). Their type is
/// [`crate::identity::Identity`].
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

/// The ADR-0007 §8 name of a committed record. It is the same type as
/// [`LoggedEvent`].
pub type Committed = LoggedEvent;

/// Whether crash recovery needs this record.
pub fn is_recovery_record(kind: &EventKind) -> bool {
    match kind {
        EventKind::Update(envelope) => matches!(
            envelope.update,
            loop_protocol::SessionUpdate::TurnStarted { .. }
                | loop_protocol::SessionUpdate::TurnEnded { .. }
        ),
        EventKind::PromptSent(_)
        | EventKind::PromptSettled(_)
        | EventKind::RuntimeStopped(_)
        | EventKind::EscalationRaised(_)
        | EventKind::EscalationAnswered(_)
        | EventKind::EscalationWithdrawn(_) => true,
        _ => false,
    }
}

/// The append rules every Event Log applies before it writes. A batch that
/// fails here writes nothing.
pub fn validate_batch(events: &[Event]) -> Result<()> {
    if events.len() > MAX_BATCH_EVENTS {
        return Err(StoreError::Invalid(format!(
            "batch exceeds {MAX_BATCH_EVENTS} events"
        )));
    }
    for event in events {
        check_seq(event.generation)?;
        if matches!(
            event.kind,
            EventKind::RuntimeStarted(_) | EventKind::RuntimeStopped(_)
        ) && (event.session_key.is_some() || event.turn.is_some())
        {
            return Err(StoreError::Invalid(
                "Runtime activity must not have Session correlation".into(),
            ));
        }
        if let EventKind::Unknown(raw) = &event.kind {
            if raw.get("kind").and_then(Value::as_str).is_none()
                || serde_json::from_value::<EventKind>(raw.clone())? != event.kind
            {
                return Err(StoreError::Invalid(
                    "Unknown must retain an unrecognized kind and lossless payload".into(),
                ));
            }
        }
        if let EventKind::Update(envelope) = &event.kind {
            if event.session_key.as_ref() != Some(&envelope.session) || event.turn != envelope.turn
            {
                return Err(StoreError::Invalid(
                    "Update correlation differs from event".into(),
                ));
            }
            if envelope.origin == Origin::Replay {
                return Err(StoreError::Invalid(
                    "Replay must not become new activity".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Sequences and generations are stored as signed 64-bit integers.
fn check_seq(seq: u64) -> Result<i64> {
    i64::try_from(seq)
        .map_err(|_| StoreError::Invalid("sequence exceeds SQLite integer range".into()))
}

/// Give an event its committed cursors. An Update's envelope gets the
/// Session sequence and the generation.
///
/// # Panics
///
/// When `event` is an Update and `session_seq` is `None`. A store gives
/// every Update a Session sequence, so this is a store bug.
pub fn stamp(mut event: Event, global_seq: u64, session_seq: Option<u64>) -> LoggedEvent {
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
