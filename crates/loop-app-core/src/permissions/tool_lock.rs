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

use std::path::{Path, PathBuf};

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

/// The environment variable for the folder every Session must be in.
pub const ROOT_ENV: &str = "LOOP_TOOL_LOCK_ROOT";

/// Why the lock could not be set up, or refuses a folder.
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
    /// The flag or the environment variable is set but empty.
    #[error("{from} is empty: name a lock (`chat`) or unset it")]
    Empty {
        /// `--tool-lock`, `--tool-lock-root` or an environment variable.
        from: &'static str,
    },
    /// A lock without the folder its Sessions must stay in.
    #[error("a tool lock needs a root folder: pass --tool-lock-root DIR or set {ROOT_ENV}")]
    NoRoot,
    /// A root without a lock: most likely a missing `--tool-lock`.
    #[error("--tool-lock-root or {ROOT_ENV} is set without a tool lock")]
    RootWithoutLock,
    /// The root is not a folder that can be used.
    #[error("tool lock root {root}: {reason}")]
    BadRoot {
        /// The root as given.
        root: PathBuf,
        /// Why it can't be used.
        reason: String,
    },
    /// A Session's folder is not inside the root.
    #[error("{cwd} is not inside the tool lock root")]
    OutsideRoot {
        /// The folder asked for.
        cwd: PathBuf,
    },
}

/// A Runtime's lock and the folder every Session of it must be in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeToolLock {
    lock: ToolLock,
    root: PathBuf,
}

impl RuntimeToolLock {
    /// `lock` with every Session inside `root`. The root must be an
    /// existing folder other than `/`; it is kept with symlinks resolved.
    pub fn new(lock: ToolLock, root: &Path) -> Result<Self, ToolLockError> {
        let bad = |reason: String| ToolLockError::BadRoot {
            root: root.to_path_buf(),
            reason,
        };
        let real = root.canonicalize().map_err(|e| bad(e.to_string()))?;
        if !real.is_dir() {
            return Err(bad("not a folder".into()));
        }
        if real.parent().is_none() {
            return Err(bad("the file system root is not allowed".into()));
        }
        Ok(Self { lock, root: real })
    }

    /// The lock.
    pub fn lock(&self) -> ToolLock {
        self.lock
    }

    /// The root, symlinks resolved.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `cwd` with symlinks resolved, when it is the root or inside it.
    pub fn admit(&self, cwd: &Path) -> Result<PathBuf, ToolLockError> {
        let outside = || ToolLockError::OutsideRoot {
            cwd: cwd.to_path_buf(),
        };
        let real = cwd.canonicalize().map_err(|_| outside())?;
        if real.starts_with(&self.root) {
            Ok(real)
        } else {
            Err(outside())
        }
    }
}

/// The lock from `--tool-lock`, else [`ENV`], with its root from
/// `--tool-lock-root`, else [`ROOT_ENV`]. A flag wins over its variable.
/// An unknown or empty name, a lock without a root and a root without a
/// lock are errors, so a typo never starts a Runtime without the lock it
/// asked for.
pub fn resolve(
    flag: Option<&str>,
    env: Option<&str>,
    root_flag: Option<&Path>,
    root_env: Option<&str>,
) -> Result<Option<RuntimeToolLock>, ToolLockError> {
    let (value, from) = match (flag, env) {
        (Some(flag), _) => (Some(flag), "--tool-lock"),
        (None, Some(env)) => (Some(env), ENV),
        (None, None) => (None, ""),
    };
    let root: Option<PathBuf> = match (root_flag, root_env) {
        (Some(root), _) if root.as_os_str().is_empty() => {
            return Err(ToolLockError::Empty {
                from: "--tool-lock-root",
            })
        }
        (Some(root), _) => Some(root.to_path_buf()),
        (None, Some("")) => return Err(ToolLockError::Empty { from: ROOT_ENV }),
        (None, Some(root)) => Some(PathBuf::from(root)),
        (None, None) => None,
    };
    let Some(value) = value else {
        return match root {
            Some(_) => Err(ToolLockError::RootWithoutLock),
            None => Ok(None),
        };
    };
    if value.is_empty() {
        return Err(ToolLockError::Empty { from });
    }
    let lock = ToolLock::parse(value).ok_or_else(|| ToolLockError::Unknown {
        value: value.to_owned(),
        from,
    })?;
    let root = root.ok_or(ToolLockError::NoRoot)?;
    RuntimeToolLock::new(lock, &root).map(Some)
}

