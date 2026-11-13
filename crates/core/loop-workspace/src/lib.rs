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
pub use capabilities::{Isolation, NetworkPolicy, ResourceLimits, WorkspaceCapabilities};
pub use error::WorkspaceError;
pub use path::WorkspacePath;
pub use platform::HostOs;
pub use process::{
    ProcessEvent, ProcessExit, ProcessSignal, PtySize, SpawnRequest, WorkspaceProcess,
};
pub use registry::{
    register_workspace_driver, registered_workspace_drivers, SandboxRegistry,
    WorkspaceDriverRegistry,
};
pub use workspace::{
    FileChunk, SnapshotId, Workspace, WorkspaceDriver, WorkspaceSpec, WorkspaceStatus,
};

pub use env::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileIdentity,
    FileInfo, FileSystem, Shell, ShellExecOptions, ShellOutput, ShellOutputCallback,
};
pub use sandbox::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus,
};
