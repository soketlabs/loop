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
    assert_eq!(record.reason.as_deref(), Some("the approver rejected it"));

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

/// Commands the splitter cannot model are unreadable (S4 §7, first slice):
/// they are not read-only, a bash deny pattern refuses them (fail closed),
/// and an allow pattern does not match them. These are the review's bypass
/// strings; each ran before this check.
mod readable_commands {
    use super::*;
    use crate::permissions::rules::{read_only_command, unreadable};

    /// ANSI-C quoting hides the newline that starts `touch PWNED`.
    const SMUGGLED: &str = "echo $'\\''\ntouch PWNED\necho '";
    /// A function named `ls` that runs `touch`.
    const FUNCTION: &str = "ls () ( touch PWNED ); ls";

    fn bypasses() -> Vec<&'static str> {
        vec![
            SMUGGLED,
            FUNCTION,
            "ls () { touch PWNED; }; ls",
            "{ touch PWNED; }",
            "echo {touch,PWNED}",
            "cat <(touch PWNED)",
            "cat <<EOF\nx\nEOF",
            "tou\\\nch PWNED",
            "echo `touch PWNED`",
            "$(printf tou)ch PWNED",
            "env touch PWNED",
            "command touch PWNED",
            "X=1 touch PWNED",
            "bash -c 'touch PWNED'",
            "printf 'touch PWNED' | sh",
            "python3 -c 'open(\"PWNED\",\"w\")'",
            "$CMD PWNED",
            "eval 'touch PWNED'",
            "xargs touch < list",
        ]
    }

    #[test]
    fn each_bypass_is_unreadable_and_not_read_only() {
        for command in bypasses() {
            assert!(unreadable(command).is_some(), "readable: {command:?}");
            assert!(!read_only_command(command), "read-only: {command:?}");
        }
        assert!(!read_only_command("tree -ao out.txt"));
        assert!(!read_only_command("tree --output=out.txt"));
        assert!(!read_only_command("git diff --ext-diff"));
    }

    /// Normal use does not regress.
    #[test]
    fn ordinary_commands_stay_readable_and_read_only() {
        for command in [
            "ls -la",
            "cat a b",
            "grep -rn x .",
            "git status",
            "git log --oneline -5",
            "git log --format='%H (%an)'",
            "rg 'fn main(' src",
            "cat Cargo.toml | grep version | sort | uniq",
            "find . -name '*.rs'",
            "echo \"${HOME}\"",
            "tree -a src",
            "echo 'a (b) {c}'",
        ] {
            assert!(unreadable(command).is_none(), "unreadable: {command:?}");
            assert!(read_only_command(command), "not read-only: {command:?}");
        }
        for command in [
            "cargo test 2>&1 | tail -5",
            "cargo build &> log",
            "make >| out",
        ] {
            assert!(unreadable(command).is_none(), "unreadable: {command:?}");
        }
        assert_eq!(
            crate::permissions::split_command("cargo test 2>&1 | tail -5").unwrap(),
            ["cargo test 2>&1", "tail -5"]
        );
    }

    /// On the user command path: read-only and plan modes refuse each one,
    /// a deny pattern refuses each one in full access (`by: unreadable`),
    /// and with no pattern the mode decides.
    #[tokio::test]
    async fn the_gate_refuses_each_bypass_on_the_user_command_path() {
        for command in bypasses() {
            for mode in [ReadOnly, Plan] {
                let (g, _c) = gate(mode, &[], vec![]);
                assert!(
                    g.check_user_command(command, None, None).await.is_err(),
                    "{mode:?} ran {command:?}"
                );
            }
            let (g, c) = gate(FullAccess, &[("bash(touch*)", "deny")], vec![]);
            let refused = g.check_user_command(command, None, None).await.unwrap_err();
            assert!(
                refused.0.contains("cannot check this command") && refused.0.contains("to run it"),
                "{command:?}: {refused}"
            );
            assert_eq!(c.take()[0].by, DecidedBy::Unreadable, "{command:?}");
            let (g, _c) = gate(FullAccess, &[], vec![]);
            assert!(g.check_user_command(command, None, None).await.is_ok());
        }
        let (g, _c) = gate(FullAccess, &[("bash(touch*)", "deny")], vec![]);
        for command in ["/usr/bin/touch x", "TOUCH x", "touch x"] {
            assert!(
                g.check_user_command(command, None, None).await.is_err(),
                "{command}"
            );
        }
        assert!(g.check_user_command("ls -la", None, None).await.is_ok());
        let (g, _c) = gate(FullAccess, &[], locked_off());
        assert!(g.check_user_command(SMUGGLED, None, None).await.is_err());
    }

    /// The same on the model's tool path, and an allow pattern never
    /// matches an unreadable command.
    #[tokio::test]
    async fn the_gate_refuses_each_bypass_on_the_model_path() {
        let (g, c) = gate(ReadOnly, &[], vec![]);
        for command in bypasses() {
            let blocked = g.before_hook()(context("m", "bash", json!({"command": command})), None)
                .await
                .expect("refused");
            assert!(blocked.block, "{command:?}");
        }
        assert_eq!(c.take().len(), bypasses().len());
        let (g, _c) = gate(FullAccess, &[("bash(touch*)", "deny")], vec![]);
        for command in bypasses() {
            let blocked =
                g.before_hook()(context("m", "bash", json!({"command": command})), None).await;
            assert!(blocked.is_some_and(|b| b.block), "{command:?}");
        }
        let (g, _c) = gate(Ask, &[("bash", "ask"), ("bash(ls*)", "allow")], vec![]);
        assert_eq!(
            g.decide("bash", &json!({"command": "ls -la"})).await,
            Decision::Allow
        );
        assert_eq!(
            g.decide("bash", &json!({"command": FUNCTION})).await,
            Decision::Ask
        );
    }

    /// The bash block list sees the command the shell runs.
    #[test]
    fn the_block_list_sees_through_ansi_c_quoting_and_functions() {
        let check = loop_agent::harness::tools::check_command_policy;
        assert!(check("echo $'\\''\nrm -rf /\necho '", &[]).is_err());
        assert!(check("ls () ( rm -rf / ); ls", &[]).is_err());
    }
}

