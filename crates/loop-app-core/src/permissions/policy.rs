//! The decision for one call: a pure function of the mode, the rules and a
//! [`Call`] the gate resolved first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loop_protocol::ToolKind;
use serde_json::Value;

use super::capability::{Capability, State};
use super::rules::{rule_for, ToolPermission};
use super::PermissionMode;

/// What the mode says about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it without asking.
    Allow,
    /// Ask the user (or refuse, when nobody can be asked).
    Ask,
    /// Refuse it; the reason goes to the model.
    Deny(String),
}

/// One tool call, with everything [`decide`] needs that would otherwise
/// touch the filesystem already worked out ([`Call::resolve`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The tool's name, e.g. `bash` or `mcp__github__create_issue`.
    pub tool: String,
    /// The tool's kind; an MCP tool its server marks read-only is a read.
    pub kind: ToolKind,
    /// The call's arguments.
    pub args: Value,
    /// The directory `accept-edits` may edit.
    pub workspace: PathBuf,
    /// The home directory, for `~/` in path patterns.
    pub home: Option<PathBuf>,
    /// Whether the call's `path` resolves inside `workspace`, following
    /// symlinks. Only `write` and `edit` calls are resolved.
    pub inside_workspace: bool,
}

impl Call {
    /// A call with the tool's default kind, nothing resolved and no home.
    pub fn new(tool: &str, args: Value, workspace: impl Into<PathBuf>) -> Self {
        Self {
            tool: tool.to_owned(),
            kind: loop_agent::protocol_events::tool_kind(tool),
            args,
            workspace: workspace.into(),
            home: None,
            inside_workspace: false,
        }
    }

    /// A call of `kind`, with the home directory and the workspace check
    /// read from the filesystem.
    pub fn resolve(tool: &str, kind: ToolKind, args: &Value, workspace: &Path) -> Self {
        let inside_workspace = is_file_write(tool)
            && args
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| inside_workspace(path, workspace));
        Self {
            kind,
            home: dirs::home_dir(),
            inside_workspace,
            ..Self::new(tool, args.clone(), workspace)
        }
    }

    pub(crate) fn path(&self) -> Option<&str> {
        self.args.get("path").and_then(Value::as_str)
    }
}

/// Tools that write files, by name.
fn is_file_write(tool: &str) -> bool {
    matches!(tool, "write" | "edit")
}

/// The decision for `call` in `mode` under the `toolPermissions` `rules`:
/// a deny rule refuses it; otherwise the mode's preset gives the call's
/// Capability a [`State`], and "ask" defers to the rules.
pub fn decide(
    mode: PermissionMode,
    rules: &BTreeMap<String, ToolPermission>,
    call: &Call,
) -> Decision {
    let rule = rule_for(rules, call);
    let tool = call.tool.as_str();
    if rule.permission == ToolPermission::Deny {
        return Decision::Deny(format!(
            "Tool `{tool}` is denied by settings.toolPermissions"
        ));
    }
    let capability = Capability::of(call);
    match mode.setting(&capability).state_for(call) {
        State::Off => Decision::Deny(off_reason(mode, &capability, tool)),
        State::On
            if mode.pattern_ask_applies()
                && rule.specific
                && rule.permission == ToolPermission::Ask =>
        {
            Decision::Ask
        }
        State::On => Decision::Allow,
        State::Ask if rule.permission == ToolPermission::Allow => Decision::Allow,
        State::Ask => Decision::Ask,
    }
}

/// Whether `call`'s tool stays in the toolset in `mode`: not when its
/// Capability is off and no scope can turn it on. Only the tool's name and
/// kind matter; `call` has no arguments yet.
pub fn available(mode: PermissionMode, call: &Call) -> bool {
    mode.setting(&Capability::of(call)).may_allow(call)
}

/// Why a call whose Capability is off in `mode` is refused.
fn off_reason(mode: PermissionMode, capability: &Capability, tool: &str) -> String {
    let mode = mode.as_str();
    match capability {
        Capability::Shell => format!(
            "{mode} mode runs only commands that read (ls, cat, grep, rg, find, git \
             status/log/diff/show, …), without output redirects; the user can switch with \
             /permissions"
        ),
        _ => {
            format!("`{tool}` is not allowed in {mode} mode; the user can switch with /permissions")
        }
    }
}

