//! Checks of streaming processes (`Workspace::spawn`) and PTYs. Each check
//! returns at once when the Workspace does not report `streaming_exec`
//! (`capabilities_match_behaviour` checks the `Unsupported` answers then).

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_workspace::{
    ExitState, PathNamespace, ProcessEvent, ProcessEventKind, ProcessSignal, PtySize, SpawnRequest,
    StreamId, Workspace, WorkspaceError, WorkspacePath, WorkspaceProcess,
};
use tokio_util::sync::CancellationToken;

use crate::{DriverFixture, SECRET_ENV, SECRET_VALUE};

/// Longest wait for one process in the checks.
const LIMIT: Duration = Duration::from_secs(30);

/// Everything one process sent until it ended.
#[derive(Debug, Default)]
pub struct Collected {
    /// Bytes per stream, joined in order.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
    /// PTY output.
    pub pty: Vec<u8>,
    /// Every `seq`, in the order received.
    pub seqs: Vec<u64>,
    /// Bytes reported by `Dropped` events.
    pub dropped: u64,
    /// The exit.
    pub exit: Option<ExitState>,
}

impl Collected {
    fn add(&mut self, event: ProcessEvent) {
        self.seqs.push(event.seq);
        match event.kind {
            ProcessEventKind::Output(bytes) => match event.stream {
                StreamId::Stdout => self.stdout.extend(bytes),
                StreamId::Stderr => self.stderr.extend(bytes),
                StreamId::Pty => self.pty.extend(bytes),
                StreamId::Control => panic!("output on the control stream"),
            },
            ProcessEventKind::Dropped { bytes } => self.dropped += bytes,
            ProcessEventKind::Exited(exit) => self.exit = Some(exit),
        }
    }

    /// Standard output as text.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Read every event of `process` until it ends (at most [`LIMIT`]).
pub async fn collect(id: &str, process: &dyn WorkspaceProcess) -> Collected {
    let mut got = Collected::default();
    let read = async {
        while let Some(event) = process.next_event().await {
            got.add(event);
        }
    };
    if tokio::time::timeout(LIMIT, read).await.is_err() {
        panic!("`{id}`: the process did not end in {LIMIT:?}: {got:?}");
    }
    got
}

/// Read until `needle` is in the output of `stream` (at most [`LIMIT`]).
async fn read_until(
    id: &str,
    process: &dyn WorkspaceProcess,
    got: &mut Collected,
    stream: StreamId,
    needle: &str,
) {
    let found = |got: &Collected| {
        let bytes = match stream {
            StreamId::Stdout => &got.stdout,
            StreamId::Stderr => &got.stderr,
            _ => &got.pty,
        };
        String::from_utf8_lossy(bytes).contains(needle)
    };
    let read = async {
        while !found(got) {
            match process.next_event().await {
                Some(event) => got.add(event),
                None => break,
            }
        }
    };
    if tokio::time::timeout(LIMIT, read).await.is_err() || !found(got) {
        panic!("`{id}`: never saw {needle:?}: {got:?}");
    }
}

fn request(command: impl Into<String>) -> SpawnRequest {
    SpawnRequest {
        command: command.into(),
        inherit_env: true,
        timeout_ms: Some(LIMIT.as_millis() as u64),
        ..Default::default()
    }
}

async fn spawn(id: &str, ws: &Arc<dyn Workspace>, req: SpawnRequest) -> Box<dyn WorkspaceProcess> {
    let command = req.command.clone();
    ws.spawn(req)
        .await
        .unwrap_or_else(|e| panic!("`{id}`: spawn `{command}`: {e}"))
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn marker(ws: &Arc<dyn Workspace>, name: &str) -> std::path::PathBuf {
    ws.resolve(&WorkspacePath::parse(name).expect("marker path"))
}

/// Assert that `marker` is never written: the command that would write it
/// after two seconds was stopped with its whole tree.
async fn assert_never_written(id: &str, ws: &Arc<dyn Workspace>, markers: &[&Path], what: &str) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    for marker in markers {
        assert!(
            !ws.env().exists(marker).await.expect("exists"),
            "`{id}`: {what}: the stopped tree kept running and wrote {}",
            marker.display()
        );
    }
}

/// Run every process check.
pub async fn processes_conform<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    process_streams_in_order(factory).await;
    process_stdin_and_pty(factory).await;
    process_signals_and_exit_states(factory).await;
    process_timeout_and_cancel_stop_the_tree(factory).await;
    process_overflow_drops_without_blocking(factory).await;
    process_detach_and_attach(factory).await;
    process_lifetime(factory).await;
    process_many_at_once(factory).await;
    process_env_is_explicit(factory).await;
    process_cwd(factory).await;
}

