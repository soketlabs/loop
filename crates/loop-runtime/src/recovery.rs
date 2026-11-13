//! Crash recovery: find activity a previous Runtime generation left unfinished.
//!
//! The Event Log is read in `global_seq` order and the last state wins, because
//! turn and Escalation ids are process-local counters that repeat.
use crate::log::{EventKind, LoggedEvent};
use loop_protocol::{SessionKey, SessionUpdate, TurnId};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

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
    /// Stale lease rows of exited processes removed (the startup sweep).
    pub leases_swept: usize,
}

/// An Escalation still open, identified by the generation that raised it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenEscalation {
    pub session: SessionKey,
    pub generation: u64,
    pub id: Value,
}

/// A turn recovery must end as `runtime_restarted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenTurn {
    pub session: SessionKey,
    pub turn_id: TurnId,
    /// The log has no `turn_started` for it: only a `prompt_sent`. Its tools
    /// may have run before the start committed, so recovery logs the start too.
    pub unstarted: bool,
}

/// Id of the turn recovery records for a prompt (at `global_seq`) whose turn
/// start was never logged. Deterministic, so a repeated pass finds it.
pub(crate) fn unstarted_turn_id(global_seq: u64) -> TurnId {
    TurnId(format!("recovered-prompt-{global_seq}"))
}

/// Unfinished work of generations older than `current`, in first-seen order.
///
/// A Session runs one prompt at a time, so a `turn_started` after a
/// `prompt_sent` belongs to it. The last prompt of a Session with no turn
/// start after it is unfinished too, unless its generation stopped cleanly.
#[cfg(test)]
pub(crate) fn unfinished(
    records: &[LoggedEvent],
    current: u64,
) -> (Vec<OpenTurn>, Vec<OpenEscalation>) {
    let mut open = Unfinished::new(current);
    for record in records {
        open.feed(record);
    }
    open.finish()
}

/// The fold behind [`unfinished`], fed one page of recovery records at a
/// time in `global_seq` order. It keeps only open work, not the records.
#[derive(Debug)]
pub(crate) struct Unfinished {
    current: u64,
    turns: HashMap<(SessionKey, TurnId), (u64, bool, bool)>,
    /// Per Session: the last prompt with no turn start yet (seq, generation).
    prompts: HashMap<SessionKey, (u64, u64)>,
    /// Generations with a clean stop.
    stopped: HashSet<u64>,
    escalations: HashMap<(SessionKey, u64, String), (u64, Option<Value>)>,
}

impl Unfinished {
    pub(crate) fn new(current: u64) -> Self {
        Self {
            current,
            turns: HashMap::new(),
            prompts: HashMap::new(),
            stopped: HashSet::new(),
            escalations: HashMap::new(),
        }
    }

