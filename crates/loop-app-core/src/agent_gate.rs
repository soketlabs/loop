//! External agents' tool calls and the tool gate.
//!
//! An ACP agent runs its own tools. Loop sees two things:
//!
//! - **permission requests**, which [`GatedHost`] passes through the gate's
//!   `tool_call` hooks before the user is asked. A block picks the agent's
//!   own "reject once" option and an allow its "allow once" option (never an
//!   "always" one). The call is judged by kind against the same Capability
//!   table as Loop's own tools: a Policy layer that turns its Capability off
//!   rejects it, a `toolPermissions` deny rule for a Loop tool of the same
//!   kind rejects it, and so does a mode that turns it off (`read-only`
//!   rejects edits and commands), all before hooks run. A layer's "ask"
//!   always reaches the user. Then `full-access` allows once; otherwise the
//!   user is asked. Each request's decision is recorded before Loop answers
//!   the agent ([`crate::permissions::decision`]).
//! - **tool-call updates**, which [`AgentToolObserver`] turns into
//!   `tool_result` events once a call finishes. These are observe-only: the
//!   agent already ran the tool.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use loop_agent::harness::{HarnessHookEvent, ToolCallInfo, ToolDecision, ToolSource};
use loop_protocol::{
    ContentBlock, HostFileSystem, HostServices, HostTerminals, InteractionOutcome,
    InteractionRequest, PermissionKind, PermissionOption, SessionUpdate, ToolCallStatus,
    ToolCallUpdate, ToolContent, ToolKind,
};

use crate::permissions::decision::{DecidedBy, DecisionAnswer, DecisionSource};
use crate::permissions::{Call, Capability, Ceiling, PermissionMode, State};
use crate::tool_gate::{Decided, Facts, ToolGate};

/// A host that runs the gate's hooks on an agent's permission requests
/// before handing them to `inner`.
pub struct GatedHost {
    inner: Arc<dyn HostServices>,
    gate: Arc<ToolGate>,
    agent: String,
    cwd: PathBuf,
}

impl GatedHost {
    /// Gate `agent`'s requests (`agent` is its id, e.g. `chatgpt`).
    pub fn new(
        inner: Arc<dyn HostServices>,
        gate: Arc<ToolGate>,
        agent: impl Into<String>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            inner,
            gate,
            agent: agent.into(),
            cwd,
        }
    }

    fn info(&self, call: &ToolCallUpdate) -> ToolCallInfo {
        let mut info = agent_call_info(
            &self.agent,
            &self.cwd,
            call.id.0.clone(),
            call.name.as_deref().or(call.title.as_deref()),
            call.kind,
            call.raw_input.clone(),
        );
        // So `match.mode` and `LOOP_PERMISSION_MODE` work for agent calls.
        info.mode = Some(self.gate.mode().as_str().to_owned());
        info
    }
}

impl GatedHost {
    /// The gate's decision on an agent's permission request: `Some` answers
    /// it without asking the user; `None` asks. The [`Decided`] is what to
    /// record; for an ask, the answer is added once it comes.
    async fn judge(
        &self,
        call: &ToolCallUpdate,
        options: &[PermissionOption],
        judged: &Call,
    ) -> (Option<InteractionOutcome>, Decided) {
        let reject = |reason: String| reject_with(options, reason);
        let allow_once = || {
            pick(options, &[PermissionKind::AllowOnce]).map(|native_id| {
                InteractionOutcome::Selected {
                    native_id,
                    note: None,
                }
            })
        };
        let ceiling = self.gate.policy().ceiling(judged);
        if let Some(Ceiling {
            state: State::Off,
            source,
        }) = ceiling
        {
            let reason = format!(
                "{} turns off {}",
                source.label(),
                Capability::of(judged).label()
            );
            let mut decided = Decided::deny(DecidedBy::Off, reason.clone());
            decided.layer = Some(source);
            return (Some(reject(reason)), decided);
        }
        // Deny rules and the mode's "off" come before hooks: a hook's
        // allow can't lift either (docs/hooks.md).
        if let Some((reason, rule)) =
            self.gate
                .agent_call_denied_by(judged.kind, &judged.args, &self.cwd)
        {
            let mut decided = Decided::deny(DecidedBy::Rule, reason.clone());
            decided.rule = rule;
            return (Some(reject(reason)), decided);
        }
        let mode = self.gate.mode();
        if mode.setting(&Capability::of(judged)).state_for(judged) == State::Off {
            let reason = format!(
                "Loop is in {} mode; the user can switch with /permissions",
                mode.as_str()
            );
            return (
                Some(reject(reason.clone())),
                Decided::deny(DecidedBy::Off, reason),
            );
        }
        // A Policy layer's "ask" goes to the user, whatever the mode or a
        // hook says.
        let must_ask = ceiling.is_some_and(|c| c.state == State::Ask);
        // The approver decides; a layer's "ask" names the layer.
        let mut asked = Decided::answered(false, None);
        if must_ask {
            asked.layer = ceiling.map(|c| c.source);
        }
        match self.gate.review_agent_call(self.info(call)).await {
            Some(ToolDecision::Block { reason }) => {
                let decided = Decided::deny(DecidedBy::Hook, reason.clone());
                return (Some(reject(reason)), decided);
            }
            Some(ToolDecision::Allow) if !must_ask => {
                if let Some(outcome) = allow_once() {
                    return (Some(outcome), Decided::allow(DecidedBy::Hook));
                }
            }
            _ => {}
        }
        if mode == PermissionMode::FullAccess && !must_ask {
            if let Some(outcome) = allow_once() {
                return (Some(outcome), Decided::allow(DecidedBy::Preset));
            }
        }
        (None, asked)
    }
}