async fn start<F, Fut>(factory: &F) -> Option<(String, DriverFixture, Arc<dyn Workspace>)>
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    if !ws.capabilities().streaming_exec {
        ws.destroy().await.expect("destroy");
        return None;
    }
    Some((id, f, ws))
}

/// Output arrives in order on each stream, `seq` starts at 1 and grows by
/// one, and the exit code is kept.
pub async fn process_streams_in_order<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let p = spawn(
        &id,
        &ws,
        request(
            "i=1; while [ $i -le 300 ]; do echo out$i; echo err$i >&2; i=$((i+1)); done; exit 3",
        ),
    )
    .await;
    let got = collect(&id, p.as_ref()).await;
    let want_out: String = (1..=300).map(|i| format!("out{i}\n")).collect();
    let want_err: String = (1..=300).map(|i| format!("err{i}\n")).collect();
    assert_eq!(got.out(), want_out, "`{id}`: stdout order");
    assert_eq!(
        String::from_utf8_lossy(&got.stderr),
        want_err,
        "`{id}`: stderr order"
    );
    assert_eq!(got.dropped, 0, "`{id}`: nothing dropped");
    let want: Vec<u64> = (1..=got.seqs.len() as u64).collect();
    assert_eq!(got.seqs, want, "`{id}`: seq starts at 1 and grows by one");
    assert_eq!(got.exit, Some(ExitState::Exited { code: 3 }), "`{id}`");
    assert!(
        p.next_event().await.is_none(),
        "`{id}`: no event after exit"
    );
    ws.destroy().await.expect("destroy");
}

/// Input reaches a pipe and a PTY; `close_stdin` ends `cat`; a PTY has the
/// asked size and follows `resize`; a pipe refuses `resize`.
pub async fn process_stdin_and_pty<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let cat = spawn(&id, &ws, request("cat")).await;
    let mut got = Collected::default();
    cat.write_stdin(b"hello pipe\n").await.expect("write");
    read_until(
        &id,
        cat.as_ref(),
        &mut got,
        StreamId::Stdout,
        "hello pipe\n",
    )
    .await;
    let resized = cat.resize(PtySize { rows: 5, cols: 5 }).await;
    assert!(
        matches!(resized, Err(ref e) if e.is_unsupported()),
        "`{id}`: resize without a PTY must be Unsupported: {resized:?}"
    );
    cat.close_stdin().await.expect("close");
    assert!(
        cat.write_stdin(b"late").await.is_err(),
        "`{id}`: write after close"
    );
    let rest = collect(&id, cat.as_ref()).await;
    assert_eq!(rest.exit, Some(ExitState::Exited { code: 0 }), "`{id}`");

    if ws.capabilities().pty {
        let tty = spawn(
            &id,
            &ws,
            SpawnRequest {
                pty: Some(PtySize { rows: 24, cols: 80 }),
                ..request("stty size; read line; stty size; echo got:$line")
            },
        )
        .await;
        let mut got = Collected::default();
        read_until(&id, tty.as_ref(), &mut got, StreamId::Pty, "24 80").await;
        tty.resize(PtySize {
            rows: 40,
            cols: 100,
        })
        .await
        .expect("resize");
        tty.write_stdin(b"typed\n").await.expect("write to the PTY");
        read_until(&id, tty.as_ref(), &mut got, StreamId::Pty, "got:typed").await;
        let rest = collect(&id, tty.as_ref()).await;
        let text =
            String::from_utf8_lossy(&got.pty).into_owned() + &String::from_utf8_lossy(&rest.pty);
        assert!(
            text.contains("40 100"),
            "`{id}`: size after resize: {text:?}"
        );
        assert!(
            got.stdout.is_empty() && got.stderr.is_empty(),
            "`{id}`: a PTY has one stream"
        );
        assert_eq!(rest.exit, Some(ExitState::Exited { code: 0 }), "`{id}`");

        // Ctrl-D from close_stdin ends `cat` on a terminal.
        let cat = spawn(
            &id,
            &ws,
            SpawnRequest {
                pty: Some(PtySize { rows: 24, cols: 80 }),
                ..request("cat")
            },
        )
        .await;
        let mut got = Collected::default();
        cat.write_stdin(b"echoed\n").await.expect("write");
        read_until(&id, cat.as_ref(), &mut got, StreamId::Pty, "echoed").await;
        cat.close_stdin().await.expect("close");
        let rest = collect(&id, cat.as_ref()).await;
        assert_eq!(rest.exit, Some(ExitState::Exited { code: 0 }), "`{id}`");
    }
    ws.destroy().await.expect("destroy");
}

