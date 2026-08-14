//! MCP adapter: bridges `loop-mcp` with `loop-agent` tool types.

pub mod bridge;

pub use bridge::mcp_tools_to_agent_tools;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use loop_mcp::server::{ToolDef, ToolOutput, ToolOutputContent, ToolProvider};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::{AgentTool, AgentToolResult};

/// Adapter that wraps a `Vec<AgentTool>` as a `loop_mcp::ToolProvider`.
pub struct LoopToolProvider {
    tools: RwLock<Vec<AgentTool>>,
}

impl LoopToolProvider {
    /// Create a provider from a snapshot of agent tools.
    pub fn new(tools: Vec<AgentTool>) -> Self {
        Self {
            tools: RwLock::new(tools),
        }
    }

    /// Update the tool set at runtime.
    pub async fn set_tools(&self, tools: Vec<AgentTool>) {
        *self.tools.write().expect("tool lock poisoned") = tools;
    }
}

#[async_trait]
impl ToolProvider for LoopToolProvider {
    fn list_tools(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().expect("tool lock poisoned");
        tools
            .iter()
            .map(|t| ToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect()
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolOutput, String> {
        let tool_execute = {
            let tools = self.tools.read().expect("tool lock poisoned");
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| format!("tool not found: {name}"))?;
            Arc::clone(&tool.execute)
        };

        let id = loop_ai::new_id();
        let cancel = CancellationToken::new();
        let result: AgentToolResult = tool_execute(id, args, Some(cancel), None).await?;

        let content = result
            .content
            .into_iter()
            .map(|c| match c {
                loop_ai::ToolResultContent::Text(t) => ToolOutputContent::Text(t.text),
                loop_ai::ToolResultContent::Image(img) => ToolOutputContent::Image {
                    data: img.data,
                    mime_type: img.mime_type,
                },
            })
            .collect();

        Ok(ToolOutput {
            content,
            is_error: false,
        })
    }
}
