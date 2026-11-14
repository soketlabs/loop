//! The conformance suite against the host driver: plain, and with reads
//! confined as under the `chat` tool lock.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::sandbox::{
    HostWorkspaceDriver, WorkspaceDriver, WorkspaceSource, WorkspaceSpec,
};
use loop_agent::harness::types::ShellExecOptions;
use loop_agent::harness::{SessionExecutionEnv, ToolEnvPolicy};
use loop_workspace_testkit::{
    processes_conform, run_all, set_sentinel_secret, transfers_conform, DriverFixture,
};
use serde_json::json;

/// Idle time of uploads in the fixtures, for the expiry check.
const UPLOAD_IDLE: Duration = Duration::from_millis(400);

fn fixture(confine_reads: bool) -> DriverFixture {
    let layout = common::layout();
    DriverFixture::new(
        Arc::new(HostWorkspaceDriver),
        WorkspaceSpec {
            source: WorkspaceSource::HostFolder(layout.project.clone()),
            options: json!({
                "confineReads": confine_reads,
                "uploadIdleMs": UPLOAD_IDLE.as_millis() as u64,
            }),
            ..Default::default()
        },
    )
    .with_outside(layout.outside.clone())
    .with_upload_idle(UPLOAD_IDLE)
    .with_guard(layout.root)
}

/// No process that a host Workspace started outlives its leader: each
/// session is empty once its leader ended (checked for a few seconds,
/// while kills settle).
async fn assert_no_leaked_processes() {
    let mut leaked = Vec::new();
    for _ in 0..50 {
        leaked = loop_agent::harness::sandbox::leaked_host_processes();
        if leaked.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("processes left behind: {leaked:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_driver_conforms() {
    run_all(|| async { fixture(false) }).await;
    assert_no_leaked_processes().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_driver_with_confined_reads_conforms() {
    run_all(|| async { fixture(true) }).await;
}

/// The process suite alone (for repeated runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_processes_conform() {
    set_sentinel_secret();
    processes_conform(&|| async { fixture(false) }).await;
    assert_no_leaked_processes().await;
}

/// The transfer suite alone (for repeated runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_transfers_conform() {
    transfers_conform(&|| async { fixture(false) }).await;
    transfers_conform(&|| async { fixture(true) }).await;
}

/// The tool env policy layered over a Workspace env removes provider keys
/// and keeps the names in `tools.passEnv`.
#[tokio::test]
async fn session_env_policy_applies_over_the_host_workspace() {
    let f = fixture(false);
    let ws = f.start().await;
    let caller = BTreeMap::from([
        ("OPENAI_API_KEY".to_owned(), "sk-kit".to_owned()),
        ("LOOP_RUNTIME_TOKEN".to_owned(), "tok".to_owned()),
        ("ANTHROPIC_API_KEY".to_owned(), "passed".to_owned()),
        ("KIT_KEEP".to_owned(), "kept".to_owned()),
    ]);
    let policy = ToolEnvPolicy::new(&[], &["ANTHROPIC_API_KEY".to_owned()]);
    let env = SessionExecutionEnv::for_tools(ws.env(), Some(&caller), &policy);
    let out = env
        .exec(
            "printf '%s|%s|%s|%s' \"${OPENAI_API_KEY-none}\" \"${LOOP_RUNTIME_TOKEN-none}\" \"$ANTHROPIC_API_KEY\" \"$KIT_KEEP\"",
            ShellExecOptions {
                timeout_ms: Some(10_000),
                ..Default::default()
            },
        )
        .await
        .expect("exec");
    assert_eq!(out.stdout, "none|none|passed|kept");
    ws.destroy().await.expect("destroy");
}

/// Under the tool lock (`confineReads`) a download obeys the same rule as
/// a file read: the opened file must be a regular file with one link
/// inside the folder. Processes and uploads are refused as unsupported.
#[tokio::test]
async fn confined_downloads_follow_the_tool_lock_rule() {
    use loop_agent::harness::sandbox::{SpawnRequest, UploadRequest, WorkspacePath};
    let locked = fixture(true);
    let outside = locked.outside.clone().expect("outside");
    let ws = locked.start().await;
    let root = ws.root();
    std::fs::write(root.join("own.txt"), "OWN").expect("own");
    std::fs::write(root.join("pair.txt"), "PAIR").expect("pair");
    std::fs::hard_link(root.join("pair.txt"), root.join("hard.txt")).expect("hard link");
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("out.txt")).expect("link");
    let read = |rel: &str| {
        let ws = Arc::clone(&ws);
        let rel = WorkspacePath::parse(rel).expect("path");
        async move { ws.read_chunk(&rel, 0, 64, None).await }
    };
    assert_eq!(read("own.txt").await.expect("own").data, b"OWN");
    for refused in ["hard.txt", "pair.txt", "out.txt"] {
        assert!(read(refused).await.is_err(), "{refused} must be refused");
    }
    let spawn = ws
        .spawn(SpawnRequest {
            command: "true".into(),
            ..Default::default()
        })
        .await;
    assert!(matches!(spawn, Err(ref e) if e.is_unsupported()));
    let upload = ws
        .begin_upload(UploadRequest {
            path: WorkspacePath::parse("new.txt").expect("path"),
            size: 1,
            sha256: None,
            if_match: None,
            mode: None,
        })
        .await;
    assert!(matches!(upload, Err(ref e) if e.is_unsupported()));
    ws.destroy().await.expect("destroy");

    // Without the lock a hard link inside the folder is a plain file.
    let open = fixture(false);
    let ws = open.start().await;
    let root = ws.root();
    std::fs::write(root.join("pair.txt"), "PAIR").expect("pair");
    std::fs::hard_link(root.join("pair.txt"), root.join("hard.txt")).expect("hard link");
    let chunk = ws
        .read_chunk(
            &WorkspacePath::parse("hard.txt").expect("path"),
            0,
            64,
            None,
        )
        .await
        .expect("hard link without the lock");
    assert_eq!(chunk.data, b"PAIR");
    ws.destroy().await.expect("destroy");
}

