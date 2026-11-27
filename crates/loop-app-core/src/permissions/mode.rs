//! The four permission modes, each a preset over Capabilities, and plan
//! mode.
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
//!
//! `plan` has `read-only`'s preset. The model investigates and proposes a
//! plan with `exit_plan`; when the user approves it, the mode goes back to
//! the one plan mode was entered from (see [`crate::tool_gate::ToolGate`]).

use super::capability::{Capability, Scope, Setting, State};

/// A string that names no [`PermissionMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission mode")]
pub struct ParsePermissionModeError;

/// How much Loop's own tools may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum PermissionMode {
    /// Investigate and propose a plan; nothing changes until the user
    /// approves it. Not in [`Self::ALL`].
    Plan,
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
    /// The four modes `/permissions` lists, strictest first. Plan mode is
    /// entered with `/plan` and left by approving a plan.
    pub const ALL: [Self; 4] = [
        Self::ReadOnly,
        Self::Ask,
        Self::AcceptEdits,
        Self::FullAccess,
    ];

    /// Parse a settings or flag value; `None` when it names no mode. See the
    /// [`FromStr`](std::str::FromStr) impl.
    pub fn parse(value: &str) -> Option<Self> {
        value.parse().ok()
    }

    /// The settings and flag name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::FullAccess => "full-access",
        }
    }

    /// Short label for the footer.
    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
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
            Self::Plan | Self::ReadOnly | Self::Ask => &["read-only"],
            Self::AcceptEdits => &["workspace-write"],
            Self::FullAccess => &["agent-full-access", "full-access"],
        }
    }

    /// One line for the `/permissions` picker.
    pub fn description(self) -> &'static str {
        match self {
            Self::Plan => "investigate and propose a plan; nothing changes until you approve it",
            Self::ReadOnly => "read, search and the web only; no edits or commands",
            Self::Ask => "ask before edits and commands",
            Self::AcceptEdits => "edit files in this workspace without asking; ask for commands",
            Self::FullAccess => "run every tool call without asking",
        }
    }

    /// The Setting this mode's preset gives `capability`.
    pub fn setting(self, capability: &Capability) -> Setting {
        use Capability::{Mcp, Other, ReadFiles, Shell, WebFetch, WebSearch, WriteFiles};
        use State::{Ask as AskFirst, Off, On};
        match (self, capability) {
            (Self::Plan, _) => Self::ReadOnly.setting(capability),
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

impl std::str::FromStr for PermissionMode {
    type Err = ParsePermissionModeError;

    /// Parse a settings or flag value. Case-insensitive; `_` and `-` are the
    /// same; surrounding whitespace is ignored.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(
            match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                "plan" => Self::Plan,
                "read-only" | "readonly" => Self::ReadOnly,
                "ask" | "default" => Self::Ask,
                "accept-edits" | "acceptedits" => Self::AcceptEdits,
                "full-access" | "fullaccess" => Self::FullAccess,
                _ => return Err(ParsePermissionModeError),
            },
        )
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
    fn from_str_accepts_aliases_and_rejects_unknown() {
        assert_eq!("read-only".parse(), Ok(ReadOnly));
        assert_eq!(" ReadOnly ".parse(), Ok(ReadOnly));
        assert_eq!("default".parse(), Ok(Ask));
        assert_eq!("accept_edits".parse(), Ok(AcceptEdits));
        assert_eq!("FULLACCESS".parse(), Ok(FullAccess));
        assert_eq!("plan".parse(), Ok(Plan));
        assert!("yolo".parse::<PermissionMode>().is_err());
        assert!("".parse::<PermissionMode>().is_err());
    }

    #[test]
    fn plan_is_read_only_under_its_own_name() {
        use crate::permissions::capability::Capability::*;
        assert_eq!(PermissionMode::parse("plan"), Some(Plan));
        assert_eq!(Plan.as_str(), "plan");
        assert_eq!(Plan.label(), "plan");
        assert!(Plan < ReadOnly, "plan is the strictest");
        assert!(
            !PermissionMode::ALL.contains(&Plan),
            "the four modes /permissions lists are unchanged"
        );
        for capability in [
            ReadFiles,
            WriteFiles,
            Shell,
            WebFetch,
            WebSearch,
            Mcp("github".into()),
            Other,
        ] {
            assert_eq!(
                Plan.setting(&capability),
                ReadOnly.setting(&capability),
                "{capability:?}"
            );
        }
        assert_eq!(Plan.agent_modes(), ["read-only"]);
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
