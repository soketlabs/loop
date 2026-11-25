//! Multi-agent orchestration: durable workflows, task planning, scheduling, and shared memory.
//!
//! This crate is designed to be used as an optional plugin for `loop-agent`.
//! It defines abstract traits (`AgentRunner`, `ShellRunner`) at integration
//! boundaries so that the host crate can provide concrete implementations
//! without creating circular dependencies.

#![deny(missing_docs)]

pub mod memory;
pub mod planner;
pub mod scheduler;
pub mod workflow;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use loop_ai::{AssistantMessageEventStream, Context, Model, SimpleStreamOptions};

/// LLM streaming function type, mirroring the definition in `loop-agent`.
pub type StreamFn = Arc<
    dyn Fn(
            Model,
            Context,
            SimpleStreamOptions,
        ) -> Pin<Box<dyn Future<Output = AssistantMessageEventStream> + Send>>
        + Send
        + Sync,
>;

/// Result returned by an agent run, decoupled from concrete message types.
pub struct AgentRunResult {
    /// Serialized output value.
    pub output: Value,
    /// Serialized agent messages (as JSON values).
    pub messages: Vec<Value>,
}

/// Trait for executing an agent turn. Implemented by the host crate to
/// wire `run_agent_loop` into the orchestration scheduler.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Run one agent turn for `prompt` under `system_prompt`.
    ///
    /// `dependency_context` holds the outputs of the tasks this one depends on,
    /// as `(task id, output)` pairs. The implementation should stop when
    /// `cancel` fires. Returns the output and messages of the run, or an error
    /// message.
    async fn run(
        &self,
        prompt: String,
        system_prompt: String,
        dependency_context: Vec<(String, Value)>,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, String>;
}

/// Result from a shell command execution.
pub struct ShellRunResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
}

/// Trait for executing shell commands. Implemented by the host crate to
/// wire `ExecutionEnv` into the orchestration scheduler.
#[async_trait]
pub trait ShellRunner: Send + Sync {
    /// Run `command` in a shell. The implementation should stop it when
    /// `cancel` fires. Returns the captured output and exit code, or an error
    /// message when the command could not be run.
    async fn exec(
        &self,
        command: &str,
        cancel: CancellationToken,
    ) -> Result<ShellRunResult, String>;
}

// ── Re-exports ──────────────────────────────────────────────────────

pub use memory::{MemoryBus, SharedMemory, TaskMemory};
pub use planner::{
    LlmPlanner, ManualPlanner, Planner, PlannerContext, PlannerError, TaskGraph, TaskKind, TaskNode,
};
pub use scheduler::{Scheduler, SchedulerConfig, Worker, WorkerContext, WorkerError, WorkerPool};
pub use workflow::{
    EventLog, MemoryEventLog, SignalRouter, WorkflowEngine, WorkflowEvent, WorkflowId,
    WorkflowResult, WorkflowState,
};
