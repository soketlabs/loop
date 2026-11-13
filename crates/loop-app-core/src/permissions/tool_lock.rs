//! The Runtime's tool lock as a Policy layer (S4 §3 and §4).
//!
//! `loop runtime start --tool-lock chat` (or `LOOP_TOOL_LOCK=chat`) fixes
//! one layer above every other, from [`LayerSource::Runtime`]:
//!
//! - a tool list ([`Layer::tools`]): every tool outside
//!   [`loop_agent::harness::tools::lock::CHAT_TOOLS`] is off, so a new tool,
//!   an MCP tool or the `subagent` tool is off too (fail closed);
//! - the shell, file writes and other tools are off, and Locked;
//! - reads are on only inside the Session's folder (its cwd).
//!
//! "Off" removes the tool from the toolset and refuses a call that arrives
//! anyway. Lower layers, the permission mode, allow rules, hooks and grants
//! can only tighten it. The gate adds the layer on every check
//! ([`crate::tool_gate::ToolGate::lock_tools`]), so replacing the Policy
//! never drops it.

use loop_agent::harness::tools::lock::CHAT_TOOLS;
pub use loop_agent::harness::tools::lock::{classify, offers, ToolClass, ToolLock, KNOWN_TOOLS};

use super::capability::{Capability, Scope, Setting, State};
use super::layers::{Layer, LayerSource};

/// The environment variable that sets the lock when the flag does not.
pub const ENV: &str = "LOOP_TOOL_LOCK";

/// The Policy layer for `lock`.
pub fn layer(lock: ToolLock) -> Layer {
    let tools: &[&str] = match lock {
        ToolLock::Chat => CHAT_TOOLS,
        ToolLock::Unknown => &[],
    };
    let off = Setting::always(State::Off);
    Layer::new(LayerSource::Runtime)
        .set(Capability::Shell, off.clone())
        .lock(Capability::Shell)
        .set(Capability::WriteFiles, off.clone())
        .lock(Capability::WriteFiles)
        .set(Capability::Other, off)
        .lock(Capability::Other)
        .set(
            Capability::ReadFiles,
            Setting::within(State::Off, Scope::InsideCwd, State::On),
        )
        .only_tools(tools.iter().copied())
}

/// Why the lock could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolLockError {
    /// The flag or the environment variable names no known lock.
    #[error("unknown tool lock `{value}` in {from}: the only lock is `chat`")]
    Unknown {
        /// What was given.
        value: String,
        /// `--tool-lock` or the environment variable.
        from: &'static str,
    },
}

/// The lock from the `--tool-lock` flag, else from [`ENV`]. An empty
/// variable means no lock; any other unknown name is an error, so a typo
/// never starts a Runtime without the lock it asked for.
pub fn resolve(flag: Option<&str>, env: Option<&str>) -> Result<Option<ToolLock>, ToolLockError> {
    let (value, from) = match (flag, env) {
        (Some(flag), _) => (flag, "--tool-lock"),
        (None, Some(env)) if !env.is_empty() => (env, ENV),
        _ => return Ok(None),
    };
    ToolLock::parse(value)
        .map(Some)
        .ok_or_else(|| ToolLockError::Unknown {
            value: value.to_owned(),
            from,
        })
}

