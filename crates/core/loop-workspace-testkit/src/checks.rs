//! The conformance checks. Each takes the factory and makes its own
//! Workspaces; each panics with a message that names the driver.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_workspace::{
    ExecutionEnv, ExecutionErrorCode, ExitState, FileErrorCode, HostOs, PathNamespace,
    ProcessEventKind, ProcessId, ShellExecOptions, ShellOutput, SpawnRequest, StreamId, UploadId,
    UploadRequest, Workspace, WorkspaceError, WorkspacePath, WorkspaceStatus,
};
use tokio_util::sync::CancellationToken;

use crate::{DriverFixture, OUTSIDE_SECRET, OUTSIDE_SECRET_TEXT, SECRET_ENV, SECRET_VALUE};

/// Longest wait for one command in the checks.
const COMMAND_LIMIT_MS: u64 = 20_000;

fn path(ws: &Arc<dyn Workspace>, rel: &str) -> PathBuf {
    let rel = WorkspacePath::parse(rel).unwrap_or_else(|e| panic!("bad check path {rel}: {e}"));
    ws.resolve(&rel)
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// A call that the capabilities say is missing must return `Unsupported`.
fn unsupported(id: &str, operation: &str, result: Result<(), WorkspaceError>) {
    match result {
        Err(e) if e.is_unsupported() => {}
        Err(e) => panic!("`{id}`: {operation} without the capability must be Unsupported: {e}"),
        Ok(()) => panic!("`{id}`: {operation} works but the capabilities deny it"),
    }
}

async fn sh(env: &Arc<dyn ExecutionEnv>, command: &str) -> ShellOutput {
    sh_with(env, command, ShellExecOptions::default()).await
}

async fn sh_with(
    env: &Arc<dyn ExecutionEnv>,
    command: &str,
    mut options: ShellExecOptions,
) -> ShellOutput {
    options.timeout_ms.get_or_insert(COMMAND_LIMIT_MS);
    env.exec(command, options)
        .await
        .unwrap_or_else(|e| panic!("exec `{command}`: {e}"))
}

/// The capabilities of the driver and of its Workspaces agree with what the
/// methods do: each missing capability gives `WorkspaceError::Unsupported`,
/// and each present one works.
pub async fn capabilities_match_behaviour<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    assert!(
        f.driver.supports(HostOs::current()),
        "driver `{id}` was given to the kit on a platform it does not support"
    );
    let driver_caps = f.driver.capabilities();
    let ws = f.start().await;
    let caps = ws.capabilities();
    assert_eq!(ws.driver_id(), id, "workspace reports another driver id");
    assert_eq!(
        caps.remote, driver_caps.remote,
        "`{id}`: remote flag differs"
    );
    assert_eq!(
        caps.own_environment,
        ws.env().sandboxed(),
        "`{id}`: own_environment must match Shell::sandboxed"
    );
    assert_eq!(
        caps.outlives_process,
        ws.outlives_process(),
        "`{id}`: outlives_process differs"
    );
    assert_eq!(ws.workspace_status(), WorkspaceStatus::Ready, "`{id}`");
    if caps.path_namespace == PathNamespace::Host {
        assert_eq!(
            ws.root(),
            ws.env().cwd(),
            "`{id}`: a local root is the env cwd"
        );
    }

    assert!(
        !caps.pty || caps.streaming_exec,
        "`{id}`: pty without streaming_exec"
    );
    let file = WorkspacePath::parse("kit-chunk.bin").expect("valid path");
    if caps.streaming_exec {
        let process = match ws
            .spawn(SpawnRequest {
                command: "printf kit-stream".into(),
                inherit_env: true,
                timeout_ms: Some(COMMAND_LIMIT_MS),
                ..Default::default()
            })
            .await
        {
            Ok(p) => p,
            Err(e) => panic!("`{id}` says streaming_exec but spawn failed: {e}"),
        };
        let mut out = Vec::new();
        let mut exit = None;
        let mut last_seq = 0;
        while let Some(event) = process.next_event().await {
            assert!(event.seq > last_seq, "`{id}`: event seq must grow");
            last_seq = event.seq;
            match event.kind {
                ProcessEventKind::Output(bytes) if event.stream == StreamId::Stdout => {
                    out.extend(bytes)
                }
                ProcessEventKind::Exited(e) => exit = Some(e),
                _ => {}
            }
        }
        assert_eq!(out, b"kit-stream", "`{id}`: spawn output");
        assert_eq!(
            exit,
            Some(ExitState::Exited { code: 0 }),
            "`{id}`: spawn exit"
        );
    } else {
        unsupported(
            &id,
            "spawn",
            ws.spawn(SpawnRequest::default()).await.map(|_| ()),
        );
        unsupported(
            &id,
            "attach_process",
            ws.attach_process(&ProcessId("none".into()), 0)
                .await
                .map(|_| ()),
        );
        unsupported(&id, "list_processes", ws.list_processes().await.map(|_| ()));
    }
    if caps.chunked_transfer {
        let upload = ws
            .begin_upload(UploadRequest {
                path: file.clone(),
                size: 6,
                sha256: None,
                if_match: None,
                mode: None,
            })
            .await
            .unwrap_or_else(|e| panic!("`{id}`: begin_upload: {e}"));
        ws.upload_write(&upload, 0, b"abc")
            .await
            .expect("upload_write");
        assert_eq!(
            ws.upload_committed_offset(&upload).await.expect("offset"),
            3
        );
        ws.upload_write(&upload, 3, b"def")
            .await
            .expect("upload_write");
        let tag = ws.upload_commit(&upload).await.expect("upload_commit");
        let chunk = ws
            .read_chunk(&file, 2, 3, Some(&tag))
            .await
            .unwrap_or_else(|e| panic!("`{id}`: read_chunk: {e}"));
        assert_eq!(chunk.data, b"cde", "`{id}`: chunk data");
        assert_eq!(chunk.total_size, 6, "`{id}`: chunk total size");
        assert_eq!(chunk.tag, tag, "`{id}`: chunk tag");
        ws.env()
            .write_file(&ws.resolve(&file), b"changed")
            .await
            .expect("rewrite");
        let changed = ws.read_chunk(&file, 0, 3, Some(&tag)).await;
        assert!(
            matches!(changed, Err(WorkspaceError::Changed { .. })),
            "`{id}`: a changed file must fail the download: {changed:?}"
        );
    } else {
        let upload = UploadId("none".into());
        unsupported(
            &id,
            "read_chunk",
            ws.read_chunk(&file, 0, 16, None).await.map(|_| ()),
        );
        unsupported(
            &id,
            "begin_upload",
            ws.begin_upload(UploadRequest {
                path: file.clone(),
                size: 1,
                sha256: None,
                if_match: None,
                mode: None,
            })
            .await
            .map(|_| ()),
        );
        unsupported(
            &id,
            "upload_write",
            ws.upload_write(&upload, 0, b"x").await.map(|_| ()),
        );
        unsupported(
            &id,
            "upload_committed_offset",
            ws.upload_committed_offset(&upload).await.map(|_| ()),
        );
        unsupported(
            &id,
            "upload_commit",
            ws.upload_commit(&upload).await.map(|_| ()),
        );
        unsupported(&id, "upload_abort", ws.upload_abort(&upload).await);
    }
    if caps.snapshot {
        ws.snapshot()
            .await
            .unwrap_or_else(|e| panic!("`{id}`: snapshot: {e}"));
    } else {
        unsupported(&id, "snapshot", ws.snapshot().await.map(|_| ()));
        unsupported(&id, "hibernate", ws.hibernate().await.map(|_| ()));
        unsupported(&id, "wake", ws.wake(None).await);
    }
    if !caps.attach {
        unsupported(&id, "attach", f.driver.attach(ws.id()).await.map(|_| ()));
    }
    ws.destroy()
        .await
        .unwrap_or_else(|e| panic!("`{id}`: destroy: {e}"));
}

