//! Core workflow types: events, state, and identifiers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::planner::task_graph::{TaskGraph, TaskId, TaskStatus};

/// Unique workflow identifier.
pub type WorkflowId = String;

/// Sequence number in the event log.
pub type SeqNo = u64;

/// Result produced by a completed task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// Output data (tool results, messages, etc).
    pub output: Value,
    /// Artifacts produced (file paths, generated content keys).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    /// Serialized messages from the agent transcript (as JSON values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Value>,
}

impl TaskResult {
    /// Create an empty result with no output.
    pub fn empty() -> Self {
        Self {
            output: Value::Null,
            artifacts: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Create a result with the given output value.
    pub fn with_output(output: Value) -> Self {
        Self {
            output,
            artifacts: Vec::new(),
            messages: Vec::new(),
        }
    }
}

/// An artifact produced by a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Classification of this artifact.
    pub kind: ArtifactKind,
    /// Optional file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Artifact data payload.
    pub data: Value,
}

/// Artifact classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A file on disk.
    File,
    /// Generated source code.
    Code,
    /// Test execution result.
    TestResult,
    /// Log output.
    Log,
    /// Application-defined type.
    Custom(String),
}

/// Final result of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowResult {
    /// Whether the workflow succeeded.
    pub success: bool,
    /// Aggregate output data.
    pub output: Value,
    /// Per-task results for completed tasks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_results: Vec<(TaskId, TaskResult)>,
}

/// Scope for memory operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Workflow-level shared memory.
    Shared,
    /// Task-scoped memory.
    Task(TaskId),
}

/// Signal that can be sent to a workflow or specific task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Signal {
    /// User steering message injected into a task (serialized as JSON).
    UserSteer {
        /// The serialized message to inject.
        message: Value,
    },
    /// Output from one task forwarded to another.
    TaskOutput {
        /// Source task.
        from_task: TaskId,
        /// Forwarded data.
        data: Value,
    },
    /// Timer fired.
    Timer {
        /// Timer name.
        name: String,
    },
    /// Cancellation request.
    Cancel,
    /// Application-defined signal.
    Custom {
        /// Signal name.
        name: String,
        /// Signal payload.
        payload: Value,
    },
}

