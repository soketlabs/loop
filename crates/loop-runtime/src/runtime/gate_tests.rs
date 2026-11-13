//! One gate on every Runtime path: user commands (`session/shell`) pass the
//! Session's Policy gate, and every gate decision is a `policy_decision`
//! record in the Event Log, written after the call's `tool_call` and before
//! the call runs.
use super::*;
use crate::log::{EventLog, LoggedEvent, MemoryEventLog};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall,
};
use loop_app_core::permissions::decision::{
    DecidedBy, DecisionOutcome, DecisionSource, PolicyDecisionRecord,
};
use loop_app_core::permissions::{
    Capability, Layer, LayerSource, PermissionMode, Policy, Setting, State,
};
use loop_protocol::{Caller, Principal, PrincipalKind, Tenant};

const LIMIT: Duration = Duration::from_secs(60);

/// A Runtime whose model calls `bash` once with `command`, then answers.
async fn runtime(
    dir: &std::path::Path,
    mode: &str,
    command: &'static str,
) -> (Arc<Runtime>, Arc<MemoryEventLog>) {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(move |context: &Context| {
        let asked = matches!(context.messages.last(), Some(Message::User(_)));
        let response = if asked {
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "gate-call".into(),
                name: "bash".into(),
                arguments: json!({ "command": command }),
                thought_signature: None,
            }])
        } else {
            FauxResponse::Text("finished".into())
        };
        Box::pin(std::future::ready(response))
    })));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let opts = BootstrapOpts {
        cwd: dir.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some(mode.into()),
        agent: None,
    };
    let log = Arc::new(MemoryEventLog::new());
    let rt = {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(globals, opts, log.clone(), store, RuntimeLimits::default())
    }
    .await
    .unwrap()
    .with_close_grace(Duration::from_millis(500))
    .into_shared();
    (rt, log)
}

/// Every decision record in `log`, with its committed record.
async fn decisions(log: &MemoryEventLog) -> Vec<(LoggedEvent, PolicyDecisionRecord)> {
    log.read_runtime_after(0)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|record| match &record.event.kind {
            EventKind::PolicyDecision(value) => {
                let decision = serde_json::from_value(value.clone()).unwrap();
                Some((record, decision))
            }
            _ => None,
        })
        .collect()
}

