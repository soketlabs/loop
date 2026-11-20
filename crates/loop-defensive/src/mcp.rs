//! The MCP server Loop starts for `/patch` (`loop-defensive serve patch`,
//! registered as `defensive`) and `/audit` (`serve audit`, registered as
//! `defensive-audit`): JSON-RPC 2.0, one message per line on stdin/stdout.
//!
//! Stdout carries protocol messages only. Calls run concurrently; the
//! session serializes the ones that touch the run.

use std::sync::Arc;

use loop_agent::types::AgentTool;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::session::Session;
use crate::tools::{self, Workflow};

/// Tools the model may treat as reads (`readOnlyHint`).
const READ_ONLY: &[&str] = &["get_task", "list_directory", "search_code", "read", "show_diff", "run_analyzer"];

/// The server name Loop registers `workflow` under.
pub fn server_name(workflow: Workflow) -> &'static str {
    match workflow {
        Workflow::Patch => "defensive",
        Workflow::Audit => "defensive-audit",
    }
}

fn text_of(result: &loop_agent::types::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Answer one request; `None` for notifications.
pub async fn handle(tools: &[AgentTool], workflow: Workflow, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg["method"].as_str().unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": server_name(workflow), "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools.iter().map(|t| json!({
            "name": t.name, "description": t.description, "inputSchema": t.parameters,
            "annotations": {"readOnlyHint": READ_ONLY.contains(&t.name.as_str())},
        })).collect::<Vec<_>>()}),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let Some(tool) = tools.iter().find(|t| t.name == name) else {
                return Some(json!({"jsonrpc": "2.0", "id": id,
                                   "error": {"code": -32602, "message": format!("unknown tool {name}")}}));
            };
            let args = params.get("arguments").cloned().filter(Value::is_object).unwrap_or(json!({}));
            let (text, is_error) = match (tool.execute)(format!("mcp-{id}"), args, None, None).await {
                Ok(r) => (text_of(&r), false),
                Err(e) => (e, true),
            };
            json!({"content": [{"type": "text", "text": text}], "isError": is_error})
        }
        other => {
            return Some(json!({"jsonrpc": "2.0", "id": id,
                               "error": {"code": -32601, "message": format!("no method {other}")}}))
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serve `workflow` on stdin/stdout until stdin closes; then finish the run.
pub async fn serve(workflow: Workflow, session: Arc<Session>) -> std::io::Result<()> {
    let tools = Arc::new(tools::tools(&session, workflow));
    let out = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let (tools, out) = (Arc::clone(&tools), Arc::clone(&out));
        tasks.spawn(async move {
            if let Some(reply) = handle(&tools, workflow, &msg).await {
                let mut out = out.lock().await;
                let _ = out.write_all(format!("{reply}\n").as_bytes()).await;
                let _ = out.flush().await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    let _ = tokio::task::spawn_blocking(move || session.shutdown()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn protocol_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(Session::new(dir.path().into(), dir.path().into()));
        let tools = tools::tools(&session, Workflow::Patch);
        let init = handle(&tools, Workflow::Patch, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).await.unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "defensive");
        assert!(handle(&tools, Workflow::Patch, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());
        let list = handle(&tools, Workflow::Patch, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"read") && names.contains(&"bash") && names.contains(&"grade_patch"));
        let read = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "read").unwrap();
        assert_eq!(read["annotations"]["readOnlyHint"], true);
        let call = handle(
            &tools,
            Workflow::Patch,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "grade_patch", "arguments": {}}}),
        )
        .await
        .unwrap();
        assert_eq!(call["result"]["content"][0]["text"], "No active task; call prepare_task first.");
        let missing = handle(&tools, Workflow::Patch, &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope"}})).await.unwrap();
        assert_eq!(missing["error"]["code"], -32602);
    }
}