/// Start, stop and destroy can be called twice; destroy works on a
/// Workspace that never started.
pub async fn lifecycle_is_idempotent<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();

    let never_started = f
        .driver
        .create(f.spec.clone())
        .await
        .unwrap_or_else(|e| panic!("`{id}`: create: {e}"));
    assert_ne!(
        never_started.workspace_status(),
        WorkspaceStatus::Ready,
        "`{id}`: ready before start"
    );
    never_started
        .destroy()
        .await
        .unwrap_or_else(|e| panic!("`{id}`: destroy before start: {e}"));
    never_started
        .destroy()
        .await
        .unwrap_or_else(|e| panic!("`{id}`: second destroy: {e}"));

    let ws = f
        .driver
        .create(f.spec.clone())
        .await
        .unwrap_or_else(|e| panic!("`{id}`: create: {e}"));
    for round in 0..2 {
        ws.start()
            .await
            .unwrap_or_else(|e| panic!("`{id}`: start {round}: {e}"));
        assert_eq!(ws.workspace_status(), WorkspaceStatus::Ready, "`{id}`");
    }
    for round in 0..2 {
        ws.stop()
            .await
            .unwrap_or_else(|e| panic!("`{id}`: stop {round}: {e}"));
        assert_eq!(ws.workspace_status(), WorkspaceStatus::Stopped, "`{id}`");
    }
    for round in 0..2 {
        ws.destroy()
            .await
            .unwrap_or_else(|e| panic!("`{id}`: destroy {round}: {e}"));
    }
    assert_ne!(
        ws.workspace_status(),
        WorkspaceStatus::Ready,
        "`{id}`: ready after destroy"
    );
}

