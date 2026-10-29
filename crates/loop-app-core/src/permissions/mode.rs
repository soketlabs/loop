//! The four permission modes.

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
}
