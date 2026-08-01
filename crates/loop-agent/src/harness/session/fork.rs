//! Session fork selection helpers.

use serde::{Deserialize, Serialize};

use crate::harness::session::types::SessionTreeEntry;
use crate::harness::types::SessionError;

/// How to select entries when forking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionForkSelection {
    /// All entries on the active branch.
    All,
    /// Entries before the latest user message.
    BeforeUserMessage,
    /// Through a specific entry id (inclusive) — pass via `through_entry`.
    ThroughEntry,
}

/// Read entries for a fork selection.
pub fn read_session_entries_for_fork(
    entries: &[SessionTreeEntry],
    leaf_id: Option<&str>,
    selection: SessionForkSelection,
    through_entry_id: Option<&str>,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    entries_for_fork_selection_inner(entries, leaf_id, selection, through_entry_id)
}

pub(crate) fn entries_for_fork_selection(
    entries: &[SessionTreeEntry],
    leaf_id: Option<&str>,
    selection: SessionForkSelection,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    entries_for_fork_selection_inner(entries, leaf_id, selection, None)
}

fn entries_for_fork_selection_inner(
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
            for e in branch {
                let id = e.id().to_string();
                out.push(e);
                if id == target {
                    break;
                }
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
