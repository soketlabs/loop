//! The `Workspace` and `WorkspaceDriver` traits.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capabilities::WorkspaceCapabilities;
use crate::error::WorkspaceError;
use crate::path::WorkspacePath;
use crate::platform::HostOs;
use crate::process::{ProcessId, ProcessInfo, SpawnRequest, WorkspaceProcess};
use crate::sandbox::{Sandbox, SandboxConfig, SandboxStatus};

/// Where the files of a new Workspace come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "source", content = "value")]
pub enum WorkspaceSource {
    /// An empty folder.
    Empty,
    /// The files of a snapshot.
    Snapshot(SnapshotId),
    /// A folder on the Runtime host (local drivers: the project folder,
    /// used in place). An empty path means the current folder.
    HostFolder(PathBuf),
}

impl Default for WorkspaceSource {
    fn default() -> Self {
        Self::HostFolder(PathBuf::new())
    }
}

/// What a driver needs to create a Workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    /// Idempotency key from the caller. A driver with the `attach`
    /// capability returns the same Workspace for a repeated key (a retried
    /// create after a lost answer). Local drivers make a new one each time.
    /// Empty means no key.
    #[serde(default)]
    pub key: String,
    /// Tenant the Workspace belongs to (for pools and quotas).
    #[serde(default)]
    pub tenant: Option<String>,
    /// Session the Workspace serves.
    #[serde(default)]
    pub session: Option<String>,
    /// Where the files come from.
    #[serde(default)]
    pub source: WorkspaceSource,
    /// Driver options (the old `SandboxConfig::options`).
    #[serde(default)]
    pub options: Value,
    /// Labels for observability.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

impl WorkspaceSpec {
    /// The host folder, when the source is one.
    pub fn host_folder(&self) -> Option<&Path> {
        match &self.source {
            WorkspaceSource::HostFolder(path) => Some(path),
            _ => None,
        }
    }

    /// The old sandbox config: `workdir` is the host folder, or empty.
    pub fn sandbox_config(&self) -> SandboxConfig {
        SandboxConfig {
            workdir: self
                .host_folder()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            options: self.options.clone(),
            labels: self.labels.clone(),
        }
    }
}

impl From<SandboxConfig> for WorkspaceSpec {
    fn from(config: SandboxConfig) -> Self {
        Self {
            source: WorkspaceSource::HostFolder(config.workdir),
            options: config.options,
            labels: config.labels,
            ..Self::default()
        }
    }
}

/// Lifecycle state of a Workspace. A superset of [`SandboxStatus`] with the
/// states that sleep, wake and loss add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    /// Created, not started.
    Created,
    /// Starting.
    Starting,
    /// Ready for tools.
    Ready,
    /// Stopping.
    Stopping,
    /// Stopped. Files stay when the driver is persistent.
    Stopped,
    /// Saving a snapshot before sleep.
    Hibernating,
    /// Asleep. Files are in a snapshot; no command runs.
    Hibernated,
    /// Restoring from a snapshot.
    Waking,
    /// Failed.
    Failed,
    /// Destroyed or lost. Every operation fails with `WorkspaceError::Gone`.
    Gone,
}

impl WorkspaceStatus {
    /// Stable lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Hibernating => "hibernating",
            Self::Hibernated => "hibernated",
            Self::Waking => "waking",
            Self::Failed => "failed",
            Self::Gone => "gone",
        }
    }
}

impl fmt::Display for WorkspaceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<SandboxStatus> for WorkspaceStatus {
    fn from(status: SandboxStatus) -> Self {
        match status {
            SandboxStatus::Created => Self::Created,
            SandboxStatus::Starting => Self::Starting,
            SandboxStatus::Ready => Self::Ready,
            SandboxStatus::Stopping => Self::Stopping,
            SandboxStatus::Stopped => Self::Stopped,
            SandboxStatus::Failed => Self::Failed,
        }
    }
}

/// Id of a saved Workspace state, owned by the driver.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub String);

/// Identity of a file version (an ETag): changes when the file changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileTag(pub String);

