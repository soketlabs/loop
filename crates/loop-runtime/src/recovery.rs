//! Crash recovery: find activity a previous Runtime generation left unfinished.
//!
//! The Event Log is read in `global_seq` order and the last state wins, because
//! turn and Escalation ids are process-local counters that repeat.
use crate::log::{EventKind, LoggedEvent};
use loop_protocol::{SessionKey, SessionUpdate, TurnId};
use serde_json::Value;
use std::collections::HashMap;

/// Reason recorded on recovery records; matches the wire stop reason.
pub(crate) const REASON: &str = "runtime_restarted";

/// What one recovery pass changed. A repeated pass reports zeros.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Turns given a `runtime_restarted` end.
    pub turns_ended: usize,
    /// Escalations of earlier generations withdrawn.
    pub escalations_withdrawn: usize,
    /// Session leases of the previous Runtime reclaimed and released.
    pub leases_reclaimed: usize,
}

/// An Escalation still open, identified by the generation that raised it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenEscalation {
    pub session: SessionKey,
    pub generation: u64,
    pub id: Value,
}

/// Unfinished work of generations older than `current`, in first-seen order.
pub(crate) fn unfinished(
    records: &[LoggedEvent],
    current: u64,
) -> (Vec<(SessionKey, TurnId)>, Vec<OpenEscalation>) {
    let mut turns: HashMap<(SessionKey, TurnId), (u64, bool)> = HashMap::new();
    let mut escalations: HashMap<(SessionKey, u64, String), (u64, Option<Value>)> = HashMap::new();
    for record in records {
        let event = &record.event;
        let Some(session) = &event.session_key else {
            continue;
        };
        match &event.kind {
            EventKind::Update(envelope) => match &envelope.update {
                // A turn that started in this generation is live, not recoverable.
                SessionUpdate::TurnStarted { turn_id } if event.generation < current => {
                    turns.insert(
                        (session.clone(), turn_id.clone()),
                        (record.global_seq, true),
                    );
                }
                SessionUpdate::TurnEnded { turn_id, .. } => {
                    if let Some(open) = turns.get_mut(&(session.clone(), turn_id.clone())) {
                        open.1 = false;
                    }
                }
                _ => {}
            },
            EventKind::EscalationRaised(payload) if event.generation < current => {
                let id = payload.get("id").cloned().unwrap_or(Value::Null);
                escalations.insert(
                    (session.clone(), event.generation, id.to_string()),
                    (record.global_seq, Some(id)),
                );
            }
            EventKind::EscalationAnswered(payload) | EventKind::EscalationWithdrawn(payload) => {
                // Recovery withdrawals name the generation that raised the request.
                let generation = payload
                    .get("generation")
                    .and_then(Value::as_u64)
                    .unwrap_or(event.generation);
                let id = payload.get("id").cloned().unwrap_or(Value::Null);
                if let Some(open) =
                    escalations.get_mut(&(session.clone(), generation, id.to_string()))
                {
                    open.1 = None;
                }
            }
            _ => {}
        }
    }
    let mut turns: Vec<_> = turns
        .into_iter()
        .filter(|(_, (_, open))| *open)
        .map(|(key, (seq, _))| (seq, key))
        .collect();
    turns.sort_by_key(|(seq, _)| *seq);
    let mut escalations: Vec<_> = escalations
        .into_iter()
        .filter_map(|((session, generation, _), (seq, id))| {
            id.map(|id| {
                (
                    seq,
                    OpenEscalation {
                        session,
                        generation,
                        id,
                    },
                )
            })
        })
        .collect();
    escalations.sort_by_key(|(seq, _)| *seq);
    (
        turns.into_iter().map(|(_, key)| key).collect(),
        escalations.into_iter().map(|(_, open)| open).collect(),
    )
}
