//! Payloads of the log record kinds that ADR-0007 §1.2 adds.
//!
//! Defined for later steps. Nothing writes these records yet, and
//! [`crate::EventKind`] has no variant for them: a stored row of one of
//! these kinds reads as `EventKind::Unknown` today. The step that first
//! writes a kind adds its variant (B5 for `tree` and `session_deleted`, A5
//! for `policy_decision`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::Principal;
use crate::tree::SessionTreeEntry;

/// Stored `kind` of a policy decision record.
pub const POLICY_DECISION: &str = "policy_decision";
/// Stored `kind` of a tree record.
pub const TREE: &str = "tree";
/// Stored `kind` of a Session tombstone.
pub const SESSION_DELETED: &str = "session_deleted";

/// `tree`: one Session tree entry, exactly as the Session store keeps it.
pub type TreeRecord = SessionTreeEntry;

/// `policy_decision`: the S4 decision record (`docs/design/s4-policy.md`
/// §12). The S4 lane owns its fields, so the log keeps its JSON as is. The
/// tool arguments are in it as a hash only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolicyDecisionRecord(pub Value);

/// `session_deleted`: the tombstone that stays after a delete (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeletedRecord {
    /// The deleted Session id.
    pub id: String,
    /// Who deleted it.
    pub by: Principal,
}

/// How a Session came to exist. A new field of `session_opened` (§1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// A new, empty Session.
    Create,
    /// A fork: copies of the selected entries of `source`.
    Fork,
    /// A clone: copies of the whole active branch of `source`.
    Clone,
    /// A `loop_session` JSONL import.
    Import,
    /// A Subagent child Session.
    Subagent,
    /// The one-time import of a Session tree written before ADR-0007 (§6.2).
    LegacyImport,
}

/// The fields that `session_opened` gains (§1.2). The present payload keeps
/// `cwd`, `name`, `parent` and `created_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpenedOrigin {
    /// How the Session came to exist.
    pub origin: SessionOrigin,
    /// The source Session of a fork or clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The last copied entry of the source, for a fork or clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_entry: Option<String>,
}
