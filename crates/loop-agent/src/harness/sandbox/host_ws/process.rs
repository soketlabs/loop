//! Streaming processes of a host Workspace (`Workspace::spawn`).
//!
//! # Model
//!
//! - **Start.** `sh -c <command>` in a new session (`setsid`), so the
//!   process leads its own session and process group. With a PTY, the
//!   slave is its stdin, stdout and stderr and its controlling terminal.
//!   A dedicated waiter thread starts the child and waits for it.
//! - **Output.** Pipe readers are Tokio tasks; the PTY reader is a thread
//!   (a PTY master is a device, and kqueue on macOS does not report it).
//!   Readers never wait for a consumer: they put output in the bounded
//!   [`Ring`], which evicts the oldest output. A child blocks on a full
//!   pipe only while a reader has not yet taken the bytes; the Runtime
//!   never blocks on the child.
//! - **Input.** `write_stdin` waits while the pipe or PTY is full (an
//!   async pipe write, or a writer thread fed by a bounded queue).
//! - **Exit and reaping.** The waiter learns of the exit without reaping
//!   (`waitid` with `WNOWAIT`), kills what is left of the session, then
//!   reaps. Every signal to the tree holds the same lock and is skipped
//!   after the reap, so a recycled pid is never signalled. After the exit
//!   the readers get a short time to drain; then the `Exited` event ends
//!   the stream.
//! - **Kill tree.** `tree::kill_session` kills the process group and
//!   every live member of the session, until none is left. Out of reach: a
//!   descendant that starts its own session (`setsid`, a daemon).
//! - **Lifetime.** A process that is not detached is killed when its last
//!   handle is dropped. A detached one runs on and can be attached again.
//!   `stop`, `destroy` and dropping the Workspace kill every process of
//!   the Workspace. Processes do not survive the Runtime process: there is
//!   no `Lost` state for this driver. When the Runtime is killed (no
//!   shutdown runs), its processes can stay; nothing kills them then.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use loop_workspace::{
    output_buffer_bytes, ExecutionError, ExecutionErrorCode, ExitState, ProcessEvent, ProcessId,
    ProcessInfo, ProcessSignal, PtySize, SpawnRequest, StreamId, WorkspaceError, WorkspaceProcess,
};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

use super::ring::{Cursor, Ring};
use super::{pty, tree};

/// Most processes of one Workspace that run at the same time.
pub(crate) const MAX_RUNNING: usize = 64;
/// Ended processes a Workspace keeps for `list_processes` and attach.
const MAX_ENDED: usize = 32;
/// How long readers may drain after the exit before `Exited` is sent.
const DRAIN: Duration = Duration::from_millis(500);
/// Bytes one read takes from a pipe or the PTY.
const READ_CHUNK: usize = 16 * 1024;
/// PTY writes that may wait in the queue of the writer thread.
const PTY_WRITE_QUEUE: usize = 4;

fn exec_error(code: ExecutionErrorCode, message: impl Into<String>) -> WorkspaceError {
    WorkspaceError::Exec(ExecutionError::new(code, message))
}

/// Every pid that leads a session this driver started, for the leak
/// check of the tests.
static STARTED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Every process of every host Workspace in this Runtime process.
static LIVE: Mutex<Vec<Weak<HostProcess>>> = Mutex::new(Vec::new());

