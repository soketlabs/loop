//! Harness capability types, errors, and resources.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

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

// The execution environment contract (files and shell) lives in the
// `loop-workspace` crate so a driver crate can implement it without this
// crate. These re-exports keep the old paths working.
pub use loop_workspace::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileIdentity,
    FileInfo, FileSystem, PathKind, PathMetadata, PolicyPath, RetryClass, Shell, ShellExecOptions,
    ShellOutput, ShellOutputCallback,
};

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
