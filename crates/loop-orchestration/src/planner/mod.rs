//! Planner layer: task decomposition and graph generation.
//!
//! Inspired by Pi's approach to planning: decompose high-level goals into
//! a directed acyclic graph of concrete tasks.

pub mod llm_planner;
pub mod manual_planner;
pub mod task_graph;

use async_trait::async_trait;
use serde_json::Value;

pub use llm_planner::LlmPlanner;
pub use manual_planner::ManualPlanner;
pub use task_graph::{TaskConfig, TaskEdge, TaskGraph, TaskId, TaskKind, TaskNode, TaskStatus};

use crate::workflow::types::TaskResult;

/// Context available to the planner when decomposing or replanning.
#[derive(Debug, Clone)]
pub struct PlannerContext {
    /// Working directory for the workflow.
    pub cwd: Option<String>,
    /// Available tool names.
    pub available_tools: Vec<String>,
    /// Available model identifiers.
    pub available_models: Vec<String>,
    /// Additional context (file contents, prior results, etc).
    pub extra: Value,
}

impl Default for PlannerContext {
    fn default() -> Self {
        Self {
            cwd: None,
            available_tools: Vec::new(),
            available_models: Vec::new(),
            extra: Value::Null,
        }
    }
}

/// Error from the planner.
#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    /// Plan decomposition failed.
    #[error("decomposition failed: {0}")]
    DecompositionFailed(String),
    /// Generated plan is invalid.
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
    /// Other error.
    #[error("{0}")]
    Other(String),
}

/// Trait for plan generation: decomposing goals into task graphs.
#[async_trait]
pub trait Planner: Send + Sync {
    /// Decompose a high-level goal into a task graph.
    async fn decompose(
        &self,
        goal: &str,
        context: &PlannerContext,
    ) -> Result<TaskGraph, PlannerError>;

    /// Replan after a task failure or unexpected result.
    async fn replan(
        &self,
        current_graph: &TaskGraph,
        failed_task: &TaskId,
        feedback: &TaskResult,
        context: &PlannerContext,
    ) -> Result<TaskGraph, PlannerError>;
}
