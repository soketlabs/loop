//! The Session tree types that ADR-0007 §8 moves from `loop-agent` into this
//! crate.
//!
//! Defined for step B5. Nothing uses them yet. `loop-agent` still has its own
//! `SessionTreeEntry` and `SessionMetadata`. These copies have the same serde
//! form. Types that belong to `loop-agent` or `loop-ai` are kept as JSON:
//! the message is a `serde_json::Value` and the thinking level is its string
//! form (`off`, `minimal`, ..., `xhigh`, `max`). `loop-agent` converts them.
//! The test kit checks that both forms stay equal.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Session metadata, as the projection serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    /// Session id.
    pub id: String,
    /// Working directory label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Parent session id when forked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Created at unix ms.
    pub created_at: i64,
    /// On-disk path when stored in JSONL (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// One entry of a Session tree. The payload of a `tree` log record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionTreeEntry {
    /// Transcript message.
    Message {
        /// Entry id.
        id: String,
        /// Parent entry id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp (unix ms).
        timestamp: i64,
        /// The `loop-agent` `AgentMessage`, as JSON.
        message: Value,
    },
    /// Thinking level change.
    ThinkingLevelChange {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// New level, in its serde form.
        thinking_level: String,
    },
    /// Model change.
    ModelChange {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Provider.
        provider: String,
        /// Model id.
        model_id: String,
    },
    /// Active tools change.
    ActiveToolsChange {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Active tool names.
        tool_names: Vec<String>,
    },
    /// Compaction summary entry.
    Compaction {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Summary text.
        summary: String,
        /// First retained entry id after cut.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        /// Opaque details.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Branch summary.
    BranchSummary {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Summary text.
        summary: String,
    },
    /// Custom opaque entry.
    Custom {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Custom type.
        custom_type: String,
        /// Payload.
        data: Value,
    },
    /// Label marker.
    Label {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Label text.
        label: String,
    },
    /// Session info (name).
    SessionInfo {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Name.
        name: String,
    },
    /// Durable leaf cursor.
    Leaf {
        /// Entry id.
        id: String,
        /// Parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Target leaf entry id (null = root).
        target_id: Option<String>,
    },
}

impl SessionTreeEntry {
    /// Entry id.
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::ThinkingLevelChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::ActiveToolsChange { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. }
            | Self::Label { id, .. }
            | Self::SessionInfo { id, .. }
            | Self::Leaf { id, .. } => id,
        }
    }

    /// Parent id.
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Message { parent_id, .. }
            | Self::ThinkingLevelChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::ActiveToolsChange { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::BranchSummary { parent_id, .. }
            | Self::Custom { parent_id, .. }
            | Self::Label { parent_id, .. }
            | Self::SessionInfo { parent_id, .. }
            | Self::Leaf { parent_id, .. } => parent_id.as_deref(),
        }
    }
}
