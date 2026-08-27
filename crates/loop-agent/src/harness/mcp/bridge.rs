//! Bridge between MCP tools and the agent's `AgentTool` interface.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, ContentBlock, Tool as McpTool};
use serde_json::Value;
use tokio::sync::RwLock;

use loop_mcp::client::McpConnection;
use crate::types::{AgentTool, AgentToolResult};
use loop_ai::{TextContent, ToolResultContent};

/// Convert all tools from all connected MCP servers into `AgentTool` instances.
///
/// Each tool is prefixed as `mcp__{server}__{tool_name}` to avoid collisions
/// with built-in tools. The execute closure proxies `call_tool` back to the
/// MCP server.
pub fn mcp_tools_to_agent_tools(
    connections: &Arc<RwLock<HashMap<String, McpConnection>>>,
) -> Vec<AgentTool> {
    let guard = connections.blocking_read();
    let mut tools = Vec::new();
    for (server_name, conn) in guard.iter() {
        for mcp_tool in &conn.tools {
            let tool = make_agent_tool(server_name, mcp_tool, connections);
            tools.push(tool);
        }
    }
    tools
}

/// Async variant for use in async contexts.
pub async fn mcp_tools_to_agent_tools_async(
    connections: &Arc<RwLock<HashMap<String, McpConnection>>>,
) -> Vec<AgentTool> {
    let guard = connections.read().await;
    let mut tools = Vec::new();
    for (server_name, conn) in guard.iter() {
        for mcp_tool in &conn.tools {
            let tool = make_agent_tool(server_name, mcp_tool, connections);
            tools.push(tool);
        }
    }
    tools
}

fn make_agent_tool(
    server_name: &str,
    mcp_tool: &McpTool,
    connections: &Arc<RwLock<HashMap<String, McpConnection>>>,
) -> AgentTool {
    let prefixed_name = format!("mcp__{}__{}", server_name, mcp_tool.name);
    let description = mcp_tool
        .description
        .clone()
        .unwrap_or_default()
        .to_string();
    let label = format!("{} ({})", mcp_tool.name, server_name);

    let parameters = Value::Object(mcp_tool.input_schema.as_ref().clone());

    let conns = Arc::clone(connections);
    let original_tool_name: String = mcp_tool.name.to_string();
    let srv_name = server_name.to_string();

    AgentTool::simple(
        prefixed_name,
        label,
        description,
        parameters,
        move |_id, args, _cancel, _on_update| {
            let conns = Arc::clone(&conns);
            let tool_name = original_tool_name.clone();
            let server = srv_name.clone();
            async move {
                let guard = conns.read().await;
                let conn = guard.get(&server).ok_or_else(|| {
                    format!("MCP server '{server}' is not connected")
                })?;

                let mut params = CallToolRequestParams::new(tool_name.clone());
                if let Some(map) = args.as_object() {
                    if !map.is_empty() {
                        params = params.with_arguments(map.clone());
                    }
                }

                let result = conn
                    .client
                    .call_tool(params)
                    .await
                    .map_err(|e| format!("MCP call_tool '{tool_name}' failed: {e}"))?;

                let content = mcp_content_to_tool_result(&result.content);

                Ok(AgentToolResult {
                    content,
                    details: serde_json::json!({
                        "mcp_server": server,
                        "mcp_tool": tool_name,
                        "is_error": result.is_error.unwrap_or(false),
                    }),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    )
}

fn mcp_content_to_tool_result(blocks: &[ContentBlock]) -> Vec<ToolResultContent> {
    let mut out = Vec::new();
    for item in blocks {
        match item {
            ContentBlock::Text(t) => {
                out.push(ToolResultContent::Text(TextContent {
                    text: t.text.clone(),
                    text_signature: None,
                }));
            }
            ContentBlock::Image(img) => {
                out.push(ToolResultContent::Image(loop_ai::ImageContent {
                    data: img.data.clone(),
                    mime_type: img.mime_type.clone(),
                }));
            }
            _ => {
                out.push(ToolResultContent::Text(TextContent {
                    text: "[unsupported MCP content type]".to_string(),
                    text_signature: None,
                }));
            }
        }
    }
    if out.is_empty() {
        out.push(ToolResultContent::Text(TextContent {
            text: "(empty result)".into(),
            text_signature: None,
        }));
    }
    out
}

/// Strip the `mcp__{server}__` prefix from a tool name, returning
/// `(server_name, original_tool_name)`.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let sep = rest.find("__")?;
    let server = &rest[..sep];
    let tool = &rest[sep + 2..];
    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mcp_tool_name() {
        assert_eq!(
            parse_mcp_tool_name("mcp__filesystem__read_file"),
            Some(("filesystem", "read_file"))
        );
        assert_eq!(parse_mcp_tool_name("read"), None);
        assert_eq!(parse_mcp_tool_name("mcp__"), None);
    }
}
