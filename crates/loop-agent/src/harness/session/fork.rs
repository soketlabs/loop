//! Session fork selection helpers.

use serde::{Deserialize, Serialize};

use crate::harness::session::types::SessionTreeEntry;
use crate::harness::types::SessionError;
use crate::types::AgentMessage;

/// How to select entries when forking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionForkSelection {
    /// All entries on the active branch.
    All,
    /// Entries before the latest user message.
    BeforeUserMessage,
    /// Through a specific entry id (inclusive) — pass via `through_entry_id`.
    ThroughEntry,
    /// Entries before a specific entry id (exclusive) — pass via `through_entry_id`.
    BeforeEntry,
}

/// A user-message fork point on the active branch.
#[derive(Debug, Clone)]
pub struct SessionForkPoint {
    /// Persisted entry id (use with [`SessionForkSelection::BeforeEntry`]).
    pub entry_id: String,
    /// 1-based index among user messages on the branch (oldest = 1).
    pub index: usize,
    /// Full user message text (for editing in the input box).
    pub text: String,
    /// Truncated user message text for picker display.
    pub preview: String,
}

/// Collect user-message fork points from an active branch (oldest → newest).
pub fn fork_points_from_branch(branch: &[SessionTreeEntry]) -> Vec<SessionForkPoint> {
    let mut out = Vec::new();
    for entry in branch {
        let SessionTreeEntry::Message { id, message, .. } = entry else {
            continue;
        };
        if message.role() != "user" {
            continue;
        }
        let text = user_message_text(message);
        if text.is_empty() {
            continue;
        }
        let preview = {
            let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            truncate_chars(&collapsed, 72)
        };
        out.push(SessionForkPoint {
            entry_id: id.clone(),
            index: out.len() + 1,
            text,
            preview,
        });
    }
    out
}

fn user_message_text(message: &AgentMessage) -> String {
    use loop_ai::{Message, UserContent, UserMessageContent};
    let Some(Message::User(u)) = message.as_llm() else {
        return String::new();
    };
    match &u.content {
        UserMessageContent::Text(s) => s.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Read entries for a fork selection.
pub fn read_session_entries_for_fork(
    entries: &[SessionTreeEntry],
    leaf_id: Option<&str>,
    selection: SessionForkSelection,
    through_entry_id: Option<&str>,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    entries_for_fork_selection(entries, leaf_id, selection, through_entry_id)
}

pub(crate) fn entries_for_fork_selection(
    entries: &[SessionTreeEntry],
    leaf_id: Option<&str>,
    selection: SessionForkSelection,
    through_entry_id: Option<&str>,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    let branch = path_to_leaf(entries, leaf_id);
    match selection {
        SessionForkSelection::All => Ok(branch),
        SessionForkSelection::BeforeUserMessage => {
            let mut out = Vec::new();
            for e in branch {
                if let SessionTreeEntry::Message { message, .. } = &e {
                    if message.role() == "user" {
                        break;
                    }
                }
                out.push(e);
            }
            Ok(out)
        }
        SessionForkSelection::ThroughEntry => {
            let target = through_entry_id.ok_or_else(|| {
                SessionError::Invalid("through_entry_id required".into())
            })?;
            let mut out = Vec::new();
            let mut found = false;
            for e in branch {
                let id = e.id().to_string();
                out.push(e);
                if id == target {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(SessionError::Invalid(format!(
                    "through_entry_id not on active branch: {target}"
                )));
            }
            Ok(out)
        }
        SessionForkSelection::BeforeEntry => {
            let target = through_entry_id.ok_or_else(|| {
                SessionError::Invalid("through_entry_id required".into())
            })?;
            let mut out = Vec::new();
            let mut found = false;
            for e in branch {
                if e.id() == target {
                    found = true;
                    break;
                }
                out.push(e);
            }
            if !found {
                return Err(SessionError::Invalid(format!(
                    "through_entry_id not on active branch: {target}"
                )));
            }
            Ok(out)
        }
    }
}

fn path_to_leaf(entries: &[SessionTreeEntry], leaf_id: Option<&str>) -> Vec<SessionTreeEntry> {
    let Some(leaf) = leaf_id else {
        return entries.to_vec();
    };
    let by_id: std::collections::HashMap<_, _> =
        entries.iter().map(|e| (e.id().to_string(), e.clone())).collect();
    let mut path = Vec::new();
    let mut cur = Some(leaf.to_string());
    while let Some(id) = cur {
        let Some(entry) = by_id.get(&id) else {
            break;
        };
        path.push(entry.clone());
        cur = entry.parent_id().map(|s| s.to_string());
    }
    path.reverse();
    path
}
