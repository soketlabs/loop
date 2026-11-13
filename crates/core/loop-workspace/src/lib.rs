//! The Workspace driver contract.
//!
//! A Workspace is the place where a Session's files and commands live: the
//! host folder, a local sandbox (Seatbelt, a Podman container) or, later, a
//! remote sandbox. This crate holds only the contract, so a driver crate
//! outside this repository can implement it without the agent, its stores,
//! its model clients or its user interface. See
//! `docs/design/workspace-driver.md`.

#![warn(missing_docs)]

mod env;
mod registry;
mod sandbox;

pub use env::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileIdentity,
    FileInfo, FileSystem, Shell, ShellExecOptions, ShellOutput, ShellOutputCallback,
};
pub use registry::SandboxRegistry;
pub use sandbox::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus,
};
