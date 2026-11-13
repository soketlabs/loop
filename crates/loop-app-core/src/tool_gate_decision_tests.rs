//! The gate's decision records on every path, and the user command
//! (`session/shell`) rule.
use super::*;
use crate::permissions::decision::{DecisionContext, DecisionError};
use crate::permissions::{Capability, Layer, LayerSource, Setting, State};
use loop_agent::BeforeToolCallContext;
use loop_ai::ToolCall;
use loop_protocol::{Principal, PrincipalKind, Tenant};
use serde_json::json;
use PermissionMode::*;

/// A sink that keeps what it is given, or fails every write.
#[derive(Default)]
struct Capture {
    records: Mutex<Vec<PolicyDecisionRecord>>,
    fail: bool,
}

#[async_trait::async_trait]
impl DecisionSink for Capture {
    fn context(&self, escalation: Option<&str>) -> DecisionContext {
        DecisionContext {
            session: Some("s1".into()),
            turn: Some("turn-1".into()),
            caller: Caller::host(Principal::local_uid(501)),
            answered_by: escalation.map(|_| Caller::host(Principal::local_uid(502))),
        }
    }

    async fn record(&self, record: &PolicyDecisionRecord) -> Result<(), DecisionError> {
        if self.fail {
            return Err(DecisionError::Write("the disk is full".into()));
        }
        self.records.lock().push(record.clone());
        Ok(())
    }
}

impl Capture {
    fn take(&self) -> Vec<PolicyDecisionRecord> {
        std::mem::take(&mut *self.records.lock())
    }
}

fn gate(
    mode: PermissionMode,
    rules: &[(&str, &str)],
    layers: Vec<Layer>,
) -> (Arc<ToolGate>, Arc<Capture>) {
    let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
        &rules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ));
    gate.set_workspace(std::env::temp_dir());
    gate.set_mode(mode);
    gate.set_policy(crate::permissions::Policy::new(layers).0);
    let capture = Arc::new(Capture::default());
    gate.set_decision_sink(Some(capture.clone()));
    (gate, capture)
}

