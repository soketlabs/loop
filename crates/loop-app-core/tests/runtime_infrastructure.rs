#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use loop_app_core::config::paths::{
    run_dir, runtime_lock_path, runtime_log_path, runtime_pid_path, runtime_socket_path,
};

#[test]
fn runtime_paths_are_private_and_keep_lock_identity_in_the_agent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let run = run_dir(&agent).unwrap();
    assert_eq!(run, agent.join("run"));
    assert_eq!(
        std::fs::metadata(&run).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        runtime_socket_path(&agent).unwrap(),
        run.join("runtime.sock")
    );
    assert_eq!(runtime_lock_path(&agent), run.join("runtime.lock"));
    assert_eq!(runtime_pid_path(&agent), run.join("runtime.pid"));
    assert_eq!(runtime_log_path(&agent), agent.join("logs/runtime.log"));
    let long_agent = agent.join("x".repeat(120));
    let fallback = runtime_socket_path(&long_agent).unwrap();
    let name = fallback.file_name().unwrap().to_str().unwrap();
    assert!(name.ends_with(".sock") && name != "runtime.sock", "{name}");
    assert!(fallback
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("/tmp/loop-"));
    assert_eq!(
        std::fs::metadata(fallback.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        runtime_lock_path(&long_agent),
        long_agent.join("run/runtime.lock")
    );
}

#[test]
fn runtime_lock_excludes_a_second_owner_and_cleans_up_its_pid_on_drop() {
    use loop_app_core::runtime_instance::RuntimeInstanceLock;
    let dir = tempfile::tempdir().unwrap();
    let first = RuntimeInstanceLock::acquire(dir.path()).unwrap();
    let pid_path = runtime_pid_path(dir.path());
    assert_eq!(
        std::fs::read_to_string(&pid_path).unwrap().trim(),
        std::process::id().to_string()
    );
    let error = RuntimeInstanceLock::acquire(dir.path()).expect_err("second owner must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains(&format!(
        "a Runtime is already running (pid {})",
        std::process::id()
    )));
    drop(first);
    assert!(!pid_path.exists());
    assert!(
        runtime_lock_path(dir.path()).exists(),
        "never unlink the lock inode"
    );
    let _next = RuntimeInstanceLock::acquire(dir.path()).unwrap();
}

#[test]
fn runtime_directory_repairs_permissions_and_rejects_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    run_dir(dir.path()).unwrap();
    assert_eq!(
        std::fs::metadata(&run).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::remove_dir(&run).unwrap();
    let target = dir.path().join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &run).unwrap();
    assert_eq!(
        run_dir(dir.path()).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn runtime_lock_never_truncates_a_symlink_target() {
    use loop_app_core::runtime_instance::RuntimeInstanceLock;
    let dir = tempfile::tempdir().unwrap();
    run_dir(dir.path()).unwrap();
    let target = dir.path().join("keep");
    std::fs::write(&target, "keep me").unwrap();
    std::os::unix::fs::symlink(&target, runtime_pid_path(dir.path())).unwrap();
    assert!(RuntimeInstanceLock::acquire(dir.path()).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "keep me");
    std::fs::remove_file(runtime_pid_path(dir.path())).unwrap();
    let _lock = RuntimeInstanceLock::acquire(dir.path()).unwrap();
    for path in [runtime_lock_path(dir.path()), runtime_pid_path(dir.path())] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn fallback_socket_is_unique_per_agent_dir_and_stable() {
    let base = tempfile::tempdir().unwrap();
    let a = base.path().join("a").join("x".repeat(120));
    let b = base.path().join("b").join("x".repeat(120));
    let sa = runtime_socket_path(&a).unwrap();
    let sb = runtime_socket_path(&b).unwrap();
    assert_ne!(sa, sb);
    assert_eq!(sa, runtime_socket_path(&a).unwrap());
    assert_eq!(sa.parent(), sb.parent());
}
