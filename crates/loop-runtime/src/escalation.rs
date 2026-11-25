//! Session-scoped Escalation arbitration and durable decisions.
//!
//! Who may answer (task I3): a recipient whose caller may act in the Session
//! ([`Need::Write`] under [`crate::access::decide`]) gets, and can answer,
//! its Escalations; an in-process recipient always. So the host's user gets
//! every Escalation of a `local` Session, as before; an outside user only
//! those of its own Sessions; nobody those of a Session it may not act in.
//! The Runtime gives the host the Session's [`Subject`]
//! ([`EscalationHost::set_access`]); until then it is host-only. The
//! `escalation_answered` record names the caller that answered.
//!
//! Who each record names (step B3): `escalation_raised` the caller that
//! started the turn; `escalation_answered` the caller that answered;
//! `escalation_withdrawn` the caller whose cancel or close withdrew it, or
//! the Runtime when it withdraws on its own. Every record is in the
//! Session's tenant.
use crate::access::{decide, Actor, Need, Subject};
use crate::log::{Event, EventKind, EventLog};
use async_trait::async_trait;
use loop_protocol::{
    rpc::InteractParams, BackendResult, Caller, HostServices, InteractionOutcome,
    InteractionRequest, SessionRef,
};
use loop_store::{Identity, Tenant};
use loop_wire::control::EscalationRecipient;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{oneshot, watch, Mutex};

/// The approver host of one live Session. Disconnected Surfaces never decide a request.
#[derive(Clone)]
pub struct EscalationHost(Arc<Inner>);
struct Inner {
    session: SessionRef,
    log: Arc<dyn EventLog>,
    generation: u64,
    /// The Session's tenant, fixed when it was created.
    tenant: Tenant,
    /// The `actor` of every record of this Session (a Subagent's parent).
    actor: Option<String>,
    state: Mutex<State>,
    interrupted: watch::Sender<bool>,
    /// The caller that started the current turn (or command), carried
    /// here from prompt admission: the task-local caller does not cross
    /// the turn's spawned task. `None` for an in-process prompt.
    turn_caller: std::sync::Mutex<Option<Caller>>,
    /// Who may see and answer this Session's Escalations.
    access: std::sync::Mutex<Subject>,
    /// Who answered recent Escalations, by id, until the decision record
    /// takes it. Holds at most [`ANSWERS_KEPT`].
    answered: std::sync::Mutex<std::collections::VecDeque<(String, Option<Caller>)>>,
}

/// Set `lock` to `value`, also when it is poisoned. A poisoned value is
/// overwritten through the guard and never read; the poison is cleared
/// only after the write, so no other thread can see the old value as clean.
fn replace_poisoned<T>(lock: &std::sync::Mutex<T>, value: T, what: &str) {
    #[allow(
        clippy::disallowed_methods,
        reason = "write-only reset: the poisoned value is overwritten, never read"
    )]
    let mut guard = match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("Runtime: the {what} lock was poisoned; replacing its value");
            poisoned.into_inner()
        }
    };
    *guard = value;
    drop(guard);
    lock.clear_poison();
}

/// How many answers [`Inner::answered`] keeps for the decision records.
const ANSWERS_KEPT: usize = 64;

