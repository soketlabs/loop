//! Sandbox trait surface for N local/remote backends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::harness::types::ExecutionEnv;

/// Lifecycle status of a sandbox instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// Created but not started.
    Created,
    /// Starting.
    Starting,
    /// Ready for tool execution.
    Ready,
    /// Stopping.
    Stopping,
    /// Stopped.
    Stopped,
    /// Failed.
    Failed,
}

/// Sandbox error.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// Not ready.
    #[error("sandbox not ready: {0}")]
    NotReady(String),
    /// Start failed.
    #[error("start failed: {0}")]
    StartFailed(String),
    /// Path escape / policy.
    #[error("policy: {0}")]
    Policy(String),
    /// Other.
    #[error("{0}")]
    Other(String),
}

/// Configuration for creating a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// Logical workspace root inside the sandbox.
    pub workdir: PathBuf,
    /// Opaque backend options.
    #[serde(default)]
    pub options: Value,
    /// Labels for observability.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// A sandbox yields an [`ExecutionEnv`] for tool calls when enabled.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Stable kind id (`local`, `remote`, …).
    fn kind(&self) -> &str;
    /// Instance id.
    fn id(&self) -> &str;
    /// Current status.
    fn status(&self) -> SandboxStatus;
    /// Underlying env tools must use when status is Ready.
    fn env(&self) -> Arc<dyn ExecutionEnv>;
    /// Start the sandbox.
    async fn start(&self) -> Result<(), SandboxError>;
    /// Stop the sandbox.
    async fn stop(&self) -> Result<(), SandboxError>;
    /// Destroy / cleanup (idempotent).
    async fn destroy(&self) -> Result<(), SandboxError>;
}

/// Factory for a sandbox kind.
#[async_trait]
pub trait SandboxFactory: Send + Sync {
    /// Kind id.
    fn kind(&self) -> &str;
    /// Create an instance.
    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError>;
}

/// Harness sandbox mode.
#[derive(Clone, Default)]
pub enum SandboxMode {
    /// Use host ExecutionEnv for tools.
    #[default]
    Disabled,
    /// Use the given sandbox env for tools.
    Enabled {
        /// Sandbox instance.
        sandbox: Arc<dyn Sandbox>,
    },
}
