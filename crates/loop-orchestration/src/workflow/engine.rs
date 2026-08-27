//! Workflow engine: state machine driven by an event log.

use std::sync::Arc;

use loop_ai::now_ms;
use tokio::sync::Notify;

use super::event_log::EventLog;
use super::signals::SignalRouter;
use super::types::{
    Signal, TaskResult, WorkflowError, WorkflowEvent, WorkflowId, WorkflowResult, WorkflowState,
    WorkflowStatus,
};
use crate::planner::task_graph::{TaskGraph, TaskId, TaskNode, TaskStatus};

/// The workflow engine manages workflow lifecycle via event sourcing.
pub struct WorkflowEngine {
    event_log: Arc<dyn EventLog>,
    signal_router: Arc<SignalRouter>,
    progress_notify: Arc<Notify>,
}

impl WorkflowEngine {
    /// Create a new workflow engine backed by the given event log.
    pub fn new(event_log: Arc<dyn EventLog>) -> Self {
        Self {
            event_log,
            signal_router: Arc::new(SignalRouter::new()),
            progress_notify: Arc::new(Notify::new()),
        }
    }

    /// Set the signal router (for sharing across scheduler and engine).
    pub fn with_signal_router(mut self, router: Arc<SignalRouter>) -> Self {
        self.signal_router = router;
        self
    }

    /// Access the signal router.
    pub fn signal_router(&self) -> &Arc<SignalRouter> {
        &self.signal_router
    }

    /// Get the progress notification handle.
    pub fn progress_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.progress_notify)
    }

    /// Start a new workflow from a task graph.
    pub async fn start_workflow(
        &self,
        workflow_id: WorkflowId,
        graph: TaskGraph,
    ) -> Result<WorkflowState, WorkflowError> {
        graph.validate().map_err(WorkflowError::InvalidState)?;

        let event = WorkflowEvent::WorkflowStarted {
            workflow_id: workflow_id.clone(),
            plan: graph.clone(),
            timestamp: now_ms(),
        };
        self.event_log.append(&workflow_id, event).await?;

        for task_id in graph.tasks.keys() {
            let deps = graph.dependencies_of(task_id);
            let event = WorkflowEvent::TaskScheduled {
                task_id: task_id.clone(),
                dependencies: deps,
                timestamp: now_ms(),
            };
            self.event_log.append(&workflow_id, event).await?;
        }

        self.event_log.replay(&workflow_id).await
    }

    /// Record that a task has started execution.
    pub async fn task_started(
        &self,
        workflow_id: &str,
        task_id: &str,
        worker_id: &str,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::TaskStarted {
            task_id: task_id.to_string(),
            worker_id: worker_id.to_string(),
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Record that a task completed successfully.
    pub async fn task_completed(
        &self,
        workflow_id: &str,
        task_id: &str,
        result: TaskResult,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::TaskCompleted {
            task_id: task_id.to_string(),
            result,
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.signal_router.unregister_task(task_id).await;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Record that a task failed.
    pub async fn task_failed(
        &self,
        workflow_id: &str,
        task_id: &str,
        error: String,
        retry_count: u32,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::TaskFailed {
            task_id: task_id.to_string(),
            error,
            retry_count,
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.signal_router.unregister_task(task_id).await;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Record that a task was cancelled.
    pub async fn task_cancelled(
        &self,
        workflow_id: &str,
        task_id: &str,
        reason: String,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::TaskCancelled {
            task_id: task_id.to_string(),
            reason,
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.signal_router.unregister_task(task_id).await;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Mark the workflow as completed.
    pub async fn complete_workflow(
        &self,
        workflow_id: &str,
        result: WorkflowResult,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::WorkflowCompleted {
            result,
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Pause a workflow.
    pub async fn pause_workflow(
        &self,
        workflow_id: &str,
        reason: String,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::WorkflowPaused {
            reason,
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Resume a paused workflow.
    pub async fn resume_workflow(&self, workflow_id: &str) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::WorkflowResumed {
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Send a signal to the workflow or a specific task.
    pub async fn send_signal(
        &self,
        workflow_id: &str,
        task_id: Option<TaskId>,
        signal: Signal,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::SignalReceived {
            task_id: task_id.clone(),
            signal: signal.clone(),
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;

        if let Some(tid) = &task_id {
            self.signal_router.send_to_task(tid, signal).await;
        } else {
            self.signal_router.broadcast(signal);
        }
        self.progress_notify.notify_waiters();
        Ok(())
    }

    /// Get the current workflow state by replaying the event log.
    pub async fn state(&self, workflow_id: &str) -> Result<WorkflowState, WorkflowError> {
        self.event_log.replay(workflow_id).await
    }

    /// Get tasks that are ready to execute.
    pub async fn ready_tasks(&self, workflow_id: &str) -> Result<Vec<TaskId>, WorkflowError> {
        let state = self.state(workflow_id).await?;
        if state.status == WorkflowStatus::Paused {
            return Ok(Vec::new());
        }
        Ok(state.ready_tasks())
    }

    /// Check if the workflow has finished (all tasks terminal).
    pub async fn is_complete(&self, workflow_id: &str) -> Result<bool, WorkflowError> {
        let state = self.state(workflow_id).await?;
        Ok(state.is_complete() || state.status == WorkflowStatus::Completed)
    }

    /// Wait until there is progress (new events appended).
    pub async fn wait_for_progress(&self) {
        self.progress_notify.notified().await;
    }

    /// Get the final workflow result (only valid after completion).
    pub async fn result(&self, workflow_id: &str) -> Result<WorkflowResult, WorkflowError> {
        let state = self.state(workflow_id).await?;
        let task_results: Vec<(TaskId, TaskResult)> = state
            .task_statuses
            .iter()
            .filter_map(|(id, status)| {
                if let TaskStatus::Completed(r) = status {
                    Some((id.clone(), r.clone()))
                } else {
                    None
                }
            })
            .collect();

        let success = !state.has_failures();
        Ok(WorkflowResult {
            success,
            output: serde_json::Value::Null,
            task_results,
        })
    }

    /// Dynamically add a new task to a running workflow.
    pub async fn add_task(
        &self,
        workflow_id: &str,
        _task: TaskNode,
        dependencies: Vec<TaskId>,
    ) -> Result<(), WorkflowError> {
        let event = WorkflowEvent::TaskScheduled {
            task_id: _task.id.clone(),
            dependencies,
            timestamp: now_ms(),
        };
        self.event_log.append(workflow_id, event).await?;
        self.progress_notify.notify_waiters();
        Ok(())
    }
}