fn edge(id: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

async fn shell(rt: &Runtime, session: &SessionRef, command: &str) -> BackendResult<ShellResult> {
    LocalShell::run(rt, session, command, None).await
}

/// `session/shell` passes the Session's gate as a `bash` call would, the
/// decision is recorded for the caller that sent it, and an allowed command
/// runs unchanged for the local user.
#[tokio::test]
async fn a_user_command_passes_the_session_s_gate_and_is_recorded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("made");
        let (rt, log) = runtime(dir.path(), "ask", "true").await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let gate = rt.route(&session).unwrap().opened.tool_gate.clone();
        let host = Caller::host(Principal::local_uid(501));

        // Control: in `ask` mode `bash` would ask; the host user typed it,
        // so it runs, unchanged.
        let out = host
            .clone()
            .scope(shell(&rt, &session, "printf 'a b'"))
            .await
            .unwrap();
        assert_eq!((out.stdout.as_str(), out.exit_code), ("a b", 0));
        let (_, record) = decisions(&log).await.pop().unwrap();
        assert_eq!(record.source, DecisionSource::UserShell);
        assert_eq!(
            (record.outcome, record.by),
            (DecisionOutcome::Allow, DecidedBy::User)
        );
        assert_eq!(record.principal, *host.principal());
        assert_eq!(record.session.as_deref(), Some(session.key.as_str()));
        assert_eq!(record.subject.programs, ["printf"]);

        // An in-process call acts as the Runtime.
        shell(&rt, &session, "true").await.unwrap();
        let (_, record) = decisions(&log).await.pop().unwrap();
        assert_eq!(record.principal, Principal::runtime());

        // A user from outside the host cannot approve its own command.
        let touch = format!("touch {}", marker.display());
        let refused = edge("alice")
            .scope(shell(&rt, &session, &touch))
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("outside this host"),
            "{refused}"
        );
        assert!(!marker.exists());
        let (_, record) = decisions(&log).await.pop().unwrap();
        assert_eq!(record.principal, *edge("alice").principal());
        assert_eq!(record.tenant.as_str(), "acme");

        // Read-only mode: only read-only commands.
        gate.set_mode(PermissionMode::ReadOnly);
        assert!(shell(&rt, &session, &touch).await.is_err());
        assert!(!marker.exists());
        shell(&rt, &session, "ls").await.unwrap();

        // A deny rule refuses it in every mode, full access included.
        gate.set_mode(PermissionMode::FullAccess);
        gate.set_permissions(loop_app_core::tool_approval::permissions_from_settings(
            &[("bash(touch*)".to_owned(), "deny".to_owned())].into(),
        ));
        let refused = shell(&rt, &session, &touch).await.unwrap_err();
        assert!(refused.to_string().contains("denied"), "{refused}");
        assert!(!marker.exists());
        let (_, record) = decisions(&log).await.pop().unwrap();
        assert_eq!(
            (record.by, record.rule.as_deref()),
            (DecidedBy::Rule, Some("bash(touch*)"))
        );

        // A Lock that turns the shell off.
        gate.set_permissions(Default::default());
        let (policy, _) = Policy::new(vec![Layer::new(LayerSource::Managed)
            .set(Capability::Shell, Setting::always(State::Off))
            .lock(Capability::Shell)]);
        gate.set_policy(policy);
        assert!(shell(&rt, &session, &touch).await.is_err());
        assert!(!marker.exists());
        gate.set_policy(Policy::default());
        shell(&rt, &session, &touch).await.unwrap();
        assert!(marker.exists(), "control: full access runs it");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A tool call's decision is recorded once, for the caller that sent the
/// prompt, after its `tool_call` update and before the tool's result.
#[tokio::test]
async fn a_tool_call_s_decision_commits_after_its_tool_call_and_before_it_runs() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path(), "full-access", "printf ran > made").await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let host = Caller::host(Principal::local_uid(777));
        let turn = host
            .scope(rt.prompt(&session, vec![ContentBlock::text("go")]))
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("made")).unwrap(),
            "ran"
        );
        let all = log.read_runtime_after(0).await.unwrap();
        let at = |find: &dyn Fn(&LoggedEvent) -> bool| {
            all.iter()
                .find(|record| find(record))
                .map(|record| record.global_seq)
                .expect("record present")
        };
        let call = at(&|r| {
            matches!(&r.event.kind, EventKind::Update(e)
                if matches!(&e.update, SessionUpdate::ToolCall(c) if c.id.0 == "gate-call"))
        });
        let ended = at(&|r| {
            matches!(&r.event.kind, EventKind::Update(e)
                if matches!(&e.update, SessionUpdate::ToolCallUpdate(u)
                    if u.id.0 == "gate-call"
                        && matches!(u.status, Some(loop_protocol::ToolCallStatus::Completed))))
        });
        let found = decisions(&log).await;
        let mine: Vec<_> = found
            .iter()
            .filter(|(_, d)| d.call_id.as_deref() == Some("gate-call"))
            .collect();
        assert_eq!(mine.len(), 1, "one record per call");
        let (committed, record) = mine[0];
        assert!(
            call < committed.global_seq && committed.global_seq < ended,
            "tool_call {call} < decision {} < result {ended}",
            committed.global_seq
        );
        assert!(
            committed.session_seq.is_none(),
            "log-only: no Session sequence"
        );
        assert_eq!(record.source, DecisionSource::Native);
        assert_eq!(
            (record.outcome, record.by),
            (DecisionOutcome::Allow, DecidedBy::Preset)
        );
        assert_eq!(record.principal, Principal::local_uid(777));
        assert_eq!(record.mode.as_deref(), Some("full-access"));
        assert!(record.turn.is_some());
        assert_eq!(
            committed.event.turn.as_ref().map(|t| t.0.clone()),
            record.turn
        );
        // Replay never sees it.
        let replay = log.read_after(&session.key, 0).await.unwrap();
        assert!(replay
            .iter()
            .all(|r| !matches!(r.event.kind, EventKind::PolicyDecision(_))));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// The `workspace/*` reads an edge user may make return no secrets: MCP
