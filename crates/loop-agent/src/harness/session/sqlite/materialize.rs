//! Materialized session summaries updated on append.

use rusqlite::{params, Connection};

use crate::harness::session::types::SessionTreeEntry;
use crate::harness::types::SessionError;

#[derive(Debug, Clone, Default)]
struct MaterializedState {
    name: Option<String>,
    message_count: u64,
    current_model: Option<(String, String)>,
    current_thinking_level: Option<String>,
}

fn empty_summary_json() -> String {
    serde_json::json!({
        "messageCount": 0,
        "currentModel": null,
        "currentThinkingLevel": null,
    })
    .to_string()
}

fn serialize_summary(state: &MaterializedState) -> String {
    let current_model = state.current_model.as_ref().map(|(provider, model_id)| {
        serde_json::json!({ "provider": provider, "modelId": model_id })
    });
    serde_json::json!({
        "name": state.name,
        "messageCount": state.message_count,
        "currentModel": current_model,
        "currentThinkingLevel": state.current_thinking_level,
    })
    .to_string()
}

fn parse_summary(json: &str) -> Result<MaterializedState, SessionError> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| SessionError::Storage(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| SessionError::Invalid("materialized session summary is not an object".into()))?;
    let message_count = obj
        .get("messageCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());
    let current_model = obj.get("currentModel").and_then(|v| {
        let m = v.as_object()?;
        Some((
            m.get("provider")?.as_str()?.to_string(),
            m.get("modelId")?.as_str()?.to_string(),
        ))
    });
    let current_thinking_level = obj
        .get("currentThinkingLevel")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(MaterializedState {
        name,
        message_count,
        current_model,
        current_thinking_level,
    })
}

fn apply_entry(state: &mut MaterializedState, entry: &SessionTreeEntry) {
    match entry {
        SessionTreeEntry::SessionInfo { name, .. } => {
            let trimmed = name.trim();
            state.name = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        SessionTreeEntry::ModelChange {
            provider, model_id, ..
        } => {
            state.current_model = Some((provider.clone(), model_id.clone()));
        }
        SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
            state.current_thinking_level = Some(format!("{thinking_level:?}"));
        }
        SessionTreeEntry::Message { .. } => {
            state.message_count += 1;
        }
        SessionTreeEntry::Label { .. }
        | SessionTreeEntry::Compaction { .. }
        | SessionTreeEntry::BranchSummary { .. }
        | SessionTreeEntry::ActiveToolsChange { .. }
        | SessionTreeEntry::Custom { .. }
        | SessionTreeEntry::Leaf { .. } => {}
    }
}

fn entry_materialized_rows(entry: &SessionTreeEntry) -> Vec<(&'static str, String)> {
    match entry {
        SessionTreeEntry::Label { label, .. } => vec![(
            "label",
            serde_json::json!({ "label": label }).to_string(),
        )],
        _ => vec![],
    }
}

/// Insert an empty materialized row for a new session.
pub fn insert_empty_materialized(
    conn: &Connection,
    session_id: &str,
) -> Result<(), SessionError> {
    conn.execute(
        "INSERT INTO session_materialized (session_id, payload) VALUES (?1, ?2)",
        params![session_id, empty_summary_json()],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    Ok(())
}

/// Apply an appended entry to materialized tables within the same transaction.
pub fn update_materialized_on_append(
    conn: &Connection,
    session_id: &str,
    entry_seq: i64,
    entry: &SessionTreeEntry,
) -> Result<(), SessionError> {
    let current_json: String = conn
        .query_row(
            "SELECT payload FROM session_materialized WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let mut state = parse_summary(&current_json)?;
    apply_entry(&mut state, entry);
    conn.execute(
        "UPDATE session_materialized SET payload = ?1 WHERE session_id = ?2",
        params![serialize_summary(&state), session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    for (entry_type, payload) in entry_materialized_rows(entry) {
        conn.execute(
            "INSERT INTO entry_materialized (session_id, entry_seq, type, payload) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, entry_seq, entry_type, payload],
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    }
    if let SessionTreeEntry::SessionInfo { name, .. } = entry {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            conn.execute(
                "UPDATE sessions SET name=?1 WHERE id=?2",
                params![trimmed, session_id],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        }
    }
    Ok(())
}