fn edge(id: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

/// A layer that turns the shell off and locks it, and a lower layer that
/// tries to turn it back on.
fn locked_off() -> Vec<Layer> {
    vec![
        Layer::new(LayerSource::Managed)
            .set(Capability::Shell, Setting::always(State::Off))
            .lock(Capability::Shell),
        Layer::new(LayerSource::Session).set(Capability::Shell, Setting::always(State::On)),
    ]
}

/// Each mode × deny rule × Lock: what a typed command gets, and the record
/// that says why. Where `bash` would ask, the host user who typed the
/// command is the approver, so it runs with no prompt (`by: user`).
#[tokio::test]
async fn a_user_command_is_judged_as_a_bash_call_in_every_mode() {
    type Row = (
        PermissionMode,
        &'static [(&'static str, &'static str)],
        bool,
        &'static str,
        Option<DecidedBy>,
    );
    const NONE: &[(&str, &str)] = &[];
    const DENY: &[(&str, &str)] = &[("bash", "deny")];
    const PATTERN: &[(&str, &str)] = &[("bash(touch*)", "deny")];
    const ASK: &[(&str, &str)] = &[("bash(touch*)", "ask")];
    let rows: &[Row] = &[
        // No rule, no lock: the mode decides.
        (ReadOnly, NONE, false, "touch x", Some(DecidedBy::Off)),
        (ReadOnly, NONE, false, "ls -la", None),
        (Plan, NONE, false, "touch x", Some(DecidedBy::Off)),
        (Plan, NONE, false, "git status", None),
        (Ask, NONE, false, "touch x", None),
        (AcceptEdits, NONE, false, "touch x", None),
        (FullAccess, NONE, false, "touch x", None),
        // A plain deny, a pattern deny: refused in every mode.
        (ReadOnly, DENY, false, "ls", Some(DecidedBy::Rule)),
        (Plan, DENY, false, "ls", Some(DecidedBy::Rule)),
        (Ask, DENY, false, "ls", Some(DecidedBy::Rule)),
        (AcceptEdits, DENY, false, "ls", Some(DecidedBy::Rule)),
        (FullAccess, DENY, false, "ls", Some(DecidedBy::Rule)),
        (Ask, PATTERN, false, "touch x", Some(DecidedBy::Rule)),
        (FullAccess, PATTERN, false, "touch x", Some(DecidedBy::Rule)),
        (
            FullAccess,
            PATTERN,
            false,
            "ls && touch x",
            Some(DecidedBy::Rule),
        ),
        (FullAccess, PATTERN, false, "ls", None),
        // A pattern ask: the user who typed it approves it.
        (Ask, ASK, false, "touch x", None),
        (FullAccess, ASK, false, "touch x", None),
        // A Lock that turns the shell off: refused in every mode, even
        // with a lower layer that turns it on.
        (ReadOnly, NONE, true, "ls", Some(DecidedBy::Off)),
        (Plan, NONE, true, "ls", Some(DecidedBy::Off)),
        (Ask, NONE, true, "touch x", Some(DecidedBy::Off)),
        (AcceptEdits, NONE, true, "touch x", Some(DecidedBy::Off)),
        (FullAccess, NONE, true, "touch x", Some(DecidedBy::Off)),
        (FullAccess, DENY, true, "touch x", Some(DecidedBy::Rule)),
    ];
    for (mode, rules, lock, command, refused_by) in rows {
        let layers = if *lock { locked_off() } else { vec![] };
        let (gate, capture) = gate(*mode, rules, layers);
        let got = gate.check_user_command(command, None, None).await;
        let row = format!("{mode:?} {rules:?} lock={lock} `{command}`");
        let records = capture.take();
        assert_eq!(records.len(), 1, "{row}: one record");
        let record = &records[0];
        assert_eq!(record.source, DecisionSource::UserShell, "{row}");
        assert_eq!(record.capability, "shell", "{row}");
        assert_eq!(record.tool, "bash", "{row}");
        match refused_by {
            Some(by) => {
                assert!(got.is_err(), "{row}: refused, got {got:?}");
                assert_eq!(record.outcome, DecisionOutcome::Deny, "{row}");
                assert_eq!(record.by, *by, "{row}");
                assert_eq!(record.reason, got.err().map(|e| e.0), "{row}");
            }
            None => {
                assert!(got.is_ok(), "{row}: runs, got {got:?}");
                assert_eq!(record.outcome, DecisionOutcome::Allow, "{row}");
            }
        }
        if *lock && refused_by == &Some(DecidedBy::Off) {
            assert_eq!(record.layer.as_deref(), Some("managed"), "{row}");
        }
    }
}

/// Who typed it: the host user approves an "ask" by typing; a user from
/// outside the host cannot, so an "ask" refuses. What the mode allows runs
/// for both. The record names the caller.
#[tokio::test]
async fn only_the_host_user_approves_a_typed_command_that_would_ask() {
    let host = Caller::host(Principal::local_uid(501));
    let (gate, capture) = gate(Ask, &[], vec![]);
    gate.check_user_command("touch x", Some("s9".into()), Some(host.clone()))
        .await
        .unwrap();
    let record = capture.take().remove(0);
    assert_eq!(record.by, DecidedBy::User);
    assert_eq!(record.principal, *host.principal());
    assert_eq!(record.session.as_deref(), Some("s9"));
    assert_eq!(record.subject.programs, ["touch"]);
    let refused = gate
        .check_user_command("touch x", None, Some(edge("alice")))
        .await
        .unwrap_err();
    assert!(refused.0.contains("outside this host"), "{refused}");
    let record = capture.take().remove(0);
    assert_eq!(record.outcome, DecisionOutcome::Deny);
    assert_eq!(record.by, DecidedBy::NoApprover);
    assert_eq!(record.principal, *edge("alice").principal());
    assert_eq!(record.tenant.as_str(), "acme");
    assert_eq!(record.via.as_ref().map(|v| v.as_str()), Some("edge"));
    // A layer's ask: the host user still approves it by typing.
    let layer = Layer::new(LayerSource::User).set(Capability::Shell, Setting::always(State::Ask));
    let (gate, capture) = gate_for(FullAccess, vec![layer]);
    gate.check_user_command("touch x", None, None)
        .await
        .unwrap();
    let record = capture.take().remove(0);
    assert_eq!(
        (record.by, record.layer.as_deref()),
        (DecidedBy::User, Some("user"))
    );
    let (gate, _) = gate_for(FullAccess, vec![]);
    gate.check_user_command("ls", None, Some(edge("alice")))
        .await
        .unwrap();
}

fn gate_for(mode: PermissionMode, layers: Vec<Layer>) -> (Arc<ToolGate>, Arc<Capture>) {
    gate(mode, &[], layers)
}

/// The tool lock is an outer layer, and the gate refuses under it too.
#[tokio::test]
async fn a_tool_lock_refuses_a_typed_command() {
    let (gate, capture) = gate(FullAccess, &[("bash", "allow")], vec![]);
    gate.lock_tools(crate::permissions::tool_lock::ToolLock::Chat);
    assert!(gate.check_user_command("ls", None, None).await.is_err());
    let record = capture.take().remove(0);
    assert_eq!(record.by, DecidedBy::Off);
    assert_eq!(record.layer.as_deref(), Some("runtime"));
}

/// The gate never acts on a decision it could not record.
#[tokio::test]
async fn a_record_that_cannot_be_written_refuses_the_call() {
    let gate = ToolGate::new(BTreeMap::new());
    gate.set_mode(FullAccess);
    gate.set_decision_sink(Some(Arc::new(Capture {
        fail: true,
        ..Capture::default()
    })));
    let blocked = gate.before_hook()(context("1", "bash", json!({"command": "ls"})), None)
        .await
        .expect("refused");
    assert!(blocked.block);
    assert!(blocked.reason.unwrap().contains("the disk is full"));
    let refused = gate.check_user_command("ls", None, None).await.unwrap_err();
    assert!(refused.0.contains("the disk is full"), "{refused}");
}

fn context(id: &str, name: &str, args: serde_json::Value) -> BeforeToolCallContext {
    BeforeToolCallContext::standalone(
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.clone(),
            thought_signature: None,
        },
        args,
    )
}

