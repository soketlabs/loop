use std::sync::Arc;

use loop_app_core::runtime::{BootstrapOpts, GlobalResources, SessionFactory};
use loop_app_core::{config::paths::settings_path, Settings};

fn opts(cwd: &std::path::Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: Some("Characterization prompt".into()),
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: None,
    }
}

fn factory(agent_dir: &std::path::Path) -> SessionFactory {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    SessionFactory::new(Arc::new(
        GlobalResources::with_models(agent_dir.into(), models).unwrap(),
    ))
}

#[tokio::test]
async fn factory_preserves_tools_prompt_permission_and_session() {
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(&dir.path().join("agent"));
    let opts = opts(dir.path());
    let workspace = factory.workspace(&opts).await.unwrap();
    let opened = factory.open(workspace.clone(), &opts).await.unwrap();
    let names: Vec<_> = opened
        .harness
        .get_tools()
        .await
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(
        names,
        [
            "read",
            "grep",
            "glob",
            "ls",
            "write",
            "edit",
            "bash",
            "todo",
            "exit_plan"
        ]
    );
    assert!(opened
        .harness
        .system_prompt()
        .await
        .starts_with("Characterization prompt"));
    assert_eq!(
        opened.tool_gate.mode(),
        loop_app_core::permissions::PermissionMode::Ask
    );
    assert_eq!(opened.session_id, opened.harness.session_id().await);
    assert_eq!(opened.harness.cwd(), workspace.cwd);
    assert_eq!(opened.harness.model().await.unwrap().provider, "faux");
    let mut resume = opts;
    resume.session_id = Some(opened.session_id.clone());
    let resumed = factory.open(workspace, &resume).await.unwrap();
    assert_eq!(resumed.session_id, opened.session_id);
}

#[tokio::test]
async fn sessions_share_workspace_but_isolate_gates_and_grants() {
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(&dir.path().join("agent"));
    let opts = opts(dir.path());
    let a = factory.workspace(&opts).await.unwrap();
    let b = factory.workspace(&opts).await.unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let one = factory.open(a, &opts).await.unwrap();
    let two = factory.open(b, &opts).await.unwrap();
    assert_ne!(one.session_id, two.session_id);
    assert!(!Arc::ptr_eq(&one.tool_gate, &two.tool_gate));
    one.approval_bridge.grant_session("bash");
    assert!(!one.approval_bridge.should_ask("bash"));
    assert!(two.approval_bridge.should_ask("bash"));
    one.tool_gate
        .set_mode(loop_app_core::permissions::PermissionMode::ReadOnly);
    assert_eq!(
        two.tool_gate.mode(),
        loop_app_core::permissions::PermissionMode::Ask
    );
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let other = factory.workspace(&self::opts(&other)).await.unwrap();
    assert!(!Arc::ptr_eq(&one.workspace, &other));
    assert_ne!(one.workspace.cwd, other.cwd);
}

#[tokio::test]
async fn never_trusted_workspace_uses_global_settings_and_notice() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir(&agent).unwrap();
    let global = Settings {
        default_project_trust: "always".into(),
        theme: "global-theme".into(),
        ..Default::default()
    };
    global.save_file(&settings_path(&agent)).unwrap();
    let project = dir.path().join(".loop");
    std::fs::create_dir(&project).unwrap();
    Settings {
        theme: "project-theme".into(),
        ..Default::default()
    }
    .save_file(&settings_path(&project))
    .unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Untrusted context").unwrap();
    std::fs::write(project.join("SYSTEM.md"), "Untrusted prompt").unwrap();
    std::fs::create_dir(project.join("hooks")).unwrap();
    std::fs::write(
        project.join("hooks/block.json"),
        r#"{"on":"settled","action":{"block":true}}"#,
    )
    .unwrap();
    std::fs::write(agent.join("SYSTEM.md"), "Global prompt").unwrap();
    let factory = factory(&agent);
    let mut opts = opts(dir.path());
    opts.no_context_files = false;
    opts.system_prompt = None;
    let workspace = factory.workspace(&opts).await.unwrap();
    assert!(!workspace.project_trusted);
    assert_eq!(workspace.settings.theme, "global-theme");
    assert_eq!(
        workspace.custom_system_prompt.as_deref(),
        Some("Global prompt")
    );
    assert!(!workspace
        .context_files
        .iter()
        .any(|f| f.content.contains("Untrusted context")));
    assert!(workspace.resources.hook_paths.is_empty());
    assert!(workspace
        .startup_notices
        .iter()
        .any(|n| n.contains("trust")));
}

