use loop_app_core::{bootstrap, BootstrapOpts};

#[test]
fn standalone_bootstrap_characterization() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "characterization_child", "--nocapture"])
        .env("LOOP_CHARACTERIZE_BOOTSTRAP", "1")
        .env("LOOP_CODING_AGENT_DIR", dir.path().join("agent"))
        .env("LOOP_CODING_AGENT_SESSION_DIR", dir.path().join("sessions"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn characterization_child() {
    if std::env::var_os("LOOP_CHARACTERIZE_BOOTSTRAP").is_none() {
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let mut opts = BootstrapOpts {
        cwd: cwd.clone(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: Some("Characterization prompt".into()),
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: Some("external".into()),
    };
    let runtime = bootstrap(opts).await.unwrap();
    let names: Vec<_> = runtime
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
    assert!(runtime
        .harness
        .system_prompt()
        .await
        .starts_with("Characterization prompt"));
    assert_eq!(
        runtime.tool_gate.mode(),
        loop_app_core::permissions::PermissionMode::Ask
    );
    assert_eq!(runtime.session_id, runtime.harness.session_id().await);
    assert_eq!(runtime.harness.cwd(), cwd);
    opts = BootstrapOpts {
        cwd,
        provider: None,
        model: None,
        theme: None,
        system_prompt: Some("Characterization prompt".into()),
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: Some(runtime.session_id.clone()),
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: Some("external".into()),
    };
    let resumed = bootstrap(opts).await.unwrap();
    assert!(resumed.resumed);
    assert_eq!(resumed.session_id, runtime.session_id);
}