/// The model's calls, MCP tools, workflow shell tasks and `loop mcp serve`
/// each get one record, with their source and the step that decided.
#[tokio::test]
async fn every_before_hook_path_records_one_decision() {
    let (gate, capture) = gate(Ask, &[("bash(rm *)", "deny")], vec![]);
    let before = gate.before_hook();
    let read = before(context("c1", "read", json!({"path": "a.txt"})), None).await;
    assert!(read.is_none());
    let rm = before(context("c2", "bash", json!({"command": "rm -rf x"})), None).await;
    assert!(rm.unwrap().block);
    let headless = before(context("c3", "bash", json!({"command": "ls"})), None).await;
    assert!(headless.unwrap().block);
    let mcp = before(context("c4", "mcp__github__list", json!({})), None).await;
    assert!(mcp.is_none());
    let workflow = before(
        context("wf-shell-t1", "bash", json!({"command": "rm -rf y"})),
        None,
    )
    .await;
    assert!(workflow.unwrap().block);
    let serve = gate.before_hook_from(Some(DecisionSource::McpServe));
    assert!(serve(context("c5", "read", json!({"path": "b"})), None)
        .await
        .is_none());

    let records = capture.take();
    let got: Vec<_> = records
        .iter()
        .map(|r| (r.call_id.as_deref(), r.source.clone(), r.outcome, r.by))
        .collect();
    assert_eq!(
        got,
        [
            // The default rules allow `read`.
            (
                Some("c1"),
                DecisionSource::Native,
                DecisionOutcome::Allow,
                DecidedBy::Rule
            ),
            (
                Some("c2"),
                DecisionSource::Native,
                DecisionOutcome::Deny,
                DecidedBy::Rule
            ),
            (
                Some("c3"),
                DecisionSource::Native,
                DecisionOutcome::Deny,
                DecidedBy::NoApprover
            ),
            (
                Some("c4"),
                DecisionSource::Mcp("github".into()),
                DecisionOutcome::Allow,
                DecidedBy::Preset
            ),
            (
                Some("wf-shell-t1"),
                DecisionSource::Workflow,
                DecisionOutcome::Deny,
                DecidedBy::Rule
            ),
            (
                Some("c5"),
                DecisionSource::McpServe,
                DecisionOutcome::Allow,
                DecidedBy::Rule
            ),
        ]
    );
    let rm = &records[1];
    assert_eq!(rm.rule.as_deref(), Some("bash(rm *)"));
    assert_eq!(rm.capability, "shell");
    assert_eq!(rm.mode.as_deref(), Some("ask"));
    assert_eq!(
        (rm.session.as_deref(), rm.turn.as_deref()),
        (Some("s1"), Some("turn-1"))
    );
    assert_eq!(rm.principal, Principal::local_uid(501));
    assert_eq!(rm.tenant.as_str(), "local");
    assert_eq!(rm.subject.programs, ["rm"]);
    assert_eq!(
        rm.args_sha256.as_deref(),
        Some(crate::permissions::decision::args_sha256(&json!({"command": "rm -rf x"})).as_str())
    );
    assert_eq!(records[3].capability, "mcp:github");
}

