//! The execution environment contract: files and shell commands of a
//! workspace. Moved from `loop_agent::harness::types`, which re-exports it.
//!
//! **Path rule.** Every path given to these methods is in the driver's
//! namespace (`WorkspaceCapabilities::path_namespace`): a host path for a
//! `host` namespace driver, a guest path for a `guest` one. Every
//! Runtime-side check on a Workspace path (permission rules, the tool lock,
//! link and owner checks) must go through these methods, never through
//! `std::fs` or another host API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Whether the agent loop may run a failed operation again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    /// Nothing happened in the Workspace; the same call can run again.
    Safe,
    /// A retry cannot succeed, or it can repeat an effect. Do not retry.
    Unsafe,
    /// The call may or may not have had its effect (a lost connection). Do
    /// not retry blindly; check the state first or tell the model.
    Unknown,
}

/// Filesystem error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
    /// The Workspace or its driver cannot be reached now; nothing happened.
    Unavailable,
    /// The Workspace does not exist any more.
    Gone,
    /// The connection was lost during the call; the effect is unknown.
    OutcomeUnknown,
    /// The environment does not implement this operation.
    Unsupported,
}

impl FileErrorCode {
    /// Retry class for the agent loop.
    pub fn retry(self) -> RetryClass {
        match self {
            Self::Unavailable => RetryClass::Safe,
            Self::OutcomeUnknown => RetryClass::Unknown,
            _ => RetryClass::Unsafe,
        }
    }
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
#[non_exhaustive]
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
    /// The Workspace or its driver cannot be reached now; the command did
    /// not start.
    Unavailable,
    /// The Workspace does not exist any more.
    Gone,
    /// The connection was lost while the command ran; whether it ran, and
    /// what it changed, is unknown.
    OutcomeUnknown,
}

impl ExecutionErrorCode {
    /// Retry class for the agent loop. A timed-out or aborted command may
    /// have changed files before it stopped, so its class is `Unknown`.
    pub fn retry(self) -> RetryClass {
        match self {
            Self::Unavailable | Self::SpawnFailed => RetryClass::Safe,
            Self::OutcomeUnknown | Self::TimedOut | Self::Aborted => RetryClass::Unknown,
            _ => RetryClass::Unsafe,
        }
    }
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

/// Kind of the object at a path, without following a final link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Anything else (socket, device, FIFO).
    Other,
}

/// Metadata of a path without following a final link (`lstat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMetadata {
    /// Kind.
    pub kind: PathKind,
    /// Size in bytes.
    pub size: u64,
    /// Owner user id, when the environment has one.
    pub uid: Option<u32>,
    /// Number of hard links, when the environment has it.
    pub nlink: Option<u64>,
    /// Permission bits (`0o7777` part of the mode), when the environment
    /// has them. The permission gate follows a link in an allow rule's
    /// prefix only inside a folder that root owns and only root may write.
    pub mode: Option<u32>,
}

/// What the permission gate needs to decide on a path, in one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyPath {
    /// The path with every link resolved (as far as it exists).
    pub resolved: PathBuf,
    /// The path exists.
    pub exists: bool,
    /// Some component of the path is (or may be) a symbolic link. A default
    /// implementation that cannot tell sets this when the resolved path
    /// differs from the lexical one.
    pub has_link: bool,
    /// Owner of the final object, when known.
    pub uid: Option<u32>,
    /// Hard links of the final object, when known.
    pub nlink: Option<u64>,
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
    /// Names the command must not receive, even with `inherit_env` (the
    /// tool env policy). A driver with its own environment already keeps
    /// only its allowlist; it must still drop these names.
    pub remove_env: Vec<String>,
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
            .field("remove_env", &self.remove_env)
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
            remove_env: Vec::new(),
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
    /// Metadata of `path` without following a final link (`lstat`). The
    /// default returns `Unsupported`: it cannot tell a link from its target.
    async fn symlink_metadata(&self, path: &Path) -> Result<PathMetadata, FileError> {
        Err(FileError::new(
            FileErrorCode::Unsupported,
            format!("symlink_metadata is not supported: {}", path.display()),
        ))
    }
    /// The target of the link at `path`. The default returns `Unsupported`.
    async fn read_link(&self, path: &Path) -> Result<PathBuf, FileError> {
        Err(FileError::new(
            FileErrorCode::Unsupported,
            format!("read_link is not supported: {}", path.display()),
        ))
    }
    /// Resolve `path` for a permission decision. The default uses
    /// `absolute_path`, `exists` and `canonical_path`, and is conservative:
    /// `has_link` is set when the resolved path differs from the lexical
    /// one, and `uid` and `nlink` are unknown.
    async fn resolve_for_policy(&self, path: &Path) -> Result<PolicyPath, FileError> {
        let absolute = self.absolute_path(path)?;
        let lexical = lexical(&absolute);
        if !self.exists(&absolute).await? {
            return Ok(PolicyPath {
                resolved: lexical,
                exists: false,
                has_link: false,
                uid: None,
                nlink: None,
            });
        }
        let resolved = self.canonical_path(&absolute).await?;
        Ok(PolicyPath {
            has_link: resolved != lexical,
            resolved,
            exists: true,
            uid: None,
            nlink: None,
        })
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

/// `path` with `.` and `..` folded, without touching the disk.
pub fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