/// Kill every process that any host Workspace of this Runtime started
/// (with its tree) and wait until each is reaped, at most `limit`. For the
/// Runtime's shutdown: a Workspace that is never dropped or destroyed
/// still leaves no process behind. Blocking; call it from a blocking
/// context.
pub fn kill_all_host_processes(limit: Duration) {
    let live: Vec<Arc<HostProcess>> = {
        let mut all = LIVE.lock();
        all.retain(|p| p.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect()
    };
    for p in &live {
        if p.ring.exit().is_none() {
            p.stop_reason(ExitState::Cancelled);
            p.signal_now(libc::SIGKILL);
        }
    }
    let deadline = std::time::Instant::now() + limit;
    while live.iter().any(|p| !*p.reaped.lock()) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kill the tree of `sid` with the process-group signal only, as when the
/// process listing finds nothing (no `/proc`). For tests.
#[doc(hidden)]
pub fn kill_tree_without_listing(sid: i32) -> Vec<i32> {
    tree::kill_session_with(sid, |_| Vec::new())
}

/// Live processes in a session that a host Workspace started and whose
/// leader already ended. Tests call this to prove that nothing is left
/// behind (the sessions of processes still running are not listed).
#[doc(hidden)]
pub fn leaked_host_processes() -> Vec<i32> {
    let started: Vec<i32> = STARTED.lock().clone();
    let mut out = Vec::new();
    for sid in started {
        // A leader that still runs (another test's process) is not a leak.
        // SAFETY: signal 0 only checks that the pid exists.
        let leader_alive =
            unsafe { libc::kill(sid, 0) } == 0 && tree::session_members(sid).contains(&sid);
        if !leader_alive {
            out.extend(tree::session_members(sid));
        }
    }
    out
}

/// What makes the stdin of a process.
enum Stdin {
    Pipe(tokio::sync::Mutex<Option<tokio::process::ChildStdin>>),
    Pty {
        queue: Mutex<Option<mpsc::Sender<PtyWrite>>>,
    },
}

type PtyWrite = (Vec<u8>, oneshot::Sender<std::io::Result<()>>);

/// One process and everything the handles share.
pub(crate) struct HostProcess {
    id: ProcessId,
    command: String,
    detached: bool,
    ring: Arc<Ring>,
    leader: i32,
    /// `true` once the leader was reaped. Held while a signal is sent.
    reaped: Arc<Mutex<bool>>,
    /// Why the driver stopped it (`TimedOut`, `Cancelled`), if it did.
    reason: Mutex<Option<ExitState>>,
    stdin: Stdin,
    master: Option<File>,
    /// The guard that kills a process that is not detached when the last
    /// handle goes.
    owner: Mutex<Weak<OwnerGuard>>,
    /// Handles open now.
    handles: AtomicUsize,
    /// Readers stop reading while this process is detached, has no handle
    /// and its buffer is full (the child then waits on its pipe or PTY).
    pause_unwatched: bool,
    /// Readers end now (a short time after the exit).
    readers_stop: Arc<AtomicBool>,
}

impl HostProcess {
    fn info(&self) -> ProcessInfo {
        ProcessInfo {
            id: self.id.clone(),
            command: self.command.clone(),
            pty: self.master.is_some(),
            detached: self.detached,
            exit: self.ring.exit(),
            last_seq: self.ring.last_seq(),
        }
    }

    /// Whether readers wait: detached, no handle, full buffer.
    fn paused(&self) -> bool {
        self.pause_unwatched
            && self.detached
            && self.handles.load(Ordering::Acquire) == 0
            && self.ring.full(READ_CHUNK)
            && self.ring.exit().is_none()
    }

    /// Set the stop reason unless one is set.
    fn stop_reason(&self, reason: ExitState) {
        let mut slot = self.reason.lock();
        if slot.is_none() {
            *slot = Some(reason);
        }
    }

    /// Send `signal` to the tree now (blocking: lists processes). Nothing
    /// after the reap.
    fn signal_now(&self, signal: i32) {
        let reaped = self.reaped.lock();
        if *reaped {
            return;
        }
        if signal == libc::SIGKILL {
            let left = tree::kill_session(self.leader);
            if !left.is_empty() {
                tracing::warn!(pids = ?left, "workspace process: members still alive after kill");
            }
        } else {
            tree::signal_session(self.leader, signal);
        }
    }

    /// Kill the tree with `reason`, from any thread; does not wait.
    fn kill_detached(self: &Arc<Self>, reason: ExitState) {
        self.stop_reason(reason);
        let process = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("loop-proc-kill".into())
            .spawn(move || process.signal_now(libc::SIGKILL));
        if let Err(error) = spawned {
            tracing::error!(%error, "workspace process: could not start the kill thread");
            self.signal_now(libc::SIGKILL);
        }
    }

    /// Kill the tree with `reason` and wait until the exit event is in the
    /// buffer (at most `limit`).
    async fn kill_and_wait(self: &Arc<Self>, reason: ExitState, limit: Duration) {
        self.stop_reason(reason);
        let process = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || process.signal_now(libc::SIGKILL)).await;
        let mut changes = self.ring.subscribe();
        let _ = tokio::time::timeout(limit, async {
            while self.ring.exit().is_none() {
                if changes.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

/// Kills a process that is not detached when dropped.
struct OwnerGuard(Arc<HostProcess>);

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        if self.0.ring.exit().is_none() {
            self.0.kill_detached(ExitState::Cancelled);
        }
    }
}

/// The processes of one Workspace.
pub(crate) struct ProcessTable {
    inner: Arc<TableInner>,
    pause_unwatched: bool,
}

/// Lock order: `slots` before `processes` before `ended`.
#[derive(Default)]
struct TableInner {
    slots: Mutex<Slots>,
    processes: Mutex<HashMap<String, Arc<HostProcess>>>,
    ended: Mutex<VecDeque<String>>,
}

#[derive(Default)]
struct Slots {
    /// Processes running or being started.
    running: usize,
    /// `stop` or `destroy` runs or ran: no process starts.
    closing: bool,
}

/// A place for one process, taken before the start; given back on drop
/// unless the process started (then its end gives it back).
struct Slot(Option<Arc<TableInner>>);

impl Slot {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(table) = self.0.take() {
            table.free_slot();
        }
    }
}

impl TableInner {
    fn free_slot(&self) {
        let mut slots = self.slots.lock();
        slots.running = slots.running.saturating_sub(1);
    }

    fn ended(&self, id: &str) {
        self.free_slot();
        let mut ended = self.ended.lock();
        ended.push_back(id.to_owned());
        while ended.len() > MAX_ENDED {
            if let Some(old) = ended.pop_front() {
                self.processes.lock().remove(&old);
            }
        }
    }
}

/// How a process is started, resolved by the Workspace.
pub(crate) struct Launch {
    pub request: SpawnRequest,
    pub cwd: PathBuf,
    pub workspace_id: String,
}

impl ProcessTable {
    /// A table; `pause_unwatched` is the spec option `pauseUnwatchedOutput`.
    pub(crate) fn new(pause_unwatched: bool) -> Self {
        Self {
            inner: Arc::new(TableInner::default()),
            pause_unwatched,
        }
    }

    /// Allow starts again (after `stop`, at `start`).
    pub(crate) fn open(&self) {
        self.inner.slots.lock().closing = false;
    }

    fn get(&self, id: &ProcessId) -> Result<Arc<HostProcess>, WorkspaceError> {
        self.inner
            .processes
            .lock()
            .get(&id.0)
            .cloned()
            .ok_or_else(|| WorkspaceError::UnknownProcess(id.0.clone()))
    }

    pub(crate) fn list(&self) -> Vec<ProcessInfo> {
        let mut out: Vec<ProcessInfo> = self
            .inner
            .processes
            .lock()
            .values()
            .map(|p| p.info())
            .collect();
        out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        out
    }

    pub(crate) fn attach(
        &self,
        id: &ProcessId,
        after_seq: u64,
    ) -> Result<Box<dyn WorkspaceProcess>, WorkspaceError> {
        let process = self.get(id)?;
        let last = process.ring.last_seq();
        if after_seq > last {
            return Err(WorkspaceError::SeqAhead {
                process: id.0.clone(),
                after_seq,
                last_seq: last,
            });
        }
        let owner = process.owner.lock().upgrade();
        if !process.detached && owner.is_none() && process.ring.exit().is_none() {
            // Its last handle went and the kill is under way.
            return Err(WorkspaceError::UnknownProcess(id.0.clone()));
        }
        Ok(Box::new(Handle::new(process, after_seq, owner)))
    }

    /// Kill every process and wait for each to be reaped (bounded).
    pub(crate) async fn kill_all(&self, clear: bool) {
        // No start after this point: a start that took its slot before
        // sees `closing` when it registers and kills its own process.
        self.inner.slots.lock().closing = true;
        let all: Vec<Arc<HostProcess>> = self.inner.processes.lock().values().cloned().collect();
        let waits = all
            .iter()
            .filter(|p| p.ring.exit().is_none())
            .map(|p| p.kill_and_wait(ExitState::Cancelled, Duration::from_secs(5)));
        futures::future::join_all(waits).await;
        if clear {
            self.inner.processes.lock().clear();
            self.inner.ended.lock().clear();
        }
    }

    /// Kill every process without waiting (for `Drop`).
    pub(crate) fn kill_all_now(&self) {
        self.inner.slots.lock().closing = true;
        for p in self.inner.processes.lock().values() {
            if p.ring.exit().is_none() {
                p.kill_detached(ExitState::Cancelled);
            }
        }
    }

    pub(crate) async fn spawn(
        &self,
        launch: Launch,
    ) -> Result<Box<dyn WorkspaceProcess>, WorkspaceError> {
        let Launch {
            request,
            cwd,
            workspace_id,
        } = launch;
        // Take a slot before any wait, so parallel starts never pass the
        // limit and a start after `stop` or `destroy` is refused.
        let slot = {
            let mut slots = self.inner.slots.lock();
            if slots.closing {
                return Err(WorkspaceError::Sandbox(
                    loop_workspace::SandboxError::NotReady("workspace is stopping".into()),
                ));
            }
            if slots.running >= MAX_RUNNING {
                return Err(WorkspaceError::Busy {
                    id: workspace_id,
                    operation: "spawn",
                });
            }
            slots.running += 1;
            Slot(Some(Arc::clone(&self.inner)))
        };
        if request.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(exec_error(ExecutionErrorCode::Aborted, "operation aborted"));
        }

        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(&request.command).current_dir(&cwd);
        if !request.inherit_env {
            command.env_clear();
        }
        command.envs(&request.env);
        for name in &request.remove_env {
            command.env_remove(name);
        }
        let pty = match request.pty {
            Some(size) => {
                let pair = pty::open(size).map_err(|e| {
                    exec_error(ExecutionErrorCode::SpawnFailed, format!("open a PTY: {e}"))
                })?;
                let io = |f: &File| {
                    f.try_clone().map(Stdio::from).map_err(|e| {
                        exec_error(ExecutionErrorCode::SpawnFailed, format!("PTY: {e}"))
                    })
                };
                command
                    .stdin(io(&pair.slave)?)
                    .stdout(io(&pair.slave)?)
                    .stderr(io(&pair.slave)?);
                Some(pair)
            }
            None => {
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                None
            }
        };
        let with_pty = pty.is_some();
        // SAFETY: getpid has no arguments.
        let runtime_pid = unsafe { libc::getpid() };
        // SAFETY: only async-signal-safe calls (setsid, ioctl, prctl,
        // getppid, close_range, fcntl) run between fork and exec; no
        // allocation and no lock.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if with_pty {
                    pty::take_controlling_terminal()?;
                }
                #[cfg(target_os = "linux")]
                {
                    // Die with the waiter thread (it lives as long as this
                    // child), so a Runtime that dies leaves no leader.
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != runtime_pid {
                        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                    }
                }
                let _ = runtime_pid;
                close_inherited_fds();
                Ok(())
            });
        }

        let ring = Arc::new(Ring::new(output_buffer_bytes(request.output_buffer_bytes)));
        let reaped = Arc::new(Mutex::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        {
            let reaped = Arc::clone(&reaped);
            std::thread::Builder::new()
                .name("loop-proc-wait".into())
                .spawn(move || waiter(command, reaped, ready_tx, exit_tx))
                .map_err(|e| {
                    exec_error(
                        ExecutionErrorCode::SpawnFailed,
                        format!("start the waiter thread: {e}"),
                    )
                })?;
        }
        let mut ready_rx = ReadyGuard {
            rx: Some(ready_rx),
            reaped: Arc::clone(&reaped),
        };
        let (leader, stdio) = match ready_rx.wait().await {
            Some(Ok(started)) => started,
            Some(Err(e)) => return Err(exec_error(ExecutionErrorCode::SpawnFailed, e.to_string())),
            None => {
                return Err(exec_error(
                    ExecutionErrorCode::SpawnFailed,
                    "the waiter thread ended",
                ))
            }
        };
        {
            let mut started = STARTED.lock();
            if started.len() >= 4096 {
                started.remove(0);
            }
            started.push(leader);
        }

        let mut readers: Vec<Reader> = Vec::new();
        let mut keep_slave = None;
        let readers_stop = Arc::new(AtomicBool::new(false));
        // Filled once the process exists; readers ask it whether to pause.
        let gate: Arc<std::sync::OnceLock<Weak<HostProcess>>> =
            Arc::new(std::sync::OnceLock::new());
        let (stdin, master) = match pty {
            Some(pair) => {
                let reader = pair.master.try_clone();
                let writer = pair.master.try_clone();
                let (reader, writer) = match (reader, writer) {
                    (Ok(r), Ok(w)) => (r, w),
                    (Err(e), _) | (_, Err(e)) => {
                        kill_unwatched(leader, &reaped);
                        return Err(exec_error(ExecutionErrorCode::SpawnFailed, e.to_string()));
                    }
                };
                readers.push(start_pty_reader(
                    reader,
                    Arc::clone(&ring),
                    Arc::clone(&readers_stop),
                    Arc::clone(&gate),
                ));
                let queue = start_pty_writer(writer);
                // Keep the slave open until the exit, so the master never
                // sees a hang-up before the child's output is read.
                keep_slave = Some(pair.slave);
                (
                    Stdin::Pty {
                        queue: Mutex::new(queue),
                    },
                    Some(pair.master),
                )
            }
            None => {
                let (child_in, child_out, child_err) = stdio;
                let pipes = (|| -> std::io::Result<_> {
                    let child_in = child_in
                        .map(tokio::process::ChildStdin::from_std)
                        .transpose()?;
                    let child_out = child_out
                        .map(tokio::process::ChildStdout::from_std)
                        .transpose()?;
                    let child_err = child_err
                        .map(tokio::process::ChildStderr::from_std)
                        .transpose()?;
                    Ok((child_in, child_out, child_err))
                })();
                let (child_in, child_out, child_err) = match pipes {
                    Ok(p) => p,
                    Err(e) => {
                        kill_unwatched(leader, &reaped);
                        return Err(exec_error(ExecutionErrorCode::SpawnFailed, e.to_string()));
                    }
                };
                if let Some(out) = child_out {
                    readers.push(start_pipe_reader(
                        out,
                        StreamId::Stdout,
                        Arc::clone(&ring),
                        Arc::clone(&gate),
                    ));
                }
                if let Some(err) = child_err {
                    readers.push(start_pipe_reader(
                        err,
                        StreamId::Stderr,
                        Arc::clone(&ring),
                        Arc::clone(&gate),
                    ));
                }
                (Stdin::Pipe(tokio::sync::Mutex::new(child_in)), None)
            }
        };

        let id = ProcessId(format!("proc-{}", loop_ai::new_id()));
        let process = Arc::new(HostProcess {
            id: id.clone(),
            command: request.command.clone(),
            detached: request.detach,
            ring,
            leader,
            reaped,
            reason: Mutex::new(None),
            stdin,
            master,
            owner: Mutex::new(Weak::new()),
            handles: AtomicUsize::new(0),
            pause_unwatched: self.pause_unwatched,
            readers_stop,
        });
        let _ = gate.set(Arc::downgrade(&process));
        let owner = (!request.detach).then(|| {
            let guard = Arc::new(OwnerGuard(Arc::clone(&process)));
            *process.owner.lock() = Arc::downgrade(&guard);
            guard
        });
        {
            let slots = self.inner.slots.lock();
            if slots.closing {
                // `stop` or `destroy` ran while this one started.
                drop(slots);
                process.stop_reason(ExitState::Cancelled);
                kill_unwatched(leader, &process.reaped);
            }
            self.inner
                .processes
                .lock()
                .insert(id.0.clone(), Arc::clone(&process));
        }
        // The process's end gives the slot back.
        slot.keep();
        {
            let mut live = LIVE.lock();
            live.retain(|p| p.strong_count() > 0);
            live.push(Arc::downgrade(&process));
        }

        tokio::spawn(supervise(
            Arc::clone(&process),
            Arc::downgrade(&self.inner),
            request.timeout_ms,
            request.cancel.clone(),
            exit_rx,
            readers,
            keep_slave,
        ));
        Ok(Box::new(Handle::new(process, 0, owner)))
    }
}

