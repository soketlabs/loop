//! The permission gate checks Workspace paths through the Session's env
//! (task C2b). [`VirtualEnv`] shows a root that is not on the host, so a
//! host read of a Workspace path finds nothing; the decisions below need
//! the facts the env reports (links, owner and mode, hard links).
#![cfg(unix)]

#[path = "../../loop-agent/tests/support/virtual_env.rs"]
mod virtual_env;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loop_agent::harness::sandbox::PathNamespace;
use loop_agent::harness::types::{FileError, FileInfo, FileSystem, PathKind, PathMetadata};
use loop_app_core::permissions::tool_lock::ToolLock;
use loop_app_core::permissions::{Decision, GatePaths, PermissionMode, ToolPermission};
use loop_app_core::tool_gate::ToolGate;
use serde_json::json;
use virtual_env::{VirtualEnv, VIRTUAL_ROOT};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `home/.secret/key`, `outside/hard-src`, and `repo` (a git root) with a
/// link out (`sec`), a dangling link, a hard link and a folder `sys` with a
/// link to `repo/src`.
fn tree() -> tempfile::TempDir {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let r = |p: &str| dir.path().canonicalize().unwrap().join(p);
    write(&r("home/.secret/key"), "k");
    write(&r("outside/hard-src"), "h");
    write(&r("repo/.git/config"), "c");
    write(&r("repo/src/a.rs"), "a");
    std::fs::create_dir_all(r("sys")).unwrap();
    symlink(r("home/.secret"), r("repo/sec")).unwrap();
    symlink(r("home/.secret/new"), r("repo/dangling")).unwrap();
    symlink(r("repo/src"), r("sys/link")).unwrap();
    symlink(r("home/.secret"), r("plink")).unwrap();
    std::fs::hard_link(r("outside/hard-src"), r("repo/hard")).unwrap();
    dir
}

fn virt(p: &str) -> PathBuf {
    Path::new(VIRTUAL_ROOT).join(p)
}

fn rules(extra: &[(String, &str)]) -> BTreeMap<String, ToolPermission> {
    let settings = extra
        .iter()
        .map(|(k, v)| (k.clone(), (*v).to_owned()))
        .collect();
    loop_app_core::tool_approval::permissions_from_settings(&settings)
}

/// A gate over `env` (guest namespace), with its cwd at `repo`.
async fn gate(env: Arc<dyn FileSystem>, rules: BTreeMap<String, ToolPermission>) -> Arc<ToolGate> {
    let paths = GatePaths::new(env, PathNamespace::Guest);
    // A host path that does not exist: a guest gate must not use it.
    ToolGate::new_for_env(rules, Path::new("/no/such/host/folder"), paths).await
}

fn is_deny(d: &Decision) -> bool {
    matches!(d, Decision::Deny(_))
}

#[tokio::test]
async fn the_gate_resolves_links_and_the_git_root_through_the_env() {
    let dir = tree();
    let env = Arc::new(VirtualEnv::new(dir.path()).at("repo"));
    let deny = rules(&[(
        format!("read({}/**)", virt("home/.secret").display()),
        "deny",
    )]);
    let gate = gate(env.clone(), deny).await;
    // `sec` is a link to the denied folder: only the env knows it.
    assert!(is_deny(
        &gate.decide("read", &json!({"path": "sec/key"})).await
    ));
    assert!(is_deny(
        &gate.decide("read", &json!({"path": "dangling"})).await
    ));
    assert_eq!(
        gate.decide("read", &json!({"path": "src/a.rs"})).await,
        Decision::Allow
    );
    // The git root (`repo`, found through the env) is what accept-edits
    // may edit; a write through the link out is asked about.
    gate.set_mode(PermissionMode::AcceptEdits);
    assert_eq!(
        gate.decide("write", &json!({"path": "src/new.rs"})).await,
        Decision::Allow
    );
    assert_eq!(
        gate.decide("write", &json!({"path": "sec/new"})).await,
        Decision::Ask
    );
    assert_eq!(
        gate.decide("write", &json!({"path": ".git/config"})).await,
        Decision::Ask
    );
    env.assert_no_leaks();
    let calls = env.calls();
    assert!(calls
        .iter()
        .any(|c| c.starts_with("exists /loop-virtual-workspace/repo/.git")));
    assert!(calls
        .iter()
        .any(|c| c.starts_with("read_link /loop-virtual-workspace/repo/dangling")));
}

/// A deny pattern whose fixed prefix is a link: only the env can say where
/// it leads (the virtual prefix is not on the host).
#[tokio::test]
async fn a_deny_pattern_prefix_resolves_through_the_env() {
    let dir = tree();
    let env = Arc::new(VirtualEnv::new(dir.path()).at("repo"));
    let deny = rules(&[(format!("read({}/**)", virt("plink").display()), "deny")]);
    let gate = gate(env.clone(), deny).await;
    assert!(is_deny(
        &gate.decide("read", &json!({"path": "sec/key"})).await
    ));
    assert_eq!(
        gate.decide("read", &json!({"path": "src/a.rs"})).await,
        Decision::Allow
    );
    env.assert_no_leaks();
}