/// `Terminate`, `Interrupt` and `Hangup` reach the process (a trap sees
/// them, or it ends by the signal); `Kill` ends as `Cancelled`.
pub async fn process_signals_and_exit_states<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let trapped = spawn(
        &id,
        &ws,
        request("trap 'echo got-term; exit 9' TERM; echo ready; while :; do sleep 0.05; done"),
    )
    .await;
    let mut got = Collected::default();
    read_until(&id, trapped.as_ref(), &mut got, StreamId::Stdout, "ready").await;
    trapped
        .signal(ProcessSignal::Terminate)
        .await
        .expect("signal");
    let rest = collect(&id, trapped.as_ref()).await;
    assert!(rest.out().contains("got-term"), "`{id}`: {rest:?}");
    assert_eq!(rest.exit, Some(ExitState::Exited { code: 9 }), "`{id}`");

    for (signal, number) in [
        (ProcessSignal::Interrupt, 2),
        (ProcessSignal::Hangup, 1),
        (ProcessSignal::Terminate, 15),
    ] {
        let p = spawn(&id, &ws, request("echo ready; exec sleep 30")).await;
        let mut got = Collected::default();
        read_until(&id, p.as_ref(), &mut got, StreamId::Stdout, "ready").await;
        p.signal(signal).await.expect("signal");
        let rest = collect(&id, p.as_ref()).await;
        assert_eq!(
            rest.exit,
            Some(ExitState::Signaled { signal: number }),
            "`{id}`: {signal:?}"
        );
    }

    let started = Instant::now();
    let p = spawn(&id, &ws, request("trap '' TERM INT HUP; sleep 30")).await;
    p.signal(ProcessSignal::Kill).await.expect("kill");
    let rest = collect(&id, p.as_ref()).await;
    assert_eq!(rest.exit, Some(ExitState::Cancelled), "`{id}`: Kill");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "`{id}`: kill was slow"
    );
    // A signal after the end is not an error and does nothing.
    p.signal(ProcessSignal::Kill).await.expect("late signal");
    ws.destroy().await.expect("destroy");
}