/// server `env` values and headers, provider URLs with credentials, web
/// search proxy credentials and provider keys in the environment stay out
/// of every answer.
#[tokio::test]
async fn workspace_reads_return_no_secrets() {
    use loop_protocol::admin::*;
    use loop_wire::RuntimeAdmin;
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(agent.join("prompts")).unwrap();
        std::fs::write(agent.join("prompts/hello.md"), "Say hello to $1\n").unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({
                "mcpServers": {
                    "local": {"command": "/usr/bin/false", "args": ["--token", "SECRET-ARG-1"],
                              "env": {"API_TOKEN": "SECRET-ENV-2"}},
                    "remote": {"url": "https://user:SECRET-URL-3@mcp.invalid/x",
                               "headers": {"Authorization": "Bearer SECRET-HEADER-4"}}
                },
                "providers": [{"id": "corp", "name": "Corp",
                               "baseUrl": "https://user:SECRET-PROVIDER-5@llm.invalid/v1"}],
                "webSearch": {"enabled": true, "provider": "tavily",
                              "apiKeyEnv": ["SECRET_KEY_NAME_6"],
                              "baseUrl": "https://user:SECRET-SEARCH-7@search.invalid"},
                "diffEditor": "/usr/bin/true"
            })
            .to_string(),
        )
        .unwrap();
        let (rt, _) = runtime(dir.path(), "ask", "true").await;
        let id = tree::workspace_id(&dir.path().canonicalize().unwrap());
        let admin: &dyn RuntimeAdmin = rt.as_ref();
        let caller = edge("alice");
        let answers = caller
            .scope(async {
                vec![
                    serde_json::to_value(
                        admin
                            .settings(SettingsParams {
                                workspace_id: id.clone(),
                            })
                            .await
                            .unwrap(),
                    )
                    .unwrap(),
                    serde_json::to_value(
                        admin
                            .mcp(McpParams {
                                workspace_id: id.clone(),
                                action: AdminAction::List,
                                name: None,
                                expected_revision: None,
                            })
                            .await
                            .unwrap(),
                    )
                    .unwrap(),
                    serde_json::to_value(
                        admin
                            .web_search(WebSearchParams {
                                workspace_id: id.clone(),
                                enabled: None,
                                provider: None,
                                expected_revision: None,
                            })
                            .await
                            .unwrap(),
                    )
                    .unwrap(),
                    serde_json::to_value(
                        admin
                            .skills(SkillsParams {
                                workspace_id: id.clone(),
                                action: AdminAction::List,
                                name: None,
                                expected_revision: None,
                            })
                            .await
                            .unwrap(),
                    )
                    .unwrap(),
                    serde_json::to_value(
                        admin
                            .templates(TemplatesParams {
                                workspace_id: id.clone(),
                                name: None,
                                args: vec![],
                            })
                            .await
                            .unwrap(),
                    )
                    .unwrap(),
                ]
            })
            .await;
        let text = serde_json::to_string(&answers).unwrap();
        for secret in [
            "SECRET-ARG-1",
            "SECRET-ENV-2",
            "SECRET-URL-3",
            "SECRET-HEADER-4",
            "SECRET-PROVIDER-5",
            "SECRET_KEY_NAME_6",
            "SECRET-SEARCH-7",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        // Control: the reads answered.
        assert!(
            text.contains("\"local\"") && text.contains("\"remote\""),
            "{text}"
        );
        assert!(text.contains("hello"), "{text}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
