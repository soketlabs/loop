//! The decision for one call: a function of the mode, the rules and a
//! [`Call`] the gate resolved first. Path patterns in the rules also look
//! at the file system, to match the file a path names through symlinks
//! ([`super::rules::rule_for`]). They look through the call's
//! [`PathFacts`]: a Session's gate fills them from the Workspace env
//! ([`super::paths::GatePaths`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loop_agent::harness::EXIT_PLAN_TOOL;
use loop_protocol::ToolKind;
use serde_json::Value;

use super::capability::{Capability, State};
use super::layers::LayerSource;
use super::paths::{Got, Lookup, PathFacts};
use super::rules::{rule_and_key, ToolPermission};
use super::PermissionMode;
use loop_store::DecidedBy;

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

/// One tool call, with the workspace check [`decide`] needs already worked
/// out ([`Call::resolve`]). Path patterns resolve the path themselves.
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
    /// The directory the tools open a relative path against; it can be
    /// below `workspace` (the git root). [`Call::new`] sets the workspace.
    pub cwd: PathBuf,
    /// More directories relative `deny` and `ask` patterns match against,
    /// such as a parent Session's git root and cwd for a subagent.
    pub rule_bases: Vec<PathBuf>,
    /// The home directory, for `~/` in path patterns.
    pub home: Option<PathBuf>,
    /// Whether the call's `path` resolves inside `workspace`, following
    /// symlinks. Only `write` and `edit` calls are resolved.
    pub inside_workspace: bool,
    /// Under a tool lock: the Session's folder and root as they resolved
    /// when it opened ([`super::tool_lock::Confinement`]).
    pub confinement: Option<std::sync::Arc<super::tool_lock::Confinement>>,
    /// Where the path checks of this call get their facts: the Runtime
    /// host for [`Call::new`] and [`Call::resolve`] (the sync API), the
    /// Session's Workspace env in the gate ([`Call::resolve_with`]).
    pub paths: PathFacts,
}

impl Call {
    /// A call with the tool's default kind, nothing resolved and no home.
    pub fn new(tool: &str, args: Value, workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            tool: tool.to_owned(),
            kind: loop_agent::protocol_events::tool_kind(tool),
            args,
            cwd: workspace.clone(),
            rule_bases: Vec::new(),
            workspace,
            home: None,
            inside_workspace: false,
            confinement: None,
            paths: PathFacts::runtime_host(),
        }
    }

    /// A call of `kind`, with the home directory and the workspace check
    /// read from the Runtime host's file system.
    pub fn resolve(tool: &str, kind: ToolKind, args: &Value, workspace: &Path) -> Self {
        Self::resolve_in(tool, kind, args, workspace, workspace)
    }

    /// [`Call::resolve`] for a cwd below the workspace: a relative `path`
    /// is where the tools open it, under `cwd`.
    pub fn resolve_in(
        tool: &str,
        kind: ToolKind,
        args: &Value,
        workspace: &Path,
        cwd: &Path,
    ) -> Self {
        Self::resolve_with(&PathFacts::runtime_host(), tool, kind, args, workspace, cwd)
    }

    /// [`Call::resolve_in`] with path facts from `paths`; the call keeps
    /// them for its other path checks.
    pub fn resolve_with(
        paths: &PathFacts,
        tool: &str,
        kind: ToolKind,
        args: &Value,
        workspace: &Path,
        cwd: &Path,
    ) -> Self {
        let inside_workspace = is_file_write(tool)
            && args
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| {
                    inside_workspace_in(paths, &cwd.join(path).to_string_lossy(), workspace)
                });
        // A guest env's home is not the Runtime's: `~` stays unexpanded and
        // `~` rules fail closed (`Lookup::home_known`).
        let home = paths.lookup().home_known().then(dirs::home_dir).flatten();
        Self {
            kind,
            home,
            inside_workspace,
            cwd: cwd.to_path_buf(),
            paths: paths.clone(),
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
    decide_why(mode, rules, call).decision
}

/// A [`Decision`] and the step that made it, for the decision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The decision.
    pub decision: Decision,
    /// The step of the check order that made it.
    pub by: DecidedBy,
    /// The `toolPermissions` key that decided, when a rule did.
    pub rule: Option<String>,
    /// The Policy layer that decided, when a layer did.
    pub layer: Option<LayerSource>,
}

impl Verdict {
    fn new(decision: Decision, by: DecidedBy) -> Self {
        Self {
            decision,
            by,
            rule: None,
            layer: None,
        }
    }

    fn rule(mut self, rule: Option<String>) -> Self {
        self.rule = rule;
        self
    }
}