/// A timeout, a cancel and `Kill` stop the whole tree: a background step of
/// the command never runs.
pub async fn process_timeout_and_cancel_stop_the_tree<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let timeout_marker = marker(&ws, "kit-proc-timeout");
    let cancel_marker = marker(&ws, "kit-proc-cancel");
    let kill_marker = marker(&ws, "kit-proc-kill");
    let tree = |m: &Path| format!("(sleep 2; touch {}) & sleep 30", quote(m));

    let started = Instant::now();
    let p = spawn(
        &id,
        &ws,
        SpawnRequest {
            timeout_ms: Some(300),
            ..request(tree(&timeout_marker))
        },
    )
    .await;
    assert_eq!(
        collect(&id, p.as_ref()).await.exit,
        Some(ExitState::TimedOut),
        "`{id}`"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "`{id}`: slow timeout"
    );

    let cancel = CancellationToken::new();
    let p = spawn(
        &id,
        &ws,
        SpawnRequest {
            cancel: Some(cancel.clone()),
            ..request(tree(&cancel_marker))
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    assert_eq!(
        collect(&id, p.as_ref()).await.exit,
        Some(ExitState::Cancelled),
        "`{id}`"
    );

    let p = spawn(&id, &ws, request(tree(&kill_marker))).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    p.signal(ProcessSignal::Kill).await.expect("kill");
    assert_eq!(
        collect(&id, p.as_ref()).await.exit,
        Some(ExitState::Cancelled),
        "`{id}`"
    );

    // A token cancelled before the start: nothing runs.
    let early = CancellationToken::new();
    early.cancel();
    let refused = ws
        .spawn(SpawnRequest {
            cancel: Some(early),
            ..request("echo never")
        })
        .await;
    assert!(refused.is_err(), "`{id}`: spawn with a cancelled token ran");

    assert_never_written(
        &id,
        &ws,
        &[&timeout_marker, &cancel_marker, &kill_marker],
        "timeout, cancel, kill",
    )
    .await;
    ws.destroy().await.expect("destroy");
}

/// A process whose output nobody reads still runs to the end (output is
/// not back-pressure); the small buffer keeps the newest output, and a
/// reader gets one `Dropped` event for the rest. Received plus dropped is
/// all the output.
pub async fn process_overflow_drops_without_blocking<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let total: u64 = 4 * 1024 * 1024;
    let p = spawn(
        &id,
        &ws,
        SpawnRequest {
            output_buffer_bytes: Some(64 * 1024),
            ..request(format!(
                "head -c {total} /dev/zero | tr '\\000' a; echo; echo tail-marker"
            ))
        },
    )
    .await;
    // Do not read: wait until the driver lists it as ended.
    let started = Instant::now();
    loop {
        let list = ws.list_processes().await.expect("list");
        let me = list
            .iter()
            .find(|i| &i.id == p.id())
            .unwrap_or_else(|| panic!("`{id}`: process not listed"));
        if me.exit.is_some() {
            break;
        }
        assert!(
            started.elapsed() < LIMIT,
            "`{id}`: a process with an unread full buffer did not end"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = collect(&id, p.as_ref()).await;
    assert!(got.dropped > 0, "`{id}`: no Dropped event");
    assert!(
        got.out().ends_with("\ntail-marker\n"),
        "`{id}`: newest output kept"
    );
    assert_eq!(
        got.dropped + got.stdout.len() as u64,
        total + 1 + "tail-marker\n".len() as u64,
        "`{id}`: received plus dropped bytes"
    );
    assert!(
        got.stdout.len() <= 64 * 1024,
        "`{id}`: kept {} bytes with a 64 KiB buffer",
        got.stdout.len()
    );
    // After the Dropped event, seqs follow on by one.
    for pair in got.seqs.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "`{id}`: seq after Dropped");
    }
    assert_eq!(got.exit, Some(ExitState::Exited { code: 0 }), "`{id}`");

    // Input is back-pressure: a write to a process that never reads waits.
    let deaf = spawn(&id, &ws, request("sleep 30")).await;
    let big = vec![b'x'; 8 * 1024 * 1024];
    let write = tokio::time::timeout(Duration::from_millis(500), deaf.write_stdin(&big)).await;
    assert!(write.is_err(), "`{id}`: 8 MiB to a full pipe did not wait");
    deaf.signal(ProcessSignal::Kill).await.expect("kill");
    collect(&id, deaf.as_ref()).await;
    ws.destroy().await.expect("destroy");
}

/// A detached process runs on without a handle; `list_processes` shows it;
/// `attach_process` replays the events after `after_seq` and goes on live.
pub async fn process_detach_and_attach<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let p = spawn(
        &id,
        &ws,
        SpawnRequest {
            detach: true,
            ..request("i=0; while [ $i -lt 8 ]; do echo line$i; i=$((i+1)); sleep 0.1; done")
        },
    )
    .await;
    let pid = p.id().clone();
    let mut first = Collected::default();
    read_until(&id, p.as_ref(), &mut first, StreamId::Stdout, "line1\n").await;
    let seen = *first.seqs.last().expect("seq");
    drop(p);

    let listed = ws.list_processes().await.expect("list");
    let info = listed
        .iter()
        .find(|i| i.id == pid)
        .unwrap_or_else(|| panic!("`{id}`: detached process not listed"));
    assert!(info.detached && !info.pty, "`{id}`: {info:?}");

    let again = ws.attach_process(&pid, seen).await.expect("attach");
    let rest = collect(&id, again.as_ref()).await;
    assert_eq!(
        rest.seqs.first(),
        Some(&(seen + 1)),
        "`{id}`: replay after_seq"
    );
    let all = first.out() + &rest.out();
    let want: String = (0..8).map(|i| format!("line{i}\n")).collect();
    assert_eq!(all, want, "`{id}`: each line once over two handles");
    assert_eq!(rest.exit, Some(ExitState::Exited { code: 0 }), "`{id}`");

    // From the start, after the end: the whole buffer again.
    let replay = ws.attach_process(&pid, 0).await.expect("attach 0");
    assert_eq!(collect(&id, replay.as_ref()).await.out(), want, "`{id}`");
    let info = ws
        .list_processes()
        .await
        .expect("list")
        .into_iter()
        .find(|i| i.id == pid)
        .expect("listed after the end");
    assert_eq!(info.exit, Some(ExitState::Exited { code: 0 }), "`{id}`");

    let unknown = ws
        .attach_process(&loop_workspace::ProcessId("no-such".into()), 0)
        .await
        .map(|_| ());
    assert!(
        matches!(unknown, Err(WorkspaceError::UnknownProcess(_))),
        "`{id}`: {unknown:?}"
    );
    ws.destroy().await.expect("destroy");
}

