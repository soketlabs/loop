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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::task_graph::*;

    fn make_graph() -> TaskGraph {
        let mut g = TaskGraph::new();
        g.add_task(TaskNode::new(
            "a",
            TaskKind::AgentTurn { prompt: "do a".into(), tools: None, model: None },
            "task a",
        ));
        g.add_task(TaskNode::new(
            "b",
            TaskKind::AgentTurn { prompt: "do b".into(), tools: None, model: None },
            "task b",
        ));
        g.add_task(TaskNode::new(
            "c",
            TaskKind::AgentTurn { prompt: "do c".into(), tools: None, model: None },
            "task c",
        ));
        g.add_dependency("b", "a");
        g.add_dependency("c", "b");
        g
    }

    #[test]
    fn initial_state_all_pending() {
        let g = make_graph();
        let state = WorkflowState::new("wf1".into(), g);
        assert_eq!(state.status, WorkflowStatus::Running);
        assert!(state.task_statuses.values().all(|s| matches!(s, TaskStatus::Pending)));
    }

    #[test]
    fn ready_tasks_returns_root_tasks() {
        let g = make_graph();
        let state = WorkflowState::new("wf1".into(), g);
        let ready = state.ready_tasks();
        assert_eq!(ready, vec!["a"]);
    }

    #[test]
    fn completing_task_unlocks_dependents() {
        let g = make_graph();
        let mut state = WorkflowState::new("wf1".into(), g);

        state.apply(1, &WorkflowEvent::TaskStarted {
            task_id: "a".into(), worker_id: "w1".into(), timestamp: 0,
        });
        assert!(state.ready_tasks().is_empty());

        state.apply(2, &WorkflowEvent::TaskCompleted {
            task_id: "a".into(), result: TaskResult::empty(), timestamp: 1,
        });
        let ready = state.ready_tasks();
        assert_eq!(ready, vec!["b"]);
    }

    #[test]
    fn is_complete_when_all_terminal() {
        let g = make_graph();
        let mut state = WorkflowState::new("wf1".into(), g);
        assert!(!state.is_complete());

        state.apply(1, &WorkflowEvent::TaskCompleted {
            task_id: "a".into(), result: TaskResult::empty(), timestamp: 0,
        });
        state.apply(2, &WorkflowEvent::TaskCompleted {
            task_id: "b".into(), result: TaskResult::empty(), timestamp: 1,
        });
        assert!(!state.is_complete());

        state.apply(3, &WorkflowEvent::TaskCompleted {
            task_id: "c".into(), result: TaskResult::empty(), timestamp: 2,
        });
        assert!(state.is_complete());
    }

    #[test]
    fn has_failures_when_task_fails() {
        let g = make_graph();
        let mut state = WorkflowState::new("wf1".into(), g);
        assert!(!state.has_failures());

        state.apply(1, &WorkflowEvent::TaskFailed {
            task_id: "a".into(), error: "boom".into(), retry_count: 0, timestamp: 0,
        });
        assert!(state.has_failures());
    }

    #[test]
    fn pause_and_resume() {
        let g = make_graph();
        let mut state = WorkflowState::new("wf1".into(), g);

        state.apply(1, &WorkflowEvent::WorkflowPaused {
            reason: "user requested".into(), timestamp: 0,
        });
        assert_eq!(state.status, WorkflowStatus::Paused);

        state.apply(2, &WorkflowEvent::WorkflowResumed { timestamp: 1 });
        assert_eq!(state.status, WorkflowStatus::Running);
    }

    #[test]
    fn workflow_completed_event() {
        let g = make_graph();
        let mut state = WorkflowState::new("wf1".into(), g);

        state.apply(1, &WorkflowEvent::WorkflowCompleted {
            result: WorkflowResult {
                success: true,
                output: serde_json::Value::Null,
                task_results: Vec::new(),
            },
            timestamp: 0,
        });
        assert_eq!(state.status, WorkflowStatus::Completed);
    }

    #[test]
    fn cancelled_tasks_are_terminal() {
        let mut g = TaskGraph::new();
        g.add_task(TaskNode::new(
            "a",
            TaskKind::AgentTurn { prompt: "x".into(), tools: None, model: None },
            "a",
        ));
        let mut state = WorkflowState::new("wf1".into(), g);

        state.apply(1, &WorkflowEvent::TaskCancelled {
            task_id: "a".into(), reason: "timeout".into(), timestamp: 0,
        });
        assert!(state.is_complete());
    }

    #[test]
    fn parallel_tasks_all_ready() {
        let mut g = TaskGraph::new();
        g.add_task(TaskNode::new("a", TaskKind::Barrier, "sync"));
        g.add_task(TaskNode::new("b", TaskKind::Barrier, "sync"));
        g.add_task(TaskNode::new("c", TaskKind::Barrier, "sync"));
        let state = WorkflowState::new("wf1".into(), g);
        let mut ready = state.ready_tasks();
        ready.sort();
        assert_eq!(ready, vec!["a", "b", "c"]);
    }

    #[test]
    fn task_result_serde_roundtrip() {
        let result = TaskResult {
            output: serde_json::json!({"key": "value"}),
            artifacts: vec![Artifact {
                kind: ArtifactKind::File,
                path: Some("/tmp/out.txt".into()),
                data: serde_json::json!("contents"),
            }],
            messages: vec![serde_json::json!({"role": "assistant"})],
        };
        let json = serde_json::to_string(&result).unwrap();
        let r2: TaskResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r2.output, result.output);
        assert_eq!(r2.artifacts.len(), 1);
    }
}
