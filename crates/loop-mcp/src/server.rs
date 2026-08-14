//! MCP server: expose tools over streamable HTTP via the `ToolProvider` trait.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerInfo, Tool as McpTool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use serde_json::Value;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Describes a tool available from the provider.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: Value,
}

/// Output from executing a tool.
pub struct ToolOutput {
    /// Result content items.
    pub content: Vec<ToolOutputContent>,
    /// Whether this result represents an error.
    pub is_error: bool,
}

/// A single content item in tool output.
pub enum ToolOutputContent {
    /// Text content.
    Text(String),
    /// Image content (base64 data + MIME type).
    Image { data: String, mime_type: String },
}

/// Trait for providing tools to the MCP server, decoupled from concrete tool types.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// List all available tools.
    fn list_tools(&self) -> Vec<ToolDef>;
    /// Execute a tool by name with the given arguments.
    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolOutput, String>;
}

/// An MCP server backed by a `ToolProvider`.
pub struct McpServer {
    provider: Arc<dyn ToolProvider>,
}

impl McpServer {
    /// Create a server with the given tool provider.
    pub fn new(provider: Arc<dyn ToolProvider>) -> Self {
        Self { provider }
    }

    fn tool_defs_to_mcp(defs: &[ToolDef]) -> Vec<McpTool> {
        defs.iter()
            .map(|t| {
                let input_schema = if t.parameters.is_object() {
                    let map = t.parameters.as_object().cloned().unwrap_or_default();
                    Arc::new(map)
                } else {
                    Arc::new(serde_json::Map::new())
                };
                McpTool::new_with_raw(
                    t.name.clone(),
                    Some(t.description.clone().into()),
                    input_schema,
                )
            })
            .collect()
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("loop", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Loop coding agent by Soket AI. Provides file read/write/edit and bash tools.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let defs = self.provider.list_tools();
        Ok(ListToolsResult {
            tools: Self::tool_defs_to_mcp(&defs),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let args = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or(Value::Object(Default::default()));

        match self.provider.call_tool(name, args).await {
            Ok(output) => {
                let content: Vec<ContentBlock> = output
                    .content
                    .into_iter()
                    .map(|c| match c {
                        ToolOutputContent::Text(t) => ContentBlock::text(t),
                        ToolOutputContent::Image { data, mime_type } => {
                            ContentBlock::image(data, mime_type)
                        }
                    })
                    .collect();
                if output.is_error {
                    Ok(CallToolResult::error(content).into())
                } else {
                    Ok(CallToolResult::success(content).into())
                }
            }
            Err(msg) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(msg)]).into())
            }
        }
    }
}

/// Factory function type for creating a tool provider for each new MCP session.
pub type SessionFactory = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Arc<dyn ToolProvider>, String>> + Send>,
        > + Send
        + Sync,
>;

struct ManagedSession {
    server: Arc<McpServer>,
    last_active: Instant,
}

/// Manages multiple MCP server sessions, one per connected client.
pub struct McpSessionManager {
    sessions: Arc<RwLock<HashMap<String, ManagedSession>>>,
    factory: SessionFactory,
    idle_timeout: Duration,
}

impl McpSessionManager {
    /// Create a new session manager.
    pub fn new(factory: SessionFactory, idle_timeout: Duration) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            factory,
            idle_timeout,
        }
    }

    /// Get or create a `McpServer` for the given session id.
    pub async fn get_or_create(&self, session_id: &str) -> Result<Arc<McpServer>, String> {
        {
            let mut sessions = self.sessions.write().await;
            if let Some(entry) = sessions.get_mut(session_id) {
                entry.last_active = Instant::now();
                return Ok(Arc::clone(&entry.server));
            }
        }

        let provider = (self.factory)().await?;
        let server = Arc::new(McpServer::new(provider));
        let entry = ManagedSession {
            server: Arc::clone(&server),
            last_active: Instant::now(),
        };
        self.sessions
            .write()
            .await
            .insert(session_id.to_string(), entry);
        Ok(server)
    }

    /// Remove sessions that have been idle longer than `idle_timeout`.
    pub async fn reap_idle(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, s| s.last_active.elapsed() < self.idle_timeout);
        before - sessions.len()
    }

    /// Number of active sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Spawn a background task that reaps idle sessions at a fixed interval.
    pub fn spawn_reaper(&self, interval: Duration, cancel: CancellationToken) {
        let sessions = Arc::clone(&self.sessions);
        let timeout = self.idle_timeout;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(interval) => {
                        let mut guard = sessions.write().await;
                        guard.retain(|_, s| s.last_active.elapsed() < timeout);
                    }
                }
            }
        });
    }

    /// Shut down all sessions.
    pub async fn shutdown_all(&self) {
        self.sessions.write().await.clear();
    }
}