/// Whether `recipient` may see and answer an Escalation of the Session
/// that `access` describes: the same rule as every other act in it. No
/// `access` (its lock is poisoned, see [`EscalationHost::access`]): nobody.
fn may_answer(recipient: &dyn EscalationRecipient, access: Option<&Subject>) -> bool {
    access.is_some_and(|access| decide(&Actor::of(recipient.caller()), access, Need::Write))
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
    recipient: Arc<dyn EscalationRecipient>,
}
impl Delivery {
    fn withdraw(self, session: &SessionRef, id: &loop_protocol::InteractionId) {
        self.recipient
            .withdraw(loop_protocol::rpc::InteractWithdrawnParams {
                session: session.clone(),
                id: id.clone(),
            });
        let _ = self.withdrawn.send(());
    }
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
                // The Runtime withdraws it on its own: the waiting tool went.
                let by = crate::runtime::in_process_caller();
                if let Err(error) = host
                    .record(
                        EventKind::EscalationWithdrawn(
                            json!({"id": pending.request.id(), "reason":"waiting tool cancelled"}),
                        ),
                        &by,
                    )
                    .await
                {
                    eprintln!("Runtime: could not log Escalation withdrawal: {error}");
                }
                for (_, delivery) in pending.deliveries {
                    delivery.withdraw(&host.0.session, pending.request.id());
                }
            }
        });
    }
}
impl EscalationHost {
    /// Create the host before exposing the Session to prompts. `tenant` is
    /// the Session's tenant and `actor` the `actor` of its records (a
    /// Subagent's parent Session); every record the host writes has them.
    pub fn new(
        session: SessionRef,
        log: Arc<dyn EventLog>,
        generation: u64,
        tenant: Tenant,
        actor: Option<String>,
    ) -> Self {
        Self(Arc::new(Inner {
            access: std::sync::Mutex::new(Subject::unowned(tenant.clone())),
            session,
            log,
            generation,
            tenant,
            actor,
            state: Mutex::new(State::default()),
            interrupted: watch::channel(false).0,
            turn_caller: std::sync::Mutex::new(None),
            answered: std::sync::Mutex::new(Default::default()),
        }))
    }
    /// Set who may see and answer this Session's Escalations: the Session's
    /// access subject (for a Subagent, its root's). Before this, only the
    /// host's user and in-process recipients may.
    ///
    /// The new value replaces the old one whole and never reads it, so a
    /// poisoned lock is overwritten and then cleared: the Subject is clean
    /// again.
    pub fn set_access(&self, access: Subject) {
        replace_poisoned(&self.0.access, access, "Escalation access");
    }
    /// Who may see and answer, or `None` when the lock is poisoned. No
    /// [`Subject`] value could stand in for "nobody": [`decide`] lets
    /// [`Actor::Internal`] (an in-process recipient) do everything to any
    /// Subject. So callers deny on `None` ([`may_answer`]).
    fn access(&self) -> Option<Subject> {
        match self.0.access.lock() {
            Ok(access) => Some(access.clone()),
            Err(_) => {
                eprintln!("Runtime: the Escalation access lock is poisoned; nobody may answer");
                None
            }
        }
    }
    /// Set the caller that started the turn about to run, at prompt
    /// admission. Escalations raised from now on belong to its turn.
    ///
    /// Replaces the value whole without reading it, so a poisoned lock is
    /// overwritten and then cleared.
    pub fn set_turn_caller(&self, caller: Option<Caller>) {
        replace_poisoned(&self.0.turn_caller, caller, "turn caller");
    }
    /// The caller that started the current turn: `Ok(None)` for an
    /// in-process turn, which acts as the Runtime. An internal error when
    /// the lock is poisoned (never read): callers must not read that as
    /// "no caller", or the turn's work would act with the Runtime's rights.
    pub fn turn_caller(&self) -> BackendResult<Option<Caller>> {
        Ok(crate::sync::lock(&self.0.turn_caller, "turn caller")?.clone())
    }
    /// The turn caller's lock, for tests that poison it.
    #[cfg(test)]
    pub(crate) fn turn_caller_lock(&self) -> &std::sync::Mutex<Option<Caller>> {
        &self.0.turn_caller
    }
    /// Who answered the Escalation `id`, once; `None` when it is not known
    /// or the lock is poisoned (never read).
    pub fn take_answered_by(&self, id: &str) -> Option<Caller> {
        let Ok(mut answered) = self.0.answered.lock() else {
            eprintln!("Runtime: the Escalation answers lock is poisoned; answerer unknown");
            return None;
        };
        let at = answered.iter().position(|(answered, _)| answered == id)?;
        answered.remove(at).and_then(|(_, caller)| caller)
    }
    /// Remember who answered `id`. On a poisoned lock the answer is
    /// dropped (logged): the deque is never read or written again.
    fn remember_answer(&self, id: String, caller: Option<Caller>) {
        let Ok(mut answered) = self.0.answered.lock() else {
            eprintln!("Runtime: the Escalation answers lock is poisoned; not remembering {id}");
            return;
        };
        if answered.len() >= ANSWERS_KEPT {
            answered.pop_front();
        }
        answered.push_back((id, caller));
    }
    /// Stop in-flight decision waits after close grace expires. The Event Log
    /// writer may already have committed; pending requests must still withdraw.
    pub(crate) fn interrupt_decisions(&self) {
        self.0.interrupted.send_replace(true);
    }
    /// `by` acting on this Session, in the Session's tenant.
    fn identity(&self, by: &Caller) -> Identity {
        Identity::acting(by, self.0.tenant.clone()).with_actor(self.0.actor.clone())
    }
    async fn record(&self, kind: EventKind, by: &Caller) -> crate::log::Result<()> {
        let decision = matches!(
            kind,
            EventKind::EscalationRaised(_) | EventKind::EscalationAnswered(_)
        );
        let events = vec![Event {
            generation: self.0.generation,
            session_key: Some(self.0.session.key.clone()),
            turn: None,
            ts_ms: crate::runtime::now(),
            kind,
            identity: self.identity(by),
        }];
        let mut interrupted = self.0.interrupted.subscribe();
        tokio::select! {
            biased;
            _ = interrupted.wait_for(|stop| *stop), if decision => {
                Err(crate::log::EventLogError::Storage("Escalation decision interrupted by Session close".into()))
            }
            result = self.0.log.append_batch(events) => result.map(|_| ()),
        }
    }
    /// Attach a Surface and send any parked requests exactly once to it.
    pub async fn attach(&self, connection: u64, recipient: Arc<dyn EscalationRecipient>) {
        let mut state = self.0.state.lock().await;
        if state.closed {
            return;
        }
        state.recipients.insert(connection, recipient.clone());
        let access = self.access();
        let mut next = state.delivery;
        for (&ticket, pending) in &mut state.pending {
            if !pending.deliveries.contains_key(&connection)
                && may_answer(recipient.as_ref(), access.as_ref())
            {
                next += 1;
                self.dispatch(ticket, pending, connection, next, recipient.clone());
            }
        }
        state.delivery = next;
    }
    /// Connections currently receiving this Session's Escalations.
    pub(crate) async fn connections(&self) -> Vec<u64> {
        self.0
            .state
            .lock()
            .await
            .recipients
            .keys()
            .copied()
            .collect()
    }
    /// Whether a request is waiting for an answer.
    pub(crate) async fn has_pending(&self) -> bool {
        !self.0.state.lock().await.pending.is_empty()
    }
    /// Detaching only removes this recipient; pending requests stay parked.
    pub async fn detach(&self, connection: u64) {
        let mut state = self.0.state.lock().await;
        state.recipients.remove(&connection);
        for pending in state.pending.values_mut() {
            if let Some(withdrawn) = pending.deliveries.remove(&connection) {
                withdrawn.withdraw(&self.0.session, pending.request.id());
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
                recipient: recipient.clone(),
            },
        );
        let params = InteractParams {
            session: Some(self.0.session.clone()),
            request: pending.request.clone(),
        };
        let host = self.clone();
        tokio::spawn(async move {
            match recipient.interact(params, rx).await {
                // A Surface that leaves (detach or exit) may still reply
                // Cancelled; that is the recipient going, not an answer.
                Ok(InteractionOutcome::Cancelled) => {
                    host.recipient_gone(ticket, connection, token).await
                }
                Ok(answer) => host.answer(ticket, connection, token, answer).await,
                Err(()) => {}
            }
        });
    }
    /// Forget this delivery only; the request stays pending for the other
    /// recipients, or parked for the next Surface.
    async fn recipient_gone(&self, ticket: u64, connection: u64, token: u64) {
        let mut state = self.0.state.lock().await;
        if let Some(pending) = state.pending.get_mut(&ticket) {
            if pending
                .deliveries
                .get(&connection)
                .is_some_and(|d| d.token == token)
            {
                pending.deliveries.remove(&connection);
            }
        }
    }
    async fn answer(&self, ticket: u64, connection: u64, token: u64, answer: InteractionOutcome) {
        let mut state = self.0.state.lock().await;
        let Some(pending) = state.pending.get(&ticket) else {
            return;
        };
        let Some(delivery) = pending
            .deliveries
            .get(&connection)
            .filter(|d| d.token == token)
        else {
            return;
        };
        let answered_by = delivery.recipient.caller();
        if !valid(&pending.request, &answer)
            || !may_answer(delivery.recipient.as_ref(), self.access().as_ref())
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
        // An in-process recipient answers as the Runtime.
        let by = answered_by
            .clone()
            .unwrap_or_else(crate::runtime::in_process_caller);
        let result = self
            .record(
                EventKind::EscalationAnswered(json!({
                    "id": pending.request.id(), "answer": answer, "grant": grant,
                    "answered_by": answered_by,
                })),
                &by,
            )
            .await;
        if result.is_ok() {
            self.remember_answer(pending.request.id().to_string(), answered_by);
        }
        if result.is_err() && *self.0.interrupted.borrow() {
            // Close owns the terminal withdrawal, even if this write committed
            // before its acknowledgement stalled. Never resume the gate here.
            return;
        }
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
            withdrawn.withdraw(&self.0.session, pending.request.id());
        }
        let _ = pending.answer.send(outcome);
    }
    /// Withdraw on cancellation; closing also refuses future requests. `by`
    /// is the caller whose cancel or close withdraws them (the Runtime's own
    /// caller when it acts on its own); the records name it.
    pub async fn withdraw(
        &self,
        reason: &str,
        closing: bool,
        by: &Caller,
    ) -> crate::log::Result<()> {
        let mut state = self.0.state.lock().await;
        state.closed |= closing;
        let pending = std::mem::take(&mut state.pending);
        let mut error = None;
        for (_, pending) in pending {
            if let Err(e) = self
                .record(
                    EventKind::EscalationWithdrawn(
                        json!({"id": pending.request.id(), "reason": reason}),
                    ),
                    by,
                )
                .await
            {
                error = Some(e);
            }
            for (_, withdrawn) in pending.deliveries {
                withdrawn.withdraw(&self.0.session, pending.request.id());
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
        // Raised for the caller that started the turn; an in-process turn
        // is the Runtime's. A poisoned turn caller is not "in process": the
        // Escalation is refused rather than raised as the Runtime's.
        let by = match self.turn_caller() {
            Ok(caller) => caller.unwrap_or_else(crate::runtime::in_process_caller),
            Err(error) => {
                return InteractionOutcome::Rejected {
                    reason: error.to_string(),
                }
            }
        };
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
                .record(
                    EventKind::EscalationRaised(json!({"id": request.id(), "request": request})),
                    &by,
                )
                .await
            {
                if !*self.0.interrupted.borrow() {
                    state.pending.remove(&ticket);
                }
                return InteractionOutcome::Rejected {
                    reason: e.to_string(),
                };
            }
            let mut pending = state.pending.remove(&ticket).unwrap();
            let recipients = state.recipients.clone();
            let access = self.access();
            for (connection, recipient) in recipients {
                if !may_answer(recipient.as_ref(), access.as_ref()) {
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::MemoryEventLog;
    use crate::sync::poison;

    /// A recipient that never answers; `caller` is who it acts for.
    struct Recipient(Option<Caller>);
    #[async_trait]
    impl EscalationRecipient for Recipient {
        fn caller(&self) -> Option<Caller> {
            self.0.clone()
        }
        fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
        async fn interact(
            &self,
            _: InteractParams,
            _: oneshot::Receiver<()>,
        ) -> Result<InteractionOutcome, ()> {
            Err(())
        }
    }

    fn host(key: &str) -> EscalationHost {
        let session = SessionRef {
            instance: "loop".into(),
            key: key.into(),
            native_id: key.into(),
        };
        let log = Arc::new(MemoryEventLog::new());
        EscalationHost::new(session, log, 1, Tenant::local(), None)
    }

    /// A poisoned access lock lets nobody answer, not even an in-process
    /// recipient (which every Subject would admit); setting the access
    /// again recovers it. Another Session's host is not affected.
    #[test]
    fn a_poisoned_access_lock_denies_every_recipient() {
        let poisoned = host("a");
        let other = host("b");
        let in_process = Recipient(None);
        let host_user = Recipient(Some(Caller::host(loop_wire::local_principal())));
        let access = poisoned.access();
        assert!(may_answer(&in_process, access.as_ref()));
        assert!(may_answer(&host_user, access.as_ref()));

        poison(&poisoned.0.access);
        let access = poisoned.access();
        assert_eq!(access, None);
        assert!(!may_answer(&in_process, access.as_ref()));
        assert!(!may_answer(&host_user, access.as_ref()));
        assert!(may_answer(&host_user, other.access().as_ref()));

        poisoned.set_access(Subject::unowned(Tenant::local()));
        assert!(!poisoned.0.access.is_poisoned());
        assert!(may_answer(&host_user, poisoned.access().as_ref()));
    }

    /// A poisoned turn caller is unknown, and a new turn's caller replaces
    /// it cleanly.
    #[test]
    fn a_poisoned_turn_caller_is_unknown_until_set_again() {
        let host = host("a");
        let caller = Caller::host(loop_wire::local_principal());
        host.set_turn_caller(Some(caller.clone()));
        poison(&host.0.turn_caller);
        assert!(host.turn_caller().is_err());
        host.set_turn_caller(Some(caller.clone()));
        assert!(!host.0.turn_caller.is_poisoned());
        assert_eq!(host.turn_caller().unwrap(), Some(caller));
    }

    /// An Escalation raised while the turn caller is poisoned is refused,
    /// not raised (and recorded) as the Runtime's.
    #[tokio::test]
    async fn a_poisoned_turn_caller_refuses_the_escalation() {
        let host = host("a");
        poison(&host.0.turn_caller);
        let request = loop_app_core::interaction::native_permission_request(
            loop_app_core::tool_approval::ApprovalKind::Bash,
            "bash",
            "run".into(),
            "echo ok".into(),
        );
        let outcome = host.interact(request).await;
        assert!(
            matches!(outcome, InteractionOutcome::Rejected { .. }),
            "{outcome:?}"
        );
        assert!(host.0.state.lock().await.pending.is_empty());
    }

    /// A poisoned answer list is never read: no answerer is known and new
    /// answers are dropped, without a panic.
    #[test]
    fn a_poisoned_answer_list_knows_no_answerer() {
        let host = host("a");
        let caller = Caller::host(loop_wire::local_principal());
        host.remember_answer("e1".into(), Some(caller.clone()));
        poison(&host.0.answered);
        assert_eq!(host.take_answered_by("e1"), None);
        host.remember_answer("e2".into(), Some(caller));
        assert_eq!(host.take_answered_by("e2"), None);
        assert!(host.0.answered.is_poisoned());
    }
}
