//! The `todo` tool: the model's checklist for multi-step work.
//!
//! The tool keeps no state. Each call sends the whole list, and the result's
//! `details.todos` carries it, so the checklist lives in the Session's
//! history and follows it through `/fork`, `/clone` and `--resume`.
//! [`crate::protocol_events`] turns each result into a `SessionUpdate::Plan`.

use loop_protocol::{PlanEntry, PlanEntryStatus};
use serde_json::{json, Value};

use super::schema_object;
use crate::types::{AgentTool, AgentToolResult};
use loop_ai::{TextContent, ToolResultContent};

/// Create the `todo` tool.
pub fn create_todo_tool() -> AgentTool {
    AgentTool::simple(
        "todo",
        "Todo",
        "Write the checklist for multi-step work. Send the whole list each time: add the \
         steps when you start, mark one in_progress as you work on it and completed when it \
         is done. Skip it for single-step requests.",
        schema_object(
            json!({
                "todos": {
                    "type": "array",
                    "description": "The full checklist, in order; it replaces the previous one",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "What the step does, in a few words"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "The step's progress"
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            }),
            &["todos"],
        ),
        move |_id, args, _cancel, _on_update| async move {
            let entries = parse_todos(&args)?;
            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: checklist_text(&entries),
                    text_signature: None,
                })],
                details: json!({ "todos": entries }),
                usage: None,
                added_tool_names: None,
                terminate: None,
            })
        },
    )
}

/// The checklist in a `todo` result's details, or `None` when `details`
/// has none.
pub fn todos_from_details(details: &Value) -> Option<Vec<PlanEntry>> {
    serde_json::from_value(details.get("todos")?.clone()).ok()
}

fn parse_todos(args: &Value) -> Result<Vec<PlanEntry>, String> {
    let items = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or("missing array field `todos`")?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .ok_or_else(|| format!("todos[{index}] needs a non-empty `content`"))?;
            let status = match item.get("status").and_then(Value::as_str) {
                Some("pending") => PlanEntryStatus::Pending,
                Some("in_progress") => PlanEntryStatus::InProgress,
                Some("completed") => PlanEntryStatus::Completed,
                other => {
                    return Err(format!(
                        "todos[{index}].status is {other:?}; use pending, in_progress or completed"
                    ))
                }
            };
            Ok(PlanEntry {
                content: content.to_owned(),
                status,
            })
        })
        .collect()
}

/// The checklist as the model reads it back.
fn checklist_text(entries: &[PlanEntry]) -> String {
    if entries.is_empty() {
        return "Checklist cleared.".into();
    }
    let done = entries
        .iter()
        .filter(|entry| entry.status == PlanEntryStatus::Completed)
        .count();
    let mut text = format!("Checklist ({done}/{} done):", entries.len());
    for entry in entries {
        let mark = match entry.status {
            PlanEntryStatus::Completed => "[x]",
            PlanEntryStatus::InProgress => "[~]",
            _ => "[ ]",
        };
        text.push_str(&format!("\n{mark} {}", entry.content));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(args: Value) -> Result<AgentToolResult, String> {
        (create_todo_tool().execute)("t".into(), args, None, None).await
    }

    #[tokio::test]
    async fn the_result_carries_the_whole_checklist() {
        let result = run(json!({"todos": [
            {"content": "read the code", "status": "completed"},
            {"content": "write the test", "status": "in_progress"},
            {"content": "  fix it  ", "status": "pending"}
        ]}))
        .await
        .unwrap();
        let ToolResultContent::Text(text) = &result.content[0] else {
            panic!("text result");
        };
        assert_eq!(
            text.text,
            "Checklist (1/3 done):\n[x] read the code\n[~] write the test\n[ ] fix it"
        );
        let entries = todos_from_details(&result.details).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].status, PlanEntryStatus::InProgress);
        assert_eq!(entries[2].content, "fix it");
    }

    #[tokio::test]
    async fn an_empty_list_clears_the_checklist() {
        let result = run(json!({"todos": []})).await.unwrap();
        assert_eq!(todos_from_details(&result.details), Some(Vec::new()));
    }

    #[tokio::test]
    async fn bad_entries_are_refused() {
        let err = run(json!({"todos": [{"content": "x", "status": "done"}]}))
            .await
            .unwrap_err();
        assert!(err.contains("todos[0].status"), "{err}");
        let err = run(json!({"todos": [{"content": " ", "status": "pending"}]}))
            .await
            .unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        assert!(run(json!({})).await.is_err());
    }

    #[test]
    fn other_details_have_no_checklist() {
        assert_eq!(todos_from_details(&json!({"path": "a"})), None);
    }
}
