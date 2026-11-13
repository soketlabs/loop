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
            .field(
                "on_output",
                &self.on_output.as_ref().map(|_| "ShellOutputCallback"),
            )
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
    /// Output was cut at the environment's cap.
    pub truncated: bool,
}

/// Identity of a regular file, used to check that a file is still the one a
/// tool wrote before removing it. `dev`/`ino`/`mtime_ns` are `None` when the
/// environment cannot report them; then only `size` is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    /// Device number.
    pub dev: Option<u64>,
    /// Inode number.
    pub ino: Option<u64>,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the epoch.
    pub mtime_ns: Option<i128>,
}

impl FileIdentity {
    /// Identity from Unix metadata (lstat or fstat).
    #[cfg(unix)]
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: Some(meta.dev()),
            ino: Some(meta.ino()),
            size: meta.len(),
            mtime_ns: Some(
                i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            ),
        }
    }

    /// Identity from Windows or other metadata (size and mtime only).
    #[cfg(not(unix))]
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128);
        Self {
            dev: None,
            ino: None,
            size: meta.len(),
            mtime_ns,
        }
    }

    /// Identity with only a size, for environments without stat.
    pub fn from_info(info: &FileInfo) -> Option<Self> {
        info.is_file.then_some(Self {
            dev: None,
            ino: None,
            size: info.size,
            mtime_ns: None,
        })
    }

    /// Whether `other` is the same file: dev/ino when both have them, else
    /// size plus mtime when both have it, else size.
    pub fn matches(&self, other: &Self) -> bool {
        if let (Some(a), Some(b), Some(c), Some(d)) = (self.dev, self.ino, other.dev, other.ino) {
            return a == c && b == d && self.size == other.size;
        }
        if let (Some(a), Some(b)) = (self.mtime_ns, other.mtime_ns) {
            return a == b && self.size == other.size;
        }
        self.size == other.size
    }
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
    /// Identity of the regular file at `path`, without following a final
    /// symlink. `Ok(None)` when `path` is not a regular file (a directory, a
    /// symlink or another kind). The default can only follow symlinks and
    /// report a size; environments with stat override it.
    async fn file_identity(&self, path: &Path) -> Result<Option<FileIdentity>, FileError> {
        Ok(FileIdentity::from_info(&self.file_info(path).await?))
    }
}

/// Shell capability.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Whether commands run in a sandbox, where a permission error is the
    /// sandbox's refusal rather than a file mode.
    fn sandboxed(&self) -> bool {
        false
    }

    /// A short note for a failed command whose `output` names a variable that
    /// Loop removed from the command environment. At most once per variable
    /// per turn. `None` when nothing was removed or the command succeeded.
    fn removed_env_note(&self, _output: &str, _exit_code: i32) -> Option<String> {
        None
    }

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
    /// No model is selected yet.
    #[error("No model selected — run /model to choose one (or pass --provider and --model)")]
    NoModelSelected,
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
    /// Another process owns the Session. The text is meant for people as is.
    #[error("{0}")]
    InUse(String),
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