/// Dropping a host Workspace (as the Runtime does at shutdown) kills its
/// processes, detached ones too, with their trees.
#[tokio::test]
async fn dropping_the_workspace_kills_its_processes() {
    use loop_agent::harness::sandbox::SpawnRequest;
    let f = fixture(false);
    let ws = f.start().await;
    let marker = ws.root().join("after-drop");
    let p = ws
        .spawn(SpawnRequest {
            command: format!("(sleep 2; touch '{}') & sleep 30", marker.display()),
            detach: true,
            ..Default::default()
        })
        .await
        .expect("spawn");
    drop(p);
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(ws);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!marker.exists(), "a process outlived its workspace");
    assert_no_leaked_processes().await;
}

/// Parallel starts never pass the limit: of 200, exactly 64 run, the rest
/// are `Busy`; `destroy` then leaves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parallel_spawns_stop_at_the_limit() {
    use loop_agent::harness::sandbox::{SpawnRequest, WorkspaceError};
    let f = fixture(false);
    let ws = f.start().await;
    let tasks: Vec<_> = (0..200)
        .map(|_| {
            let ws = Arc::clone(&ws);
            tokio::spawn(async move {
                ws.spawn(SpawnRequest {
                    command: "sleep 30".into(),
                    detach: true,
                    ..Default::default()
                })
                .await
            })
        })
        .collect();
    let mut ok = 0;
    let mut busy = 0;
    let mut handles = Vec::new();
    for task in tasks {
        match task.await.expect("task") {
            Ok(p) => {
                ok += 1;
                handles.push(p);
            }
            Err(WorkspaceError::Busy { .. }) => busy += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!((ok, busy), (64, 136));
    ws.destroy().await.expect("destroy");
    drop(handles);
    assert_no_leaked_processes().await;
}

/// A start that races `destroy` either fails or is killed: no process
/// runs after `destroy` returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_spawn_racing_destroy_leaves_nothing() {
    use loop_agent::harness::sandbox::{ExitState, SpawnRequest};
    for _ in 0..20 {
        let f = fixture(false);
        let ws = f.start().await;
        let marker = ws.root().join("raced");
        let spawns: Vec<_> = (0..8)
            .map(|_| {
                let ws = Arc::clone(&ws);
                let marker = marker.clone();
                tokio::spawn(async move {
                    ws.spawn(SpawnRequest {
                        command: format!("sleep 1; touch '{}'", marker.display()),
                        detach: true,
                        ..Default::default()
                    })
                    .await
                })
            })
            .collect();
        tokio::task::yield_now().await;
        ws.destroy().await.expect("destroy");
        for s in spawns {
            if let Ok(p) = s.await.expect("task") {
                let mut exit = None;
                while let Some(e) = tokio::time::timeout(Duration::from_secs(10), p.next_event())
                    .await
                    .expect("ends after destroy")
                {
                    if let loop_agent::harness::sandbox::ProcessEventKind::Exited(x) = e.kind {
                        exit = Some(x);
                    }
                }
                assert_eq!(exit, Some(ExitState::Cancelled));
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "a process ran after destroy");
    }
    assert_no_leaked_processes().await;
}

/// A `spawn` call dropped at any point leaves no command running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_spawn_call_leaves_no_command() {
    use loop_agent::harness::sandbox::SpawnRequest;
    let f = fixture(false);
    let ws = f.start().await;
    for i in 0..40 {
        let marker = ws.root().join(format!("dropped-{i}"));
        let call = ws.spawn(SpawnRequest {
            command: format!("sleep 1; touch '{}'", marker.display()),
            ..Default::default()
        });
        // Drop the call after a few microseconds to a few milliseconds.
        let _ = tokio::time::timeout(Duration::from_micros(50 * i), call).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    for i in 0..40 {
        assert!(
            !ws.root().join(format!("dropped-{i}")).exists(),
            "the command of dropped call {i} ran on"
        );
    }
    ws.destroy().await.expect("destroy");
    assert_no_leaked_processes().await;
}

/// A detached process that nobody watches stops being read when its
/// buffer is full: it waits on its output and does not finish. Attaching
/// resumes it. With `pauseUnwatchedOutput: false` it runs to the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwatched_detached_output_pauses_when_the_buffer_is_full() {
    use loop_agent::harness::sandbox::{ExitState, ProcessEventKind, SpawnRequest, WorkspaceSpec};
    for pause in [true, false] {
        let layout = common::layout();
        let ws = HostWorkspaceDriver
            .create(WorkspaceSpec {
                source: WorkspaceSource::HostFolder(layout.project.clone()),
                options: json!({ "pauseUnwatchedOutput": pause }),
                ..Default::default()
            })
            .await
            .expect("create");
        ws.start().await.expect("start");
        let done = ws.root().join("done");
        let p = ws
            .spawn(SpawnRequest {
                command: format!(
                    "head -c 8000000 /dev/zero | tr '\\000' a; touch '{}'",
                    done.display()
                ),
                detach: true,
                output_buffer_bytes: Some(64 * 1024),
                ..Default::default()
            })
            .await
            .expect("spawn");
        let id = p.id().clone();
        drop(p);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(done.exists(), !pause, "pause={pause}");
        let again = ws.attach_process(&id, 0).await.expect("attach");
        let mut exit = None;
        while let Some(e) = tokio::time::timeout(Duration::from_secs(20), again.next_event())
            .await
            .expect("resumes")
        {
            if let ProcessEventKind::Exited(x) = e.kind {
                exit = Some(x);
            }
        }
        assert_eq!(exit, Some(ExitState::Exited { code: 0 }));
        assert!(done.exists());
        ws.destroy().await.expect("destroy");
        drop(layout);
    }
}
