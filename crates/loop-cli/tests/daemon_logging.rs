#![cfg(unix)]

use loop_cli::debug_log::{init_tracing, TracingMode};

#[test]
fn daemon_logging_writes_to_runtime_log_without_stderr() {
    const CHILD_AGENT: &str = "LOOP_TEST_DAEMON_LOG_AGENT";
    if let Some(agent) = std::env::var_os(CHILD_AGENT) {
        let agent = std::path::PathBuf::from(agent);
        let cwd = agent.join("workspace");
        let path =
            init_tracing(false, &cwd, TracingMode::Daemon { agent_dir: &agent }, None).unwrap();
        assert_eq!(path.unwrap(), agent.join("logs/runtime.log"));
        tracing::warn!("daemon logging test marker");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon_logging_writes_to_runtime_log_without_stderr",
            "--nocapture",
        ])
        .env(CHILD_AGENT, dir.path())
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = std::fs::read_to_string(dir.path().join("logs/runtime.log")).unwrap();
    assert!(log.contains("daemon logging test marker"));
    assert!(!log.contains('\u{1b}'));
    assert!(!dir.path().join("workspace/target/debug/logs").exists());
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &str| {
        std::fs::metadata(dir.path().join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("logs"), 0o700);
    assert_eq!(mode("logs/runtime.log"), 0o600);
}

#[test]
fn daemon_log_refuses_symlink() {
    const CHILD_AGENT: &str = "LOOP_TEST_DAEMON_LOG_SYMLINK";
    if let Some(agent) = std::env::var_os(CHILD_AGENT) {
        let agent = std::path::PathBuf::from(agent);
        let result = init_tracing(
            false,
            &agent,
            TracingMode::Daemon { agent_dir: &agent },
            None,
        );
        assert!(result.is_err());
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("elsewhere");
    std::fs::write(&target, "").unwrap();
    std::fs::create_dir(dir.path().join("logs")).unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("logs/runtime.log")).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "daemon_log_refuses_symlink", "--nocapture"])
        .env(CHILD_AGENT, dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
}