/// Whether a skill may run code: its folder holds anything but text files
/// (`.md`, `.markdown`, `.txt`), such as a script the skill tells the model
/// to run, or a symlink. A folder that cannot be read, or is too deep or too
/// big to check, counts as running code.
pub fn skill_runs_code(skill: &loop_agent::harness::types::Skill) -> bool {
    const TEXT: &[&str] = &["md", "markdown", "txt"];
    let Some(dir) = skill.path.parent() else {
        return true;
    };
    let walk = walkdir::WalkDir::new(dir).follow_links(false).max_depth(6);
    for (seen, entry) in walk.into_iter().enumerate() {
        let Ok(entry) = entry else {
            return true;
        };
        let kind = entry.file_type();
        if seen > 500 || kind.is_symlink() || (kind.is_dir() && entry.depth() == 6) {
            return true;
        }
        if kind.is_dir() {
            continue;
        }
        let text = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| TEXT.contains(&e.to_ascii_lowercase().as_str()));
        if !text {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Call, Decision, PermissionMode, Policy};
    use serde_json::json;

    fn defaults() -> std::collections::BTreeMap<String, crate::permissions::ToolPermission> {
        crate::tool_approval::permissions_from_settings(&Default::default())
    }

    #[test]
    fn the_flag_wins_and_a_typo_is_an_error() {
        assert_eq!(resolve(None, None), Ok(None));
        assert_eq!(resolve(None, Some("")), Ok(None));
        assert_eq!(resolve(Some("chat"), None), Ok(Some(ToolLock::Chat)));
        assert_eq!(resolve(None, Some("chat")), Ok(Some(ToolLock::Chat)));
        assert_eq!(
            resolve(Some("chat"), Some("nope")),
            Ok(Some(ToolLock::Chat))
        );
        let error = resolve(Some("caht"), None).unwrap_err();
        assert!(error.to_string().contains("--tool-lock"), "{error}");
        let error = resolve(None, Some("CHAT")).unwrap_err();
        assert!(error.to_string().contains(ENV), "{error}");
        assert!(resolve(Some(""), None).is_err(), "an empty flag is a typo");
    }

    /// Every tool Loop registers, in every mode: only the chat tools stay,
    /// and every other one is refused at call time too.
    #[test]
    fn the_layer_removes_every_tool_but_the_chat_tools_in_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let (policy, _) = Policy::new(vec![layer(ToolLock::Chat)]);
        let mut tools: Vec<&str> = KNOWN_TOOLS.to_vec();
        tools.extend([
            "mcp__fs__read_file",
            "mcp__gh__create_issue",
            "find",
            "new_tool",
        ]);
        for mode in [
            PermissionMode::ReadOnly,
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::FullAccess,
            PermissionMode::Plan,
        ] {
            for tool in &tools {
                let offered = offers(ToolLock::Chat, tool);
                let bare = Call::new(tool, serde_json::Value::Null, &ws);
                assert_eq!(
                    policy.available(mode, &bare),
                    offered,
                    "{mode:?} {tool} offered"
                );
                let call = Call::new(tool, json!({"path": "a.txt", "command": "true"}), &ws);
                let decision = policy.decide(mode, &defaults(), &call);
                if !offered {
                    assert!(
                        matches!(decision, Decision::Deny(_)),
                        "{mode:?} {tool}: {decision:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn reads_stay_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // A `.git` above the Session's folder must not widen reads to the
        // git root: `sibling` is another user's folder.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("sibling")).unwrap();
        std::fs::write(root.join("sibling/secret.txt"), "secret").unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join("docs")).unwrap();
        std::fs::write(ws.join("docs/a.txt"), "a").unwrap();
        std::fs::write(root.join("other.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("other.txt"), ws.join("link")).unwrap();
        let (policy, _) = Policy::new(vec![layer(ToolLock::Chat)]);
        let decide = |tool: &str, args: serde_json::Value| {
            let call = Call {
                cwd: ws.clone(),
                ..Call::new(tool, args, &root)
            };
            policy.decide(PermissionMode::FullAccess, &defaults(), &call)
        };
        assert_eq!(
            decide("read", json!({"path": "docs/a.txt"})),
            Decision::Allow
        );
        assert_eq!(decide("grep", json!({"pattern": "a"})), Decision::Allow);
        assert_eq!(decide("ls", json!({})), Decision::Allow);
        assert_eq!(
            decide("glob", json!({"pattern": "*", "path": "docs"})),
            Decision::Allow
        );
        for (tool, args) in [
            ("read", json!({"path": "../other.txt"})),
            ("read", json!({"path": root.join("other.txt")})),
            ("read", json!({"path": "/etc/passwd"})),
            ("grep", json!({"pattern": "x", "path": ".."})),
            ("ls", json!({"path": "/"})),
            ("read", json!({})),
            ("read", json!({"path": "../sibling/secret.txt"})),
            (
                "grep",
                json!({"pattern": "secret", "path": root.join("sibling")}),
            ),
            ("glob", json!({"pattern": "*", "path": "../"})),
        ] {
            assert!(
                matches!(decide(tool, args.clone()), Decision::Deny(_)),
                "{tool} {args}"
            );
        }
        #[cfg(unix)]
        assert!(matches!(
            decide("read", json!({"path": "link"})),
            Decision::Deny(_)
        ));
    }

    /// Lower layers (user, project, Session) and allow rules can't widen
    /// the lock; a Lock makes Loop ignore their settings and say so.
    #[test]
    fn lower_layers_and_allow_rules_cannot_widen_the_lock() {
        let on = Setting::always(State::On);
        let mut layers = vec![layer(ToolLock::Chat)];
        for source in [
            LayerSource::Managed,
            LayerSource::User,
            LayerSource::Project,
            LayerSource::Session,
        ] {
            layers.push(
                Layer::new(source)
                    .set(Capability::Shell, on.clone())
                    .set(Capability::WriteFiles, on.clone())
                    .set(Capability::Other, on.clone())
                    .set(Capability::ReadFiles, on.clone())
                    .only_tools(["bash", "write", "read", "subagent"]),
            );
        }
        let (policy, notices) = Policy::new(layers);
        assert!(
            notices
                .iter()
                .any(|n| n.contains("the Runtime's tool lock locks the shell")),
            "{notices:?}"
        );
        let rules = crate::tool_approval::permissions_from_settings(
            &[
                ("bash".to_owned(), "allow".to_owned()),
                ("write".to_owned(), "allow".to_owned()),
                ("subagent".to_owned(), "allow".to_owned()),
            ]
            .into(),
        );
        for tool in ["bash", "write", "edit", "subagent", "todo"] {
            let call = Call::new(tool, json!({"path": "a", "command": "true"}), "/ws");
            assert!(
                !policy.available(PermissionMode::FullAccess, &call),
                "{tool}"
            );
            let Decision::Deny(reason) = policy.decide(PermissionMode::FullAccess, &rules, &call)
            else {
                panic!("{tool} allowed");
            };
            assert!(reason.contains("the Runtime's tool lock"), "{reason}");
        }
        // A lower list only tightens: `grep` is on the lock's list but not
        // on theirs.
        let grep = Call::new("grep", serde_json::Value::Null, "/ws");
        assert!(!policy.available(PermissionMode::FullAccess, &grep));
    }

    /// The gate replaces any copy of the layer, so a Policy built from an
    /// old one (as `subagent::confine` does) holds exactly one.
    #[test]
    fn with_only_keeps_one_lock_layer() {
        let (base, _) = Policy::new(vec![layer(ToolLock::Chat)]);
        let (base, _) = base.with(Layer::new(LayerSource::Session));
        let twice = base.with_only(layer(ToolLock::Chat));
        assert_eq!(twice, base.with_only(layer(ToolLock::Chat)));
        let read = Call::new("read", serde_json::Value::Null, "/ws");
        assert!(twice.available(PermissionMode::Ask, &read));
    }

    #[test]
    fn skills_with_files_that_are_not_text_run_code() {
        let dir = tempfile::tempdir().unwrap();
        let skill = |name: &str, files: &[&str]| {
            let root = dir.path().join(name);
            std::fs::create_dir_all(root.join("refs")).unwrap();
            std::fs::write(root.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
            for file in files {
                std::fs::write(root.join(file), "x").unwrap();
            }
            loop_agent::harness::types::Skill {
                name: name.into(),
                description: String::new(),
                body: String::new(),
                path: root.join("SKILL.md"),
                disable_model_invocation: false,
            }
        };
        assert!(!skill_runs_code(&skill(
            "text",
            &["refs/notes.md", "README.txt"]
        )));
        assert!(skill_runs_code(&skill("script", &["run.sh"])));
        assert!(skill_runs_code(&skill("nested", &["refs/tool.py"])));
        assert!(skill_runs_code(&skill("binary", &["refs/tool"])));
    }
}