/// One piece of a file, for download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    /// Offset of `data` in the file.
    pub offset: u64,
    /// The bytes; at most the requested length.
    pub data: Vec<u8>,
    /// File size when the chunk was read.
    pub total_size: u64,
    /// `data` ends at the end of the file.
    pub eof: bool,
    /// Version of the file this chunk is from. Pass it as `if_match` for
    /// the next chunk; the driver fails with `WorkspaceError::Changed` when
    /// the file changed in between.
    pub tag: FileTag,
}

/// Id of an upload session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UploadId(pub String);

/// A request to start an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRequest {
    /// Destination.
    pub path: WorkspacePath,
    /// Total size in bytes. More data is `TooLarge`.
    pub size: u64,
    /// Expected SHA-256 of the whole file, lowercase hex. Checked at commit.
    pub sha256: Option<String>,
    /// Commit only when the destination still has this tag (`None`: any).
    pub if_match: Option<FileTag>,
    /// Unix mode for a new file; an existing file keeps its mode. `None`
    /// is the driver default (0644).
    pub mode: Option<u32>,
}

/// A Workspace reopened by `WorkspaceDriver::attach`.
pub struct Attached {
    /// The Workspace.
    pub workspace: Arc<dyn Workspace>,
    /// Fencing epoch of this attachment. Each attach makes it larger; calls
    /// through an older attachment fail with `WorkspaceError::Fenced`, so
    /// two Runtime cells never drive one Workspace after a failover.
    pub epoch: u64,
}

/// A Workspace: where one Session's files and commands live.
///
/// It extends [`Sandbox`] (identity, status, `env()`, start, stop, destroy),
/// so every `Arc<dyn Workspace>` is also an `Arc<dyn Sandbox>` for the
/// harness. The methods below have defaults: a driver that does not
/// implement an operation returns `WorkspaceError::Unsupported` and says so
/// in [`Workspace::capabilities`].
///
/// Paths of `env()` are in the driver's namespace
/// (`capabilities().path_namespace`). Runtime-side checks on them must use
/// `env()`, never a host API.
///
/// Sleep: the Runtime decides when a Session is idle and calls
/// `hibernate`. A pool may also put a Workspace to sleep. A call on a
/// sleeping Workspace (`exec`, file calls) wakes it first, inside the call;
/// when the wake fails, the call fails with a retryable `Unavailable` code.
/// `hibernate` while a call is in flight returns `WorkspaceError::Busy`.
#[async_trait]
pub trait Workspace: Sandbox {
    /// Id of the driver that made this Workspace (`host`, `seatbelt`, …).
    fn driver_id(&self) -> &str;

    /// What this Workspace can do and which limits it enforces.
    fn capabilities(&self) -> WorkspaceCapabilities;

    /// Lifecycle state, with the sleep and loss states.
    fn workspace_status(&self) -> WorkspaceStatus {
        self.status().into()
    }

    /// Fencing epoch of this handle (0 for drivers without `attach`).
    fn epoch(&self) -> u64 {
        0
    }

    /// The Workspace root in the driver's own namespace. For local drivers
    /// this is the host project folder; for a guest-namespace driver it is
    /// a path that is not valid on the Runtime host.
    fn root(&self) -> PathBuf {
        self.env().cwd().to_path_buf()
    }

    /// `path` in the driver's namespace, for `env()` calls.
    fn resolve(&self, path: &WorkspacePath) -> PathBuf {
        path.to_native(&self.root())
    }

    /// Start a streaming process, with a PTY when `request.pty` is set.
    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<Box<dyn WorkspaceProcess>, WorkspaceError> {
        let _ = request;
        Err(WorkspaceError::unsupported(self.driver_id(), "spawn"))
    }

