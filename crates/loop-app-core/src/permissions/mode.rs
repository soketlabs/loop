//! The four permission modes, each a preset over Capabilities.
//!
//! | Capability | `read-only` | `ask` | `accept-edits` | `full-access` |
//! |---|---|---|---|---|
//! | read files | on | ask | ask | on |
//! | write files | off | ask | ask; on within the workspace | on |
//! | shell | off; on within read-only commands | ask | ask | on |
//! | web fetch, web search | on | ask | on | on |
//! | each MCP server | off; on within `readOnlyHint` | ask | ask | on |
//! | other tools | on | ask | ask | on |
//!
//! "Ask" defers to `toolPermissions`: an allow rule runs the call without
//! asking. That is why `ask` mode reads files and calls MCP tools freely.

use super::capability::{Capability, Scope, Setting, State};

/// How much Loop's own tools may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum PermissionMode {
    /// Reading, searching and the web only.
    ReadOnly,
    /// Ask before edits and commands (`toolPermissions`).
    #[default]
    Ask,
    /// Edit files in the workspace without asking; ask for commands.
    AcceptEdits,
    /// Run every tool call without asking.
    FullAccess,
}

impl PermissionMode {
    /// Every mode, strictest first.
    pub const ALL: [Self; 4] = [
        Self::ReadOnly,
        Self::Ask,
        Self::AcceptEdits,
        Self::FullAccess,
    ];

    /// Parse a settings or flag value.
    pub fn parse(value: &str) -> Option<Self> {
        Some(
            match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                "read-only" | "readonly" => Self::ReadOnly,
                "ask" | "default" => Self::Ask,
                "accept-edits" | "acceptedits" => Self::AcceptEdits,
                "full-access" | "fullaccess" => Self::FullAccess,
                _ => return None,
            },
        )
    }

    /// The settings and flag name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::FullAccess => "full-access",
        }
    }

    /// Short label for the footer.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept edits",
            Self::FullAccess => "full access",
        }
    }

    /// The agent mode to ask an ACP agent for, in the order to try: Codex
    /// calls them `read-only`, `workspace-write` and `agent-full-access`.
    /// In `ask`, the agent asks before edits and network use, and Loop asks
    /// the user.
    pub fn agent_modes(self) -> &'static [&'static str] {
        match self {
            Self::ReadOnly | Self::Ask => &["read-only"],
            Self::AcceptEdits => &["workspace-write"],
            Self::FullAccess => &["agent-full-access", "full-access"],
        }
    }

    /// One line for the `/permissions` picker.
    pub fn description(self) -> &'static str {
        match self {
            Self::ReadOnly => "read, search and the web only; no edits or commands",
            Self::Ask => "ask before edits and commands",
            Self::AcceptEdits => "edit files in this workspace without asking; ask for commands",
            Self::FullAccess => "run every tool call without asking",
        }
    }

    /// The Setting this mode's preset gives `capability`.
    pub fn setting(self, capability: &Capability) -> Setting {
        use Capability::*;
        use State::{Ask as AskFirst, Off, On};
        match (self, capability) {
            (Self::FullAccess, _) => Setting::always(On),
            (Self::Ask, _) => Setting::always(AskFirst),
            (Self::ReadOnly, WriteFiles) => Setting::always(Off),
            (Self::ReadOnly, Shell) => Setting::within(Off, Scope::ReadOnlyCommands, On),
            (Self::ReadOnly, Mcp(_)) => Setting::within(Off, Scope::ReadOnlyHint, On),
            (Self::ReadOnly, ReadFiles | WebFetch | WebSearch | Other) => Setting::always(On),
            (Self::AcceptEdits, WriteFiles) => Setting::within(AskFirst, Scope::Workspace, On),
            (Self::AcceptEdits, WebFetch | WebSearch) => Setting::always(On),
            (Self::AcceptEdits, ReadFiles | Shell | Mcp(_) | Other) => Setting::always(AskFirst),
        }
    }

    /// Whether a `toolPermissions` pattern `ask`, such as
    /// `write(src/generated/*)`, still asks where this mode turns a
    /// Capability on.
    pub fn pattern_ask_applies(self) -> bool {
        self == Self::AcceptEdits
    }
}

#[cfg(test)]
mod tests {
    use super::PermissionMode::{self, *};

    #[test]
    fn parse_and_names_round_trip() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(PermissionMode::parse("Accept_Edits"), Some(AcceptEdits));
        assert_eq!(PermissionMode::parse("yolo"), None);
        assert!(ReadOnly < Ask && Ask < AcceptEdits && AcceptEdits < FullAccess);
    }

    #[test]
    fn each_preset_is_stricter_than_the_next() {
        use crate::permissions::capability::Capability::*;
        use crate::permissions::{Call, State};
        let capabilities = [
            ReadFiles,
            WriteFiles,
            Shell,
            WebFetch,
            WebSearch,
            Mcp("github".into()),
            Other,
        ];
        let call = Call::new("bash", serde_json::json!({"command": "rm -rf x"}), "/ws");
        for pair in PermissionMode::ALL.windows(2) {
            for capability in &capabilities {
                let state = |mode: PermissionMode| mode.setting(capability).state;
                // `read-only` turns reads on without asking, `ask` asks: it is
                // stricter on edits and commands, not on reads.
                if pair[0] == ReadOnly && state(ReadOnly) == State::On {
                    continue;
                }
                assert!(
                    state(pair[0]) <= state(pair[1]),
                    "{capability:?}: {:?} is looser than {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
        assert_eq!(
            ReadOnly.setting(&Shell).state_for(&call),
            State::Off,
            "a command that writes is off in read-only"
        );
    }
}
