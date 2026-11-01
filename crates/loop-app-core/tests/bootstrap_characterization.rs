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
    assert_eq!(runtime.harness.cwd(), cwd.canonicalize().unwrap());
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

#[cfg(unix)]
#[test]
fn standalone_symlink_preserves_lexical_ancestor_context() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let entry = root.join("entry");
    let physical = root.join("physical");
    let project = physical.join("project");
    let agent = root.join("agent");
    for path in [&entry, &project, &agent, &root.join("home")] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(agent.join("AGENTS.md"), "fixture: global instructions").unwrap();
    std::fs::write(project.join("AGENTS.md"), "fixture: project instructions").unwrap();
    std::fs::write(
        entry.join("AGENTS.md"),
        "fixture: lexical ancestor instructions",
    )
    .unwrap();
    std::fs::write(
        physical.join("AGENTS.md"),
        "fixture: physical ancestor instructions",
    )
    .unwrap();
    std::os::unix::fs::symlink(&project, entry.join("project-link")).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "symlink_characterization_child", "--nocapture"])
        .env("LOOP_CHARACTERIZE_LINK_ROOT", &root)
        .env("LOOP_CODING_AGENT_DIR", &agent)
        .env("LOOP_CODING_AGENT_SESSION_DIR", root.join("sessions"))
        .env("HOME", root.join("home"))
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_characterization_child() {
    use loop_app_core::system_prompt::{
        build_system_prompt, default_tool_snippets, load_context_files, BuildSystemPromptOptions,
    };
    let Some(root) = std::env::var_os("LOOP_CHARACTERIZE_LINK_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let cwd = root.join("entry/project-link");
    let agent = root.join("agent");
    // This is the public context loader bootstrap called before tier extraction.
    let expected_context = load_context_files(&cwd, &agent);
    let fixture_context = |files: &[loop_app_core::system_prompt::ContextFile]| {
        files
            .iter()
            .filter(|f| f.content.starts_with("fixture:"))
            .map(|f| (f.path.clone(), f.content.clone()))
            .collect::<Vec<_>>()
    };
    let expected_fixture = vec![
        (
            agent.join("AGENTS.md"),
            "fixture: global instructions".into(),
        ),
        (
            cwd.join("AGENTS.md"),
            "fixture: project instructions".into(),
        ),
        (
            root.join("entry/AGENTS.md"),
            "fixture: lexical ancestor instructions".into(),
        ),
    ];
    assert_eq!(fixture_context(&expected_context), expected_fixture);
    let expected_prompt = build_system_prompt(BuildSystemPromptOptions {
        custom_prompt: Some("Characterization prompt"),
        append_system_prompt: None,
        cwd: &cwd,
        selected_tools: &[],
        tool_snippets: &default_tool_snippets(),
        context_files: &expected_context,
    });
    let runtime = bootstrap(BootstrapOpts {
        cwd: cwd.clone(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: Some("Characterization prompt".into()),
        append_system_prompt: None,
        no_context_files: false,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: Some("external".into()),
    })
    .await
    .unwrap();
    assert_eq!(fixture_context(&runtime.context_files), expected_fixture);
    assert_eq!(runtime.harness.system_prompt().await, expected_prompt);
    assert_eq!(runtime.harness.cwd(), cwd.canonicalize().unwrap());
    assert_eq!(runtime.cwd, cwd.canonicalize().unwrap());
}
