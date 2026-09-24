//! Event log: append-only durable record of workflow events.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify};

use super::types::{SeqNo, WorkflowError, WorkflowEvent, WorkflowId, WorkflowState};
use crate::planner::task_graph::TaskGraph;

/// Durable event log for workflow history and replay.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Append an event and return its sequence number.
    async fn append(
        &self,
        workflow_id: &str,
        event: WorkflowEvent,
    ) -> Result<SeqNo, WorkflowError>;

    /// Read events after a given sequence number.
    async fn read(
        &self,
        workflow_id: &str,
        after_seq: SeqNo,
    ) -> Result<Vec<(SeqNo, WorkflowEvent)>, WorkflowError>;

    /// Replay the full event log to reconstruct workflow state.
    async fn replay(&self, workflow_id: &str) -> Result<WorkflowState, WorkflowError>;
}

/// In-memory event log for development and testing.
pub struct MemoryEventLog {
    logs: Mutex<HashMap<WorkflowId, Vec<(SeqNo, WorkflowEvent)>>>,
    graphs: Mutex<HashMap<WorkflowId, TaskGraph>>,
    notify: Arc<Notify>,
}

impl MemoryEventLog {
    /// Create a new empty in-memory event log.
    pub fn new() -> Self {
        Self {
            logs: Mutex::new(HashMap::new()),
            graphs: Mutex::new(HashMap::new()),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Get a Notify handle to await new events.
    pub fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }
}

impl Default for MemoryEventLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventLog for MemoryEventLog {
    async fn append(
        &self,
        workflow_id: &str,
        event: WorkflowEvent,
    ) -> Result<SeqNo, WorkflowError> {
        let mut logs = self.logs.lock().await;
        let entries = logs.entry(workflow_id.to_string()).or_default();
        let seq = entries.len() as SeqNo + 1;

        if let WorkflowEvent::WorkflowStarted { plan, .. } = &event {
            self.graphs
                .lock()
                .await
                .insert(workflow_id.to_string(), plan.clone());
        }

        entries.push((seq, event));
        self.notify.notify_waiters();
        Ok(seq)
    }

    async fn read(
        &self,
        workflow_id: &str,
        after_seq: SeqNo,
    ) -> Result<Vec<(SeqNo, WorkflowEvent)>, WorkflowError> {
        let logs = self.logs.lock().await;
        let entries = logs
            .get(workflow_id)
            .ok_or_else(|| WorkflowError::NotFound(workflow_id.to_string()))?;
        Ok(entries
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .cloned()
            .collect())
    }

    async fn replay(&self, workflow_id: &str) -> Result<WorkflowState, WorkflowError> {
        let logs = self.logs.lock().await;
        let entries = logs
            .get(workflow_id)
            .ok_or_else(|| WorkflowError::NotFound(workflow_id.to_string()))?;

        let graph = self
            .graphs
            .lock()
            .await
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| {
                WorkflowError::InvalidState("no graph found for workflow".to_string())
            })?;

        let mut state = WorkflowState::new(workflow_id.to_string(), graph);
        for (seq, event) in entries {
            state.apply(*seq, event);
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::task_graph::*;

    fn test_graph() -> TaskGraph {
        let mut g = TaskGraph::new();
        g.add_task(TaskNode::new(
            "t1",
            TaskKind::AgentTurn { prompt: "hello".into(), tools: None, model: None },
            "task 1",
        ));
        g.add_task(TaskNode::new(
            "t2",
            TaskKind::ShellCommand { command: "echo done".into() },
            "task 2",
        ));
        g.add_dependency("t2", "t1");
        g
    }

    #[tokio::test]
    async fn append_and_read() {
        let log = MemoryEventLog::new();
        let graph = test_graph();

        let seq1 = log.append("wf1", WorkflowEvent::WorkflowStarted {
            workflow_id: "wf1".into(),
            plan: graph,
            timestamp: 100,
        }).await.unwrap();
        assert_eq!(seq1, 1);

        let seq2 = log.append("wf1", WorkflowEvent::TaskScheduled {
            task_id: "t1".into(),
            dependencies: vec![],
            timestamp: 101,
        }).await.unwrap();
        assert_eq!(seq2, 2);

        let events = log.read("wf1", 0).await.unwrap();
        assert_eq!(events.len(), 2);

        let events_after_1 = log.read("wf1", 1).await.unwrap();
        assert_eq!(events_after_1.len(), 1);
    }

    #[tokio::test]
    async fn replay_reconstructs_state() {
        let log = MemoryEventLog::new();
        let graph = test_graph();

        log.append("wf1", WorkflowEvent::WorkflowStarted {
            workflow_id: "wf1".into(),
            plan: graph,
            timestamp: 100,
        }).await.unwrap();

        log.append("wf1", WorkflowEvent::TaskScheduled {
            task_id: "t1".into(),
            dependencies: vec![],
            timestamp: 101,
        }).await.unwrap();

        log.append("wf1", WorkflowEvent::TaskCompleted {
            task_id: "t1".into(),
            result: crate::workflow::types::TaskResult::empty(),
            timestamp: 102,
        }).await.unwrap();

        let state = log.replay("wf1").await.unwrap();
        assert_eq!(state.workflow_id, "wf1");
        assert!(matches!(
            state.task_statuses.get("t1"),
            Some(TaskStatus::Completed(_))
        ));
        assert!(matches!(
            state.task_statuses.get("t2"),
            Some(TaskStatus::Pending)
        ));
    }

    #[tokio::test]
    async fn read_unknown_workflow_is_error() {
        let log = MemoryEventLog::new();
        let result = log.read("ghost", 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn replay_unknown_workflow_is_error() {
        let log = MemoryEventLog::new();
        let result = log.replay("ghost").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn separate_workflows_are_isolated() {
        let log = MemoryEventLog::new();

        let g1 = test_graph();
        let mut g2 = TaskGraph::new();
        g2.add_task(TaskNode::new("x", TaskKind::Barrier, "barrier"));

        log.append("wf1", WorkflowEvent::WorkflowStarted {
            workflow_id: "wf1".into(), plan: g1, timestamp: 0,
        }).await.unwrap();

        log.append("wf2", WorkflowEvent::WorkflowStarted {
            workflow_id: "wf2".into(), plan: g2, timestamp: 0,
        }).await.unwrap();

        let s1 = log.replay("wf1").await.unwrap();
        let s2 = log.replay("wf2").await.unwrap();
        assert_eq!(s1.graph.tasks.len(), 2);
        assert_eq!(s2.graph.tasks.len(), 1);
    }
}
