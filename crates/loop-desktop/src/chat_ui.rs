//! Chat presentation helpers (tool labels, summaries, activity).

use loop_agent::harness::AgentHarnessPhase;
use serde_json::Value;

use crate::state::{ChatRow, ToolCardStatus};

/// Compact relative timestamp for the session list.
pub fn relative_time(unix_ms: i64) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let delta = (now - unix_ms).max(0);
    let secs = delta / 1000;
    if secs < 45 {
        "just now".into()
    } else if secs < 90 {
        "1 min ago".into()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 36 * 3600 {
        let hours = secs / 3600;
        if hours == 1 {
            "1 hour ago".into()
        } else {
            format!("{hours} hours ago")
        }
    } else if secs < 14 * 86400 {
        let days = secs / 86400;
        if days == 1 {
            "yesterday".into()
        } else {
            format!("{days} days ago")
        }
    } else {
        chrono::DateTime::from_timestamp_millis(unix_ms)
            .map(|t| t.format("%b %d").to_string())
            .unwrap_or_else(|| "earlier".into())
    }
}

/// Folder name for the open project.
pub fn project_label(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("project")
        .to_string()
}

/// Live status copy while the harness is working.
pub fn activity_label(
    streaming: bool,
    phase: AgentHarnessPhase,
    rows: &[ChatRow],
) -> Option<String> {
    if !streaming {
        return None;
    }
    if let Some(ChatRow::Tool {
        name,
        summary,
        status: ToolCardStatus::Running | ToolCardStatus::Pending,
        ..
    }) = rows.iter().rev().find(|r| matches!(r, ChatRow::Tool { .. }))
    {
        return Some(tool_activity_label(name, summary, ToolCardStatus::Running));
    }
    if rows
        .iter()
        .rev()
        .any(|r| matches!(r, ChatRow::Thinking { done: false, .. }))
    {
        return Some("Thinking".into());
    }
    Some(match phase {
        AgentHarnessPhase::Idle | AgentHarnessPhase::Turn => "Working".into(),
        AgentHarnessPhase::Compaction => "Compacting context".into(),
        AgentHarnessPhase::BranchSummary => "Summarizing".into(),
        AgentHarnessPhase::Retry => "Retrying".into(),
    })
}

/// Short human-readable tool activity, e.g. "Reading `src/main.rs`".
pub fn tool_activity_label(name: &str, summary: &str, status: ToolCardStatus) -> String {
    let target = if summary.is_empty() {
        "…".to_string()
    } else {
        summary.to_string()
    };
    match status {
        ToolCardStatus::Pending | ToolCardStatus::Running => {
            format!("{} {}", tool_running_verb(name), target)
        }
        ToolCardStatus::Success => format!("{} {}", tool_done_verb(name), target),
        ToolCardStatus::Error => format!("Failed to {} {}", tool_base_verb(name), target),
    }
}

fn tool_base_verb(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "write" => "write",
        "edit" => "edit",
        "bash" | "shell" => "run",
        "grep" | "search" => "search",
        _ => "use",
    }
}

fn tool_running_verb(name: &str) -> String {
    match name {
        "read" => "Reading".into(),
        "write" => "Writing".into(),
        "edit" => "Editing".into(),
        "bash" | "shell" => "Running".into(),
        "grep" | "search" => "Searching".into(),
        other => format!("Running {other}"),
    }
}

fn tool_done_verb(name: &str) -> String {
    match name {
        "read" => "Read".into(),
        "write" => "Wrote".into(),
        "edit" => "Edited".into(),
        "bash" | "shell" => "Ran".into(),
        "grep" | "search" => "Searched".into(),
        other => format!("Used {other}"),
    }
}

/// Truncate tool args to a short summary (matches CLI behavior).
pub fn tool_args_summary(name: &str, args: &Value) -> String {
    let pick = |keys: &[&str]| {
        for k in keys {
            if let Some(s) = args.get(*k).and_then(|v| v.as_str()) {
                let mut t: String = s.chars().take(72).collect();
                if s.chars().count() > 72 {
                    t.push('…');
                }
                return Some(t);
            }
        }
        None
    };
    match name {
        "read" | "write" | "edit" => pick(&["path", "file_path", "file"]).unwrap_or_else(|| "…".into()),
        "bash" | "shell" => pick(&["command"]).unwrap_or_else(|| "…".into()),
        "grep" | "search" => pick(&["pattern", "query"]).unwrap_or_else(|| "…".into()),
        _ => {
            if let Some(path) = pick(&["path", "file_path", "file", "command"]) {
                return path;
            }
            let s = args.to_string();
            let mut t: String = s.chars().take(48).collect();
            if s.chars().count() > 48 {
                t.push('…');
            }
            t
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_preview_extracts_content() {
        let args = json!({"path": "src/main.rs", "content": "fn main() {}\n"});
        assert_eq!(tool_args_summary("write", &args), "src/main.rs");
        assert_eq!(tool_content_preview("write", &args), "fn main() {}\n");
    }

    #[test]
    fn edit_preview_prefers_new_text() {
        let args = json!({
            "path": "a.rs",
            "oldText": "old",
            "newText": "new body"
        });
        assert_eq!(tool_content_preview("edit", &args), "new body");
    }

    #[test]
    fn truncate_keeps_tail() {
        let detail = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let (preview, more) = truncate_tool_detail(&detail, 5);
        assert_eq!(more, 25);
        assert!(!preview.starts_with("…"));
        assert!(preview.contains("line 30"));
        assert!(!preview.contains("line 1\n"));
    }
}

/// Whether this tool should render as a boxed output card (shell / terminal).
pub fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "shell")
}

/// Whether this tool should render as an editing content box while streaming.
pub fn is_file_mutation_tool(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

/// Extract file body preview from write/edit tool args (may be partial while streaming).
pub fn tool_content_preview(name: &str, args: &Value) -> String {
    match name {
        "write" => args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "edit" => {
            let new_text = args.get("newText").and_then(|v| v.as_str()).unwrap_or("");
            if !new_text.is_empty() {
                return new_text.to_string();
            }
            args.get("oldText")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
        _ => String::new(),
    }
}

/// Cap preview text for the chat card (keeps the UI responsive on large writes).
/// Returns `(preview, hidden_line_count)` where preview is the trailing lines.
pub fn truncate_tool_detail(detail: &str, max_lines: usize) -> (String, usize) {
    let total = detail.lines().count();
    if total <= max_lines {
        return (detail.to_string(), 0);
    }
    let kept: Vec<&str> = detail.lines().rev().take(max_lines).collect();
    let preview = kept.into_iter().rev().collect::<Vec<_>>().join("\n");
    (preview, total.saturating_sub(max_lines))
}
