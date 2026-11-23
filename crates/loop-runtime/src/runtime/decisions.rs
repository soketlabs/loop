//! The Runtime's decision sink: every Policy gate decision of a Session is
//! an `EventKind::PolicyDecision` record in the Event Log (S4 §12, ADR-0007
//! §1.2).
//!
//! Ordering (ADR-0007 §3.1): the gate awaits [`DecisionSink::record`]
//! before the call runs. For a call with an effect (everything but an
//! allowed read of files) the sink first runs the Session's pump barrier,
//! which commits every update the backend sent before it, the call's
//! `tool_call` included, and then appends the decision and waits for the
//! commit. So `tool_call` commits first, then the decision, both before the
//! tool runs. A record that cannot be written refuses the call.
//!
//! The record is a log-only, audit-only row: it takes no Session sequence,
//! so attach and replay never see it.
use super::*;
use loop_app_core::permissions::decision::{
    DecisionContext, DecisionError, DecisionSink, PolicyDecisionRecord,
};
use loop_protocol::{Caller, Principal, TurnId};

/// Writes one Session's decision records to the Event Log.
pub(super) struct LogDecisions {
    log: Arc<dyn EventLog>,
    generation: u64,
    session: SessionKey,
    /// The Session's tenant: the row's tenant (the record keeps its own).
    tenant: Tenant,
    /// The `actor` of the Session's records.
    actor: Option<String>,
    escalations: crate::EscalationHost,
    native: std::sync::Weak<loop_agent::native_backend::NativeBackend>,
    /// The live Session, once it exists, for its pump barrier.
    live: std::sync::OnceLock<std::sync::Weak<LiveSession>>,
}

impl LogDecisions {
    pub(super) fn new(
        log: Arc<dyn EventLog>,
        generation: u64,
        session: SessionKey,
        tenant: Tenant,
        actor: Option<String>,
        escalations: crate::EscalationHost,
        native: &Arc<loop_agent::native_backend::NativeBackend>,
    ) -> Arc<Self> {
        Arc::new(Self {
            log,
            generation,
            session,
            tenant,
            actor,
            escalations,
            native: Arc::downgrade(native),
            live: std::sync::OnceLock::new(),
        })
    }

    /// Bind the live Session, whose pump the barrier drains.
    pub(super) fn bind(&self, live: &Arc<LiveSession>) {
        let _ = self.live.set(Arc::downgrade(live));
    }
}

#[async_trait]
impl DecisionSink for LogDecisions {
    fn context(&self, escalation: Option<&str>) -> DecisionContext {
        DecisionContext {
            session: Some(self.session.0.clone()),
            turn: self
                .native
                .upgrade()
                .and_then(|native| native.current_turn())
                .map(|turn| turn.0),
            // A decision with no caller (an in-process prompt, work the
            // Runtime started) is the Runtime's own.
            caller: self
                .escalations
                .turn_caller()
                .unwrap_or_else(|| Caller::host(Principal::runtime())),
            answered_by: escalation.and_then(|id| self.escalations.take_answered_by(id)),
        }
    }

    async fn record(&self, record: &PolicyDecisionRecord) -> Result<(), DecisionError> {
        // `tool_call` first: commit what the backend already sent. An
        // allowed read has no effect to order, so it skips the barrier.
        let barrier_first = !loop_app_core::permissions::decision::has_no_effect(record);
        if let Some(live) = self
            .live
            .get()
            .and_then(std::sync::Weak::upgrade)
            .filter(|_| barrier_first)
        {
            barrier(&live)
                .await
                .map_err(|e| DecisionError::Write(e.to_string()))?;
        }
        // One source for who acted: the record's principal and `via`. The
        // row is in the Session's tenant. The record's own `tenant` is the
        // caller's; the two differ only when a caller of another tenant uses
        // the Session, which access checks (task I3) will refuse.
        let identity = Identity {
            tenant: self.tenant.clone(),
            principal: record.principal.clone(),
            via: record.via.clone(),
            actor: self.actor.clone(),
        };
        let event = Event {
            generation: self.generation,
            session_key: Some(self.session.clone()),
            turn: record.turn.clone().map(TurnId),
            ts_ms: record.ts_ms,
            kind: EventKind::PolicyDecision(serde_json::to_value(record)?),
            identity,
        };
        self.log
            .append_batch(vec![event])
            .await
            .map(|_| ())
            .map_err(|e| DecisionError::Write(e.to_string()))
    }
}