/// Text, binary, append, list, info, identity, remove and a 4 MiB file.
pub async fn file_round_trips<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let env = ws.env();

    env.create_dir(&path(&ws, "kit/a/b"))
        .await
        .unwrap_or_else(|e| panic!("`{id}`: create_dir: {e}"));
    let text = path(&ws, "kit/a/b/t.txt");
    env.write_file(&text, "héllo\n".as_bytes())
        .await
        .unwrap_or_else(|e| panic!("`{id}`: write_file: {e}"));
    env.append_file(&text, b"more")
        .await
        .unwrap_or_else(|e| panic!("`{id}`: append_file: {e}"));
    assert_eq!(
        env.read_text_file(&text).await.expect("read text"),
        "héllo\nmore",
        "`{id}`: text round trip"
    );

    let all: Vec<u8> = (0..=255u8).collect();
    let bin = path(&ws, "kit/a/b/all.bin");
    env.write_file(&bin, &all).await.expect("write binary");
    assert_eq!(
        env.read_binary_file(&bin).await.expect("read binary"),
        all,
        "`{id}`: binary round trip"
    );
    let info = env.file_info(&bin).await.expect("file_info");
    assert!(info.is_file && !info.is_dir, "`{id}`: file_info kind");
    assert_eq!(info.size, 256, "`{id}`: file_info size");

    let mut names: Vec<String> = env
        .list_dir(&path(&ws, "kit/a/b"))
        .await
        .expect("list_dir")
        .into_iter()
        .filter_map(|i| i.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    assert_eq!(names, ["all.bin", "t.txt"], "`{id}`: list_dir");

    let first = env.file_identity(&bin).await.expect("identity");
    let second = env.file_identity(&bin).await.expect("identity again");
    match (first, second) {
        (Some(a), Some(b)) => assert!(a.matches(&b), "`{id}`: identity is stable"),
        other => panic!("`{id}`: no identity for a regular file: {other:?}"),
    }
    assert_eq!(
        env.file_identity(&path(&ws, "kit/a"))
            .await
            .expect("dir identity"),
        None,
        "`{id}`: a directory has no file identity"
    );

    let missing = path(&ws, "kit/missing.txt");
    assert!(!env.exists(&missing).await.expect("exists"), "`{id}`");
    let err = env
        .read_text_file(&missing)
        .await
        .expect_err("missing read");
    assert_eq!(
        err.code,
        FileErrorCode::NotFound,
        "`{id}`: missing file code"
    );

    let root = env
        .canonical_path(&ws.root())
        .await
        .expect("canonical root");
    let canon = env.canonical_path(&text).await.expect("canonical file");
    assert!(
        canon.starts_with(&root),
        "`{id}`: canonical path left the root"
    );

    env.remove(&text).await.expect("remove");
    assert!(!env.exists(&text).await.expect("exists"), "`{id}`: removed");

    let big: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let big_path = path(&ws, "kit/big.bin");
    env.write_file(&big_path, &big).await.expect("write 4 MiB");
    assert!(
        env.read_binary_file(&big_path).await.expect("read 4 MiB") == big,
        "`{id}`: 4 MiB round trip"
    );
    ws.destroy().await.expect("destroy");
}