/// No record keeps a secret: an approver's refusal of a fetch with
/// credentials and a token, of a search, a hook's block text, an ACP
/// agent's title, and a user command with a token.
mod records_keep_no_secrets {
    use super::*;

    const MARKS: &[&str] = &[
        "S3CRET",
        "T0KEN",
        "QUERY-SECRET",
        "HOOK-SECRET",
        "CMD-SECRET",
    ];

    fn clean(records: &[PolicyDecisionRecord]) {
        let text = serde_json::to_string(records).unwrap();
        for mark in MARKS {
            assert!(!text.contains(mark), "{mark} in {text}");
        }
    }

    async fn rejected(tool: &str, args: serde_json::Value) -> Vec<PolicyDecisionRecord> {
        let (gate, mut prompts) = gate_with_approver_and_sink(&[(tool, "ask")]);
        let capture = Arc::new(Capture::default());
        gate.set_decision_sink(Some(capture.clone()));
        let asking = {
            let gate = Arc::clone(&gate);
            let tool = tool.to_owned();
            tokio::spawn(async move { gate.before_hook()(context("c1", &tool, args), None).await })
        };
        let prompt = prompts.recv().await.unwrap();
        let _ = prompt
            .response_tx
            .send(crate::tool_approval::ApprovalDecision::Reject {
                reason: Some("because QUERY-SECRET".into()),
            });
        assert!(asking.await.unwrap().unwrap().block);
        capture.take()
    }

