//! Sandbox trait surface for N local/remote backends.
//!
//! The lifecycle traits and types live in the `loop-workspace` contract
//! crate, so a driver crate outside this repository can implement them
//! without this crate. They are re-exported here under their old paths.

use std::sync::Arc;

pub use loop_workspace::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus, Workspace,
};

/// Harness sandbox mode.
#[derive(Clone, Default)]
pub enum SandboxMode {
    /// Use host ExecutionEnv for tools.
    #[default]
    Disabled,
    /// Use the given sandbox env for tools.
    Enabled {
        /// Sandbox instance. A [`Workspace`] is a [`Sandbox`], so callers
        /// that need only the lifecycle use it as before; `spawn`, transfer
        /// and capabilities stay reachable.
        sandbox: Arc<dyn Workspace>,
    },
}