/// A driver that confines reads or writes refuses paths outside the
/// Workspace: plain, through `..` and through a symbolic link. A driver
/// that does not confine them must say so (the outside file is readable).
pub async fn path_escapes_are_refused<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let Some(outside) = f.outside.clone() else {
        return;
    };
    let ws = f.start().await;
    let caps = ws.capabilities();
    let env = ws.env();
    let secret = outside.join(OUTSIDE_SECRET);

    let root = ws.root();
    // The absolute path, and the same file through `..` from the root when
    // the outside folder is a sibling of the root.
    let mut reads = vec![secret.clone()];
    if let (Some(name), true) = (outside.file_name(), outside.parent() == root.parent()) {
        reads.push(root.join("..").join(name).join(OUTSIDE_SECRET));
    }
    for target in &reads {
        let read = env.read_text_file(target).await;
        if caps.confines_reads {
            assert!(
                read.is_err(),
                "`{id}`: read of {} outside the workspace: {read:?}",
                target.display()
            );
        } else {
            assert_eq!(
                read.as_deref().ok(),
                Some(OUTSIDE_SECRET_TEXT),
                "`{id}`: reads are not confined, so {} must be readable",
                target.display()
            );
        }
    }
    if caps.confines_reads && caps.confines_shell {
        let shell = sh(&env, &format!("cat {}", quote(&secret))).await;
        assert!(
            !shell.stdout.contains(OUTSIDE_SECRET_TEXT),
            "`{id}`: a shell command read the outside file"
        );
    }

    let link = path(&ws, "kit-link");
    let linked = sh(&env, &format!("ln -s {} {}", quote(&secret), quote(&link))).await;
    if caps.confines_reads && linked.exit_code == 0 {
        let through = env.read_text_file(&link).await;
        assert!(
            through.is_err(),
            "`{id}`: read through a link to outside: {through:?}"
        );
    }

    if caps.confines_writes {
        let targets = [
            outside.join("kit-written.txt"),
            root.join("..").join("kit-dotdot.txt"),
        ];
        for target in &targets {
            let write = env.write_file(target, b"escape").await;
            assert!(write.is_err(), "`{id}`: write to {}", target.display());
        }
        let write_link = path(&ws, "kit-write-link");
        let new_outside = outside.join("kit-through-link.txt");
        let made = sh(
            &env,
            &format!("ln -s {} {}", quote(&new_outside), quote(&write_link)),
        )
        .await;
        if made.exit_code == 0 {
            let through = env.write_file(&write_link, b"escape").await;
            assert!(through.is_err(), "`{id}`: write through a link to outside");
        }
        let shell_target = outside.join("kit-shell.txt");
        if caps.confines_shell {
            let _ = sh(&env, &format!("echo escape > {}", quote(&shell_target))).await;
        }
        if caps.path_namespace == PathNamespace::Host {
            for target in targets.iter().chain([&new_outside, &shell_target]) {
                assert!(
                    !target.exists(),
                    "`{id}`: a write reached {}",
                    target.display()
                );
            }
            assert_eq!(
                std::fs::read_to_string(&secret).ok().as_deref(),
                Some(OUTSIDE_SECRET_TEXT),
                "`{id}`: the outside file changed"
            );
        }
    }
    ws.destroy().await.expect("destroy");
}