/// Kill a child whose waiter runs but whose handle was never made.
fn kill_unwatched(leader: i32, reaped: &Arc<Mutex<bool>>) {
    let reaped = reaped.lock();
    if !*reaped {
        tree::kill_session(leader);
    }
}

type Started = (
    i32,
    (
        Option<std::process::ChildStdin>,
        Option<std::process::ChildStdout>,
        Option<std::process::ChildStderr>,
    ),
);

/// The waiter thread: start the child, wait for its exit without reaping,
/// kill the rest of its session, reap.
fn waiter(
    mut command: std::process::Command,
    reaped: Arc<Mutex<bool>>,
    ready: oneshot::Sender<std::io::Result<Started>>,
    exit: oneshot::Sender<std::io::Result<std::process::ExitStatus>>,
) {
    let spawned = command.spawn();
    // The command holds the parent's copies of the PTY slave; close them.
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let leader = child.id() as i32;
    let stdio = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    if ready.send(Ok((leader, stdio))).is_err() {
        // The spawn call went away: nobody owns this process.
        tree::kill_session(leader);
    }
    let waited = wait_without_reaping(leader);
    let status = {
        let mut reaped = reaped.lock();
        match waited {
            // A zombie, not reaped: its ids are still ours.
            Ok(()) => {
                tree::kill_session(leader);
            }
            // The leader may already be reaped (ECHILD): signal nothing.
            Err(error) => {
                tracing::warn!(%error, "workspace process: waitid failed; not signalling");
            }
        }
        let status = child.wait();
        *reaped = true;
        status
    };
    let _ = exit.send(status);
}