/// Reject an agent's request with its own "reject" option, or plainly.
fn reject_with(options: &[PermissionOption], reason: String) -> InteractionOutcome {
    let option = pick(options, &[PermissionKind::RejectOnce])
        .or_else(|| pick(options, &[PermissionKind::RejectAlways]));
    match option {
        Some(native_id) => InteractionOutcome::Selected {
            native_id,
            note: Some(reason),
        },
        None => InteractionOutcome::Rejected { reason },
    }
}

/// Whether the user's answer lets the agent run the call.
fn approved(options: &[PermissionOption], outcome: &InteractionOutcome) -> bool {
    match outcome {
        InteractionOutcome::Selected { native_id, .. } => options.iter().any(|o| {
            &o.native_id == native_id
                && matches!(
                    o.kind_hint,
                    PermissionKind::AllowOnce | PermissionKind::AllowAlways
                )
        }),
        _ => false,
    }
}

#[async_trait]
impl HostServices for GatedHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        let InteractionRequest::Permission {
            tool_call: Some(call),
            options,
            ..
        } = &request
        else {
            return self.inner.interact(request).await;
        };
        // An agent's tool names aren't Loop's, so its call is judged by
        // kind; one with no kind may run anything.
        let judged = Call {
            kind: call.kind.unwrap_or(ToolKind::Execute),
            ..Call::new(
                "agent",
                call.raw_input.clone().unwrap_or_default(),
                self.cwd.clone(),
            )
        };
        let (answer, mut decided) = self.judge(call, options, &judged).await;
        let tool = call
            .name
            .clone()
            .or_else(|| call.title.clone())
            .unwrap_or_else(|| "agent".to_owned());
        let outcome = match answer {
            Some(outcome) => outcome,
            None => {
                let outcome = self.inner.interact(request.clone()).await;
                decided.answer = Some(if approved(options, &outcome) {
                    DecisionAnswer::Allow
                } else {
                    DecisionAnswer::Deny
                });
                decided.escalation = Some(request.id().to_string());
                outcome
            }
        };
        let facts = Facts {
            source: DecisionSource::Agent(self.agent.clone()),
            tool: &tool,
            kind: judged.kind,
            call_id: Some(&call.id.0),
            args: &judged.args,
            caller: None,
            session: None,
        };
        // The agent runs the call once Loop answers: record first, and
        // refuse when the record cannot be written.
        match self.gate.record(facts, &decided).await {
            Ok(()) => outcome,
            Err(reason) => reject_with(options, reason),
        }
    }

    fn file_system(&self) -> Option<&dyn HostFileSystem> {
        self.inner.file_system()
    }

    fn terminals(&self) -> Option<&dyn HostTerminals> {
        self.inner.terminals()
    }
}

fn pick(options: &[PermissionOption], kinds: &[PermissionKind]) -> Option<String> {
    options
        .iter()
        .find(|o| kinds.contains(&o.kind_hint))
        .map(|o| o.native_id.clone())
}

fn agent_call_info(
    agent: &str,
    cwd: &std::path::Path,
    call_id: String,
    tool: Option<&str>,
    kind: Option<ToolKind>,
    raw_input: Option<serde_json::Value>,
) -> ToolCallInfo {
    ToolCallInfo {
        source: ToolSource::Agent {
            id: agent.to_owned(),
        },
        tool: tool.unwrap_or("tool").to_owned(),
        kind: kind.unwrap_or(ToolKind::Other),
        args: raw_input.unwrap_or(serde_json::Value::Null),
        call_id,
        session_id: String::new(),
        cwd: cwd.to_path_buf(),
        mode: None,
    }
}

