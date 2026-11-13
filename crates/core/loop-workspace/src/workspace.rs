//! The `Workspace` and `WorkspaceDriver` traits.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capabilities::WorkspaceCapabilities;
use crate::error::WorkspaceError;
use crate::path::WorkspacePath;
use crate::platform::HostOs;
use crate::process::{SpawnRequest, WorkspaceProcess};
use crate::sandbox::{Sandbox, SandboxConfig, SandboxStatus};

/// What a driver needs to create a Workspace. Today this is the sandbox
/// config: the project folder, backend options and labels.
pub type WorkspaceSpec = SandboxConfig;

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
}

/// A Workspace: where one Session's files and commands live.
///
/// It extends [`Sandbox`] (identity, status, `env()`, start, stop, destroy),
/// so every `Arc<dyn Workspace>` is also an `Arc<dyn Sandbox>` for the
/// harness. The methods below have defaults: a driver that does not
/// implement an operation returns `WorkspaceError::Unsupported` and says so
/// in [`Workspace::capabilities`].
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

    /// The Workspace root in the driver's own namespace. For local drivers
    /// this is the host project folder; for a remote driver it is a guest
    /// path that is not valid on the Runtime host.
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

    /// Read at most `max_len` bytes of `path` from `offset`.
    async fn read_chunk(
        &self,
        path: &WorkspacePath,
        offset: u64,
        max_len: usize,
    ) -> Result<FileChunk, WorkspaceError> {
        let _ = (path, offset, max_len);
        Err(WorkspaceError::unsupported(self.driver_id(), "read_chunk"))
    }

    /// Write `data` at `offset` of an upload to `path`. Offset 0 starts a
    /// new upload. The driver keeps the upload in a temporary file and moves
    /// it to `path` when `last` is true, so no reader sees half a file.
    async fn write_chunk(
        &self,
        path: &WorkspacePath,
        offset: u64,
        data: &[u8],
        last: bool,
    ) -> Result<(), WorkspaceError> {
        let _ = (path, offset, data, last);
        Err(WorkspaceError::unsupported(self.driver_id(), "write_chunk"))
    }

    /// Save the files (and, where the driver can, the running state).
    async fn snapshot(&self) -> Result<SnapshotId, WorkspaceError> {
        Err(WorkspaceError::unsupported(self.driver_id(), "snapshot"))
    }

    /// Save a snapshot and release compute. Running commands are stopped
    /// (see the design note); files stay.
    async fn hibernate(&self) -> Result<SnapshotId, WorkspaceError> {
        Err(WorkspaceError::unsupported(self.driver_id(), "hibernate"))
    }

    /// Restore from the last snapshot with the same files and become Ready.
    async fn wake(&self) -> Result<(), WorkspaceError> {
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
    /// restart.
    async fn attach(&self, id: &str) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let _ = id;
        Err(WorkspaceError::unsupported(self.id(), "attach"))
    }
}
