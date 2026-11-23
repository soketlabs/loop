//! What a backend supports.

use serde::{Deserialize, Serialize};

/// Features a backend declares. Each must match what the backend can actually
/// do: a surface hides or rejects an operation whose capability is false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Capabilities {
    /// Host-local, atomic stop only when no Sessions are loaded.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub runtime_stop_empty: bool,
    /// Accepts a caller environment in session/new.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_environment: bool,
    /// Provider credentials; host-local only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub host_providers: bool,
    /// MCP OAuth credentials; host-local only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub host_mcp_auth: bool,
    /// Telemetry configuration; host-local only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub host_tracing: bool,
    /// Returns the current Session snapshot without attaching.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_config: bool,
    /// Reads redacted effective workspace settings.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_settings: bool,
    /// Activates Session skills.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_skills: bool,
    /// Starts workflows and streams progress.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workflow: bool,
    /// Can reopen a past session, replaying its history.
    pub load_session: bool,
    /// Can reopen a past session without replaying history.
    pub resume_session: bool,
    /// Can switch models within a session.
    pub set_model: bool,
    /// Has selectable session modes.
    pub modes: bool,
    /// Reports token usage.
    pub usage: bool,
    /// Accepts images in prompts.
    pub images: bool,
    /// Accepts more input while a turn is running.
    pub steering: bool,
    /// Sends file reads and writes to Loop instead of touching disk itself.
    pub fs_client: bool,
    /// Runs commands through Loop's terminal instead of its own.
    pub terminal_client: bool,
    /// Manages several Sessions through list and close requests.
    pub multi_session: bool,
    /// Supports attach, cursor replay and detach.
    pub attach: bool,
    /// Lists and navigates Session branches.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_tree: bool,
    /// Changes Session metadata.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_rename: bool,
    /// Creates separately leased forks and clones.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_fork: bool,
    /// Transfers bounded Session payloads.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_transfer: bool,
    /// Serves immutable transcript pages.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub branch_snapshot: bool,
    /// Has selectable reasoning effort levels (`session/efforts`).
    pub efforts: bool,
    /// `session/attach` returns a Session snapshot with its result.
    pub attach_snapshot: bool,
    /// Compacts the Session's context on request (`session/compact`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub compaction: bool,
    /// Reports Session token statistics (`session/stats`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_stats: bool,
    /// Runs a user command in the Session's environment (`session/shell`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub local_shell: bool,
    /// Reads and overrides the Session's response timeout (`session/ttfb`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_ttfb: bool,
    /// Lists and toggles workspace MCP servers (`workspace/mcp`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_mcp: bool,
    /// Reads and changes workspace web search (`workspace/web_search`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_web_search: bool,
    /// Lists and toggles workspace skills (`workspace/skills`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_skills: bool,
    /// Lists and renders prompt templates (`workspace/templates`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_templates: bool,
    /// Rereads workspace definitions (`workspace/reload`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_reload: bool,
    /// Reads workspace trust; changes it host-locally (`workspace/trust`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_trust: bool,
    /// Reads and replaces the Session's sandbox (`session/sandbox`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_sandbox: bool,
    /// Reads and sets the Session's permission mode (`session/permission_mode`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub permission_mode: bool,
    /// Pages the host's usage log (`runtime/usage`); host-local only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub host_usage: bool,
    /// Reads and saves new-Session defaults (`runtime/defaults`); host-local only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub host_defaults: bool,
    /// Lists loaded Sessions for the Runtime panel (`runtime/sessions`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub runtime_sessions: bool,
    /// The Runtime's tool lock (`loop runtime start --tool-lock`), when it
    /// has one. It holds for every Session for the life of the Runtime: a
    /// client can hide the shell, file edits and workflows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_lock: Option<ToolLock>,
}

/// A Runtime-wide tool lock: a fixed set of tools for every Session, which
/// no setting, Session option or permission mode can widen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolLock {
    /// Chat only: reading files in the Session's workspace and the web
    /// tools. No shell, file writes, Subagents, workflows, MCP servers or
    /// hooks.
    Chat,
    /// A lock this reader does not know. Treat it as locking everything.
    #[serde(other)]
    Unknown,
}

impl ToolLock {
    /// The lock named `name` (`chat`); `None` for any other name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "chat" => Some(Self::Chat),
            _ => None,
        }
    }

    /// The name on the command line and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ToolLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_lock_names() {
        assert_eq!(ToolLock::parse("chat"), Some(ToolLock::Chat));
        assert_eq!(ToolLock::parse("Chat"), None);
        assert_eq!(ToolLock::parse(""), None);
        assert_eq!(ToolLock::parse("unknown"), None);
        let caps = Capabilities {
            tool_lock: Some(ToolLock::Chat),
            ..Capabilities::default()
        };
        let wire = serde_json::to_value(caps).unwrap();
        assert_eq!(wire["tool_lock"], "chat");
        assert!(serde_json::to_value(Capabilities::default())
            .unwrap()
            .get("tool_lock")
            .is_none());
        // A newer lock reads as Unknown, which a client treats as locked.
        let read: Capabilities =
            serde_json::from_value(serde_json::json!({"tool_lock": "docs"})).unwrap();
        assert_eq!(read.tool_lock, Some(ToolLock::Unknown));
    }
}
