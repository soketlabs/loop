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
        model,
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
        model,
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
        model: model.clone(),
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
        model,
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