/// Dropping the last handle of a process that is not detached kills its
/// tree; `stop` and `destroy` kill detached processes too, and a stopped
/// Workspace starts none.
pub async fn process_lifetime<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, f, ws)) = start(factory).await else {
        return;
    };
    let dropped = marker(&ws, "kit-proc-dropped");
    let p = spawn(
        &id,
        &ws,
        request(format!("(sleep 2; touch {}) & sleep 30", quote(&dropped))),
    )
    .await;
    let pid = p.id().clone();
    drop(p);

    let stopped = marker(&ws, "kit-proc-stopped");
    let detached = spawn(
        &id,
        &ws,
        SpawnRequest {
            detach: true,
            ..request(format!("(sleep 2; touch {}) & sleep 30", quote(&stopped)))
        },
    )
    .await;
    let detached_id = detached.id().clone();
    drop(detached);
    tokio::time::sleep(Duration::from_millis(200)).await;
    ws.stop().await.expect("stop");
    let after_stop = ws
        .list_processes()
        .await
        .expect("list after stop")
        .into_iter()
        .find(|i| i.id == detached_id)
        .expect("listed");
    assert!(
        after_stop.exit.is_some(),
        "`{id}`: stop left a process running"
    );
    assert!(
        ws.spawn(request("echo no")).await.is_err(),
        "`{id}`: a stopped workspace started a process"
    );
    let killed = ws
        .list_processes()
        .await
        .expect("list")
        .into_iter()
        .find(|i| i.id == pid);
    if let Some(info) = killed {
        assert!(
            info.exit.is_some(),
            "`{id}`: dropped handle left it running"
        );
    }

    // Destroy, on a second Workspace.
    let ws2 = f.start().await;
    let destroyed = marker(&ws2, "kit-proc-destroyed");
    let p = spawn(
        &id,
        &ws2,
        SpawnRequest {
            detach: true,
            ..request(format!("(sleep 2; touch {}) & sleep 30", quote(&destroyed)))
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    ws2.destroy().await.expect("destroy");
    let exit = tokio::time::timeout(Duration::from_secs(10), collect(&id, p.as_ref()))
        .await
        .expect("exit after destroy")
        .exit;
    assert!(exit.is_some(), "`{id}`: no exit after destroy");

    assert_never_written(
        &id,
        &ws,
        &[&dropped, &stopped, &destroyed],
        "drop, stop, destroy",
    )
    .await;
    ws.destroy().await.expect("destroy");
}

/// Sixteen processes at once each get their own output, in parallel.
pub async fn process_many_at_once<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    let started = Instant::now();
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let ws = Arc::clone(&ws);
            let id = id.clone();
            tokio::spawn(async move {
                let pty = (i % 2 == 1).then_some(PtySize { rows: 24, cols: 80 });
                let pty = pty.filter(|_| ws.capabilities().pty);
                let p = spawn(
                    &id,
                    &ws,
                    SpawnRequest {
                        pty,
                        ..request(format!("sleep 1; printf n{i}"))
                    },
                )
                .await;
                let got = collect(&id, p.as_ref()).await;
                let text =
                    String::from_utf8_lossy(if pty.is_some() { &got.pty } else { &got.stdout })
                        .into_owned();
                (i, text, got.exit)
            })
        })
        .collect();
    for task in tasks {
        let (i, text, exit) = task.await.expect("task");
        assert_eq!(text, format!("n{i}"), "`{id}`: output of process {i}");
        assert_eq!(exit, Some(ExitState::Exited { code: 0 }), "`{id}`");
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "`{id}`: 16 one-second processes took {:?}",
        started.elapsed()
    );
    ws.destroy().await.expect("destroy");
}