#[tokio::test]
async fn injected_models_run_a_native_turn_with_the_selected_tool_prompt() {
    use loop_protocol::{AgentBackend, ContentBlock, TurnOutcome};
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(&dir.path().join("agent"));
    let mut opts = opts(dir.path());
    opts.system_prompt = None;
    let workspace = factory.workspace(&opts).await.unwrap();
    let opened = factory.open(workspace, &opts).await.unwrap();
    let prompt = opened.harness.system_prompt().await;
    assert!(prompt.contains("- bash: Run shell commands"));
    assert!(!prompt.contains("- web_search:"));
    let session = opened.native.current_session().await;
    let turn = opened
        .native
        .prompt(&session, vec![ContentBlock::text("hello")])
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), turn.outcome())
            .await
            .unwrap(),
        TurnOutcome::Completed(_)
    ));
    assert!(!opened.native.transcript(&session).await.unwrap().is_empty());
}

#[tokio::test]
async fn normalized_workspace_cache_and_hook_registries_are_session_local() {
    use loop_agent::harness::{HarnessHookEvent, HookOutcome};
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(&dir.path().join("agent"));
    let opts = opts(dir.path());
    let mut alias = self::opts(&dir.path().join("."));
    alias.permission_mode = Some("read-only".into());
    let (a, b) = tokio::join!(factory.workspace(&opts), factory.workspace(&alias));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(Arc::ptr_eq(&a, &b));
    let one = factory.open(a, &opts).await.unwrap();
    let two = factory.open(b, &alias).await.unwrap();
    assert_eq!(
        two.tool_gate.mode(),
        loop_app_core::permissions::PermissionMode::ReadOnly
    );
    one.harness.hooks().on(|_| async {
        HookOutcome {
            summary: Some("one only".into()),
            ..Default::default()
        }
    });
    assert_eq!(
        one.harness
            .hooks()
            .emit(HarnessHookEvent::Settled)
            .await
            .summary
            .as_deref(),
        Some("one only")
    );
    assert!(two
        .harness
        .hooks()
        .emit(HarnessHookEvent::Settled)
        .await
        .summary
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_hook_commands_run_in_each_owning_workspace() {
    use loop_agent::harness::HarnessHookEvent;
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(agent.join("hooks")).unwrap();
    std::fs::write(agent.join("hooks/cwd.json"), r#"{"on":"settled","action":{"command":["sh","-c","printf '%s' \"$LOOP_PROJECT_DIR\" > hook-cwd.txt"]}}"#).unwrap();
    let factory = factory(&agent);
    let mut paths = Vec::new();
    for name in ["one", "two"] {
        let cwd = dir.path().join(name);
        std::fs::create_dir(&cwd).unwrap();
        let opts = opts(&cwd);
        let workspace = factory.workspace(&opts).await.unwrap();
        let opened = factory.open(workspace, &opts).await.unwrap();
        opened.harness.hooks().emit(HarnessHookEvent::Settled).await;
        assert_eq!(
            std::fs::read_to_string(cwd.join("hook-cwd.txt")).unwrap(),
            opened.workspace.cwd.to_string_lossy()
        );
        paths.push(opened.workspace.cwd.clone());
    }
    assert_ne!(paths[0], paths[1]);
}

#[tokio::test]
async fn recorded_trust_enables_project_input_without_mutating_global_settings() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let project = dir.path().join(".loop");
    std::fs::create_dir_all(project.join("hooks")).unwrap();
    std::fs::write(project.join("SYSTEM.md"), "Trusted project prompt").unwrap();
    std::fs::write(
        project.join("hooks/block.json"),
        r#"{"on":"settled","action":{"block":true}}"#,
    )
    .unwrap();
    Settings {
        theme: "project-theme".into(),
        ..Default::default()
    }
    .save_file(&settings_path(&project))
    .unwrap();
    let factory = factory(&agent);
    let mut trust =
        loop_app_core::config::trust::TrustStore::load(agent.join("trust.json")).unwrap();
    trust.set(dir.path(), true).unwrap();
    let opts = opts(dir.path());
    let workspace = factory.workspace(&opts).await.unwrap();
    assert!(workspace.project_trusted);
    assert_eq!(workspace.settings.theme, "project-theme");
    let opened = factory.open(workspace, &opts).await.unwrap();
    assert!(opened
        .harness
        .system_prompt()
        .await
        .starts_with("Trusted project prompt"));
    assert!(
        opened
            .harness
            .hooks()
            .emit(loop_agent::harness::HarnessHookEvent::Settled)
            .await
            .cancel
    );
}

#[tokio::test]
async fn gate_uses_the_session_workspace_for_relative_paths() {
    use loop_app_core::permissions::{Decision, PermissionMode};
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(&dir.path().join("agent"));
    let opts = opts(dir.path());
    let workspace = factory.workspace(&opts).await.unwrap();
    let opened = factory.open(workspace, &opts).await.unwrap();
    opened.tool_gate.set_mode(PermissionMode::AcceptEdits);
    assert_eq!(
        opened
            .tool_gate
            .decide("write", &serde_json::json!({"path": "inside.txt"}))
            .await,
        Decision::Allow
    );
    assert_eq!(
        opened
            .tool_gate
            .decide("write", &serde_json::json!({"path": "../outside.txt"}))
            .await,
        Decision::Ask
    );
}
