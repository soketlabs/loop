//! The conformance checks. Each takes the factory and makes its own
//! Workspaces; each panics with a message that names the driver.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_workspace::{
    ExecutionEnv, ExecutionErrorCode, FileErrorCode, HostOs, ProcessEvent, ShellExecOptions,
    ShellOutput, SpawnRequest, Workspace, WorkspacePath, WorkspaceStatus,
};
use tokio_util::sync::CancellationToken;

use crate::{DriverFixture, OUTSIDE_SECRET, OUTSIDE_SECRET_TEXT};

/// Longest wait for one command in the checks.
const COMMAND_LIMIT_MS: u64 = 20_000;

fn path(ws: &Arc<dyn Workspace>, rel: &str) -> PathBuf {
    let rel = WorkspacePath::parse(rel).unwrap_or_else(|e| panic!("bad check path {rel}: {e}"));
    ws.resolve(&rel)
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
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
    if !caps.remote {
        assert_eq!(
            ws.root(),
            ws.env().cwd(),
            "`{id}`: a local root is the env cwd"
        );
    }

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
        while let Some(event) = process.next_event().await {
            match event {
                ProcessEvent::Stdout(bytes) => out.extend(bytes),
                ProcessEvent::Stderr(_) => {}
                ProcessEvent::Exited(e) => exit = Some(e),
            }
        }
        assert_eq!(out, b"kit-stream", "`{id}`: spawn output");
        assert_eq!(exit.and_then(|e| e.code), Some(0), "`{id}`: spawn exit");
    } else if !caps.pty {
        match ws.spawn(SpawnRequest::default()).await {
            Err(e) => assert!(e.is_unsupported(), "`{id}`: spawn: {e}"),
            Ok(_) => panic!("`{id}`: spawn works but the capabilities deny it"),
        }
    }
    if caps.chunked_transfer {
        ws.write_chunk(&file, 0, b"abc", false)
            .await
            .unwrap_or_else(|e| panic!("`{id}`: write_chunk: {e}"));
        ws.write_chunk(&file, 3, b"def", true)
            .await
            .unwrap_or_else(|e| panic!("`{id}`: write_chunk last: {e}"));
        let chunk = ws
            .read_chunk(&file, 2, 3)
            .await
            .unwrap_or_else(|e| panic!("`{id}`: read_chunk: {e}"));
        assert_eq!(chunk.data, b"cde", "`{id}`: chunk data");
        assert_eq!(chunk.total_size, 6, "`{id}`: chunk total size");
        assert!(!chunk.eof, "`{id}`: chunk eof");
    } else {
        let read = ws.read_chunk(&file, 0, 16).await;
        assert!(
            read.as_ref().is_err_and(|e| e.is_unsupported()),
            "`{id}`: read_chunk without the capability: {read:?}"
        );
        let write = ws.write_chunk(&file, 0, b"x", true).await;
        assert!(
            write.as_ref().is_err_and(|e| e.is_unsupported()),
            "`{id}`: write_chunk without the capability: {write:?}"
        );
    }
    if caps.snapshot {
        ws.snapshot()
            .await
            .unwrap_or_else(|e| panic!("`{id}`: snapshot: {e}"));
    } else {
        for (name, result) in [
            ("snapshot", ws.snapshot().await.map(|_| ())),
            ("hibernate", ws.hibernate().await.map(|_| ())),
            ("wake", ws.wake().await),
        ] {
            assert!(
                result.as_ref().is_err_and(|e| e.is_unsupported()),
                "`{id}`: {name} without the capability: {result:?}"
            );
        }
    }
    if !caps.attach {
        match f.driver.attach(ws.id()).await {
            Err(e) => assert!(e.is_unsupported(), "`{id}`: attach: {e}"),
            Ok(_) => panic!("`{id}`: attach works but the capabilities deny it"),
        }
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

    let read = env.read_text_file(&secret).await;
    if caps.confines_reads {
        assert!(
            read.is_err(),
            "`{id}`: read outside the workspace: {read:?}"
        );
    } else {
        assert_eq!(
            read.as_deref().ok(),
            Some(OUTSIDE_SECRET_TEXT),
            "`{id}`: reads are not confined, so the outside file must be readable"
        );
    }

    let root = ws.root();
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
        let _ = sh(&env, &format!("echo escape > {}", quote(&shell_target))).await;
        if !caps.remote {
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

/// Variables in `ShellExecOptions::env` reach the command (a driver with
/// its own environment may drop names outside its allowlist), and without
/// `inherit_env` a driver that uses the Runtime environment passes none of
/// it. The Session layer relies on this to apply the tool env policy
/// (`removed_tool_env_names`, `tools.passEnv`).
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
    let vars = HashMap::from([("LOOP_WS_KIT_VAR".to_owned(), "kit-value".to_owned())]);

    for inherit_env in [true, false] {
        let out = sh_with(
            &env,
            "printf %s \"$LOOP_WS_KIT_VAR\"",
            ShellExecOptions {
                env: Some(vars.clone()),
                inherit_env,
                ..Default::default()
            },
        )
        .await;
        if caps.own_environment {
            // A driver with its own environment may keep only an allowlist of
            // names (Seatbelt does). It must never put another value there.
            assert!(
                out.stdout == "kit-value" || out.stdout.is_empty(),
                "`{id}`: env override (inherit_env={inherit_env}): {:?}",
                out.stdout
            );
        } else {
            assert_eq!(
                out.stdout, "kit-value",
                "`{id}`: env override (inherit_env={inherit_env})"
            );
        }
    }

    // A variable of the Runtime process that every Unix process has.
    if !caps.own_environment && std::env::var_os("HOME").is_some() {
        let out = sh_with(
            &env,
            "printf %s \"${HOME-unset}\"",
            ShellExecOptions {
                env: Some(vars),
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
/// and the Workspace still runs commands after.
pub async fn exec_timeout_and_cancel<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let env = ws.env();

    let started = Instant::now();
    let timed_out = env
        .exec(
            "sleep 30",
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
            "sleep 30",
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
