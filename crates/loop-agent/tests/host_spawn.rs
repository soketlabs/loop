//! Process checks of the host Workspace that need their own test binary:
//! they open descriptors without close-on-exec, re-run this binary as a
//! child, or start processes outside the driver.

#![cfg(unix)]

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::sandbox::{
    kill_all_host_processes, kill_tree_without_listing, ExitState, HostWorkspaceDriver,
    ProcessEventKind, SpawnRequest, StreamId, Workspace, WorkspaceDriver, WorkspaceSource,
    WorkspaceSpec,
};

async fn workspace(dir: &std::path::Path) -> Arc<dyn Workspace> {
    let ws = HostWorkspaceDriver
        .create(WorkspaceSpec {
            source: WorkspaceSource::HostFolder(dir.to_path_buf()),
            ..Default::default()
        })
        .await
        .unwrap();
    ws.start().await.unwrap();
    ws
}

async fn output(p: &dyn loop_agent::harness::sandbox::WorkspaceProcess) -> (String, ExitState) {
    let mut out = Vec::new();
    let mut exit = None;
    while let Some(e) = tokio::time::timeout(Duration::from_secs(20), p.next_event())
        .await
        .expect("event in time")
    {
        match e.kind {
            ProcessEventKind::Output(b) if e.stream == StreamId::Stdout => out.extend(b),
            ProcessEventKind::Exited(x) => exit = Some(x),
            _ => {}
        }
    }
    (
        String::from_utf8_lossy(&out).into_owned(),
        exit.expect("exit"),
    )
}

/// A descriptor of the Runtime without close-on-exec does not reach a
/// spawned command.
#[tokio::test]
async fn runtime_descriptors_do_not_leak_into_commands() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path()).await;
    let marker = dir.path().join("marker");
    std::fs::write(&marker, "m").unwrap();
    let path = std::ffi::CString::new(marker.to_str().unwrap()).unwrap();
    // SAFETY: a valid C string; no O_CLOEXEC on purpose.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    assert!(fd > 2);
    let p = ws
        .spawn(SpawnRequest {
            command: format!("if [ -e /dev/fd/{fd} ]; then echo leaked; else echo closed; fi"),
            inherit_env: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let (out, exit) = output(p.as_ref()).await;
    // SAFETY: our own descriptor, closed once.
    unsafe { libc::close(fd) };
    assert_eq!(out.trim(), "closed");
    assert_eq!(exit, ExitState::Exited { code: 0 });
    ws.destroy().await.unwrap();
}

/// With no process listing at all (no `/proc`, `hidepid`), the kill still
/// stops the whole group.
#[test]
fn an_empty_listing_still_kills_the_group() {
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "sleep 30 & sleep 30"]);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let sid = child.id() as i32;
    assert!(kill_tree_without_listing(sid).is_empty());
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    // The background `sleep` of the group is gone too.
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: signal 0 to a group only checks that it exists.
    assert_eq!(unsafe { libc::kill(-sid, 0) }, -1);
}

/// The Runtime-wide kill stops processes of a Workspace that is never
/// dropped or destroyed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_all_host_processes_stops_every_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path()).await;
    let p = ws
        .spawn(SpawnRequest {
            command: "sleep 30".into(),
            detach: true,
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::task::spawn_blocking(|| kill_all_host_processes(Duration::from_secs(5)))
        .await
        .unwrap();
    let (_, exit) = output(p.as_ref()).await;
    assert_eq!(exit, ExitState::Cancelled);
    std::mem::forget(ws);
}

/// Linux: when the Runtime process dies without any shutdown, the kernel
/// kills the leader (`PR_SET_PDEATHSIG` on the waiter thread).
#[cfg(target_os = "linux")]
#[test]
fn a_dead_runtime_takes_its_leaders_along() {
    const CHILD: &str = "LOOP_PDEATH_CHILD";
    if let Some(pid_file) = std::env::var_os(CHILD) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let ws = workspace(dir.path()).await;
            let p = ws
                .spawn(SpawnRequest {
                    command: format!(
                        "echo $$ > '{}'; exec sleep 60",
                        std::path::Path::new(&pid_file).display()
                    ),
                    inherit_env: true,
                    detach: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            let _ = p.next_event().await;
            std::mem::forget(p);
            std::mem::forget(ws);
            std::mem::forget(dir);
        });
        // Die without any shutdown.
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_dead_runtime_takes_its_leaders_along",
            "--nocapture",
        ])
        .env(CHILD, &pid_file)
        .status()
        .unwrap();
    assert!(status.success());
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for _ in 0..100 {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let gone = state.is_empty()
            || state
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
        if gone {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the leader {pid} outlived its Runtime");
}