/// Whether `path` (relative to `workspace` when not absolute) resolves inside
/// `workspace`, following symlinks of the parts that exist.
pub fn inside_workspace(path: &str, workspace: &Path) -> bool {
    let joined = workspace.join(path);
    let (Some(target), Some(root)) = (resolve(&joined), resolve(workspace)) else {
        return false;
    };
    target.starts_with(root)
}

/// Canonicalise the longest existing ancestor of `path` and append the
/// rest, rejecting `..` in the part that does not exist yet.
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        let name = existing.file_name()?.to_owned();
        if name == ".." {
            return None;
        }
        rest.push(name);
        existing = existing.parent()?.to_path_buf();
    }
}

/// The directory `accept-edits` lets Loop edit: the git root containing
/// `cwd`, or `cwd` itself.
pub fn workspace_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use PermissionMode::*;

    fn defaults() -> BTreeMap<String, ToolPermission> {
        crate::tool_approval::permissions_from_settings(&Default::default())
    }

    /// A call whose path is inside the workspace when `inside` is set; no
    /// filesystem is touched.
    fn call(tool: &str, args: Value, inside: bool) -> Call {
        Call {
            inside_workspace: inside,
            ..Call::new(tool, args, "/ws")
        }
    }

    fn want(row: &str, got: &Decision, want: &str) {
        let ok = match want {
            "allow" => *got == Decision::Allow,
            "ask" => *got == Decision::Ask,
            _ => matches!(got, Decision::Deny(_)),
        };
        assert!(ok, "{row}: want {want}, got {got:?}");
    }

    #[test]
    fn mode_table() {
        let path = json!({"path": "src/main.rs"});
        let cmd = json!({"command": "cargo test"});
        let ls = json!({"command": "git log | wc -l"});
        let url = json!({"url": "https://example.com"});
        let rows: &[(PermissionMode, &str, &Value, &str)] = &[
            (ReadOnly, "read", &path, "allow"),
            (ReadOnly, "web_fetch", &url, "allow"),
            (ReadOnly, "web_search", &url, "allow"),
            (ReadOnly, "write", &path, "deny"),
            (ReadOnly, "edit", &path, "deny"),
            (ReadOnly, "bash", &cmd, "deny"),
            (ReadOnly, "bash", &ls, "allow"),
            (ReadOnly, "mcp__github__create_issue", &cmd, "deny"),
            (Ask, "read", &path, "allow"),
            (Ask, "web_fetch", &url, "allow"),
            (Ask, "write", &path, "ask"),
            (Ask, "edit", &path, "ask"),
            (Ask, "bash", &cmd, "ask"),
            (Ask, "bash", &ls, "ask"),
            (Ask, "mcp__github__create_issue", &cmd, "allow"),
            (AcceptEdits, "read", &path, "allow"),
            (AcceptEdits, "web_search", &url, "allow"),
            (AcceptEdits, "write", &path, "allow"),
            (AcceptEdits, "edit", &path, "allow"),
            (AcceptEdits, "bash", &cmd, "ask"),
            (AcceptEdits, "mcp__github__create_issue", &cmd, "allow"),
            (FullAccess, "write", &path, "allow"),
            (FullAccess, "bash", &cmd, "allow"),
            (FullAccess, "web_fetch", &url, "allow"),
            (FullAccess, "mcp__github__create_issue", &cmd, "allow"),
        ];
        for (mode, tool, args, expected) in rows {
            let got = decide(*mode, &defaults(), &call(tool, (*args).clone(), true));
            want(&format!("{mode:?} {tool} {args}"), &got, expected);
        }
    }

    #[test]
    fn accept_edits_asks_outside_the_workspace_and_for_protected_files() {
        for (path, inside) in [
            ("../elsewhere.txt", false),
            ("/etc/hosts", false),
            (".git/config", true),
            ("src/.env", true),
            (".env.local", true),
            (".loop/settings.json", true),
        ] {
            let got = decide(
                AcceptEdits,
                &defaults(),
                &call("write", json!({ "path": path }), inside),
            );
            assert_eq!(got, Decision::Ask, "{path}");
        }
        let got = decide(
            AcceptEdits,
            &defaults(),
            &call("write", json!({"path": "src/new/file.rs"}), true),
        );
        assert_eq!(got, Decision::Allow);
    }

    #[test]
    fn rules_under_the_modes() {
        let r = crate::tool_approval::permissions_from_settings(
            &[
                ("bash", "deny"),
                ("bash(cargo test*)", "allow"),
                ("write(src/generated/*)", "ask"),
                ("write", "allow"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        );
        let ls = call("bash", json!({"command": "ls"}), false);
        let tests = call("bash", json!({"command": "cargo test -p x"}), false);
        let generated = call("write", json!({"path": "src/generated/a.rs"}), true);
        let outside = call("write", json!({"path": "/tmp/x"}), false);
        let rows: &[(PermissionMode, &Call, &str)] = &[
            // A deny rule applies in every mode, even to a read-only command.
            (ReadOnly, &ls, "deny"),
            (FullAccess, &ls, "deny"),
            // An allow pattern beats the plain deny, but not read-only.
            (Ask, &tests, "allow"),
            (ReadOnly, &tests, "deny"),
            // A pattern ask still asks in accept-edits; a plain ask does not.
            (AcceptEdits, &generated, "ask"),
            (FullAccess, &generated, "allow"),
            // A plain allow lifts accept-edits' ask outside the workspace.
            (AcceptEdits, &outside, "allow"),
            (Ask, &outside, "allow"),
        ];
        for (mode, c, expected) in rows {
            let got = decide(*mode, &r, c);
            want(&format!("{mode:?} {} {}", c.tool, c.args), &got, expected);
        }
    }

    #[test]
    fn read_only_allows_mcp_tools_marked_read_only() {
        let mut mcp = call("mcp__github__list_issues", json!({}), false);
        mcp.kind = ToolKind::Read;
        assert_eq!(decide(ReadOnly, &defaults(), &mcp), Decision::Allow);
        mcp.kind = ToolKind::Other;
        assert!(matches!(
            decide(ReadOnly, &defaults(), &mcp),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn deny_rules_apply_in_every_mode() {
        let r = crate::tool_approval::permissions_from_settings(
            &[("read".to_owned(), "deny".to_owned())].into(),
        );
        for mode in PermissionMode::ALL {
            let got = decide(mode, &r, &call("read", json!({}), true));
            assert!(matches!(got, Decision::Deny(_)), "{mode:?}");
        }
    }

    #[test]
    fn resolving_a_call_checks_the_workspace_once() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("repo");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let write = |path: &str| {
            Call::resolve("write", ToolKind::Edit, &json!({ "path": path }), &ws).inside_workspace
        };
        assert!(write("src/new/file.rs"));
        assert!(write(&ws.join("a.txt").to_string_lossy()));
        assert!(!write("../elsewhere.txt"));
        assert!(!write("/etc/hosts"));
        assert!(
            !Call::resolve("read", ToolKind::Read, &json!({"path": "a"}), &ws).inside_workspace,
            "only writes are resolved"
        );
    }

    #[test]
    fn off_capabilities_leave_the_toolset() {
        let tool = |name: &str, kind: Option<ToolKind>| {
            let mut call = Call::new(name, Value::Null, "/ws");
            if let Some(kind) = kind {
                call.kind = kind;
            }
            call
        };
        let rows = [
            (ReadOnly, tool("write", None), false),
            (ReadOnly, tool("edit", None), false),
            (ReadOnly, tool("mcp__github__create_issue", None), false),
            (
                ReadOnly,
                tool("mcp__github__list_issues", Some(ToolKind::Read)),
                true,
            ),
            // Read-only commands still run.
            (ReadOnly, tool("bash", None), true),
            (ReadOnly, tool("read", None), true),
            (ReadOnly, tool("web_fetch", None), true),
            (Ask, tool("write", None), true),
            (AcceptEdits, tool("mcp__github__create_issue", None), true),
            (FullAccess, tool("bash", None), true),
        ];
        for (mode, call, want) in rows {
            assert_eq!(available(mode, &call), want, "{mode:?} {}", call.tool);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_outside() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
        assert!(!inside_workspace("link/file.txt", &ws));
        assert!(inside_workspace("file.txt", &ws));
    }
}