/// Follows an agent's tool calls on its update stream and reports each one
/// that finishes as a `tool_result` event.
pub struct AgentToolObserver {
    agent: String,
    cwd: PathBuf,
    open: HashMap<String, ToolCallInfo>,
}

impl AgentToolObserver {
    /// An observer for `agent`'s live updates.
    pub fn new(agent: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            agent: agent.into(),
            cwd,
            open: HashMap::new(),
        }
    }

    /// The event `update` completes, if any.
    pub fn on_update(&mut self, session: &str, update: &SessionUpdate) -> Option<HarnessHookEvent> {
        match update {
            SessionUpdate::ToolCall(call) => {
                let mut info = agent_call_info(
                    &self.agent,
                    &self.cwd,
                    call.id.0.clone(),
                    Some(call.name.as_deref().unwrap_or(&call.title)),
                    Some(call.kind),
                    call.raw_input.clone(),
                );
                info.session_id = session.to_owned();
                if finished(Some(call.status)) {
                    return Some(result_event(
                        info,
                        call.status == ToolCallStatus::Failed,
                        &call.content,
                    ));
                }
                self.open.insert(call.id.0.clone(), info);
                None
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(info) = self.open.get_mut(&update.id.0) {
                    if let Some(input) = &update.raw_input {
                        info.args = input.clone();
                    }
                    if let Some(kind) = update.kind {
                        info.kind = kind;
                    }
                }
                if !finished(update.status) {
                    return None;
                }
                let info = self.open.remove(&update.id.0)?;
                Some(result_event(
                    info,
                    update.status == Some(ToolCallStatus::Failed),
                    update.content.as_deref().unwrap_or_default(),
                ))
            }
            _ => None,
        }
    }
}

fn finished(status: Option<ToolCallStatus>) -> bool {
    matches!(
        status,
        Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
    )
}

