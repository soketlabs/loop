//! The Workspace driver contract.
//!
//! A Workspace is the place where a Session's files and commands live: the
//! host folder, a local sandbox (Seatbelt, a Podman container) or, later, a
//! remote sandbox. This crate holds only the contract, so a driver crate
//! outside this repository can implement it without the agent, its stores,
//! its model clients or its user interface. See
//! `docs/design/workspace-driver.md`.

#![warn(missing_docs)]

mod adapter;
mod capabilities;
mod env;
mod error;
mod path;
mod platform;
mod process;
mod registry;
mod sandbox;
mod workspace;

pub use adapter::{CapabilitiesFn, FactoryDriver, SandboxWorkspace};
pub use capabilities::{
    Isolation, NetworkPolicy, PathNamespace, ResourceLimits, WorkspaceCapabilities,
};
pub use env::{
    lexical, ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode,
    FileIdentity, FileInfo, FileSystem, PathKind, PathMetadata, PolicyPath, RetryClass, Shell,
    ShellExecOptions, ShellOutput, ShellOutputCallback,
};
pub use error::WorkspaceError;
pub use path::WorkspacePath;
pub use platform::HostOs;
pub use process::{
    output_buffer_bytes, ExitState, ProcessEvent, ProcessEventKind, ProcessId, ProcessInfo,
    ProcessSignal, PtySize, SpawnRequest, StreamId, WorkspaceProcess, DEFAULT_OUTPUT_BUFFER_BYTES,
    MAX_OUTPUT_BUFFER_BYTES, MIN_OUTPUT_BUFFER_BYTES,
};
pub use registry::{
    register_workspace_driver, registered_workspace_drivers, replace_builtin_workspace_driver,
    reset_registered_workspace_drivers, SandboxRegistry, WorkspaceDriverRegistry,
};
pub use sandbox::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus,
};
pub use workspace::{
    Attached, FileChunk, FileTag, SnapshotId, UploadId, UploadRequest, Workspace, WorkspaceDriver,
    WorkspaceSource, WorkspaceSpec, WorkspaceStatus,
};
