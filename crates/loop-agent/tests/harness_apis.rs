//! AgentHarness extended API tests.

use std::path::PathBuf;
use std::sync::Arc;

use loop_agent::harness::hooks::{HarnessHookEvent, HookOutcome};
use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    AgentHarnessResources, CompactionSettings, HostExecutionEnv, PromptTemplate, SandboxMode,
    Skill,
};
use loop_agent::AgentMessage;
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::Models;

async fn make_harness(script: FauxScript, resources: AgentHarnessResources) -> AgentHarness {
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("apis".into())).await.unwrap();
    let host = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));

    AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources,
    })
}

#[tokio::test]
async fn skill_prompts_with_faux() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("skill-done".into()));

    let resources = AgentHarnessResources {
        skills: vec![Skill {
            name: "demo".into(),
            description: "A demo".into(),
            body: "Do the thing.".into(),
            path: PathBuf::from("/tmp/SKILL.md"),
            disable_model_invocation: false,
        }],
        ..Default::default()
    };

    let harness = make_harness(script, resources).await;
    let msg = harness.skill("demo", Some("extra args")).await.unwrap();
    assert_eq!(msg.role(), "assistant");
}

#[tokio::test]
async fn prompt_from_template_with_resources() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("tpl-done".into()));

    let resources = AgentHarnessResources {
        prompt_templates: vec![PromptTemplate {
            name: "greet".into(),
            body: "Hello $1!".into(),
            path: PathBuf::from("/tmp/greet.md"),
            argument_hint: None,
        }],
        ..Default::default()
    };

    let harness = make_harness(script, resources).await;
    let msg = harness
        .prompt_from_template("greet", "world")
        .await
        .unwrap();
    assert_eq!(msg.role(), "assistant");
}

#[tokio::test]
async fn compact_with_aggressive_settings() {
    let script = FauxScript::new();
    let harness = make_harness(script, Default::default()).await;

    harness
        .set_compaction_settings(CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
            trigger_tokens: None,
            ..Default::default()
        })
        .await;

    for i in 0..30 {
        harness
            .append_message(AgentMessage::user_text(format!(
                "msg {i} {}",
                "x".repeat(800)
            )))
            .await
            .unwrap();
    }

    let result = harness.compact(None).await.unwrap();
    assert!(!result.summary.is_empty());
    assert!(result.tokens_before > 0);
}

#[tokio::test]
async fn navigate_tree_moves_leaf() {
    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, None).await.unwrap();
    let e1 = session
        .append_message(AgentMessage::user_text("one"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("two"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("three"))
        .await
        .unwrap();
    let first_id = e1.id().to_string();

    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(FauxScript::new()));
    let model = models.get_model("faux", "faux-model").unwrap();
    let host = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });

    let ctx_before = harness.create_turn_state().await.unwrap();
    assert_eq!(ctx_before.messages.len(), 3);

    let nav = harness.navigate_tree(&first_id, false).await.unwrap();
    assert!(!nav.cancelled);

    let ctx_after = harness.create_turn_state().await.unwrap();
    assert_eq!(ctx_after.messages.len(), 1);

    let rows = harness.session_tree().await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows[0].current, "the tree marks where the Session is");

    harness.navigate_tree_to(None, false).await.unwrap();
    let ctx_root = harness.create_turn_state().await.unwrap();
    assert!(ctx_root.messages.is_empty(), "back before the first prompt");
}

#[tokio::test]
async fn request_shutdown_clears_and_rejects_prompt() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("nope".into()));
    let harness = make_harness(script, Default::default()).await;

    harness.steer(AgentMessage::user_text("steer"));
    harness.follow_up(AgentMessage::user_text("follow"));
    harness.next_turn(AgentMessage::user_text("next"));

    harness.request_shutdown();

    assert!(harness.is_shutting_down());
    let err = harness.prompt("hello").await.unwrap_err();
    assert!(matches!(
        err,
        loop_agent::harness::AgentHarnessError::ShuttingDown
    ));

    harness.wait_for_shutdown().await;
}

#[tokio::test]
async fn hook_cancels_compact() {
    let script = FauxScript::new();
    let harness = make_harness(script, Default::default()).await;

    harness.on(|event| async move {
        if matches!(event, HarnessHookEvent::SessionBeforeCompact { .. }) {
            HookOutcome {
                cancel: true,
                ..Default::default()
            }
        } else {
            HookOutcome::default()
        }
    });

    harness
        .set_compaction_settings(CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
            trigger_tokens: None,
            ..Default::default()
        })
        .await;

    for i in 0..20 {
        harness
            .append_message(AgentMessage::user_text(format!("m{i} {}", "y".repeat(500))))
            .await
            .unwrap();
    }

    let err = harness.compact(None).await.unwrap_err();
    assert!(matches!(
        err,
        loop_agent::harness::AgentHarnessError::Hook(_)
    ));
}