    /// Reopen process `id` and replay its buffered events after `after_seq`.
    async fn attach_process(
        &self,
        id: &ProcessId,
        after_seq: u64,
    ) -> Result<Box<dyn WorkspaceProcess>, WorkspaceError> {
        let _ = (id, after_seq);
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "attach_process",
        ))
    }

    /// Processes the driver knows (running, and ended ones still buffered).
    async fn list_processes(&self) -> Result<Vec<ProcessInfo>, WorkspaceError> {
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "list_processes",
        ))
    }

    /// Read at most `max_len` bytes of `path` from `offset`. With
    /// `if_match`, fail with `Changed` when the file has another tag.
    async fn read_chunk(
        &self,
        path: &WorkspacePath,
        offset: u64,
        max_len: usize,
        if_match: Option<&FileTag>,
    ) -> Result<FileChunk, WorkspaceError> {
        let _ = (path, offset, max_len, if_match);
        Err(WorkspaceError::unsupported(self.driver_id(), "read_chunk"))
    }

    /// Start an upload. The data goes to a temporary file in the
    /// destination folder.
    async fn begin_upload(&self, request: UploadRequest) -> Result<UploadId, WorkspaceError> {
        let _ = request;
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "begin_upload",
        ))
    }

    /// Write `data` at `offset`. `offset` must not be beyond the committed
    /// offset (a resend of bytes already stored is allowed). Returns the new
    /// committed offset.
    async fn upload_write(
        &self,
        upload: &UploadId,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, WorkspaceError> {
        let _ = (upload, offset, data);
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "upload_write",
        ))
    }

    /// Bytes stored so far, to resume after a lost connection.
    async fn upload_committed_offset(&self, upload: &UploadId) -> Result<u64, WorkspaceError> {
        let _ = upload;
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "upload_committed_offset",
        ))
    }

    /// Check size and checksum, then move the temporary file into place in
    /// one rename. Returns the tag of the new file.
    async fn upload_commit(&self, upload: &UploadId) -> Result<FileTag, WorkspaceError> {
        let _ = upload;
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "upload_commit",
        ))
    }

    /// Drop an upload and its temporary file. Not an error twice.
    async fn upload_abort(&self, upload: &UploadId) -> Result<(), WorkspaceError> {
        let _ = upload;
        Err(WorkspaceError::unsupported(
            self.driver_id(),
            "upload_abort",
        ))
    }

    /// Save the files (and, where the driver can, the running state).
    async fn snapshot(&self) -> Result<SnapshotId, WorkspaceError> {
        Err(WorkspaceError::unsupported(self.driver_id(), "snapshot"))
    }

    /// Save a snapshot and release compute. Running processes end (their
    /// exit is `Lost` unless the driver saves memory); files stay. `Busy`
    /// while a call is in flight.
    async fn hibernate(&self) -> Result<SnapshotId, WorkspaceError> {
        Err(WorkspaceError::unsupported(self.driver_id(), "hibernate"))
    }

    /// Restore from `snapshot` (or the last one when `None`) and become
    /// Ready.
    async fn wake(&self, snapshot: Option<SnapshotId>) -> Result<(), WorkspaceError> {
        let _ = snapshot;
        Err(WorkspaceError::unsupported(self.driver_id(), "wake"))
    }
}

/// Makes Workspaces of one kind. A driver crate outside this repository
/// implements this trait and registers it (see [`crate::register_workspace_driver`]).
#[async_trait]
pub trait WorkspaceDriver: Send + Sync {
    /// Unique driver id (`host`, `seatbelt`, `podman`, `remote`, …).
    fn id(&self) -> &str;

    /// The sandbox kind this driver serves: `local` for `sandbox.mode =
    /// local`, `off` for the host, or a new kind such as `remote`.
    fn kind(&self) -> &str;

    /// What Workspaces of this driver can do with default options.
    fn capabilities(&self) -> WorkspaceCapabilities;

    /// Whether the driver can run on `os`. Selection skips a driver that
    /// cannot. The default is every platform.
    fn supports(&self, os: HostOs) -> bool {
        let _ = os;
        true
    }

    /// Check that the external tools or the service the driver needs are
    /// there, without creating a Workspace. The default does nothing.
    async fn check(&self) -> Result<(), WorkspaceError> {
        Ok(())
    }

    /// Create a Workspace (not yet started).
    async fn create(&self, spec: WorkspaceSpec) -> Result<Arc<dyn Workspace>, WorkspaceError>;

    /// Reopen an existing Workspace by id, for example after a Runtime
    /// restart, with a new fencing epoch.
    async fn attach(&self, id: &str) -> Result<Attached, WorkspaceError> {
        let _ = id;
        Err(WorkspaceError::unsupported(self.id(), "attach"))
    }
}
