//! Harness capability types, errors, and resources.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Convenience Ok.
pub fn ok<T, E>(value: T) -> Result<T, E> {
    Ok(value)
}

/// Convenience Err.
pub fn err<T, E>(error: E) -> Result<T, E> {
    Err(error)
}

/// Map Result error or panic-equivalent for high-level APIs.
pub fn get_or_throw<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, String> {
    result.map_err(|e| e.to_string())
}

/// Filesystem error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    /// Not found.
    NotFound,
    /// Permission denied.
    PermissionDenied,
    /// Already exists.
    AlreadyExists,
    /// Not a directory.
    NotADirectory,
    /// Is a directory.
    IsDirectory,
    /// Invalid path.
    InvalidPath,
    /// I/O error.
    Io,
    /// Other.
    Other,
}

/// Filesystem error.
#[derive(Debug, Clone, Error)]
#[error("{code:?}: {message}")]
pub struct FileError {
    /// Error code.
    pub code: FileErrorCode,
    /// Message.
    pub message: String,
}

impl FileError {
    /// Construct a file error.
    pub fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Shell execution error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    /// Spawn failed.
    SpawnFailed,
    /// Timed out.
    TimedOut,
    /// Aborted.
    Aborted,
    /// I/O error.
    Io,
    /// Other.
    Other,
}

/// Shell execution error.
#[derive(Debug, Clone, Error)]
#[error("{code:?}: {message}")]
pub struct ExecutionError {
    /// Error code.
    pub code: ExecutionErrorCode,
    /// Message.
    pub message: String,
}

impl ExecutionError {
    /// Construct an execution error.
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// File metadata.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// Absolute path.
    pub path: PathBuf,
    /// Is directory.
    pub is_dir: bool,
    /// Is file.
    pub is_file: bool,
    /// Size in bytes.
    pub size: u64,
}

/// Callback invoked with the cumulative combined stdout+stderr text as it arrives.
pub type ShellOutputCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Options for shell exec.
#[derive(Clone)]
pub struct ShellExecOptions {
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Environment overrides.
    pub env: Option<std::collections::HashMap<String, String>>,
    /// Whether to inherit process env.
    pub inherit_env: bool,
    /// Timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Cancellation.
    pub cancel: Option<CancellationToken>,
    /// Optional live output callback (host env streams; sandboxes may ignore).
    pub on_output: Option<ShellOutputCallback>,
}

impl std::fmt::Debug for ShellExecOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellExecOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout_ms", &self.timeout_ms)
            .field("cancel", &self.cancel.as_ref().map(|_| "CancellationToken"))
            .field("on_output", &self.on_output.as_ref().map(|_| "ShellOutputCallback"))
            .finish()
    }
}

impl Default for ShellExecOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            inherit_env: true,
            timeout_ms: None,
            cancel: None,
            on_output: None,
        }
    }
}

/// Shell command result.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    /// stdout.
    pub stdout: String,
    /// stderr.
    pub stderr: String,
    /// Exit code.
    pub exit_code: i32,
}

/// Filesystem capability (Result-based, never panics).
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Current working directory.
    fn cwd(&self) -> &Path;
    /// Absolute path resolution.
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError>;
    /// Join paths.
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf;
    /// Read text file.
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError>;
    /// Read binary file.
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError>;
    /// Write file (create/overwrite).
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError>;
    /// Append to file.
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError>;
    /// File info.
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError>;
    /// List directory.
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError>;
    /// Exists.
    async fn exists(&self, path: &Path) -> Result<bool, FileError>;
    /// Create directory (recursive).
    async fn create_dir(&self, path: &Path) -> Result<(), FileError>;
    /// Remove file or empty dir.
    async fn remove(&self, path: &Path) -> Result<(), FileError>;
    /// Canonical path.
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError>;
    /// Create temp directory under cwd or system temp.
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError>;
}

/// Shell capability.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Execute a command string via the system shell.
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError>;
}

/// Combined execution environment for tools.
pub trait ExecutionEnv: FileSystem + Shell {}

impl<T> ExecutionEnv for T where T: FileSystem + Shell {}

/// Tool context carrying an execution environment.
#[derive(Clone)]
pub struct ExecutionToolContext {
    /// Execution environment.
    pub env: Arc<dyn ExecutionEnv>,
}

/// Harness phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHarnessPhase {
    /// Idle.
    Idle,
    /// Running a turn.
    Turn,
    /// Compacting.
    Compaction,
    /// Branch summarization.
    BranchSummary,
    /// Retrying summarization.
    Retry,
    /// Running a multi-agent workflow.
    #[cfg(feature = "orchestration")]
    Workflow,
}

/// Compaction result.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Generated or hook-supplied summary.
    pub summary: String,
    /// Estimated tokens before compaction.
    pub tokens_before: u64,
}

/// Navigate-tree result.
#[derive(Debug, Clone)]
pub struct NavigateTreeResult {
    /// Whether the operation was cancelled by a hook.
    pub cancelled: bool,
    /// Branch summary if generated or supplied.
    pub summary: Option<String>,
}

/// High-level harness error.
#[derive(Debug, Error)]
pub enum AgentHarnessError {
    /// Busy.
    #[error("busy")]
    Busy,
    /// Shutting down.
    #[error("shutting down")]
    ShuttingDown,
    /// Hook failure after commit.
    #[error("hook: {0}")]
    Hook(String),
    /// Session error.
    #[error("session: {0}")]
    Session(#[from] SessionError),
    /// Compaction error.
    #[error("compaction: {0}")]
    Compaction(String),
    /// Sandbox error.
    #[error("sandbox: {0}")]
    Sandbox(String),
    /// Agent loop error (preserves structured error from the inner turn loop).
    #[error("agent loop: {0}")]
    AgentLoop(#[from] crate::agent_loop::AgentLoopError),
    /// Other.
    #[error("{0}")]
    Other(String),
}

/// Session error.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Invalid.
    #[error("invalid: {0}")]
    Invalid(String),
    /// I/O.
    #[error("io: {0}")]
    Io(String),
    /// Storage.
    #[error("storage: {0}")]
    Storage(String),
}

/// Compaction error.
#[derive(Debug, Error)]
pub enum CompactionError {
    /// Failed.
    #[error("{0}")]
    Failed(String),
}

/// Branch summary error.
#[derive(Debug, Error)]
pub enum BranchSummaryError {
    /// Failed.
    #[error("{0}")]
    Failed(String),
}

/// Loaded skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Skill name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Body markdown.
    pub body: String,
    /// Source path.
    pub path: PathBuf,
    /// Disable model invocation listing.
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// Prompt template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    /// Template name.
    pub name: String,
    /// Body with $1 / $@ placeholders.
    pub body: String,
    /// Source path.
    pub path: PathBuf,
    /// Optional argument hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// Harness resources.
#[derive(Debug, Clone, Default)]
pub struct AgentHarnessResources {
    /// Skills.
    pub skills: Vec<Skill>,
    /// Prompt templates.
    pub prompt_templates: Vec<PromptTemplate>,
}

/// Opaque JSON details bag.
pub type JsonObject = Value;
