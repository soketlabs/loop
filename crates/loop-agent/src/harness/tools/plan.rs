//! The `exit_plan` tool: in plan mode, the model hands the user its plan
//! to approve.
//!
//! The approval happens before the call runs: the host's permission gate
//! asks the user and, on approval, leaves plan mode. By the time the tool
//! runs, the plan is approved, so it only tells the model to carry it out.

use serde_json::json;

use super::{arg_str, schema_object};
use crate::types::{AgentTool, AgentToolResult};
use loop_ai::{TextContent, ToolResultContent};

/// The tool's name.
pub const EXIT_PLAN_TOOL: &str = "exit_plan";

/// Create the `exit_plan` tool.
pub fn create_exit_plan_tool() -> AgentTool {
    AgentTool::simple(
        EXIT_PLAN_TOOL,
        "Exit plan",
        "You are in plan mode: investigate with tools that only read, then call this with \
         your plan to ask the user to approve it. Files can't be changed and commands that \
         change things can't run until they do. If they ask for changes, revise the plan and \
         call it again.",
        schema_object(
            json!({
                "plan": {
                    "type": "string",
                    "description": "The plan in Markdown: what you will change, where, and how \
                                    you will check it"
                }
            }),
            &["plan"],
        ),
        move |_id, args, _cancel, _on_update| async move {
            let plan = arg_str(&args, "plan")?;
            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: "The user approved the plan and plan mode is off. Carry out the plan \
                           now; edits and commands follow the user's permission mode."
                        .into(),
                    text_signature: None,
                })],
                details: json!({ "plan": plan }),
                usage: None,
                added_tool_names: None,
                terminate: None,
            })
        },
    )
}
