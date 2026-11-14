//! Streaming processes and PTYs in a Workspace.
//!
//! `Shell::exec` runs one command to the end and returns its output. A
//! terminal and long commands need more: input, live output, a window size,
//! signals, an exit event, and a way back after a lost connection.
//! [`crate::Workspace::spawn`] returns a [`WorkspaceProcess`] for that. The
//! `host` driver implements it; the default returns
//! `WorkspaceError::Unsupported`.
//!
//! # Contract
//!
//! - **Events.** Each event has a `seq` and a [`StreamId`]. The first event
//!   has `seq` 1 and each next event has one more, for a reader that keeps
//!   up. Order is kept within a stream; stdout and stderr are two pipes, so
//!   their relative order is only approximate.
//! - **Ring buffer.** The driver keeps the newest events of each process in
//!   a bounded buffer (`SpawnRequest::output_buffer_bytes`, default
//!   [`DEFAULT_OUTPUT_BUFFER_BYTES`], clamped to
//!   [`MIN_OUTPUT_BUFFER_BYTES`]..=[`MAX_OUTPUT_BUFFER_BYTES`]). When it must
//!   evict events that a reader did not take, that reader gets one
//!   [`ProcessEventKind::Dropped`] event in their place, on
//!   [`StreamId::Control`], with the `seq` of the last evicted event (so
//!   the next event has the next `seq`) and the number of output bytes it
//!   lost. A process never waits for a slow reader: output is not
//!   back-pressure. Input (`write_stdin`) is back-pressure: it waits while
//!   the pipe or PTY is full. The `Exited` event is never evicted.
//! - **Attach.** `Workspace::attach_process(id, after_seq)` gives the events
//!   with `seq > after_seq` that are still in the buffer. If some were
//!   evicted, the first event is `Dropped`. Its `bytes` is exact for a live
//!   handle and for `after_seq` 0; when `after_seq` itself was evicted, it
//!   counts every evicted byte (an upper bound).
//! - **Kill tree.** `signal(Kill)`, a timeout and a cancel stop the process
//!   and every descendant. Each driver says what it cannot reach (the host
//!   driver: a descendant that starts its own session with `setsid`).
//! - **Drop.** When the last handle is dropped and `detach` is `false`, the
//!   driver kills the tree. With `detach` `true` the process keeps running
//!   and `attach_process` finds it.
//! - **Stdin of a PTY.** `close_stdin` sends the end-of-file character
//!   (Ctrl-D) once and refuses later writes; the PTY stays open, because
//!   closing it hangs up the terminal.
//! - **Lost.** When the driver cannot know how a process ended (the
//!   Workspace restarted, the connection broke), the exit is
//!   [`ExitState::Lost`], never a guessed code.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::WorkspaceError;
use crate::path::WorkspacePath;

/// Default size of a process's event buffer: 1 MiB of output.
pub const DEFAULT_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;
/// Smallest event buffer a driver uses; a smaller request gets this.
pub const MIN_OUTPUT_BUFFER_BYTES: usize = 64 * 1024;
/// Largest event buffer a driver uses; a larger request gets this.
pub const MAX_OUTPUT_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// The buffer size for `requested` (see [`SpawnRequest::output_buffer_bytes`]).
pub fn output_buffer_bytes(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_OUTPUT_BUFFER_BYTES)
        .clamp(MIN_OUTPUT_BUFFER_BYTES, MAX_OUTPUT_BUFFER_BYTES)
}

/// Terminal window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

/// Id of a process, unique in its Workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessId(pub String);

/// A request to start a process in a Workspace.
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// Command string for the Workspace shell (`sh -c`).
    pub command: String,
    /// Working directory; `None` is the Workspace root.
    pub cwd: Option<WorkspacePath>,
    /// Variables to set.
    pub env: HashMap<String, String>,
    /// Keep the driver's base environment.
    pub inherit_env: bool,
    /// Names the process must not receive, even with `inherit_env` (the
    /// tool env policy, applied by the Session layer).
    pub remove_env: Vec<String>,
    /// Give the process a PTY of this size. `None` means plain pipes.
    pub pty: Option<PtySize>,
    /// Stop the process tree after this many milliseconds.
    pub timeout_ms: Option<u64>,
    /// Stop the process tree when this token is cancelled.
    pub cancel: Option<CancellationToken>,
    /// Keep running when the handle is dropped.
    pub detach: bool,
    /// Size of the event ring buffer in bytes of output; `None` is 1 MiB.
    /// Clamped to 64 KiB..=16 MiB ([`output_buffer_bytes`]).
    pub output_buffer_bytes: Option<usize>,
}

/// A signal for a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSignal {
    /// Ctrl-C.
    Interrupt,
    /// Polite stop.
    Terminate,
    /// Forced stop of the whole tree.
    Kill,
    /// The terminal closed.
    Hangup,
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ExitState {
    /// It exited with this code.
    Exited {
        /// Exit code.
        code: i32,
    },
    /// A signal stopped it.
    Signaled {
        /// Signal number.
        signal: i32,
    },
    /// The driver stopped it because of `timeout_ms`.
    TimedOut,
    /// The driver stopped it because of `cancel` or `signal(Kill)`.
    Cancelled,
    /// The driver cannot know how it ended.
    Lost,
}

/// The stream an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamId {
    /// Standard output.
    Stdout,
    /// Standard error (never with a PTY).
    Stderr,
    /// The PTY (output and echo together).
    Pty,
    /// Events about the process itself (exit).
    Control,
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEventKind {
    /// Output bytes. Bytes, not text: a PTY sends raw bytes.
    Output(Vec<u8>),
    /// The ring buffer evicted this many output bytes that this reader did
    /// not get. The event has the `seq` of the last evicted event.
    Dropped {
        /// Bytes lost.
        bytes: u64,
    },
    /// The process ended. No event follows.
    Exited(ExitState),
}

/// One event of a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEvent {
    /// Sequence number: 1 for the first event, then one more for each.
    pub seq: u64,
    /// Stream.
    pub stream: StreamId,
    /// What happened.
    pub kind: ProcessEventKind,
}

/// A process as `list_processes` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Id.
    pub id: ProcessId,
    /// The command string.
    pub command: String,
    /// It has a PTY.
    pub pty: bool,
    /// It keeps running without a handle.
    pub detached: bool,
    /// `None` while it runs.
    pub exit: Option<ExitState>,
    /// The `seq` of its newest event.
    pub last_seq: u64,
}

/// A running process in a Workspace. Methods take `&self` so one task can
/// read events while another writes input or sends signals.
#[async_trait]
pub trait WorkspaceProcess: Send + Sync {
    /// Process id.
    fn id(&self) -> &ProcessId;
    /// Write bytes to stdin (or the PTY).
    async fn write_stdin(&self, data: &[u8]) -> Result<(), WorkspaceError>;
    /// Close stdin. Writes after this fail.
    async fn close_stdin(&self) -> Result<(), WorkspaceError>;
    /// Change the PTY size. `Unsupported` without a PTY.
    async fn resize(&self, size: PtySize) -> Result<(), WorkspaceError>;
    /// Send a signal to the whole process tree.
    async fn signal(&self, signal: ProcessSignal) -> Result<(), WorkspaceError>;
    /// The next event; `None` after the `Exited` event was returned.
    async fn next_event(&self) -> Option<ProcessEvent>;
}
