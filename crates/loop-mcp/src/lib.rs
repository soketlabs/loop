//! MCP (Model Context Protocol) client and server for Loop.
//!
//! This crate provides:
//! - `McpClientManager`: manages connections to external MCP servers via stdio or streamable HTTP
//! - `McpServer`: exposes tools over streamable HTTP, abstracted via the `ToolProvider` trait
//!
//! The `ToolProvider` trait decouples the MCP server from concrete tool types,
//! allowing the host crate (`loop-agent`) to provide its own implementation.

pub mod client;
pub mod server;

pub use client::{McpClientManager, McpConnection, McpServerEntry, McpTransport};
pub use server::{McpServer, McpSessionManager, ToolDef, ToolOutput, ToolProvider};