    #[tokio::test]
    async fn on_every_path() {
        let fetch = rejected(
            "web_fetch",
            json!({"url": "https://user:S3CRET@h.example/x?token=T0KEN"}),
        )
        .await;
        assert_eq!(fetch[0].subject.host.as_deref(), Some("https://h.example"));
        clean(&fetch);
        clean(&rejected("web_search", json!({"query": "QUERY-SECRET"})).await);

        let (g, capture) = super::gate(FullAccess, &[], vec![]);
        let (_dir, registry) = super::super::tests::hooks(&[json!(
            {"on": "tool_call", "action": {"block": true, "reason": "HOOK-SECRET in https://u:S3CRET@x"}}
        )]);
        g.set_hooks(registry);
        assert!(
            g.before_hook()(context("c2", "bash", json!({"command": "ls"})), None)
                .await
                .unwrap()
                .block
        );
        clean(&capture.take());

        let (g, capture) = super::gate(FullAccess, &[("bash(curl*)", "deny")], vec![]);
        let command = "curl -H 'Authorization: CMD-SECRET' https://u:S3CRET@h/x?token=T0KEN";
        assert!(g.check_user_command(command, None, None).await.is_err());
        let records = capture.take();
        assert_eq!(records[0].subject.programs, ["curl"]);
        clean(&records);

        let (g, capture) = super::gate(FullAccess, &[], vec![]);
        let host = crate::agent_gate::GatedHost::new(
            Arc::new(Nobody),
            Arc::clone(&g),
            "codex",
            std::env::temp_dir(),
        );
        let mut call = loop_protocol::ToolCallUpdate::new(loop_protocol::ToolCallId("a1".into()));
        call.title = Some("curl https://u:S3CRET@h?token=T0KEN".into());
        call.kind = Some(ToolKind::Execute);
        call.raw_input = Some(json!({"command": "curl CMD-SECRET"}));
        let request = loop_protocol::InteractionRequest::Permission {
            id: loop_protocol::InteractionId("i1".into()),
            tool_call: Some(call),
            options: vec![loop_protocol::PermissionOption {
                native_id: "once".into(),
                label: "once".into(),
                kind_hint: loop_protocol::PermissionKind::AllowOnce,
            }],
        };
        use loop_protocol::HostServices;
        let _ = host.interact(request).await;
        let records = capture.take();
        assert_eq!(records[0].tool, "agent");
        clean(&records);
    }

    struct Nobody;
    #[async_trait::async_trait]
    impl loop_protocol::HostServices for Nobody {
        async fn interact(
            &self,
            _: loop_protocol::InteractionRequest,
        ) -> loop_protocol::InteractionOutcome {
            loop_protocol::InteractionOutcome::Cancelled
        }
    }
}

/// The block list refuses a typed command, and the record says so: the
/// stored outcome is what happened.
#[tokio::test]
async fn the_block_list_refusal_is_the_recorded_outcome() {
    let (g, capture) = gate(FullAccess, &[], vec![]);
    let refused = g
        .check_user_command("rm -rf /", None, None)
        .await
        .unwrap_err();
    assert!(!refused.0.is_empty());
    let record = capture.take().remove(0);
    assert_eq!(
        (record.outcome, record.by),
        (DecisionOutcome::Deny, DecidedBy::Rule)
    );
    assert_eq!(record.rule.as_deref(), Some("bash block list"));
    g.check_user_command("rm -rf ./build", None, None)
        .await
        .unwrap();
    assert_eq!(capture.take()[0].outcome, DecisionOutcome::Allow);
}