    /// Fold the next record.
    pub(crate) fn feed(&mut self, record: &LoggedEvent) {
        let current = self.current;
        let event = &record.event;
        if matches!(event.kind, EventKind::RuntimeStopped(_)) {
            self.stopped.insert(event.generation);
        }
        let Some(session) = &event.session_key else {
            return;
        };
        match &event.kind {
            // The prompt never started a turn.
            EventKind::PromptSettled(_) => {
                self.prompts.remove(session);
            }
            EventKind::PromptSent(_) => {
                if event.generation < current {
                    self.prompts
                        .insert(session.clone(), (record.global_seq, event.generation));
                } else {
                    self.prompts.remove(session);
                }
            }
            EventKind::Update(envelope) => match &envelope.update {
                // A turn that started in this generation is live, not recoverable.
                SessionUpdate::TurnStarted { turn_id } if event.generation < current => {
                    self.prompts.remove(session);
                    self.turns.insert(
                        (session.clone(), turn_id.clone()),
                        (record.global_seq, true, false),
                    );
                }
                // Recovery's own start for an unstarted prompt settles it.
                SessionUpdate::TurnStarted { .. } => {
                    self.prompts.remove(session);
                }
                // Ids are process-local counters. A live turn of this generation
                // must not close an old one; only recovery's own end may.
                SessionUpdate::TurnEnded { stop, .. }
                    if event.generation >= current
                        && *stop != loop_protocol::StopReason::RuntimeRestarted => {}
                SessionUpdate::TurnEnded { turn_id, .. } => {
                    if let Some(open) = self.turns.get_mut(&(session.clone(), turn_id.clone())) {
                        open.1 = false;
                    }
                }
                _ => {}
            },
            EventKind::EscalationRaised(payload) if event.generation < current => {
                let id = payload.get("id").cloned().unwrap_or(Value::Null);
                self.escalations.insert(
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
                    self.escalations
                        .get_mut(&(session.clone(), generation, id.to_string()))
                {
                    open.1 = None;
                }
            }
            _ => {}
        }
    }

    /// The open turns and Escalations, in first-seen order.
    pub(crate) fn finish(self) -> (Vec<OpenTurn>, Vec<OpenEscalation>) {
        let Self {
            mut turns,
            prompts,
            stopped,
            escalations,
            ..
        } = self;
        for (session, (seq, generation)) in prompts {
            if !stopped.contains(&generation) {
                turns.insert((session, unstarted_turn_id(seq)), (seq, true, true));
            }
        }
        let mut turns: Vec<_> = turns
            .into_iter()
            .filter(|(_, (_, open, _))| *open)
            .map(|((session, turn_id), (seq, _, unstarted))| {
                (
                    seq,
                    OpenTurn {
                        session,
                        turn_id,
                        unstarted,
                    },
                )
            })
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
            turns.into_iter().map(|(_, turn)| turn).collect(),
            escalations.into_iter().map(|(_, open)| open).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Event;
    use loop_protocol::{StopReason, UpdateEnvelope};

    fn turn(global_seq: u64, generation: u64, update: SessionUpdate) -> LoggedEvent {
        LoggedEvent {
            global_seq,
            session_seq: Some(global_seq),
            event: Event::update(
                generation,
                0,
                UpdateEnvelope::live(
                    "loop".into(),
                    generation,
                    "s".into(),
                    Some(TurnId("turn-1".into())),
                    0,
                    update,
                ),
            ),
        }
    }
    fn ended(stop: StopReason) -> SessionUpdate {
        SessionUpdate::TurnEnded {
            turn_id: TurnId("turn-1".into()),
            stop,
            error: None,
        }
    }

    #[test]
    fn a_live_turn_with_a_reused_id_does_not_hide_an_old_open_turn() {
        let started = SessionUpdate::TurnStarted {
            turn_id: TurnId("turn-1".into()),
        };
        let records = vec![
            turn(1, 1, started.clone()),
            turn(2, 2, started),
            turn(3, 2, ended(StopReason::EndTurn)),
        ];
        assert_eq!(unfinished(&records, 2).0.len(), 1);
        let mut recovered = records;
        recovered.push(turn(4, 2, ended(StopReason::RuntimeRestarted)));
        assert!(unfinished(&recovered, 2).0.is_empty());
    }

    fn prompt(global_seq: u64, generation: u64) -> LoggedEvent {
        LoggedEvent {
            global_seq,
            session_seq: None,
            event: Event {
                generation,
                session_key: Some("s".into()),
                turn: None,
                ts_ms: 0,
                kind: EventKind::PromptSent(serde_json::json!({})),
            },
        }
    }

    #[test]
    fn the_last_prompt_without_a_turn_start_is_unfinished_unless_its_generation_stopped() {
        let started = SessionUpdate::TurnStarted {
            turn_id: TurnId("turn-1".into()),
        };
        // A prompt with its turn, then a prompt whose turn start never committed.
        let mut records = vec![
            prompt(1, 1),
            turn(2, 1, started),
            turn(3, 1, ended(StopReason::EndTurn)),
            prompt(4, 1),
        ];
        let (turns, _) = unfinished(&records, 2);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].unstarted);
        assert_eq!(turns[0].turn_id, unstarted_turn_id(4));
        // Recovery's own start and end settle it for a later pass.
        let id = turns[0].turn_id.clone();
        let mut recovered = records.clone();
        for (seq, update) in [
            (
                5,
                SessionUpdate::TurnStarted {
                    turn_id: id.clone(),
                },
            ),
            (
                6,
                SessionUpdate::TurnEnded {
                    turn_id: id,
                    stop: StopReason::RuntimeRestarted,
                    error: None,
                },
            ),
        ] {
            recovered.push(turn(seq, 2, update));
        }
        assert!(unfinished(&recovered, 3).0.is_empty());
        // A clean stop means the prompt never ran a turn.
        records.push(LoggedEvent {
            global_seq: 5,
            session_seq: None,
            event: Event {
                generation: 1,
                session_key: None,
                turn: None,
                ts_ms: 0,
                kind: EventKind::RuntimeStopped(serde_json::json!({})),
            },
        });
        assert!(unfinished(&records, 2).0.is_empty());
    }

    #[test]
    fn a_settled_prompt_is_not_unfinished() {
        let mut settled = prompt(2, 1);
        settled.event.kind = EventKind::PromptSettled(serde_json::json!({}));
        assert!(unfinished(&[prompt(1, 1), settled], 2).0.is_empty());
    }

    /// An `escalation_answered` row written before `answered_by` existed
    /// still closes its Escalation, as one with it does; a `policy_decision`
    /// row between them changes nothing.
    #[test]
    fn old_and_new_answer_rows_both_close_their_escalation() {
        let record = |global_seq, kind| LoggedEvent {
            global_seq,
            session_seq: None,
            event: Event {
                generation: 1,
                session_key: Some("s".into()),
                turn: None,
                ts_ms: 0,
                kind,
            },
        };
        let raised = || EventKind::EscalationRaised(serde_json::json!({"id": "i1", "request": {}}));
        for answer in [
            serde_json::json!({"id": "i1", "answer": {"outcome": "cancelled"}, "grant": null}),
            serde_json::json!({"id": "i1", "answer": {"outcome": "cancelled"}, "grant": null,
                "answered_by": {"principal": {"kind": "person", "issuer": "local",
                    "id": "uid:501"}, "tenant": "local"}}),
        ] {
            let records = [
                record(1, raised()),
                record(
                    2,
                    EventKind::PolicyDecision(serde_json::json!({"id": "pd-1"})),
                ),
                record(3, EventKind::EscalationAnswered(answer)),
            ];
            let (_, open) = unfinished(&records, 2);
            assert!(open.is_empty(), "the answered Escalation is still open");
        }
        let (_, open) = unfinished(&[record(1, raised())], 2);
        assert_eq!(open.len(), 1, "control: an unanswered one stays open");
    }
}