/// Output, exit codes, stderr and the working directory.
pub async fn exec_runs_commands<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let env = ws.env();

    let out = sh(&env, "echo hello; echo oops >&2; exit 7").await;
    assert_eq!(out.stdout, "hello\n", "`{id}`: stdout");
    assert!(
        out.stderr.contains("oops"),
        "`{id}`: stderr: {:?}",
        out.stderr
    );
    assert_eq!(out.exit_code, 7, "`{id}`: exit code");
    assert!(!out.truncated, "`{id}`: small output cut");

    let root = env
        .canonical_path(&ws.root())
        .await
        .expect("canonical root");
    let pwd = sh(&env, "pwd -P").await;
    assert_eq!(
        Path::new(pwd.stdout.trim()),
        root,
        "`{id}`: default cwd is the root"
    );

    let sub = path(&ws, "kit-cwd");
    env.create_dir(&sub).await.expect("create_dir");
    let pwd = sh_with(
        &env,
        "pwd -P",
        ShellExecOptions {
            cwd: Some(sub.clone()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        Path::new(pwd.stdout.trim()),
        root.join("kit-cwd"),
        "`{id}`: cwd option"
    );

    let made = sh(&env, "printf from-shell > kit-shell.txt").await;
    assert_eq!(made.exit_code, 0, "`{id}`: write from shell: {made:?}");
    assert_eq!(
        env.read_text_file(&path(&ws, "kit-shell.txt"))
            .await
            .expect("file tools see shell writes"),
        "from-shell",
        "`{id}`: shell and file tools share files"
    );
    ws.destroy().await.expect("destroy");
}

/// The Runtime environment does not leak into commands: a secret set in
/// the Runtime process is absent for both `inherit_env` values (a driver
/// without its own environment gets the name in `remove_env`, as the
/// Session layer sends it). An override arrives when the driver's
/// allowlist permits its name. The Session layer relies on this to apply
/// the tool env policy (`removed_tool_env_names`, `tools.passEnv`).
pub async fn exec_env_is_explicit<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let env = ws.env();
    let caps = ws.capabilities();
    assert_eq!(
        std::env::var(SECRET_ENV).as_deref(),
        Ok(SECRET_VALUE),
        "run the checks through run_all, which sets the sentinel"
    );

    for inherit_env in [true, false] {
        let remove_env = if caps.own_environment {
            Vec::new()
        } else {
            vec![SECRET_ENV.to_owned()]
        };
        let out = sh_with(
            &env,
            &format!("printf %s \"${{{SECRET_ENV}-absent}}\""),
            ShellExecOptions {
                inherit_env,
                remove_env,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            out.stdout, "absent",
            "`{id}`: the Runtime secret leaked (inherit_env={inherit_env})"
        );
    }

    // A name the driver allows.
    let name = match &caps.env_allowlist {
        None => Some("LOOP_WS_KIT_VAR".to_owned()),
        Some(list) => list
            .iter()
            .find(|n| n.as_str() == "TZ")
            .or_else(|| {
                list.iter()
                    .find(|n| !n.ends_with('*') && !["PATH", "HOME"].contains(&n.as_str()))
            })
            .cloned(),
    };
    if let Some(name) = name {
        for inherit_env in [true, false] {
            let out = sh_with(
                &env,
                &format!("printf %s \"${{{name}-absent}}\""),
                ShellExecOptions {
                    env: Some(HashMap::from([(name.clone(), "kit-value".to_owned())])),
                    inherit_env,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                out.stdout, "kit-value",
                "`{id}`: allowed override {name} (inherit_env={inherit_env})"
            );
        }
    }

    // Without inherit_env, a driver that uses the Runtime environment
    // passes none of it.
    if !caps.own_environment && std::env::var_os("HOME").is_some() {
        let out = sh_with(
            &env,
            "printf %s \"${HOME-unset}\"",
            ShellExecOptions {
                inherit_env: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            out.stdout, "unset",
            "`{id}`: without inherit_env the Runtime environment must not leak"
        );
    }
    ws.destroy().await.expect("destroy");
}

/// A timeout and a cancel stop a long command quickly with the typed codes,
/// the command really stops (a later step of it never runs), and the
/// Workspace still runs commands after.
pub async fn exec_timeout_and_cancel<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let env = ws.env();
    let timeout_marker = path(&ws, "kit-after-timeout");
    let cancel_marker = path(&ws, "kit-after-cancel");

    let started = Instant::now();
    let timed_out = env
        .exec(
            &format!("sleep 2; touch {}", quote(&timeout_marker)),
            ShellExecOptions {
                timeout_ms: Some(300),
                ..Default::default()
            },
        )
        .await;
    match timed_out {
        Err(e) => assert_eq!(e.code, ExecutionErrorCode::TimedOut, "`{id}`: {e}"),
        Ok(out) => panic!("`{id}`: timeout did not stop the command: {out:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "`{id}`: timeout took {:?}",
        started.elapsed()
    );

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let cancelled = env
        .exec(
            &format!("sleep 2; touch {}", quote(&cancel_marker)),
            ShellExecOptions {
                cancel: Some(cancel),
                timeout_ms: Some(COMMAND_LIMIT_MS),
                ..Default::default()
            },
        )
        .await;
    match cancelled {
        Err(e) => assert_eq!(e.code, ExecutionErrorCode::Aborted, "`{id}`: {e}"),
        Ok(out) => panic!("`{id}`: cancel did not stop the command: {out:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "`{id}`: cancel took {:?}",
        started.elapsed()
    );

    // Wait past the sleep: a command that kept running would now write.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for marker in [&timeout_marker, &cancel_marker] {
        assert!(
            !env.exists(marker).await.expect("exists"),
            "`{id}`: the stopped command kept running and wrote {}",
            marker.display()
        );
    }

    let after = sh(&env, "echo still-here").await;
    assert_eq!(after.stdout, "still-here\n", "`{id}`: usable after stop");
    ws.destroy().await.expect("destroy");
}

/// Output above the driver's cap is cut and flagged; output below it is not.
pub async fn exec_output_is_capped<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let env = ws.env();
    let cap = ws.capabilities().limits.max_output_bytes;
    let size = cap.map_or(2 * 1024 * 1024, |c| c + 256 * 1024);
    let out = sh(&env, &format!("head -c {size} /dev/zero | tr '\\000' a")).await;
    assert_eq!(out.exit_code, 0, "`{id}`: {}", out.stderr);
    match cap {
        Some(cap) => {
            assert!(out.truncated, "`{id}`: output above the cap not flagged");
            assert!(
                out.stdout.len() as u64 <= cap,
                "`{id}`: kept {} bytes, cap {cap}",
                out.stdout.len()
            );
        }
        None if !out.truncated => {
            assert_eq!(out.stdout.len() as u64, size, "`{id}`: output lost");
        }
        None => {}
    }
    ws.destroy().await.expect("destroy");
}

/// Eight commands at once each get their own output, and run in parallel.
pub async fn exec_runs_concurrently<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let started = Instant::now();
    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let env = ws.env();
            tokio::spawn(async move { (i, sh(&env, &format!("sleep 1; printf {i}")).await) })
        })
        .collect();
    for task in tasks {
        let (i, out) = task.await.expect("task");
        assert_eq!(out.stdout, i.to_string(), "`{id}`: output of command {i}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "`{id}`: 8 one-second commands took {:?}; they do not run in parallel",
        started.elapsed()
    );
    ws.destroy().await.expect("destroy");
}
