//! MCP client manager: connect to external MCP servers via stdio or streamable HTTP.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::RwLock;

/// Transport configuration for connecting to an MCP server.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Spawn a local child process and communicate over stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Connect to a remote MCP server over streamable HTTP.
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

/// Configuration for an external MCP server to connect to.
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    /// Human-readable name (used as key and tool prefix).
    pub name: String,
    /// How to reach the server.
    pub transport: McpTransport,
}

/// A live connection to an MCP server.
pub struct McpConnection {
    /// Server name.
    pub name: String,
    /// The running rmcp client service.
    pub client: RunningService<RoleClient, ()>,
    /// Tools discovered from this server.
    pub tools: Vec<rmcp::model::Tool>,
}

/// Manages connections to multiple external MCP servers.
pub struct McpClientManager {
    connections: Arc<RwLock<HashMap<String, McpConnection>>>,
}

impl McpClientManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect to a single MCP server. Returns the tool count on success.
    pub async fn connect(&self, entry: &McpServerEntry) -> Result<usize, String> {
        let client = match &entry.transport {
            McpTransport::Stdio { command, args, env } => {
                let args = args.clone();
                let envs: Vec<(String, String)> =
                    env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

                let cmd = tokio::process::Command::new(command).configure(|cmd| {
                    for arg in &args {
                        cmd.arg(arg);
                    }
                    for (k, v) in &envs {
                        cmd.env(k, v);
                    }
                });

                let transport = TokioChildProcess::new(cmd).map_err(|e| {
                    format!("failed to spawn MCP server '{}': {e}", entry.name)
                })?;

                ().serve(transport).await.map_err(|e| {
                    format!(
                        "failed to initialize MCP session with '{}': {e}",
                        entry.name
                    )
                })?
            }
            McpTransport::Http { url, .. } => {
                let transport = StreamableHttpClientTransport::from_uri(url.as_str());

                ().serve(transport).await.map_err(|e| {
                    format!(
                        "failed to connect to MCP server '{}' at {}: {e}",
                        entry.name, url
                    )
                })?
            }
        };

        let tools_result = client.list_tools(None).await.map_err(|e| {
            format!("failed to list tools from '{}': {e}", entry.name)
        })?;

        let tools = tools_result.tools;
        let count = tools.len();

        let conn = McpConnection {
            name: entry.name.clone(),
            client,
            tools,
        };

        self.connections.write().await.insert(entry.name.clone(), conn);
        Ok(count)
    }

    /// Connect to all entries, logging errors but not failing the whole batch.
    pub async fn connect_all(
        &self,
        entries: &[McpServerEntry],
    ) -> Vec<(String, Result<usize, String>)> {
        let mut results = Vec::new();
        for entry in entries {
            let result = self.connect(entry).await;
            results.push((entry.name.clone(), result));
        }
        results
    }

    /// Disconnect a single server by name. Returns true if it was connected.
    pub async fn disconnect(&self, name: &str) -> bool {
        if let Some(conn) = self.connections.write().await.remove(name) {
            let _ = conn.client.cancel().await;
            true
        } else {
            false
        }
    }

    /// Disconnect all servers.
    pub async fn disconnect_all(&self) {
        let mut conns = self.connections.write().await;
        for (_, conn) in conns.drain() {
            let _ = conn.client.cancel().await;
        }
    }

    /// List connected server names and their tool counts.
    pub async fn list_connections(&self) -> Vec<(String, usize)> {
        self.connections
            .read()
            .await
            .iter()
            .map(|(name, conn)| (name.clone(), conn.tools.len()))
            .collect()
    }

    /// Get a read lock on connections (for bridge tool generation).
    pub fn connections(&self) -> &Arc<RwLock<HashMap<String, McpConnection>>> {
        &self.connections
    }
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}