/// [`decide`], with the step that decided.
pub fn decide_why(
    mode: PermissionMode,
    rules: &BTreeMap<String, ToolPermission>,
    call: &Call,
) -> Verdict {
    let (rule, key) = rule_and_key(rules, call);
    let tool = call.tool.as_str();
    if rule.permission == ToolPermission::Deny {
        let unreadable = (tool == "bash" && rule.specific)
            .then(|| call.args.get("command").and_then(Value::as_str))
            .flatten()
            .and_then(loop_agent::harness::tools::shell::unreadable_advice);
        if let Some(advice) = unreadable {
            let pattern = key.as_deref().unwrap_or("bash(…)");
            return Verdict::new(
                Decision::Deny(format!(
                    "`bash` is refused: Loop cannot check this command against the deny rule \
                     `{pattern}` in settings.toolPermissions, because {advice}."
                )),
                DecidedBy::Unreadable,
            )
            .rule(key);
        }
        return Verdict::new(
            Decision::Deny(format!(
                "Tool `{tool}` is denied by settings.toolPermissions"
            )),
            DecidedBy::Rule,
        )
        .rule(key);
    }
    // Ending plan mode always asks the user, and only plan mode has it.
    if tool == EXIT_PLAN_TOOL {
        return match mode {
            PermissionMode::Plan => Verdict::new(Decision::Ask, DecidedBy::Preset),
            _ => Verdict::new(
                Decision::Deny("plan mode is off; carry on with the work".into()),
                DecidedBy::Off,
            ),
        };
    }
    let capability = Capability::of(call);
    match mode.setting(&capability).state_for(call) {
        State::Off => Verdict::new(
            Decision::Deny(off_reason(mode, &capability, tool)),
            DecidedBy::Off,
        ),
        State::On
            if mode.pattern_ask_applies()
                && rule.specific
                && rule.permission == ToolPermission::Ask =>
        {
            Verdict::new(Decision::Ask, DecidedBy::Rule).rule(key)
        }
        State::On => Verdict::new(Decision::Allow, DecidedBy::Preset),
        State::Ask if rule.permission == ToolPermission::Allow && key.is_some() => {
            Verdict::new(Decision::Allow, DecidedBy::Rule).rule(key)
        }
        State::Ask if rule.permission == ToolPermission::Allow => {
            Verdict::new(Decision::Allow, DecidedBy::Preset)
        }
        State::Ask => Verdict::new(Decision::Ask, DecidedBy::Preset),
    }
}

/// Whether `call`'s tool stays in the toolset in `mode`: not when its
/// Capability is off and no scope can turn it on. Only the tool's name and
/// kind matter; `call` has no arguments yet.
pub fn available(mode: PermissionMode, call: &Call) -> bool {
    match mode {
        // Every tool stays offered in plan mode (calls are still checked),
        // so the work can start in the same turn once the plan is approved.
        PermissionMode::Plan => true,
        _ if call.tool == EXIT_PLAN_TOOL => false,
        _ => mode.setting(&Capability::of(call)).may_allow(call),
    }
}

/// Why a call whose Capability is off in `mode` is refused.
fn off_reason(mode: PermissionMode, capability: &Capability, tool: &str) -> String {
    if mode == PermissionMode::Plan {
        return format!(
            "`{tool}` can't run in plan mode: investigate with read-only tools, then call \
             `{EXIT_PLAN_TOOL}` with your plan; once the user approves it you can make changes"
        );
    }
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
/// `workspace`, following symlinks of the parts that exist, on the Runtime
/// host.
pub fn inside_workspace(path: &str, workspace: &Path) -> bool {
    inside_workspace_in(&PathFacts::runtime_host(), path, workspace)
}

/// [`inside_workspace`] with facts from `paths`.
pub(crate) fn inside_workspace_in(paths: &PathFacts, path: &str, workspace: &Path) -> bool {
    let joined = workspace.join(path);
    let (Some(target), Some(root)) = (
        resolve(paths, &joined).path(),
        resolve(paths, workspace).path(),
    ) else {
        return false;
    };
    target.starts_with(root)
}

/// A path as [`resolve`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Resolved: the longest existing ancestor canonical, the rest as
    /// written.
    Path(PathBuf),
    /// Cannot be resolved, and the kernel could not open it either: a link
    /// loop, or `..` below a missing part.
    Gone,
    /// A fact was unknown (an env error, or a limit). Fails closed: a deny
    /// or ask rule that needed this form counts as matching.
    Unknown,
}

impl Resolved {
    /// The path, when it resolved.
    pub(crate) fn path(self) -> Option<PathBuf> {
        match self {
            Self::Path(path) => Some(path),
            _ => None,
        }
    }
}

/// Canonicalise the longest existing ancestor of `path` and append the
/// rest, rejecting `..` in the part that does not exist yet. A dangling
/// symlink is followed to its target, so a write through it lands where
/// the result says. See [`Resolved`] for the other answers.
pub(crate) fn resolve(paths: &PathFacts, path: &Path) -> Resolved {
    resolve_at(paths.lookup(), path, 0)
}

/// Symlinks followed before giving up (the kernel's own limit is 40).
const MAX_LINKS: usize = 40;

