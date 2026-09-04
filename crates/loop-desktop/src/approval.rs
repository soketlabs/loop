//! Tool approval state bridged from `ToolApprovalBridge`.

use loop_app_core::tool_approval::{ApprovalDecision, ApprovalKind, ApprovalPrompt};

/// Display-only approval prompt in the UI snapshot.
#[derive(Debug, Clone)]
pub struct ApprovalUiPrompt {
    pub kind: ApprovalKind,
    pub tool_name: String,
    pub summary: String,
    pub detail: String,
}

impl ApprovalUiPrompt {
    pub fn from_prompt(prompt: &ApprovalPrompt) -> Self {
        Self {
            kind: prompt.kind,
            tool_name: prompt.tool_name.clone(),
            summary: prompt.summary.clone(),
            detail: prompt.detail.clone(),
        }
    }
}

/// Pending approval holding the response channel (not cloneable).
pub struct PendingApproval {
    pub prompt: ApprovalPrompt,
}

impl PendingApproval {
    pub fn respond(self, decision: ApprovalDecision) {
        let _ = self.prompt.response_tx.send(decision);
    }
}
