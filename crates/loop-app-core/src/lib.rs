//! Shared bootstrap, configuration, and runtime wiring for Loop CLI and desktop.

/// Tracing target of log records that repeat a warning already printed to
/// the user's terminal. A log layer that writes to that terminal skips it.
pub const USER_NOTICE_TARGET: &str = "loop_app_core::user_notice";

pub mod agent_gate;
pub mod agents;
pub mod config;
pub mod extensions;
pub mod hook_command;
pub mod hooks_load;
pub mod interaction;
pub mod model_selection;
pub mod permissions;
pub mod resources;
pub mod runtime;
pub mod runtime_instance;
pub mod sandbox_builder;
pub mod sessions;
pub mod system_prompt;
pub mod theme;
pub mod tool_approval;
pub mod tool_gate;
pub mod web_tools;

pub use config::*;
pub use runtime::{
    bootstrap, build_models, build_tools, mcp_server_entries, BootstrapOpts, ConnectedProvider,
    GlobalResources, OpenedSession, Runtime, SessionFactory, WorkspaceResources,
};
pub use tool_approval::ToolApprovalBridge;