/// An "ask" the user answers is one record with the answer and who gave
/// it; a hook's block and a Session grant name themselves.
#[tokio::test]
async fn asks_hooks_and_grants_are_recorded_with_who_decided() {
    let (gate, mut prompts) = gate_with_approver_and_sink(&[("bash", "ask")]);
    let capture = Arc::new(Capture::default());
    gate.set_decision_sink(Some(capture.clone()));
    let asking = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.before_hook()(context("c1", "bash", json!({"command": "ls"})), None).await
        })
    };
    let prompt = prompts.recv().await.unwrap();
    let _ = prompt
        .response_tx
        .send(crate::tool_approval::ApprovalDecision::Reject {
            reason: Some("not now".into()),
        });
    assert!(asking.await.unwrap().unwrap().block);
    let record = capture.take().remove(0);
    assert_eq!(record.outcome, DecisionOutcome::Ask);
    assert_eq!(record.answer, Some(DecisionAnswer::Deny));
    assert_eq!(record.by, DecidedBy::Approver);
    assert!(record.reason.unwrap().contains("not now"));

    gate.approver().unwrap().grant_session("bash");
    assert!(
        gate.before_hook()(context("c2", "bash", json!({"command": "ls"})), None)
            .await
            .is_none()
    );
    let record = capture.take().remove(0);
    assert_eq!(
        (record.outcome, record.by),
        (DecisionOutcome::Allow, DecidedBy::Grant)
    );

    let (_dir, registry) = super::tests::hooks(&[json!(
        {"on": "tool_call", "action": {"block": true, "reason": "hook says no"}}
    )]);
    gate.set_hooks(registry);
    let blocked = gate.before_hook()(context("c3", "bash", json!({"command": "ls"})), None)
        .await
        .unwrap();
    assert!(blocked.block);
    let record = capture.take().remove(0);
    assert_eq!(
        (record.outcome, record.by),
        (DecisionOutcome::Deny, DecidedBy::Hook)
    );
}

fn gate_with_approver_and_sink(
    permissions: &[(&str, &str)],
) -> (
    Arc<ToolGate>,
    tokio::sync::mpsc::UnboundedReceiver<crate::tool_approval::ApprovalPrompt>,
) {
    let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
        &permissions
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ));
    let env: Arc<dyn loop_agent::harness::ExecutionEnv> = Arc::new(
        loop_agent::harness::HostExecutionEnv::new(std::env::temp_dir()),
    );
    let (bridge, prompts) = ToolApprovalBridge::new(env, None, Default::default());
    gate.set_approver(Some(Arc::new(bridge)));
    (gate, prompts)
}