/// A compaction made between turns of one run is saved in the session, so that rebuilding the
/// session (a resume) gives the compacted context and not the full history.
#[tokio::test]
async fn in_loop_compaction_is_saved_in_the_session() {
    use loop_ai::ToolCall;

    let dir = std::env::temp_dir().join(format!("loop-inloop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 1..=5 {
        std::fs::write(
            dir.join(format!("f{i}.txt")),
            format!("file {i} {}", "z".repeat(4000)),
        )
        .unwrap();
    }

    let read = |i: usize| {
        FauxResponse::ToolCalls(vec![ToolCall {
            id: format!("call-{i}"),
            name: "read".into(),
            arguments: serde_json::json!({ "path": dir.join(format!("f{i}.txt")) }),
            thought_signature: None,
        }])
    };
    let script = FauxScript::new();
    for i in 1..=4 {
        script.push(read(i));
    }
    // Compaction triggers after the fourth read and takes the next response as its summary.
    script.push(FauxResponse::Text(
        "STATE: read f1..f4, nothing else to carry".into(),
    ));
    script.push(read(5));
    script.push(FauxResponse::Text("all done".into()));

    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("inloop".into())).await.unwrap();
    let host: Arc<dyn loop_agent::harness::ExecutionEnv> =
        Arc::new(HostExecutionEnv::new(dir.clone()));
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: Arc::clone(&host),
        tools: vec![loop_agent::harness::tools::create_read_tool(host)],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });
    harness
        .set_compaction_settings(CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 800,
            trigger_tokens: Some(3500),
            ..Default::default()
        })
        .await;

    let last = harness.prompt("read f1..f5, then say done").await.unwrap();
    assert_eq!(last.role(), "assistant");

    let rebuilt = harness.create_turn_state().await.unwrap().messages;
    assert_eq!(
        rebuilt[0].role(),
        "compactionSummary",
        "a resume starts from the saved summary: {:?}",
        rebuilt.iter().map(|m| m.role()).collect::<Vec<_>>()
    );
    assert!(
        format!("{:?}", rebuilt[0]).contains("nothing else to carry"),
        "the saved summary is the one the model wrote"
    );
    // user prompt + 5 x (assistant call + result) + final answer = 12 messages without it.
    assert!(
        rebuilt.len() < 12,
        "older messages are gone: {}",
        rebuilt.len()
    );
    assert_eq!(
        rebuilt.last().unwrap().role(),
        "assistant",
        "the final answer is kept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A summary that cannot be made is reported once and then left alone for a few turns: the run
/// goes on with the full context and without a failing extra request after every turn.
#[tokio::test]
async fn a_failed_in_loop_compaction_is_reported_once_and_backs_off() {
    use loop_ai::ToolCall;

    let dir = std::env::temp_dir().join(format!("loop-inloop-fail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f.txt"), "z".repeat(4000)).unwrap();
    let read = |i: usize| {
        FauxResponse::ToolCalls(vec![ToolCall {
            id: format!("call-{i}"),
            name: "read".into(),
            arguments: serde_json::json!({ "path": dir.join("f.txt") }),
            thought_signature: None,
        }])
    };
    let script = FauxScript::new();
    for i in 1..=4 {
        script.push(read(i));
    }
    // Over the trigger after the fourth read: the summary request fails.
    script.push(FauxResponse::Error("summarizer down".into()));
    // No summary is asked for after the next two turns, so these are the agent's own replies.
    script.push(read(5));
    script.push(read(6));
    script.push(FauxResponse::Text("all done".into()));

    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("inloop-fail".into())).await.unwrap();
    let host: Arc<dyn loop_agent::harness::ExecutionEnv> =
        Arc::new(HostExecutionEnv::new(dir.clone()));
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: Arc::clone(&host),
        tools: vec![loop_agent::harness::tools::create_read_tool(host)],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });
    harness
        .set_compaction_settings(CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 800,
            trigger_tokens: Some(3500),
            ..Default::default()
        })
        .await;
    let notices = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    harness.subscribe({
        let notices = Arc::clone(&notices);
        move |event| {
            if let loop_agent::AgentEvent::Notice { message } = event {
                notices.lock().push(message);
            }
            async {}
        }
    });

    let last = harness
        .prompt("read f.txt six times, then say done")
        .await
        .unwrap();
    assert!(format!("{last:?}").contains("all done"), "{last:?}");
    let notices = notices.lock().clone();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("compaction failed") && notices[0].contains("summarizer down"),
        "{notices:?}"
    );
    // Nothing was dropped: prompt + 6 x (call + result) + final answer.
    assert_eq!(
        harness.create_turn_state().await.unwrap().messages.len(),
        14
    );
    let _ = std::fs::remove_dir_all(&dir);
}
