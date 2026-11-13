//! Streaming processes and PTYs in a Workspace.
//!
//! `Shell::exec` runs one command to the end and returns its output. A
//! terminal and long commands need more: input, live output, a window size,
//! signals and an exit event. [`crate::Workspace::spawn`] returns a
//! [`WorkspaceProcess`] for that. No driver in this repository implements it
//! yet; the default returns `WorkspaceError::Unsupported`.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::WorkspaceError;

/// Terminal window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

/// A request to start a process in a Workspace.
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// Command string for the Workspace shell (`sh -c`).
    pub command: String,
    /// Working directory in the Workspace namespace; `None` is the root.
    pub cwd: Option<PathBuf>,
    /// Variables to set.
    pub env: Option<HashMap<String, String>>,
    /// Keep the driver's base environment. The caller (the Session layer)
    /// applies the tool env policy before the request reaches the driver.
    pub inherit_env: bool,
    /// Give the process a PTY of this size. `None` means plain pipes.
    pub pty: Option<PtySize>,
    /// Stop the process after this many milliseconds.
    pub timeout_ms: Option<u64>,
}

/// A signal for a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSignal {
    /// Ctrl-C.
    Interrupt,
    /// Polite stop.
    Terminate,
    /// Forced stop.
    Kill,
    /// The terminal closed.
    Hangup,
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessExit {
    /// Exit code, when the process exited.
    pub code: Option<i32>,
    /// Signal number, when a signal stopped it.
    pub signal: Option<i32>,
    /// The driver stopped it because of `timeout_ms`.
    pub timed_out: bool,
}

/// Output or the end of a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvent {
    /// Bytes on stdout (or the PTY).
    Stdout(Vec<u8>),
    /// Bytes on stderr (never with a PTY).
    Stderr(Vec<u8>),
    /// The process ended. No event follows.
    Exited(ProcessExit),
}

/// A running process in a Workspace. Methods take `&self` so one task can
/// read events while another writes input or sends signals.
#[async_trait]
pub trait WorkspaceProcess: Send + Sync {
    /// Process id in the Workspace namespace, for logs.
    fn id(&self) -> String;
    /// Write bytes to stdin (or the PTY).
    async fn write_stdin(&self, data: &[u8]) -> Result<(), WorkspaceError>;
    /// Close stdin. Writes after this fail.
    async fn close_stdin(&self) -> Result<(), WorkspaceError>;
    /// Change the PTY size. Unsupported without a PTY.
    async fn resize(&self, size: PtySize) -> Result<(), WorkspaceError>;
    /// Send a signal to the process group.
    async fn signal(&self, signal: ProcessSignal) -> Result<(), WorkspaceError>;
    /// The next event; `None` after `Exited` was returned.
    async fn next_event(&self) -> Option<ProcessEvent>;
}