/// The Runtime's sentinel secret never reaches a spawned process (with the
/// name in `remove_env` for a driver without its own environment, as the
/// Session layer sends it), an override arrives, and without
/// `inherit_env` nothing of the Runtime environment does. With and without
/// a PTY.
pub async fn process_env_is_explicit<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory).await else {
        return;
    };
    assert_eq!(
        std::env::var(SECRET_ENV).as_deref(),
        Ok(SECRET_VALUE),
        "call set_sentinel_secret first"
    );
    let caps = ws.capabilities();
    let ptys: &[Option<PtySize>] = if caps.pty {
        &[None, Some(PtySize { rows: 24, cols: 80 })]
    } else {
        &[None]
    };
    for &pty in ptys {
        let text = |got: &Collected| {
            String::from_utf8_lossy(if pty.is_some() { &got.pty } else { &got.stdout })
                .trim_end()
                .to_owned()
        };
        for inherit_env in [true, false] {
            let remove_env = if caps.own_environment {
                Vec::new()
            } else {
                vec![SECRET_ENV.to_owned()]
            };
            let p = spawn(
                &id,
                &ws,
                SpawnRequest {
                    inherit_env,
                    remove_env,
                    pty,
                    env: HashMap::from([("LOOP_WS_KIT_VAR".to_owned(), "kit-value".to_owned())]),
                    ..request(format!(
                        "printf '%s|%s' \"${{{SECRET_ENV}-absent}}\" \"${{LOOP_WS_KIT_VAR-absent}}\""
                    ))
                },
            )
            .await;
            let got = collect(&id, p.as_ref()).await;
            if caps.env_allowlist.is_none() {
                assert_eq!(
                    text(&got),
                    "absent|kit-value",
                    "`{id}`: env (inherit_env={inherit_env}, pty={})",
                    pty.is_some()
                );
            } else {
                assert!(
                    text(&got).starts_with("absent|"),
                    "`{id}`: the Runtime secret leaked: {:?}",
                    text(&got)
                );
            }
        }
        if !caps.own_environment && std::env::var_os("HOME").is_some() {
            let p = spawn(
                &id,
                &ws,
                SpawnRequest {
                    inherit_env: false,
                    pty,
                    ..request("printf %s \"${HOME-unset}\"")
                },
            )
            .await;
            assert_eq!(
                text(&collect(&id, p.as_ref()).await),
                "unset",
                "`{id}`: without inherit_env the Runtime environment leaked"
            );
        }
    }
    ws.destroy().await.expect("destroy");
}

/// `cwd` is a folder under the root; a link that leads out is refused.
pub async fn process_cwd<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, f, ws)) = start(factory).await else {
        return;
    };
    let env = ws.env();
    env.create_dir(&marker(&ws, "kit-spawn-cwd/inner"))
        .await
        .expect("create_dir");
    let root = env.canonical_path(&ws.root()).await.expect("root");
    let p = spawn(
        &id,
        &ws,
        SpawnRequest {
            cwd: Some(WorkspacePath::parse("kit-spawn-cwd/inner").expect("path")),
            ..request("pwd -P")
        },
    )
    .await;
    let got = collect(&id, p.as_ref()).await;
    assert_eq!(
        Path::new(got.out().trim()),
        root.join("kit-spawn-cwd/inner"),
        "`{id}`: cwd"
    );
    let missing = ws
        .spawn(SpawnRequest {
            cwd: Some(WorkspacePath::parse("kit-no-such-folder").expect("path")),
            ..request("pwd")
        })
        .await;
    assert!(missing.is_err(), "`{id}`: a missing cwd must fail");
    if let (Some(outside), PathNamespace::Host) = (&f.outside, ws.capabilities().path_namespace) {
        let link = marker(&ws, "kit-cwd-link");
        let made = env
            .exec(
                &format!("ln -s {} {}", quote(outside), quote(&link)),
                Default::default(),
            )
            .await
            .expect("ln");
        if made.exit_code == 0 {
            let out = ws
                .spawn(SpawnRequest {
                    cwd: Some(WorkspacePath::parse("kit-cwd-link").expect("path")),
                    ..request("pwd")
                })
                .await;
            assert!(
                matches!(out, Err(WorkspaceError::InvalidPath { .. })),
                "`{id}`: a cwd link that leads out must be refused: {:?}",
                out.map(|_| ())
            );
        }
    }
    ws.destroy().await.expect("destroy");
}
