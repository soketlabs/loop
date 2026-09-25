//! AgentHarness basic tests with faux + memory session.

use std::sync::Arc;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    HostExecutionEnv, SandboxMode,
};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::Models;

#[tokio::test]
async fn harness_prompt_persists() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("harness-ok".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("h".into())).await.unwrap();
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

    let msg = harness.prompt("hello").await.unwrap();
    assert_eq!(msg.role(), "assistant");
    harness.wait_for_idle().await;
    assert_eq!(harness.phase(), loop_agent::harness::AgentHarnessPhase::Idle);
}

#[tokio::test]
async fn harness_start_new_session_resets_id() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("first".into()));
    script.push(FauxResponse::Text("second".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("h".into())).await.unwrap();
    let old_id = session.metadata().id.clone();
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

    harness.prompt("hello").await.unwrap();
    harness.wait_for_idle().await;

    let new_id = harness
        .start_new_session(None, Some("fresh".into()))
        .await
        .unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(harness.session_id().await, new_id);

    // Fresh session has no prior context — another prompt still works.
    let msg = harness.prompt("again").await.unwrap();
    assert_eq!(msg.role(), "assistant");
}

#[tokio::test]
async fn harness_resume_restores_session_context() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("persisted-reply".into()));
    script.push(FauxResponse::Text("after-resume".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, Some("resume-me".into())).await.unwrap();
    let session_id = session.metadata().id.clone();

    let harness = AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(&models),
        model: Some(model.clone()),
        session,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });

    harness.prompt("hello").await.unwrap();
    harness.wait_for_idle().await;

    let ctx_before = harness.session_context().await.unwrap();
    assert!(
        ctx_before.messages.len() >= 2,
        "expected user+assistant in session"
    );

    // Simulate `loop --resume <id>`: open the same session in a new harness.
    let resumed = repo.open(&session_id).await.unwrap();
    let harness2 = AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session: resumed,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });

    let ctx = harness2.session_context().await.unwrap();
    assert_eq!(ctx.messages.len(), ctx_before.messages.len());
    assert_eq!(ctx.messages[0].role(), "user");
    assert_eq!(harness2.session_id().await, session_id);

    let msg = harness2.prompt("continue").await.unwrap();
    assert_eq!(msg.role(), "assistant");
    harness2.wait_for_idle().await;

    let ctx_after = harness2.session_context().await.unwrap();
    assert!(ctx_after.messages.len() > ctx.messages.len());
}

#[tokio::test]
async fn harness_fork_through_user_message() {
    use loop_agent::harness::SessionForkSelection;

    let script = FauxScript::new();
    script.push(FauxResponse::Text("a1".into()));
    script.push(FauxResponse::Text("a2".into()));
    script.push(FauxResponse::Text("forked-reply".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("fork-src".into())).await.unwrap();
    let source_id = session.metadata().id.clone();
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

    harness.prompt("one").await.unwrap();
    harness.wait_for_idle().await;
    harness.prompt("two").await.unwrap();
    harness.wait_for_idle().await;

    let points = harness.fork_points().await.unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!(points[0].preview, "one");
    assert_eq!(points[1].preview, "two");

    let forked_id = harness
        .fork_session(
            SessionForkSelection::BeforeEntry,
            Some(&points[1].entry_id),
            Some("forked".into()),
        )
        .await
        .unwrap();
    assert_ne!(forked_id, source_id);
    assert_eq!(harness.session_id().await, forked_id);

    let ctx = harness.session_context().await.unwrap();
    // Before second user message: first turn (user + assistant) remains.
    assert_eq!(ctx.messages.len(), 2);
    assert_eq!(ctx.messages[0].role(), "user");
    assert_eq!(ctx.messages[1].role(), "assistant");
    assert_eq!(points[1].text, "two");

    let msg = harness.prompt("two-edited").await.unwrap();
    assert_eq!(msg.role(), "assistant");
    harness.wait_for_idle().await;

    let ctx2 = harness.session_context().await.unwrap();
    assert!(ctx2.messages.len() >= 4);
}

#[tokio::test]
async fn prompt_without_model_fails_cleanly_until_one_is_selected() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("after-select".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("h".into())).await.unwrap();
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: None,
        session,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });

    assert!(harness.model().await.is_none());
    let err = harness.prompt("hello").await.unwrap_err();
    assert!(matches!(err, loop_agent::harness::AgentHarnessError::NoModelSelected));
    assert!(err.to_string().contains("/model"));
    assert_eq!(harness.phase(), loop_agent::harness::AgentHarnessPhase::Idle);
    assert!(
        harness.session_context().await.unwrap().messages.is_empty(),
        "a rejected prompt must not be recorded"
    );

    harness.set_model(model).await;
    let reply = harness.prompt("hello").await.unwrap();
    assert_eq!(reply.role(), "assistant");
    harness.wait_for_idle().await;

    harness.clear_model().await;
    assert!(harness.model().await.is_none());
}

#[tokio::test]
async fn tool_env_is_available_without_a_model() {
    let models = Arc::new(Models::new());
    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("h".into())).await.unwrap();
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: None,
        session,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });
    // Startup and /sandbox rebuild tools before any model is chosen.
    assert!(harness.tool_env().await.is_ok());
}