/// Whether a locked read of `path` stays inside `folder`: the path, with
/// symlinks and `..` resolved, is in the folder, and it is not a file with
/// more than one hard link (a hard link to a file outside the folder looks
/// like a file inside it, so every hard-linked file is refused).
pub fn inside_folder(path: &Path, folder: &Path) -> bool {
    let (Some(real), Some(root)) = (super::policy::resolve(path), super::policy::resolve(folder))
    else {
        return false;
    };
    real.starts_with(&root) && !hard_linked(&real)
}

#[cfg(unix)]
fn hard_linked(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.nlink() > 1)
}

#[cfg(not(unix))]
fn hard_linked(_path: &Path) -> bool {
    false
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
    fn the_flag_wins_and_a_typo_or_an_empty_value_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let real = root.canonicalize().unwrap();
        let root_text = root.to_str().unwrap();
        assert_eq!(resolve(None, None, None, None), Ok(None));
        let chat = |l: Option<RuntimeToolLock>| l.map(|l| (l.lock(), l.root().to_path_buf()));
        let want = Some((ToolLock::Chat, real.clone()));
        assert_eq!(
            resolve(Some("chat"), None, Some(root), None).map(chat),
            Ok(want.clone())
        );
        assert_eq!(
            resolve(None, Some("chat"), None, Some(root_text)).map(chat),
            Ok(want.clone())
        );
        assert_eq!(
            resolve(Some("chat"), Some("nope"), None, Some(root_text)).map(chat),
            Ok(want)
        );
        let error = |r: Result<Option<RuntimeToolLock>, ToolLockError>| r.unwrap_err().to_string();
        assert!(error(resolve(Some("caht"), None, Some(root), None)).contains("--tool-lock"));
        assert!(error(resolve(None, Some("CHAT"), Some(root), None)).contains(ENV));
        assert!(error(resolve(Some(""), None, Some(root), None)).contains("empty"));
        assert!(error(resolve(None, Some(""), Some(root), None)).contains("empty"));
        assert_eq!(
            resolve(Some("chat"), None, None, None).unwrap_err(),
            ToolLockError::NoRoot
        );
        assert!(error(resolve(Some("chat"), None, None, Some(""))).contains("empty"));
        assert_eq!(
            resolve(None, None, Some(root), None).unwrap_err(),
            ToolLockError::RootWithoutLock
        );
        assert!(matches!(
            resolve(Some("chat"), None, Some(&root.join("missing")), None),
            Err(ToolLockError::BadRoot { .. })
        ));
        assert!(matches!(
            resolve(Some("chat"), None, Some(Path::new("/")), None),
            Err(ToolLockError::BadRoot { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn only_folders_inside_the_root_are_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("alice");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(base.join("bob")).unwrap();
        std::os::unix::fs::symlink(base.join("bob"), root.join("link")).unwrap();
        let lock = RuntimeToolLock::new(ToolLock::Chat, &root).unwrap();
        assert_eq!(lock.admit(&root), Ok(root.clone()));
        assert_eq!(lock.admit(&root.join("sub")), Ok(root.join("sub")));
        for cwd in [
            base.clone(),
            base.join("bob"),
            root.join("link"),
            root.join("sub/../.."),
            root.join("missing"),
            PathBuf::from("/"),
        ] {
            assert!(lock.admit(&cwd).is_err(), "{}", cwd.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_and_symlinks_out_are_not_inside() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(base.join("secret"), "s").unwrap();
        std::fs::write(ws.join("own"), "o").unwrap();
        std::fs::hard_link(base.join("secret"), ws.join("hard")).unwrap();
        std::os::unix::fs::symlink(base.join("secret"), ws.join("soft")).unwrap();
        assert!(inside_folder(&ws.join("own"), &ws));
        assert!(inside_folder(&ws, &ws));
        assert!(!inside_folder(&ws.join("hard"), &ws));
        assert!(!inside_folder(&ws.join("soft"), &ws));
        assert!(!inside_folder(&base, &ws));
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
