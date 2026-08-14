//! Workflow checkpointing: periodic state snapshots for fast replay.

use serde::{Deserialize, Serialize};

use super::types::{SeqNo, WorkflowState, WorkflowId};

/// A serialized checkpoint of workflow state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Workflow this checkpoint belongs to.
    pub workflow_id: WorkflowId,
    /// Unique checkpoint identifier.
    pub snapshot_id: String,
    /// Sequence number this checkpoint was taken at.
    pub at_seq: SeqNo,
    /// Serialized workflow state.
    pub state_json: String,
    /// Unix epoch milliseconds when created.
    pub created_at: i64,
}

impl Checkpoint {
    /// Create a checkpoint from the current workflow state.
    pub fn from_state(state: &WorkflowState) -> Result<Self, String> {
        let state_json = serde_json::to_string(state)
            .map_err(|e| format!("failed to serialize state: {e}"))?;
        Ok(Self {
            workflow_id: state.workflow_id.clone(),
            snapshot_id: format!("cp_{}", uuid::Uuid::now_v7()),
            at_seq: state.last_seq,
            state_json,
            created_at: loop_ai::now_ms(),
        })
    }
}

/// Serializable form of WorkflowState (for checkpoint storage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableWorkflowState {
    /// Workflow identifier.
    pub workflow_id: WorkflowId,
    /// Task statuses serialized.
    pub task_statuses_json: String,
    /// Graph serialized.
    pub graph_json: String,
    /// Status string.
    pub status: String,
    /// Last seq.
    pub last_seq: SeqNo,
}