/// The effects of an approver's answer come only after the answer is
/// recorded: with a sink that cannot write, approving a plan leaves the
/// mode as it was, and "accept all" grants nothing.
#[tokio::test]
async fn an_answer_s_effects_wait_for_its_record() {
    use crate::tool_approval::ApprovalDecision;
    let (gate, mut prompts) = gate_with_approver_and_sink(&[("bash", "ask")]);
    gate.set_decision_sink(Some(Arc::new(Capture {
        fail: true,
        ..Capture::default()
    })));
    gate.set_mode(Ask);
    gate.set_mode(Plan);
    let pending = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.before_hook()(context("p", EXIT_PLAN_TOOL, json!({"plan": "x"})), None).await
        })
    };
    let prompt = prompts.recv().await.unwrap();
    prompt
        .response_tx
        .send(ApprovalDecision::AcceptSession)
        .unwrap();
    assert!(pending.await.unwrap().unwrap().block);
    assert_eq!(gate.mode(), Plan, "the mode changed without a record");

    gate.set_mode(Ask);
    let pending = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.before_hook()(context("b", "bash", json!({"command": "ls"})), None).await
        })
    };
    let prompt = prompts.recv().await.unwrap();
    prompt
        .response_tx
        .send(ApprovalDecision::AcceptSession)
        .unwrap();
    assert!(pending.await.unwrap().unwrap().block);
    assert!(
        gate.approver().unwrap().should_ask("bash"),
        "a grant was made without a record"
    );

    // Control: with a sink that writes, both effects happen.
    gate.set_decision_sink(Some(Arc::new(Capture::default())));
    let pending = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.before_hook()(context("b2", "bash", json!({"command": "ls"})), None).await
        })
    };
    prompts
        .recv()
        .await
        .unwrap()
        .response_tx
        .send(ApprovalDecision::AcceptSession)
        .unwrap();
    assert!(pending.await.unwrap().is_none());
    assert!(!gate.approver().unwrap().should_ask("bash"));
}

/// A file edit that asks is reviewed after the write; its answer gets its
/// own record, with the same call id.
#[cfg(unix)]
#[tokio::test]
async fn the_answer_to_a_file_review_is_recorded() {
    use crate::tool_approval::ApprovalDecision;
    let ws = tempfile::tempdir().unwrap();
    let env: Arc<dyn loop_agent::harness::ExecutionEnv> = Arc::new(
        loop_agent::harness::HostExecutionEnv::new(ws.path().to_path_buf()),
    );
    let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
        &Default::default(),
    ));
    gate.set_workspace(ws.path().to_path_buf());
    let (bridge, mut prompts) = ToolApprovalBridge::new(Arc::clone(&env), None, Default::default());
    bridge.set_open_external_diff(false);
    gate.set_approver(Some(Arc::new(bridge)));
    let capture = Arc::new(Capture::default());
    gate.set_decision_sink(Some(capture.clone()));
    let args = json!({"path": "a.txt", "content": "agent text"});
    let before = context("w1", "write", args.clone());
    assert!(gate.before_hook()(before.clone(), None).await.is_none());
    let tool = loop_agent::harness::tools::create_write_tool(Arc::clone(&env));
    let result = (tool.execute)("w1".into(), args.clone(), None, None)
        .await
        .unwrap();
    let after = loop_agent::AfterToolCallContext {
        assistant_message: before.assistant_message,
        tool_call: before.tool_call,
        args,
        result,
        is_error: false,
        context: Default::default(),
    };
    let reviewing = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { gate.after_hook()(after, None).await })
    };
    prompts
        .recv()
        .await
        .unwrap()
        .response_tx
        .send(ApprovalDecision::Reject { reason: None })
        .unwrap();
    let _ = reviewing.await.unwrap();
    let records = capture.take();
    let got: Vec<_> = records
        .iter()
        .map(|r| (r.call_id.as_deref(), r.outcome, r.answer, r.by))
        .collect();
    assert_eq!(
        got,
        [
            (Some("w1"), DecisionOutcome::Ask, None, DecidedBy::Preset),
            (
                Some("w1"),
                DecisionOutcome::Ask,
                Some(DecisionAnswer::Deny),
                DecidedBy::Approver
            ),
        ]
    );
}

/// What local users see after the unreadable-command rule.
mod unreadable_for_local_users {
    use super::*;
    use crate::permissions::rules::read_only_command;