/// Block until `pid` has exited, leaving it a zombie (not reaped).
fn wait_without_reaping(pid: i32) -> std::io::Result<()> {
    loop {
        // SAFETY: `info` is a zeroed POD out-parameter that lives for the
        // call; `pid` is our own unreaped child.
        let rc = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A reader of a pipe or the PTY: done when it reached the end; `abort`
/// ends a pipe reader task (the PTY reader thread sees `readers_stop`).
struct Reader {
    done: oneshot::Receiver<()>,
    task: Option<tokio::task::AbortHandle>,
}

/// Whether a reader of `gate`'s process must wait now.
fn paused(gate: &std::sync::OnceLock<Weak<HostProcess>>) -> bool {
    gate.get()
        .and_then(Weak::upgrade)
        .is_some_and(|p| p.paused())
}

/// How long a paused reader waits before it looks again.
const PAUSE_POLL: Duration = Duration::from_millis(50);

fn start_pipe_reader<R>(
    mut pipe: R,
    stream: StreamId,
    ring: Arc<Ring>,
    gate: Arc<std::sync::OnceLock<Weak<HostProcess>>>,
) -> Reader
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (done, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            while paused(&gate) {
                tokio::time::sleep(PAUSE_POLL).await;
            }
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => ring.push_output(stream, buf[..n].to_vec()),
            }
        }
        let _ = done.send(());
    });
    Reader {
        done: rx,
        task: Some(task.abort_handle()),
    }
}

