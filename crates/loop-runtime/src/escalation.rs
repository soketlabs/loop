//! Session-scoped Escalation arbitration and durable decisions.
use crate::log::{Event, EventKind, EventLog};
use async_trait::async_trait;
use loop_protocol::{
    rpc::InteractParams, HostServices, InteractionOutcome, InteractionRequest, SessionRef,
};
use loop_wire::control::EscalationRecipient;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{oneshot, Mutex};

/// The approver host of one live Session. Disconnected Surfaces never decide a request.
#[derive(Clone)]
pub struct EscalationHost(Arc<Inner>);
struct Inner {
    session: SessionRef,
    log: Arc<dyn EventLog>,
    generation: u64,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    next: u64,
    closed: bool,
    delivery: u64,
    recipients: HashMap<u64, Arc<dyn EscalationRecipient>>,
    pending: HashMap<u64, Pending>,
}
struct Pending {
    request: InteractionRequest,
    answer: oneshot::Sender<InteractionOutcome>,
    deliveries: HashMap<u64, Delivery>,
}
struct Delivery {
    token: u64,
    withdrawn: oneshot::Sender<()>,
}
struct WaitingTool {
    host: EscalationHost,
    ticket: u64,
}
impl Drop for WaitingTool {
    fn drop(&mut self) {
        let host = self.host.clone();
        let ticket = self.ticket;
        tokio::spawn(async move {
            let mut state = host.0.state.lock().await;
            if let Some(pending) = state.pending.remove(&ticket) {
                if let Err(error) = host
                    .record(EventKind::EscalationWithdrawn(
                        json!({"id": pending.request.id(), "reason":"waiting tool cancelled"}),
                    ))
                    .await
                {
                    eprintln!("Runtime: could not log Escalation withdrawal: {error}");
                }
                for (_, delivery) in pending.deliveries {
                    let _ = delivery.withdrawn.send(());
                }
            }
        });
    }
}
impl EscalationHost {
    /// Create the host before exposing the Session to prompts.
    pub fn new(session: SessionRef, log: Arc<dyn EventLog>, generation: u64) -> Self {
        Self(Arc::new(Inner {
            session,
            log,
            generation,
            state: Mutex::new(State::default()),
        }))
    }
    async fn record(&self, kind: EventKind) -> crate::log::Result<()> {
        self.0
            .log
            .append_batch(vec![Event {
                generation: self.0.generation,
                session_key: Some(self.0.session.key.clone()),
                turn: None,
                ts_ms: crate::runtime::now(),
                kind,
            }])
            .await
            .map(|_| ())
    }
    /// Attach a Surface and send any parked requests exactly once to it.
    pub async fn attach(&self, connection: u64, recipient: Arc<dyn EscalationRecipient>) {
        let mut state = self.0.state.lock().await;
        if state.closed {
            return;
        }
        state.recipients.insert(connection, recipient.clone());
        let mut next = state.delivery;
        for (&ticket, pending) in &mut state.pending {
            if !pending.deliveries.contains_key(&connection) {
                next += 1;
                self.dispatch(ticket, pending, connection, next, recipient.clone());
            }
        }
        state.delivery = next;
    }
    /// Detaching only removes this recipient; pending requests stay parked.
    pub async fn detach(&self, connection: u64) {
        let mut state = self.0.state.lock().await;
        state.recipients.remove(&connection);
        for pending in state.pending.values_mut() {
            if let Some(withdrawn) = pending.deliveries.remove(&connection) {
                let _ = withdrawn.withdrawn.send(());
            }
        }
    }
    fn dispatch(
        &self,
        ticket: u64,
        pending: &mut Pending,
        connection: u64,
        token: u64,
        recipient: Arc<dyn EscalationRecipient>,
    ) {
        let (tx, rx) = oneshot::channel();
        pending.deliveries.insert(
            connection,
            Delivery {
                token,
                withdrawn: tx,
            },
        );
        let params = InteractParams {
            session: Some(self.0.session.clone()),
            request: pending.request.clone(),
        };
        let host = self.clone();
        tokio::spawn(async move {
            if let Ok(answer) = recipient.interact(params, rx).await {
                host.answer(ticket, connection, token, answer).await;
            }
        });
    }
    async fn answer(&self, ticket: u64, connection: u64, token: u64, answer: InteractionOutcome) {
        let mut state = self.0.state.lock().await;
        let Some(pending) = state.pending.get(&ticket) else {
            return;
        };
        if !pending
            .deliveries
            .get(&connection)
            .is_some_and(|d| d.token == token)
            || !valid(&pending.request, &answer)
        {
            return;
        }
        let grant = match (&pending.request, &answer) {
            (
                InteractionRequest::Permission { tool_call, .. },
                InteractionOutcome::Selected { native_id, .. },
            ) if native_id == loop_app_core::interaction::native_options::ACCEPT_SESSION => Some(
                json!({"scope": "session", "tool": tool_call.as_ref().and_then(|t| t.name.as_ref()), "group": tool_call.as_ref().and_then(|t| t.name.as_deref()).and_then(loop_app_core::tool_approval::approval_group)}),
            ),
            _ => None,
        };
        let result = self
            .record(EventKind::EscalationAnswered(json!({
                "id": pending.request.id(), "answer": answer, "grant": grant,
            })))
            .await;
        let mut pending = state.pending.remove(&ticket).unwrap();
        // The winner already completed its JSON-RPC request. Only other original recipients withdraw.
        pending.deliveries.remove(&connection);
        let outcome = match result {
            Ok(()) => answer,
            Err(e) => InteractionOutcome::Rejected {
                reason: e.to_string(),
            },
        };
        for (_, withdrawn) in pending.deliveries {
            let _ = withdrawn.withdrawn.send(());
        }
        let _ = pending.answer.send(outcome);
    }
    /// Withdraw on cancellation; closing also refuses future requests.
    pub async fn withdraw(&self, reason: &str, closing: bool) -> crate::log::Result<()> {
        let mut state = self.0.state.lock().await;
        state.closed |= closing;
        let pending = std::mem::take(&mut state.pending);
        let mut error = None;
        for (_, pending) in pending {
            if let Err(e) = self
                .record(EventKind::EscalationWithdrawn(
                    json!({"id": pending.request.id(), "reason": reason}),
                ))
                .await
            {
                error = Some(e);
            }
            for (_, withdrawn) in pending.deliveries {
                let _ = withdrawn.withdrawn.send(());
            }
            let _ = pending.answer.send(InteractionOutcome::Cancelled);
        }
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
fn valid(request: &InteractionRequest, answer: &InteractionOutcome) -> bool {
    match (request, answer) {
        (
            InteractionRequest::Permission { options, .. },
            InteractionOutcome::Selected { native_id, .. },
        ) => options.iter().any(|o| &o.native_id == native_id),
        (
            InteractionRequest::Question {
                choices: Some(choices),
                ..
            },
            InteractionOutcome::Selected { native_id, .. },
        ) => choices.iter().any(|o| &o.native_id == native_id),
        (_, InteractionOutcome::Cancelled | InteractionOutcome::Rejected { .. }) => true,
        (
            InteractionRequest::Question { choices: None, .. } | InteractionRequest::Form { .. },
            InteractionOutcome::Answered { .. },
        ) => true,
        (InteractionRequest::Url { .. }, InteractionOutcome::Selected { .. }) => true,
        _ => false,
    }
}
#[async_trait]
impl HostServices for EscalationHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        let (tx, rx) = oneshot::channel();
        let waiting = {
            let mut state = self.0.state.lock().await;
            if state.closed {
                return InteractionOutcome::Cancelled;
            }
            state.next += 1;
            let ticket = state.next;
            // Register cleanup before awaiting the durable write. Its writer may
            // commit even when this future is dropped before acknowledgement.
            state.pending.insert(
                ticket,
                Pending {
                    request: request.clone(),
                    answer: tx,
                    deliveries: HashMap::new(),
                },
            );
            let waiting = WaitingTool {
                host: self.clone(),
                ticket,
            };
            if let Err(e) = self
                .record(EventKind::EscalationRaised(
                    json!({"id": request.id(), "request": request}),
                ))
                .await
            {
                state.pending.remove(&ticket);
                return InteractionOutcome::Rejected {
                    reason: e.to_string(),
                };
            }
            let mut pending = state.pending.remove(&ticket).unwrap();
            let recipients = state.recipients.clone();
            for (connection, recipient) in recipients {
                state.delivery += 1;
                self.dispatch(ticket, &mut pending, connection, state.delivery, recipient);
            }
            state.pending.insert(ticket, pending);
            waiting
        };
        let outcome = rx.await.unwrap_or(InteractionOutcome::Cancelled);
        drop(waiting);
        outcome
    }
}
