//! Multi-agent orchestration: durable workflows, task planning, scheduling, and shared memory.
//!
//! This crate is designed to be used as an optional plugin for `loop-agent`.
//! Integration points with the host crate are abstract traits (such as
//! `Worker` and `Planner`) so that the host can provide concrete
//! implementations without creating circular dependencies.

#![deny(missing_docs)]

pub mod memory;
pub mod planner;
pub mod scheduler;
pub mod workflow;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
