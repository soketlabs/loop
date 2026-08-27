//! Agent-visible memory tools: memory_read, memory_write, memory_list.

use std::sync::Arc;

use loop_orchestration::scheduler::worker::{SharedMemoryAccess, TaskMemoryAccess};
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult};

/// Create the `memory_read` tool for agents.
pub fn create_memory_read_tool(
    shared: Arc<dyn SharedMemoryAccess>,
    task: Arc<dyn TaskMemoryAccess>,
) -> AgentTool {
    AgentTool::simple(
        "memory_read",
        "Read Memory",
        "Read a value from shared (workflow-level) or task (local) memory by key.",
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["shared", "task"],
                    "description": "Which memory scope to read from"
                },
                "key": {
                    "type": "string",
                    "description": "The key to read"
                }
            },
            "required": ["scope", "key"]
        }),
        move |_id, args, _cancel, _on_update| {
            let shared = Arc::clone(&shared);
            let task = Arc::clone(&task);
            async move {
                let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("task");
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing key argument".to_string())?;

                let value = match scope {
                    "shared" => shared.get(key).await,
                    "task" => task.get(key).await,
                    _ => return Err(format!("invalid scope: {scope}")),
                };

                match value {
                    Some(v) => Ok(AgentToolResult::text(serde_json::to_string_pretty(&v).unwrap_or_default())),
                    None => Ok(AgentToolResult::text(format!("Key '{key}' not found in {scope} memory"))),
                }
            }
        },
    )
}

/// Create the `memory_write` tool for agents.
pub fn create_memory_write_tool(
    shared: Arc<dyn SharedMemoryAccess>,
    task: Arc<dyn TaskMemoryAccess>,
    writer_id: String,
) -> AgentTool {
    AgentTool::simple(
        "memory_write",
        "Write Memory",
        "Write a value to shared (workflow-level) or task (local) memory.",
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["shared", "task"],
                    "description": "Which memory scope to write to"
                },
                "key": {
                    "type": "string",
                    "description": "The key to write"
                },
                "value": {
                    "description": "The value to store (any JSON value)"
                }
            },
            "required": ["scope", "key", "value"]
        }),
        move |_id, args, _cancel, _on_update| {
            let shared = Arc::clone(&shared);
            let task = Arc::clone(&task);
            let writer = writer_id.clone();
            async move {
                let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("task");
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing key argument".to_string())?;
                let value = args
                    .get("value")
                    .cloned()
                    .unwrap_or(Value::Null);

                match scope {
                    "shared" => shared.set(key, value, &writer).await,
                    "task" => task.set(key, value).await,
                    _ => return Err(format!("invalid scope: {scope}")),
                }

                Ok(AgentToolResult::text(format!("Written to {scope} memory: {key}")))
            }
        },
    )
}

/// Create the `memory_list` tool for agents.
pub fn create_memory_list_tool(
    shared: Arc<dyn SharedMemoryAccess>,
    task: Arc<dyn TaskMemoryAccess>,
) -> AgentTool {
    AgentTool::simple(
        "memory_list",
        "List Memory Keys",
        "List keys in shared or task memory, optionally filtered by prefix.",
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["shared", "task"],
                    "description": "Which memory scope to list"
                },
                "prefix": {
                    "type": "string",
                    "description": "Optional key prefix filter"
                }
            },
            "required": ["scope"]
        }),
        move |_id, args, _cancel, _on_update| {
            let shared = Arc::clone(&shared);
            let task = Arc::clone(&task);
            async move {
                let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("task");
                let prefix = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("");

                let keys = match scope {
                    "shared" => shared.list_keys(prefix).await,
                    "task" => {
                        let all = task.list_keys().await;
                        if prefix.is_empty() {
                            all
                        } else {
                            all.into_iter().filter(|k| k.starts_with(prefix)).collect()
                        }
                    }
                    _ => return Err(format!("invalid scope: {scope}")),
                };

                Ok(AgentToolResult::text(serde_json::to_string_pretty(&keys).unwrap_or_default()))
            }
        },
    )
}