/// Events that make up the workflow's durable history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum WorkflowEvent {
    /// Workflow was created with its task graph.
    WorkflowStarted {
        /// Workflow identifier.
        workflow_id: WorkflowId,
        /// Initial plan.
        plan: TaskGraph,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A task was scheduled for execution.
    TaskScheduled {
        /// Task identifier.
        task_id: TaskId,
        /// Task dependencies.
        dependencies: Vec<TaskId>,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A task began executing on a worker.
    TaskStarted {
        /// Task identifier.
        task_id: TaskId,
        /// Worker that picked up the task.
        worker_id: String,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A task completed successfully.
    TaskCompleted {
        /// Task identifier.
        task_id: TaskId,
        /// Task result.
        result: TaskResult,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A task failed.
    TaskFailed {
        /// Task identifier.
        task_id: TaskId,
        /// Error description.
        error: String,
        /// Number of retries attempted.
        retry_count: u32,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A task was cancelled.
    TaskCancelled {
        /// Task identifier.
        task_id: TaskId,
        /// Cancellation reason.
        reason: String,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A signal was received.
    SignalReceived {
        /// Target task (None = workflow-level).
        task_id: Option<TaskId>,
        /// The signal.
        signal: Signal,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// Shared or task memory was updated.
    MemoryUpdated {
        /// Memory scope.
        scope: MemoryScope,
        /// Key that changed.
        key: String,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// A state checkpoint was created.
    CheckpointCreated {
        /// Snapshot identifier.
        snapshot_id: String,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// Workflow completed.
    WorkflowCompleted {
        /// Final result.
        result: WorkflowResult,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// Workflow was paused.
    WorkflowPaused {
        /// Reason for pausing.
        reason: String,
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
    /// Workflow was resumed from pause.
    WorkflowResumed {
        /// Unix epoch milliseconds.
        timestamp: i64,
    },
}

/// Reconstructed workflow state from event replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Workflow identifier.
    pub workflow_id: WorkflowId,
    /// Task graph.
    pub graph: TaskGraph,
    /// Current status of each task.
    pub task_statuses: HashMap<TaskId, TaskStatus>,
    /// Top-level workflow status.
    pub status: WorkflowStatus,
    /// Last applied event sequence number.
    pub last_seq: SeqNo,
}

impl WorkflowState {
    /// Create initial state from a workflow ID and graph.
    pub fn new(workflow_id: WorkflowId, graph: TaskGraph) -> Self {
        let task_statuses: HashMap<TaskId, TaskStatus> = graph
            .tasks
            .keys()
            .map(|id| (id.clone(), TaskStatus::Pending))
            .collect();
        Self {
            workflow_id,
            graph,
            task_statuses,
            status: WorkflowStatus::Running,
            last_seq: 0,
        }
    }

    /// Apply a single event to advance state.
    pub fn apply(&mut self, seq: SeqNo, event: &WorkflowEvent) {
        self.last_seq = seq;
        match event {
            WorkflowEvent::TaskScheduled { task_id, .. } => {
                self.task_statuses
                    .entry(task_id.clone())
                    .or_insert(TaskStatus::Pending);
            }
            WorkflowEvent::TaskStarted { task_id, .. } => {
                self.task_statuses
                    .insert(task_id.clone(), TaskStatus::Running);
            }
            WorkflowEvent::TaskCompleted { task_id, result, .. } => {
                self.task_statuses
                    .insert(task_id.clone(), TaskStatus::Completed(result.clone()));
            }
            WorkflowEvent::TaskFailed { task_id, error, .. } => {
                self.task_statuses
                    .insert(task_id.clone(), TaskStatus::Failed(error.clone()));
            }
            WorkflowEvent::TaskCancelled { task_id, reason, .. } => {
                self.task_statuses
                    .insert(task_id.clone(), TaskStatus::Cancelled(reason.clone()));
            }
            WorkflowEvent::WorkflowCompleted { .. } => {
                self.status = WorkflowStatus::Completed;
            }
            WorkflowEvent::WorkflowPaused { .. } => {
                self.status = WorkflowStatus::Paused;
            }
            WorkflowEvent::WorkflowResumed { .. } => {
                self.status = WorkflowStatus::Running;
            }
            _ => {}
        }
    }

    /// Get tasks whose dependencies are all completed and are still pending.
    pub fn ready_tasks(&self) -> Vec<TaskId> {
        self.graph
            .tasks
            .keys()
            .filter(|id| {
                matches!(
                    self.task_statuses.get(*id),
                    Some(TaskStatus::Pending) | None
                )
            })
            .filter(|id| {
                let deps = self.graph.dependencies_of(id);
                deps.iter().all(|dep| {
                    matches!(
                        self.task_statuses.get(dep),
                        Some(TaskStatus::Completed(_))
                    )
                })
            })
            .cloned()
            .collect()
    }

    /// Whether all tasks are in a terminal state.
    pub fn is_complete(&self) -> bool {
        self.task_statuses.values().all(|s| {
            matches!(
                s,
                TaskStatus::Completed(_) | TaskStatus::Failed(_) | TaskStatus::Cancelled(_)
            )
        })
    }

    /// Whether any task has failed without the workflow being completed.
    pub fn has_failures(&self) -> bool {
        self.task_statuses
            .values()
            .any(|s| matches!(s, TaskStatus::Failed(_)))
    }
}

/// Top-level workflow status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    /// Workflow is actively executing.
    Running,
    /// Workflow is paused.
    Paused,
    /// Workflow finished.
    Completed,
    /// Workflow failed.
    Failed,
}

/// Workflow-level error.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// Workflow not found.
    #[error("workflow not found: {0}")]
    NotFound(String),
    /// Event log storage error.
    #[error("event log error: {0}")]
    EventLog(String),
    /// Invalid workflow state.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// Other error.
    #[error("{0}")]
    Other(String),
}
