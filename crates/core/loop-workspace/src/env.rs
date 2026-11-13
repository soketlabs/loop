//! The execution environment contract: files and shell commands of a
//! workspace. Moved from `loop_agent::harness::types`, which re-exports it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

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