/// Owner and mode bits of a folder decide whether an allow rule follows a
/// link in its prefix; the env reports them for paths the host does not
/// have at all.
#[tokio::test]
async fn owner_and_mode_facts_come_from_the_env() {
    let dir = tree();
    let env = Arc::new(VirtualEnv::new(dir.path()).at("repo"));
    let dir_meta = |uid, mode| PathMetadata {
        kind: PathKind::Dir,
        size: 0,
        uid: Some(uid),
        nlink: Some(2),
        mode: Some(mode),
    };
    env.report_metadata(Path::new("/"), dir_meta(0, 0o755));
    env.report_metadata(Path::new(VIRTUAL_ROOT), dir_meta(0, 0o755));
    let allow = rules(&[(format!("write({}/**)", virt("sys/link").display()), "allow")]);
    let gate = gate(env.clone(), allow).await;
    let args = json!({"path": "src/a.rs"});
    // `sys` belongs to root and only root may write it: the link is followed.
    env.report_metadata(&virt("sys"), dir_meta(0, 0o755));
    assert_eq!(gate.decide("write", &args).await, Decision::Allow);
    // Anyone may write `sys`: the link is not followed.
    env.report_metadata(&virt("sys"), dir_meta(0, 0o777));
    assert_eq!(gate.decide("write", &args).await, Decision::Ask);
    // A user owns `sys`.
    env.report_metadata(&virt("sys"), dir_meta(501, 0o755));
    assert_eq!(gate.decide("write", &args).await, Decision::Ask);
    env.assert_no_leaks();
}

/// The hard-link rule of the tool lock uses the link count the env reports.
#[tokio::test]
async fn the_hard_link_count_comes_from_the_env() {
    let dir = tree();
    let env = Arc::new(VirtualEnv::new(dir.path()).at("repo"));
    let gate = gate(env.clone(), rules(&[])).await;
    gate.lock_tools(ToolLock::Chat);
    let read = |p: &str| json!({ "path": p });
    assert_eq!(
        gate.decide("read", &read("src/a.rs")).await,
        Decision::Allow
    );
    assert!(is_deny(&gate.decide("read", &read("hard")).await));
    assert!(is_deny(&gate.decide("read", &read("sec/key")).await));
    // The env says `a.rs` has two links (the host file has one).
    env.report_metadata(
        &virt("repo/src/a.rs"),
        PathMetadata {
            kind: PathKind::File,
            size: 1,
            uid: Some(501),
            nlink: Some(2),
            mode: Some(0o644),
        },
    );
    assert!(is_deny(&gate.decide("read", &read("src/a.rs")).await));
    env.assert_no_leaks();
}

/// A guest env's decisions never use host facts: on macOS `/tmp` is a link
/// to `/private/tmp` on the host, which an allow rule could follow.
#[tokio::test]
async fn a_guest_env_never_uses_host_facts() {
    let dir = tree();
    let env = Arc::new(VirtualEnv::new(dir.path()).at("repo"));
    let allow = rules(&[("write(/tmp/**)".into(), "allow")]);
    let gate = gate(env.clone(), allow).await;
    assert_eq!(
        gate.decide("write", &json!({"path": "/private/tmp/x"}))
            .await,
        Decision::Ask
    );
    // The search filter cannot wait for the env: for a guest env it has no
    // facts, so a deny rule hides what it cannot check (fail closed).
    let filter = gate.search_filter();
    assert!(filter("grep", &virt("repo/src/a.rs")));
    gate.set_permissions(rules(&[(
        format!("read({}/**)", virt("home").display()),
        "deny",
    )]));
    let filter = gate.search_filter();
    assert!(!filter("grep", &virt("repo/src/a.rs")));
    env.assert_no_leaks();
}

/// An env that cannot report links (the trait defaults) fails closed: deny
/// and ask rules apply to a path it cannot resolve, allow rules and the
/// workspace scope do not.
struct NoLinkFacts(VirtualEnv);

#[async_trait]
impl FileSystem for NoLinkFacts {
    fn cwd(&self) -> &Path {
        self.0.cwd()
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.0.absolute_path(path)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.0.join_path(base, child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.0.read_text_file(path).await
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.0.read_binary_file(path).await
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.0.write_file(path, data).await
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.0.append_file(path, data).await
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.0.file_info(path).await
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.0.list_dir(path).await
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.0.exists(path).await
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.0.create_dir(path).await
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.0.remove(path).await
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.0.canonical_path(path).await
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.0.create_temp_dir(prefix).await
    }
}

#[tokio::test]
async fn an_env_without_link_facts_fails_closed() {
    let dir = tree();
    let env = Arc::new(NoLinkFacts(VirtualEnv::new(dir.path()).at("repo")));
    let set = rules(&[
        (
            format!("read({}/**)", virt("home/.secret").display()),
            "deny",
        ),
        (format!("write({}/**)", virt("repo/src").display()), "allow"),
    ]);
    let gate = gate(env.clone(), set).await;
    // An existing file resolves (canonical path), so its rules apply.
    assert_eq!(
        gate.decide("read", &json!({"path": "src/a.rs"})).await,
        Decision::Allow
    );
    // A new file needs `read_link`: unknown, so the deny rule applies ...
    let Decision::Deny(reason) = gate.decide("read", &json!({"path": "src/new.rs"})).await else {
        panic!("a path the env cannot resolve must be denied");
    };
    assert_eq!(reason, "Tool `read` is denied by settings.toolPermissions");
    // ... and the allow rule does not.
    assert_eq!(
        gate.decide("write", &json!({"path": "src/new.rs"})).await,
        Decision::Ask
    );
    gate.set_mode(PermissionMode::AcceptEdits);
    assert_eq!(
        gate.decide("write", &json!({"path": "src/new.rs"})).await,
        Decision::Ask
    );
    env.0.assert_no_leaks();
}