fn start_pty_reader(
    master: File,
    ring: Arc<Ring>,
    stop: Arc<AtomicBool>,
    gate: Arc<std::sync::OnceLock<Weak<HostProcess>>>,
) -> Reader {
    let (done, rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("loop-pty-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if paused(&gate) {
                    std::thread::sleep(PAUSE_POLL);
                    continue;
                }
                // Wait at most 200 ms for data, so `stop` is seen even when
                // a process that left the session still holds the slave.
                match pty::wait_readable(&master, Duration::from_millis(200)) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(_) => break,
                }
                match (&master).read(&mut buf) {
                    // macOS ends with 0, Linux with EIO after the hang-up.
                    Ok(0) => break,
                    Ok(n) => ring.push_output(StreamId::Pty, buf[..n].to_vec()),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
            let _ = done.send(());
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "workspace process: could not start the PTY reader");
    }
    Reader {
        done: rx,
        task: None,
    }
}

/// Mark every descriptor from 3 up close-on-exec, in the child between
/// fork and exec (async-signal-safe calls only). Descriptors that another
/// thread of the Runtime opened without `O_CLOEXEC`, or before it set the
/// flag, do not reach the command. Stdio (0, 1, 2) stays; the error pipe
/// of `std::process` is already close-on-exec, so it keeps working.
fn close_inherited_fds() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: close_range takes integers; CLOSE_RANGE_CLOEXEC closes
        // nothing now.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if rc == 0 {
            return;
        }
    }
    // SAFETY: getdtablesize has no arguments.
    let max = unsafe { libc::getdtablesize() }.clamp(3, 1 << 20);
    for fd in 3..max {
        // SAFETY: fcntl on any number is safe; a closed one fails with EBADF.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}

/// The receiver of the waiter's "started" message. When the spawn call is
/// dropped before it reads the message, the drop kills the child, so no
/// child runs without a supervisor.
struct ReadyGuard {
    rx: Option<oneshot::Receiver<std::io::Result<Started>>>,
    reaped: Arc<Mutex<bool>>,
}

impl ReadyGuard {
    /// The waiter's message; `None` when the waiter ended without one.
    async fn wait(&mut self) -> Option<std::io::Result<Started>> {
        let got = self.rx.as_mut()?.await.ok();
        self.rx = None;
        got
    }
}

impl Drop for ReadyGuard {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        rx.close();
        if let Ok(Ok((leader, _))) = rx.try_recv() {
            kill_unwatched(leader, &self.reaped);
        }
    }
}

