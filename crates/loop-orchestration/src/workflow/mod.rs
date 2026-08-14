//! Workflow layer: durable event-sourced workflow runtime.
//!
//! Inspired by Temporal's concepts: event history, replay, signals, timers,
//! and resumable execution -- but not AI-specific.

pub mod checkpoint;
pub mod engine;
pub mod event_log;
pub mod signals;
pub mod sqlite_event_log;
pub mod types;

pub use engine::WorkflowEngine;
pub use event_log::{EventLog, MemoryEventLog};
pub use signals::SignalRouter;
pub use types::{
    Artifact, ArtifactKind, MemoryScope, Signal, TaskResult, WorkflowError, WorkflowEvent,
    WorkflowId, WorkflowResult, WorkflowState, WorkflowStatus,
};

#[cfg(feature = "sqlite")]
pub use sqlite_event_log::SqliteEventLog;