fn result_event(call: ToolCallInfo, is_error: bool, content: &[ToolContent]) -> HarnessHookEvent {
    let output = content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Content {
                content: ContentBlock::Text { text },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    HarnessHookEvent::ToolResult {
        call,
        is_error,
        output,
    }
}

/// Feed `backend`'s live tool calls to the gate's `tool_result` hooks until
/// the backend's stream closes.
pub fn observe_agent_tools(
    backend: &dyn loop_protocol::AgentBackend,
    gate: Arc<ToolGate>,
    agent: String,
    cwd: PathBuf,
) {
    let mut updates = backend.subscribe();
    tokio::spawn(async move {
        let mut observer = AgentToolObserver::new(agent, cwd);
        loop {
            match updates.recv().await {
                Ok(envelope) => {
                    if envelope.origin != loop_protocol::Origin::Live {
                        continue;
                    }
                    if let Some(mut event) =
                        observer.on_update(&envelope.session.0, &envelope.update)
                    {
                        if let HarnessHookEvent::ToolResult { call, .. } = &mut event {
                            call.mode = Some(gate.mode().as_str().to_owned());
                        }
                        gate.observe(event).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use loop_protocol::{InteractionId, ToolCall, ToolCallId};
    use serde_json::json;

    /// Records whether the user was asked; answers with the first option.
    struct Asked(AtomicBool);

    #[async_trait]
    impl HostServices for Asked {
        async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
            self.0.store(true, Ordering::SeqCst);
            InteractionOutcome::Cancelled
        }
    }

    fn gate_with(hook: serde_json::Value) -> (tempfile::TempDir, Arc<ToolGate>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        std::fs::write(&path, hook.to_string()).unwrap();
        let registry = loop_agent::harness::HookRegistry::default();
        crate::hooks_load::register_hooks(&registry, crate::hooks_load::load_hooks(&[path]));
        let gate = ToolGate::new(Default::default());
        gate.set_hooks(registry);
        (dir, gate)
    }

    fn option(id: &str, kind: PermissionKind) -> PermissionOption {
        PermissionOption {
            native_id: id.into(),
            label: id.into(),
            kind_hint: kind,
        }
    }

    fn exec_request(command: &str) -> InteractionRequest {
        let mut call = ToolCallUpdate::new(ToolCallId("t1".into()));
        call.title = Some(format!("Run {command}"));
        call.kind = Some(ToolKind::Execute);
        call.raw_input = Some(json!({ "command": command }));
        InteractionRequest::Permission {
            id: InteractionId("i1".into()),
            tool_call: Some(call),
            options: vec![
                option("always", PermissionKind::AllowAlways),
                option("once", PermissionKind::AllowOnce),
                option("no", PermissionKind::RejectOnce),
            ],
        }
    }

    async fn ask(gate: Arc<ToolGate>, request: InteractionRequest) -> (InteractionOutcome, bool) {
        let asked = Arc::new(Asked(AtomicBool::new(false)));
        let host = GatedHost::new(asked.clone(), gate, "chatgpt", PathBuf::from("/tmp"));
        let outcome = host.interact(request).await;
        (outcome, asked.0.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_hook_block_rejects_once_without_asking() {
        let (_dir, gate) = gate_with(json!({
            "name": "no-rm", "on": "tool_call",
            "match": {"source": "agent:chatgpt", "kind": "execute", "args": {"command": "rm -rf*"}},
            "action": {"block": true}
        }));
        let (outcome, asked) = ask(Arc::clone(&gate), exec_request("rm -rf build")).await;
        assert!(!asked);
        assert_eq!(
            outcome,
            InteractionOutcome::Selected {
                native_id: "no".into(),
                note: Some("Blocked by hook no-rm".into())
            }
        );
        let (_, asked) = ask(gate, exec_request("ls")).await;
        assert!(asked, "unmatched requests still reach the user");
    }

    #[tokio::test]
    async fn agent_calls_carry_the_permission_mode() {
        let (_dir, gate) = gate_with(json!({
            "on": "tool_call", "match": {"source": "agent", "mode": "accept-edits"},
            "action": {"block": true}
        }));
        let (_, asked) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(asked, "the hook is for accept-edits only");
        gate.set_mode(PermissionMode::AcceptEdits);
        let (_, asked) = ask(gate, exec_request("ls")).await;
        assert!(!asked, "a mode-matched hook blocks the agent's call");
    }

    #[tokio::test]
    async fn a_hook_allow_picks_allow_once_never_always() {
        let (_dir, gate) = gate_with(json!({
            "on": "tool_call", "match": {"source": "agent"}, "action": {"allow": true}
        }));
        let (outcome, asked) = ask(gate, exec_request("ls")).await;
        assert!(!asked);
        assert_eq!(
            outcome,
            InteractionOutcome::Selected {
                native_id: "once".into(),
                note: None
            }
        );
    }

    #[tokio::test]
    async fn a_block_without_a_reject_option_is_rejected() {
        let (_dir, gate) = gate_with(json!({"on": "tool_call", "action": {"block": true}}));
        let mut request = exec_request("ls");
        if let InteractionRequest::Permission { options, .. } = &mut request {
            options.retain(|o| o.kind_hint != PermissionKind::RejectOnce);
        }
        let (outcome, asked) = ask(gate, request).await;
        assert!(!asked);
        assert!(matches!(outcome, InteractionOutcome::Rejected { .. }));
    }

    #[test]
    fn finished_agent_calls_become_tool_results() {
        let mut observer = AgentToolObserver::new("chatgpt", PathBuf::from("/tmp"));
        let start = SessionUpdate::ToolCall(ToolCall {
            id: ToolCallId("t1".into()),
            name: None,
            title: "Run cargo test".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(json!({"command": "cargo test"})),
        });
        assert!(observer.on_update("s1", &start).is_none());

        let mut done = ToolCallUpdate::new(ToolCallId("t1".into()));
        done.status = Some(ToolCallStatus::Failed);
        done.content = Some(vec![ToolContent::Content {
            content: ContentBlock::Text {
                text: "1 failed".into(),
            },
        }]);
        let event = observer
            .on_update("s1", &SessionUpdate::ToolCallUpdate(done))
            .expect("a result");
        let HarnessHookEvent::ToolResult {
            call,
            is_error,
            output,
        } = event
        else {
            panic!("expected a tool result");
        };
        assert!(is_error);
        assert_eq!(output, "1 failed");
        assert_eq!(call.tool, "Run cargo test");
        assert_eq!(call.session_id, "s1");
        assert_eq!(call.args["command"], "cargo test");
        assert_eq!(
            call.source,
            ToolSource::Agent {
                id: "chatgpt".into()
            }
        );
    }

    #[tokio::test]
    async fn policy_layers_tighten_agent_requests() {
        use crate::permissions::{Capability, Layer, LayerSource, Policy, Setting, State};
        let layer = |state| {
            Policy::new(vec![
                Layer::new(LayerSource::Org).set(Capability::Shell, Setting::always(state))
            ])
            .0
        };
        let (_dir, gate) = gate_with(json!({"on": "tool_call", "action": {"allow": true}}));
        gate.set_mode(PermissionMode::FullAccess);

        gate.set_policy(layer(State::Off));
        let (outcome, asked) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(!asked);
        assert!(
            matches!(&outcome, InteractionOutcome::Selected { native_id, note: Some(note) }
                if native_id == "no" && note.contains("Org Policy turns off the shell")),
            "{outcome:?}"
        );

        gate.set_policy(layer(State::Ask));
        let (_, asked) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(
            asked,
            "a layer's ask reaches the user despite full-access and a hook allow"
        );
    }

    #[tokio::test]
    async fn the_permission_mode_answers_agent_requests() {
        let gate = ToolGate::new(Default::default());
        gate.set_mode(PermissionMode::FullAccess);
        let (outcome, asked) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(!asked);
        assert_eq!(
            outcome,
            InteractionOutcome::Selected {
                native_id: "once".into(),
                note: None
            }
        );

        gate.set_mode(PermissionMode::ReadOnly);
        let (outcome, asked) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(!asked);
        assert!(
            matches!(outcome, InteractionOutcome::Selected { native_id, .. } if native_id == "no")
        );

        gate.set_mode(PermissionMode::AcceptEdits);
        let (_, asked) = ask(gate, exec_request("ls")).await;
        assert!(asked, "accept-edits asks the user about commands");
    }

    /// Keeps every decision record it is given.
    #[derive(Default)]
    struct Records(parking_lot::Mutex<Vec<crate::permissions::decision::PolicyDecisionRecord>>);

    #[async_trait]
    impl crate::permissions::decision::DecisionSink for Records {
        fn context(
            &self,
            escalation: Option<&str>,
        ) -> crate::permissions::decision::DecisionContext {
            let me = loop_protocol::Caller::host(loop_protocol::Principal::local_uid(501));
            crate::permissions::decision::DecisionContext {
                session: Some("s1".into()),
                turn: None,
                answered_by: escalation.map(|_| me.clone()),
                caller: me,
            }
        }
        async fn record(
            &self,
            record: &crate::permissions::decision::PolicyDecisionRecord,
        ) -> Result<(), crate::permissions::decision::DecisionError> {
            self.0.lock().push(record.clone());
            Ok(())
        }
    }

    /// Each permission request of an ACP agent is one decision record,
    /// written before Loop answers the agent: a deny rule, `full-access`,
    /// and an ask with the user's answer and who gave it.
    #[tokio::test]
    async fn every_agent_request_is_recorded_with_its_source() {
        use crate::permissions::decision::{DecidedBy, DecisionOutcome};
        let records = Arc::new(Records::default());
        let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
            &[("bash(rm *)".to_owned(), "deny".to_owned())].into(),
        ));
        gate.set_decision_sink(Some(records.clone()));
        let (denied, asked) = ask(Arc::clone(&gate), exec_request("rm -rf x")).await;
        assert!(!asked);
        assert!(
            matches!(denied, InteractionOutcome::Selected { ref native_id, .. } if native_id == "no")
        );
        let (_, asked) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(asked, "ask mode asks the user");
        gate.set_mode(PermissionMode::FullAccess);
        let (allowed, _) = ask(Arc::clone(&gate), exec_request("ls")).await;
        assert!(
            matches!(allowed, InteractionOutcome::Selected { ref native_id, .. } if native_id == "once")
        );
        let records = std::mem::take(&mut *records.0.lock());
        let got: Vec<_> = records
            .iter()
            .map(|r| (r.source.to_string(), r.outcome, r.answer, r.by))
            .collect();
        use crate::permissions::decision::DecisionAnswer;
        assert_eq!(
            got,
            [
                (
                    "agent:chatgpt".to_owned(),
                    DecisionOutcome::Deny,
                    None,
                    DecidedBy::Rule
                ),
                (
                    "agent:chatgpt".to_owned(),
                    DecisionOutcome::Ask,
                    Some(DecisionAnswer::Deny),
                    DecidedBy::Approver
                ),
                (
                    "agent:chatgpt".to_owned(),
                    DecisionOutcome::Allow,
                    None,
                    DecidedBy::Preset
                ),
            ]
        );
        assert_eq!(records[0].rule.as_deref(), Some("bash(rm *)"));
        assert_eq!(records[0].call_id.as_deref(), Some("t1"));
        assert_eq!(records[1].escalation.as_deref(), Some("i1"));
        assert_eq!(
            records[1].answered_by,
            Some(loop_protocol::Principal::local_uid(501))
        );
        assert_eq!(records[2].capability, "shell");
    }
}