fn resolve_at(lookup: &dyn Lookup, path: &Path, links: usize) -> Resolved {
    if path.components().count() > super::paths::MAX_COMPONENTS {
        lookup.note_limit();
        return Resolved::Unknown;
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        match lookup.canonical(&existing) {
            Got::Yes(canonical) => {
                let mut out = canonical;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return Resolved::Path(out);
            }
            Got::Unknown => return Resolved::Unknown,
            Got::No => {}
        }
        // A symlink whose target does not exist: resolve the target.
        match lookup.read_link(&existing) {
            Got::Yes(target) => {
                if links >= MAX_LINKS {
                    return Resolved::Gone;
                }
                let Some(parent) = existing.parent() else {
                    return Resolved::Gone;
                };
                let mut next = parent.join(target);
                for part in rest.iter().rev() {
                    next.push(part);
                }
                return resolve_at(lookup, &next, links + 1);
            }
            Got::Unknown => return Resolved::Unknown,
            Got::No => {}
        }
        let Some(name) = existing.file_name().map(|n| n.to_owned()) else {
            return Resolved::Gone;
        };
        if name == ".." {
            return Resolved::Gone;
        }
        rest.push(name);
        let Some(parent) = existing.parent() else {
            return Resolved::Gone;
        };
        existing = parent.to_path_buf();
    }
}

/// The directory `accept-edits` lets Loop edit: the git root containing
/// `cwd`, or `cwd` itself, on the Runtime host. A Session's gate finds it
/// through its Workspace env ([`super::paths::GatePaths::workspace_root`]).
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

    /// The workspace check and the protected names join a relative path
    /// to the cwd, which can be below the git root.
    #[cfg(unix)]
    #[test]
    fn accept_edits_with_the_cwd_below_the_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        let svc = repo.join("svc");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&svc).unwrap();
        let decision = |path: &str| {
            let call = Call::resolve_in(
                "write",
                ToolKind::Edit,
                &json!({ "path": path }),
                &repo,
                &svc,
            );
            decide(AcceptEdits, &defaults(), &call)
        };
        want("../.git/config", &decision("../.git/config"), "ask");
        want("../x", &decision("../x"), "allow");
        want("../../outside", &decision("../../outside"), "ask");
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
    fn plan_mode_reads_and_asks_before_it_ends() {
        let path = json!({"path": "src/main.rs"});
        let cmd = json!({"command": "cargo test"});
        let ls = json!({"command": "git log | wc -l"});
        let plan = json!({"plan": "1. edit main.rs"});
        let rows: &[(PermissionMode, &str, &Value, &str)] = &[
            (Plan, "read", &path, "allow"),
            (Plan, "grep", &path, "allow"),
            (Plan, "todo", &path, "allow"),
            (Plan, "write", &path, "deny"),
            (Plan, "bash", &cmd, "deny"),
            (Plan, "bash", &ls, "allow"),
            (Plan, "mcp__github__create_issue", &cmd, "deny"),
            (Plan, "exit_plan", &plan, "ask"),
            (Ask, "exit_plan", &plan, "deny"),
            (FullAccess, "exit_plan", &plan, "deny"),
        ];
        for (mode, tool, args, expected) in rows {
            let got = decide(*mode, &defaults(), &call(tool, (*args).clone(), true));
            want(&format!("{mode:?} {tool} {args}"), &got, expected);
        }
        let Decision::Deny(reason) = decide(Plan, &defaults(), &call("edit", path, true)) else {
            panic!("edits are off in plan mode");
        };
        assert!(reason.contains("exit_plan"), "{reason}");

        let tool = |name: &str| Call::new(name, Value::Null, "/ws");
        // Plan mode keeps every tool offered, so the work can start in the
        // same turn once the plan is approved; calls are still checked.
        assert!(available(Plan, &tool("write")));
        assert!(available(Plan, &tool("exit_plan")));
        assert!(!available(Ask, &tool("exit_plan")));
        assert!(!available(ReadOnly, &tool("exit_plan")));
    }

    #[test]
    fn search_tools_run_without_asking_in_every_mode() {
        let rows = [
            ("grep", json!({"pattern": "fn main", "path": "src"})),
            ("glob", json!({"pattern": "*.rs"})),
            ("ls", json!({})),
        ];
        for mode in PermissionMode::ALL {
            for (tool, args) in &rows {
                let got = decide(mode, &defaults(), &call(tool, args.clone(), false));
                assert_eq!(got, Decision::Allow, "{mode:?} {tool}");
                assert!(available(mode, &call(tool, Value::Null, false)));
            }
        }
        let r = crate::tool_approval::permissions_from_settings(
            &[("grep(~/secrets/*)".to_owned(), "deny".to_owned())].into(),
        );
        let secret = Call {
            home: Some("/home/me".into()),
            ..call(
                "grep",
                json!({"pattern": "key", "path": "~/secrets/a"}),
                false,
            )
        };
        assert!(matches!(decide(ReadOnly, &r, &secret), Decision::Deny(_)));
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
