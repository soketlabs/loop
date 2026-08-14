//! Orchestration adapter: bridges `loop-orchestration` with `loop-agent` types.
//!
//! This module provides concrete worker implementations (`AgentWorker`,
//! `ShellWorker`, `SubWorkflowWorker`) that plug into the orchestration
//! scheduler, plus memory tool builders that wrap orchestration traits
//! into `AgentTool` instances.

pub mod agent_worker;
pub mod sub_workflow_worker;
pub mod tools;

pub use agent_worker::{AgentWorker, ShellWorker, create_spawn_task_tool};
pub use sub_workflow_worker::SubWorkflowWorker;
pub use tools::{create_memory_list_tool, create_memory_read_tool, create_memory_write_tool};
