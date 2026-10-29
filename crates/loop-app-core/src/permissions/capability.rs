//! Capabilities: the things a tool call may do, each with a state of off,
//! ask or on and an optional scope.
//!
//! A [`Setting`] has a base state and, optionally, a different state for
//! calls within a [`Scope`]: `accept-edits` sets file writes to "ask, on
//! within the workspace", and `read-only` sets the shell to "off, on within
//! read-only commands".

use loop_protocol::ToolKind;
use serde_json::Value;

use super::rules::read_only_command;
use super::Call;

/// One thing a tool call may do.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Read and search files.
    ReadFiles,
    /// Write, edit, move or delete files.
    WriteFiles,
    /// Run shell commands.
    Shell,
    /// Fetch web pages.
    WebFetch,
    /// Search the web.
    WebSearch,
    /// Call the tools of one MCP server.
    Mcp(String),
    /// Tools no other Capability covers.
    Other,
}

impl Capability {
    /// The Capability `call` exercises, by tool name and then by kind.
    pub fn of(call: &Call) -> Self {
        match call.tool.as_str() {
            "write" | "edit" => return Self::WriteFiles,
            "bash" => return Self::Shell,
            "web_search" => return Self::WebSearch,
            _ => {}
        }
        if let Some(rest) = call.tool.strip_prefix("mcp__") {
            let server = rest.split_once("__").map_or(rest, |(server, _)| server);
            return Self::Mcp(server.to_owned());
        }
        match call.kind {
            ToolKind::Read | ToolKind::Search => Self::ReadFiles,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move => Self::WriteFiles,
            ToolKind::Execute => Self::Shell,
            ToolKind::Fetch => Self::WebFetch,
            _ => Self::Other,
        }
    }
}

/// Whether a Capability may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Unavailable.
    Off,
    /// Ask first, unless a `toolPermissions` allow rule covers the call.
    Ask,
    /// Use it without asking.
    On,
}

/// Which calls a [`Setting`]'s scoped state applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Paths inside the workspace, except `.git/`, `.loop/` and `.env*`.
    Workspace,
    /// Shell commands that only read ([`read_only_command`]).
    ReadOnlyCommands,
    /// MCP tools their server marks read-only (`readOnlyHint`).
    ReadOnlyHint,
}

impl Scope {
    /// Whether `call` is within the scope.
    pub fn contains(&self, call: &Call) -> bool {
        match self {
            Self::Workspace => {
                call.inside_workspace && call.path().is_some_and(|path| !protected(path))
            }
            Self::ReadOnlyCommands => {
                call.tool == "bash"
                    && call
                        .args
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(read_only_command)
            }
            Self::ReadOnlyHint => call.kind == ToolKind::Read,
        }
    }
}

impl Scope {
    /// Whether some call of `call`'s tool could be within the scope, before
    /// its arguments are known.
    pub fn may_contain(&self, call: &Call) -> bool {
        match self {
            Self::Workspace => matches!(call.tool.as_str(), "write" | "edit"),
            Self::ReadOnlyCommands => call.tool == "bash",
            Self::ReadOnlyHint => self.contains(call),
        }
    }
}

/// Paths `accept-edits` still asks about: version control, Loop's own
/// settings and environment files.
fn protected(path: &str) -> bool {
    use std::path::{Component, Path};
    let path = Path::new(path);
    path.components()
        .any(|c| matches!(c, Component::Normal(name) if name == ".git" || name == ".loop"))
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(".env"))
}

/// The state of one Capability: `state`, or `within`'s state for calls in
/// its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// The state outside the scope, or everywhere without one.
    pub state: State,
    /// A different state for calls within a scope.
    pub within: Option<(Scope, State)>,
}

impl Setting {
    /// The same state for every call.
    pub const fn always(state: State) -> Self {
        Self {
            state,
            within: None,
        }
    }

    /// `state`, but `inner` within `scope`.
    pub const fn within(state: State, scope: Scope, inner: State) -> Self {
        Self {
            state,
            within: Some((scope, inner)),
        }
    }

    /// Whether some call of `call`'s tool could be anything but off.
    pub fn may_allow(&self, call: &Call) -> bool {
        self.state != State::Off
            || self
                .within
                .as_ref()
                .is_some_and(|(scope, inner)| *inner != State::Off && scope.may_contain(call))
    }

    /// The state for `call`.
    pub fn state_for(&self, call: &Call) -> State {
        match &self.within {
            Some((scope, inner)) if scope.contains(call) => *inner,
            _ => self.state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str) -> Call {
        Call::new(tool, json!({}), "/ws")
    }

    #[test]
    fn calls_map_to_capabilities() {
        let rows = [
            ("read", Capability::ReadFiles),
            ("grep", Capability::ReadFiles),
            ("write", Capability::WriteFiles),
            ("edit", Capability::WriteFiles),
            ("bash", Capability::Shell),
            ("web_fetch", Capability::WebFetch),
            ("web_search", Capability::WebSearch),
            (
                "mcp__github__create_issue",
                Capability::Mcp("github".into()),
            ),
            ("mcp__odd", Capability::Mcp("odd".into())),
            ("todo", Capability::Other),
        ];
        for (tool, capability) in rows {
            assert_eq!(Capability::of(&call(tool)), capability, "{tool}");
        }
        let mut delete = call("remove");
        delete.kind = ToolKind::Delete;
        assert_eq!(Capability::of(&delete), Capability::WriteFiles);
    }

    #[test]
    fn a_scope_changes_the_state_only_within_it() {
        let shell = Setting::within(State::Off, Scope::ReadOnlyCommands, State::On);
        let bash = |command: &str| Call::new("bash", json!({ "command": command }), "/ws");
        assert_eq!(shell.state_for(&bash("git status")), State::On);
        assert_eq!(shell.state_for(&bash("git push")), State::Off);

        let write = Setting::within(State::Ask, Scope::Workspace, State::On);
        let file = |path: &str, inside: bool| Call {
            inside_workspace: inside,
            ..Call::new("write", json!({ "path": path }), "/ws")
        };
        assert_eq!(write.state_for(&file("src/a.rs", true)), State::On);
        assert_eq!(write.state_for(&file("/etc/hosts", false)), State::Ask);
        for path in [".git/config", ".loop/settings.json", "src/.env.local"] {
            assert_eq!(write.state_for(&file(path, true)), State::Ask, "{path}");
        }

        let mcp = Setting::within(State::Off, Scope::ReadOnlyHint, State::On);
        let mut list = call("mcp__github__list_issues");
        assert_eq!(mcp.state_for(&list), State::Off);
        list.kind = ToolKind::Read;
        assert_eq!(mcp.state_for(&list), State::On);
    }

    #[test]
    fn off_is_the_strictest_state() {
        assert!(State::Off < State::Ask && State::Ask < State::On);
    }
}