    /// Forms that use a wrapper or shell syntax, common in real use.
    const FORMS: &[&str] = &[
        "env FOO=1 cargo test",
        "FOO=1 cargo test",
        "sh -c 'cargo test && cargo clippy'",
        "xargs echo < list.txt",
        "time cargo build",
        "echo $(pwd)",
        "cat <<EOF\nhello\nEOF",
        "(cd src && ls)",
        "for f in *.rs; do wc -l $f; done",
    ];

    /// With no bash deny pattern (the default), these run as before: they
    /// are not read-only, so `ask` and `accept-edits` ask as for any
    /// command that is not read-only, `full-access` allows them, and a
    /// typed command runs.
    #[tokio::test]
    async fn with_no_deny_pattern_they_behave_as_before() {
        for command in FORMS {
            assert!(!read_only_command(command), "{command:?}");
            for (mode, want) in [
                (Ask, Decision::Ask),
                (AcceptEdits, Decision::Ask),
                (FullAccess, Decision::Allow),
            ] {
                let (g, _c) = gate(mode, &[], vec![]);
                let got = g.decide("bash", &json!({ "command": command })).await;
                assert_eq!(got, want, "{mode:?} {command:?}");
                assert!(
                    g.check_user_command(command, None, None).await.is_ok(),
                    "{mode:?} typed {command:?}"
                );
            }
        }
    }

    /// With a bash deny pattern, the refusal names the construct and says
    /// how to run the command.
    #[tokio::test]
    async fn with_a_deny_pattern_the_refusal_says_how_to_proceed() {
        let (g, _c) = gate(FullAccess, &[("bash(rm *)", "deny")], vec![]);
        for (command, advice) in [
            (
                "env FOO=1 cargo test",
                "remove `env` and run the inner command directly",
            ),
            ("FOO=1 cargo test", "set the variable in its own part first"),
            (
                "sh -c 'cargo test'",
                "remove `sh` and run the inner command directly",
            ),
            ("echo $(pwd)", "run the inner command on its own first"),
            ("cat <<EOF\nx\nEOF", "put the text in a file"),
            ("(cd src && ls)", "without the parentheses"),
            ("cat {a,b}.txt", "without `{` and `}`"),
        ] {
            let refused = g.check_user_command(command, None, None).await.unwrap_err();
            assert!(refused.0.contains("`bash(rm *)`"), "{refused}");
            assert!(refused.0.contains(advice), "{command:?}: {refused}");
        }
        // The advice works: the plain forms run.
        for command in [
            "cargo test",
            "export FOO=1; cargo test",
            "cd src && ls",
            "cat a.txt b.txt",
        ] {
            assert!(
                g.check_user_command(command, None, None).await.is_ok(),
                "{command}"
            );
        }
    }

    /// Which common read-only forms stay read-only, and which lost it.
    #[test]
    fn common_read_only_forms() {
        for command in [
            "git diff",
            "git show HEAD",
            "git log -p",
            "git log --oneline -5",
            "echo $HOME",
            "echo ${HOME}",
            "ls -la | grep x",
            "grep -E 'a|(b)' src",
            "find . \\( -name a -o -name b \\)",
            "wc -l < file",
        ] {
            assert!(read_only_command(command), "lost: {command:?}");
        }
        // Were read-only before, are not now.
        for command in [
            "cat {a,b}.txt",
            "ls ~/{a,b}",
            "git log HEAD@{1}",
            "git diff @{upstream}",
            "cat <<<x",
            "echo $'a\\tb'",
            "tree -ao out.txt",
            "git diff --ext-diff",
        ] {
            assert!(!read_only_command(command), "still read-only: {command:?}");
        }
        // Were not read-only before either.
        for command in [
            "ls $(pwd)",
            "for f in *; do cat $f; done",
            "while true; do ls; done",
        ] {
            assert!(!read_only_command(command), "{command:?}");
        }
    }
}
