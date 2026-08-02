//! Session tree entry types and store traits.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::types::SessionError;
use crate::types::{AgentMessage, AgentThinkingLevel};

/// Session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Tree entry stored in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionTreeEntry {
    /// Transcript message.
    Message {
        /// Entry id.
        id: String,
        /// Parent entry id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Timestamp.
        timestamp: i64,
        /// Message payload.
        message: AgentMessage,
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
        /// New level.
        thinking_level: AgentThinkingLevel,
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

/// Pending write without generated fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PendingSessionWrite {
    /// Message.
    Message {
        /// Message.
        message: AgentMessage,
    },
    /// Thinking level.
    ThinkingLevelChange {
        /// Level.
        thinking_level: AgentThinkingLevel,
    },
    /// Model.
    ModelChange {
        /// Provider.
        provider: String,
        /// Model id.
        model_id: String,
    },
    /// Active tools.
    ActiveToolsChange {
        /// Names.
        tool_names: Vec<String>,
    },
    /// Compaction.
    Compaction {
        /// Summary.
        summary: String,
        /// First kept.
        first_kept_entry_id: Option<String>,
        /// Details.
        details: Option<Value>,
    },
    /// Branch summary.
    BranchSummary {
        /// Summary.
        summary: String,
    },
    /// Leaf move.
    Leaf {
        /// Target.
        target_id: Option<String>,
    },
    /// Label.
    Label {
        /// Label.
        label: String,
    },
    /// Session info.
    SessionInfo {
        /// Name.
        name: String,
    },
}

/// Reader over a session's entries.
#[async_trait]
pub trait SessionReader: Send + Sync {
    /// Metadata.
    fn metadata(&self) -> &SessionMetadata;
    /// Current leaf id.
    async fn read_head(&self) -> Result<Option<String>, SessionError>;
    /// Read one entry.
    async fn read_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError>;
    /// Read all entries (optionally from cursor).
    async fn read_entries(
        &self,
        after_seq: Option<u64>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError>;
    /// Path from root/compaction to leaf.
    async fn read_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError>;
}

/// Session store.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Create a session.
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError>;
    /// Load by id.
    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError>;
    /// List sessions.
    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError>;
    /// Append an entry (generates id/parent/timestamp).
    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError>;
    /// Delete session.
    async fn delete(&self, id: &str) -> Result<(), SessionError>;
    /// Fork session.
    async fn fork(
        &self,
        source_id: &str,
        selection: super::SessionForkSelection,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError>;
}

/// Built context from a session branch.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    /// Messages for the agent.
    pub messages: Vec<AgentMessage>,
    /// Thinking level.
    pub thinking_level: AgentThinkingLevel,
    /// Provider/model if known.
    pub model: Option<(String, String)>,
    /// Active tool names if set.
    pub active_tool_names: Option<Vec<String>>,
}

/// High-level session facade.
pub struct Session {
    store: Arc<dyn SessionStore>,
    reader: Arc<dyn SessionReader>,
}

impl Session {
    /// Wrap a reader + store.
    pub fn new(store: Arc<dyn SessionStore>, reader: Arc<dyn SessionReader>) -> Self {
        Self { store, reader }
    }

    /// Underlying store.
    pub fn store(&self) -> Arc<dyn SessionStore> {
        Arc::clone(&self.store)
    }

    /// Metadata.
    pub fn metadata(&self) -> &SessionMetadata {
        self.reader.metadata()
    }

    /// Append a message.
    pub async fn append_message(&self, message: AgentMessage) -> Result<SessionTreeEntry, SessionError> {
        self.store
            .append_entry(
                &self.metadata().id,
                PendingSessionWrite::Message { message },
            )
            .await
    }

    /// Move leaf.
    pub async fn move_to(&self, target_id: Option<String>) -> Result<SessionTreeEntry, SessionError> {
        self.store
            .append_entry(
                &self.metadata().id,
                PendingSessionWrite::Leaf { target_id },
            )
            .await
    }

    /// Build agent context from the active branch.
    pub async fn build_context(&self) -> Result<SessionContext, SessionError> {
        let leaf = self.reader.read_head().await?;
        let entries = self
            .reader
            .read_path_to_root_or_compaction(leaf.as_deref())
            .await?;
        Ok(default_context_from_entries(&entries))
    }

    /// Underlying reader.
    pub fn reader(&self) -> Arc<dyn SessionReader> {
        Arc::clone(&self.reader)
    }

    /// Read all session entries (full tree, including compacted history).
    pub async fn read_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
        self.reader.read_entries(None).await
    }

    /// Read the active branch path (root/compaction → leaf).
    pub async fn read_branch(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let leaf = self.reader.read_head().await?;
        self.reader
            .read_path_to_root_or_compaction(leaf.as_deref())
            .await
    }
}

/// Project entries to session context (compaction-aware).
pub fn default_context_from_entries(entries: &[SessionTreeEntry]) -> SessionContext {
    let mut ctx = SessionContext::default();
    let mut start = 0usize;
    for (i, e) in entries.iter().enumerate() {
        if let SessionTreeEntry::Compaction { summary, timestamp, .. } = e {
            ctx.messages.clear();
            ctx.messages.push(crate::messages::create_compaction_summary_message(summary));
            // keep timestamp usage quiet
            let _ = timestamp;
            start = i + 1;
        }
    }
    for e in &entries[start..] {
        match e {
            SessionTreeEntry::Message { message, .. } => ctx.messages.push(message.clone()),
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                ctx.thinking_level = *thinking_level;
            }
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => {
                ctx.model = Some((provider.clone(), model_id.clone()));
            }
            SessionTreeEntry::ActiveToolsChange { tool_names, .. } => {
                ctx.active_tool_names = Some(tool_names.clone());
            }
            SessionTreeEntry::BranchSummary { summary, .. } => {
                ctx.messages
                    .push(crate::messages::create_branch_summary_message(summary));
            }
            _ => {}
        }
    }
    ctx
}

/// Create a session id.
pub fn create_session_id() -> String {
    loop_ai::new_id()
}