fn start_pty_writer(mut master: File) -> Option<mpsc::Sender<PtyWrite>> {
    let (tx, mut rx) = mpsc::channel::<PtyWrite>(PTY_WRITE_QUEUE);
    let spawned = std::thread::Builder::new()
        .name("loop-pty-write".into())
        .spawn(move || {
            while let Some((data, ack)) = rx.blocking_recv() {
                let result = master.write_all(&data).and_then(|()| master.flush());
                let _ = ack.send(result);
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(error) => {
            tracing::error!(%error, "workspace process: could not start the PTY writer");
            None
        }
    }
}

/// Wait for the exit, the timeout, the cancel or a kill request; then
/// drain and send `Exited`.
async fn supervise(
    process: Arc<HostProcess>,
    table: Weak<TableInner>,
    timeout_ms: Option<u64>,
    cancel: Option<tokio_util::sync::CancellationToken>,
    mut exit: oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
    readers: Vec<Reader>,
    keep_slave: Option<File>,
) {
    let timeout = async {
        match timeout_ms {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending().await,
        }
    };
    let cancelled = async {
        match &cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timeout, cancelled);
    let mut timeout_done = false;
    let mut cancel_done = false;
    let status = loop {
        let reason = tokio::select! {
            status = &mut exit => break status.ok(),
            () = &mut timeout, if !timeout_done => {
                timeout_done = true;
                ExitState::TimedOut
            }
            () = &mut cancelled, if !cancel_done => {
                cancel_done = true;
                ExitState::Cancelled
            }
        };
        process.stop_reason(reason);
        let p = Arc::clone(&process);
        let _ = tokio::task::spawn_blocking(move || p.signal_now(libc::SIGKILL)).await;
    };
    // The leader is reaped and its session killed: close the parent's
    // slave so the PTY reader reaches the end, and let readers drain.
    drop(keep_slave);
    let (done, tasks): (Vec<_>, Vec<_>) = readers.into_iter().map(|r| (r.done, r.task)).unzip();
    let _ = tokio::time::timeout(DRAIN, futures::future::join_all(done)).await;
    // Readers still blocked (a process outside the session holds the pipe
    // or the slave) end now: their threads and descriptors are released.
    process.readers_stop.store(true, Ordering::Release);
    for task in tasks.into_iter().flatten() {
        task.abort();
    }
    let state = match *process.reason.lock() {
        Some(reason) => reason,
        None => match status {
            Some(Ok(status)) => match (status.code(), status.signal()) {
                (Some(code), _) => ExitState::Exited { code },
                (None, Some(signal)) => ExitState::Signaled { signal },
                _ => ExitState::Lost,
            },
            _ => ExitState::Lost,
        },
    };
    process.ring.push_exit(state);
    // No more input: let the PTY writer thread end.
    if let Stdin::Pty { queue } = &process.stdin {
        queue.lock().take();
    }
    if let Some(table) = table.upgrade() {
        table.ended(&process.id.0);
    }
}

/// A handle to a process: its own read position, shared input and signals.
struct Handle {
    process: Arc<HostProcess>,
    reader: tokio::sync::Mutex<(Cursor, watch::Receiver<u64>)>,
    _owner: Option<Arc<OwnerGuard>>,
}

impl Handle {
    fn new(process: Arc<HostProcess>, after_seq: u64, owner: Option<Arc<OwnerGuard>>) -> Self {
        let cursor = process.ring.cursor_after(after_seq);
        let changes = process.ring.subscribe();
        process.handles.fetch_add(1, Ordering::AcqRel);
        Self {
            process,
            reader: tokio::sync::Mutex::new((cursor, changes)),
            _owner: owner,
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.process.handles.fetch_sub(1, Ordering::AcqRel);
    }
}

fn closed() -> WorkspaceError {
    exec_error(ExecutionErrorCode::Io, "stdin is closed")
}

#[async_trait]
impl WorkspaceProcess for Handle {
    fn id(&self) -> &ProcessId {
        &self.process.id
    }

    async fn write_stdin(&self, data: &[u8]) -> Result<(), WorkspaceError> {
        match &self.process.stdin {
            Stdin::Pipe(pipe) => {
                let mut pipe = pipe.lock().await;
                let pipe = pipe.as_mut().ok_or_else(closed)?;
                pipe.write_all(data)
                    .await
                    .map_err(|e| exec_error(ExecutionErrorCode::Io, e.to_string()))?;
                pipe.flush()
                    .await
                    .map_err(|e| exec_error(ExecutionErrorCode::Io, e.to_string()))
            }
            Stdin::Pty { queue } => {
                let sender = queue.lock().clone().ok_or_else(closed)?;
                let (ack, done) = oneshot::channel();
                sender
                    .send((data.to_vec(), ack))
                    .await
                    .map_err(|_| closed())?;
                done.await
                    .map_err(|_| closed())?
                    .map_err(|e| exec_error(ExecutionErrorCode::Io, e.to_string()))
            }
        }
    }

    async fn close_stdin(&self) -> Result<(), WorkspaceError> {
        match &self.process.stdin {
            Stdin::Pipe(pipe) => {
                pipe.lock().await.take();
                Ok(())
            }
            Stdin::Pty { queue } => {
                let Some(sender) = queue.lock().take() else {
                    return Ok(());
                };
                // Ctrl-D: end of file for a reader in canonical mode.
                let (ack, done) = oneshot::channel();
                if sender.send((vec![0x04], ack)).await.is_ok() {
                    let _ = done.await;
                }
                Ok(())
            }
        }
    }

    async fn resize(&self, size: PtySize) -> Result<(), WorkspaceError> {
        let Some(master) = &self.process.master else {
            return Err(WorkspaceError::unsupported(
                super::super::drivers::HOST_DRIVER_ID,
                "resize",
            ));
        };
        pty::resize(master, size).map_err(|e| exec_error(ExecutionErrorCode::Io, e.to_string()))
    }

    async fn signal(&self, signal: ProcessSignal) -> Result<(), WorkspaceError> {
        let number = match signal {
            ProcessSignal::Interrupt => libc::SIGINT,
            ProcessSignal::Terminate => libc::SIGTERM,
            ProcessSignal::Hangup => libc::SIGHUP,
            ProcessSignal::Kill => {
                self.process.stop_reason(ExitState::Cancelled);
                libc::SIGKILL
            }
        };
        let process = Arc::clone(&self.process);
        tokio::task::spawn_blocking(move || process.signal_now(number))
            .await
            .map_err(|e| exec_error(ExecutionErrorCode::Io, e.to_string()))
    }

    async fn next_event(&self) -> Option<ProcessEvent> {
        let mut reader = self.reader.lock().await;
        let (cursor, changes) = &mut *reader;
        self.process.ring.next_event(cursor, changes).await
    }
}
