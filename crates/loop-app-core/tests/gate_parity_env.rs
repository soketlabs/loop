//! The gate's path decisions through the env (task C2b).
//!
//! - The matrix of `support/gate_matrix.rs` through [`GatePaths`] over the
//!   host env gives the sync (recorded) decisions, and over [`VirtualEnv`]
//!   (a root that is not on the host) the same ones: every fact came from
//!   the env.
//! - A fact the env cannot give never makes a decision more permissive:
//!   each lookup of each case fails once in turn (and at random in random
//!   trees), and the decision must be as strict or stricter.
//! - The decision-limit attack (a crafted chain that runs the env-call
//!   budget out) and the guest "unknown" cases of the review: never allow
//!   where the full facts deny.
#![cfg(unix)]

#[path = "../../loop-agent/tests/support/virtual_env.rs"]
mod virtual_env;

#[path = "support/gate_matrix.rs"]
mod gate_matrix;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use gate_matrix::{cases, line, search_cases, strictness, sync_line, tree, Case};
use loop_agent::harness::sandbox::PathNamespace;
use loop_agent::harness::types::{FileError, FileErrorCode, FileInfo, FileSystem, PathMetadata};
use loop_agent::harness::HostExecutionEnv;
use loop_app_core::permissions::{
    search_may_show_with, Call, Decision, GatePaths, PermissionMode, Policy, ToolPermission,
    Verdict,
};
use loop_store::DecidedBy;
use serde_json::json;
use virtual_env::{VirtualEnv, VIRTUAL_ROOT};

/// The decision through `paths` (the env), with every fact from it. Past a
/// limit the gate refuses, as `ToolGate::verdict` does.
async fn env_verdict(paths: &GatePaths, case: &Case) -> Verdict {
    let kind = loop_agent::protocol_events::tool_kind(case.tool);
    paths
        .run(|facts| {
            let call = Call {
                home: Some(case.home.clone()),
                ..Call::resolve_with(
                    facts,
                    case.tool,
                    kind,
                    &case.args,
                    &case.workspace,
                    &case.cwd,
                )
            };
            Policy::default().decide_why(case.mode, &case.rules, &call)
        })
        .await
        .unwrap_or_else(|limit| Verdict {
            decision: Decision::Deny(format!("`{}` is refused: {limit}.", case.tool)),
            by: DecidedBy::Unreadable,
            rule: None,
            layer: None,
        })
}

async fn env_line(paths: &GatePaths, case: &Case, roots: &[&Path]) -> String {
    line(&case.id, &env_verdict(paths, case).await, roots)
}

/// Search lines through `paths`.
async fn env_search_lines(root: &Path, raw: &Path, paths: &GatePaths) -> Vec<String> {
    let mut out = Vec::new();
    for c in search_cases(root, raw) {
        let shown = paths
            .run(|facts| {
                search_may_show_with(
                    facts,
                    &c.rules,
                    c.tool,
                    &c.path,
                    &c.workspace,
                    &c.cwd,
                    &[],
                    Some(&c.home),
                )
            })
            .await
            .unwrap_or(false);
        out.push(format!("{} => {shown}", c.id));
    }
    out
}

/// `want` and `got` must be equal, or, for an id with a marker of
/// `stricter_ok`, `got` must be at least as strict.
fn compare(want: &str, got: &str, stricter_ok: &[&str], differ: &mut Vec<String>) {
    if want == got {
        return;
    }
    let id = want.split(" => ").next().unwrap_or("");
    if stricter_ok.iter().any(|m| id.contains(m)) && strictness(got) >= strictness(want) {
        return;
    }
    differ.push(format!("want {want}\n got {got}"));
}

/// The host env gives every decision the sync code gives (and so the
/// recorded one), except that a path past the limits is refused.
#[tokio::test]
async fn decisions_through_the_host_env_match() {
    let tree = tree();
    let roots = [tree.root.as_path(), tree.raw.as_path()];
    let paths = GatePaths::new(
        Arc::new(HostExecutionEnv::new(&tree.root)),
        PathNamespace::Host,
    );
    let mut differ = Vec::new();
    for case in cases(&tree.root, &tree.raw) {
        let want = sync_line(&case, &roots);
        let got = env_line(&paths, &case, &roots).await;
        compare(&want, &got, &["|$DEEP"], &mut differ);
    }
    let want = gate_matrix::search_lines(&tree.root, &tree.raw);
    let got = env_search_lines(&tree.root, &tree.raw, &paths).await;
    for (w, g) in want.iter().zip(&got) {
        compare(w, g, &[], &mut differ);
    }
    assert!(
        differ.is_empty(),
        "{}:\n{}",
        differ.len(),
        differ.join("\n")
    );
}

/// An env whose root is not on the host gives the same decisions: every
/// link, `..` and existence fact came from the env. It is a guest env, so
/// `~` rules and an unreadable folder fail closed (stricter, never looser).
#[tokio::test]
async fn decisions_through_a_virtual_root_match() {
    let tree = tree();
    let real_roots = [tree.root.as_path(), tree.raw.as_path()];
    let expected: BTreeMap<String, String> = cases(&tree.root, &tree.raw)
        .iter()
        .map(|case| (case.id.clone(), sync_line(case, &real_roots)))
        .collect();
    let env = Arc::new(VirtualEnv::new(&tree.root));
    let paths = GatePaths::new(env.clone(), PathNamespace::Guest);
    let virt = Path::new(VIRTUAL_ROOT);
    let mut differ = Vec::new();
    let mut checked = 0;
    for case in cases(virt, virt) {
        if case.host_only || case.id.contains("|raw|") {
            continue;
        }
        checked += 1;
        let got = env_line(&paths, &case, &[virt]).await;
        let want = &expected[&case.id];
        let stricter: &[&str] = if case.uses_home {
            &[""]
        } else {
            &["|$DEEP", "locked/"]
        };
        compare(want, &got, stricter, &mut differ);
    }
    assert!(checked > 3000, "{checked}");
    let want = gate_matrix::search_lines(&tree.root, &tree.root);
    let got = env_search_lines(virt, virt, &paths).await;
    for (w, g) in want.iter().zip(&got) {
        // `~` rules (the `home` and `five` sets) and the unreadable folder
        // fail closed.
        compare(w, g, &["|home|", "|five|", "locked/"], &mut differ);
    }
    assert!(
        differ.is_empty(),
        "{}:\n{}",
        differ.len(),
        differ.join("\n")
    );
    env.assert_no_leaks();
    assert!(env.called("canonical_path") && env.called("read_link"));
}

/// An env over another that fails one chosen gate lookup (`canonical_path`,
/// `read_link`, `symlink_metadata` or `exists`), counted from 0, or the
/// lookups of one path; it counts every lookup.
struct FailOne {
    inner: Arc<dyn FileSystem>,
    at: Option<usize>,
    path: Option<PathBuf>,
    error: FileErrorCode,
    calls: AtomicUsize,
}

impl FailOne {
    fn new(inner: Arc<dyn FileSystem>, at: Option<usize>) -> Self {
        Self {
            inner,
            at,
            path: None,
            error: FileErrorCode::Unavailable,
            calls: AtomicUsize::new(0),
        }
    }

    fn check(&self, path: &Path) -> Result<(), FileError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if Some(n) == self.at || self.path.as_deref() == Some(path) {
            return Err(FileError::new(self.error, "injected failure"));
        }
        Ok(())
    }
}

#[async_trait]
impl FileSystem for FailOne {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.inner.absolute_path(path)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.inner.join_path(base, child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.inner.read_text_file(path).await
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path).await
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.inner.write_file(path, data).await
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.inner.append_file(path, data).await
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.inner.file_info(path).await
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(path).await
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.check(path)?;
        self.inner.exists(path).await
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.inner.create_dir(path).await
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.inner.remove(path).await
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.check(path)?;
        self.inner.canonical_path(path).await
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.inner.create_temp_dir(prefix).await
    }
    async fn symlink_metadata(&self, path: &Path) -> Result<PathMetadata, FileError> {
        self.check(path)?;
        self.inner.symlink_metadata(path).await
    }
    async fn read_link(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.check(path)?;
        self.inner.read_link(path).await
    }
}

/// The strictness of a verdict: allow < ask < deny.
fn rank(verdict: &Verdict) -> u8 {
    match verdict.decision {
        Decision::Allow => 0,
        Decision::Ask => 1,
        Decision::Deny(_) => 2,
    }
}

/// Fail each lookup of `case` once in turn: no decision may be more
/// permissive than the one with every fact. Returns the lookups tried.
async fn inject_each(root: &Path, case: &Case, namespace: PathNamespace) -> usize {
    let host: Arc<dyn FileSystem> = Arc::new(HostExecutionEnv::new(root));
    let counted = Arc::new(FailOne::new(host.clone(), None));
    let full = env_verdict(&GatePaths::new(counted.clone(), namespace), case).await;
    let lookups = counted.calls.load(Ordering::SeqCst);
    for at in 0..lookups {
        let failing = Arc::new(FailOne::new(host.clone(), Some(at)));
        let got = env_verdict(&GatePaths::new(failing, namespace), case).await;
        assert!(
            rank(&got) >= rank(&full),
            "{}: lookup {at} unknown gave {:?}, all facts gave {:?}",
            case.id,
            got.decision,
            full.decision
        );
    }
    lookups
}

/// Every resolve caller and every deny/ask site of the matrix: an unknown
/// fact at any lookup never loosens a decision.
#[tokio::test]
async fn an_unknown_fact_at_any_lookup_never_loosens_a_decision() {
    let tree = tree();
    let mut tried = 0;
    for case in cases(&tree.root, &tree.raw) {
        let mode_ok = matches!(case.mode, PermissionMode::Ask | PermissionMode::AcceptEdits);
        if !mode_ok || !case.id.contains("|repo|") || case.id.contains("$DEEP") {
            continue;
        }
        tried += inject_each(&tree.root, &case, PathNamespace::Host).await;
    }
    assert!(tried > 5000, "{tried} lookups tried");
}

/// A tiny random number source (no new crate): xorshift.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
}

/// Random trees of folders and links (to folders, files, missing paths,
/// through `..`), random rules and calls; one random lookup fails. The
/// decision is never more permissive than the one with every fact.
#[tokio::test]
async fn random_trees_with_one_unknown_fact_never_loosen() {
    use std::os::unix::fs::symlink;
    let names = ["a", "b", "c", "s", "t"];
    for seed in 1..=60u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        gate_matrix::write(&root.join("secret/key"), "k");
        for _ in 0..6 {
            let d = format!("{}/{}", rng.pick(&names), rng.pick(&names));
            gate_matrix::write(&ws.join(&d).join("f"), "f");
        }
        let targets = [
            root.join("secret"),
            root.join("secret/key"),
            root.join("secret/missing"),
            ws.join("a"),
            PathBuf::from("../secret"),
            PathBuf::from("b/../s"),
            PathBuf::from("l0"),
        ];
        for i in 0..5 {
            let at = ws.join(format!("l{i}"));
            let _ = symlink(rng.pick(&targets), &at);
        }
        let patterns = [
            format!("read({}/secret/**)", root.display()),
            format!("read({}/l1/**)", ws.display()),
            "read(s/**)".to_owned(),
            "write(a/**)".to_owned(),
            "edit(*.rs)".to_owned(),
        ];
        let perms = ["deny", "ask", "allow"];
        let mut settings = BTreeMap::new();
        for _ in 0..3 {
            settings.insert(rng.pick(&patterns).clone(), (*rng.pick(&perms)).to_owned());
        }
        let rules: BTreeMap<String, ToolPermission> =
            loop_app_core::tool_approval::permissions_from_settings(&settings);
        let parts = [
            "l0", "l1", "l2", "l3", "l4", "a", "b", "s", "..", "f", "new", "key",
        ];
        for _ in 0..12 {
            let n = 1 + rng.next() % 4;
            let path: Vec<&str> = (0..n).map(|_| *rng.pick(&parts)).collect();
            let tool = *rng.pick(&["read", "write", "edit", "grep"]);
            let mode = *rng.pick(&[PermissionMode::Ask, PermissionMode::AcceptEdits]);
            let case = Case {
                id: format!("seed {seed} {tool} {}", path.join("/")),
                rules: rules.clone(),
                mode,
                tool,
                args: json!({ "path": path.join("/") }),
                workspace: ws.clone(),
                cwd: ws.clone(),
                home: root.join("home"),
                host_only: false,
                uses_home: false,
            };
            inject_each(&root, &case, PathNamespace::Host).await;
        }
    }
}

/// The review's attack: `link/../x`, where the kernel opens a denied file
/// and the lexical form `x` is a dangling chain long enough to run the
/// env-call budget out. Swept past the limits: never allowed.
#[tokio::test]
async fn a_chain_that_runs_the_budget_out_is_never_allowed() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(t.join("secret/sub")).unwrap();
    std::fs::write(t.join("secret/x"), "s").unwrap();
    let repo = t.join("repo");
    std::fs::create_dir_all(repo.join("d")).unwrap();
    symlink(t.join("secret/sub"), repo.join("link")).unwrap();
    symlink(format!("l1/{}", "a/".repeat(200)), repo.join("x")).unwrap();
    let rules = loop_app_core::tool_approval::permissions_from_settings(
        &[(
            format!("read({}/secret/**)", t.display()),
            "deny".to_owned(),
        )]
        .into_iter()
        .collect(),
    );
    let host = GatePaths::new(Arc::new(HostExecutionEnv::new(&repo)), PathNamespace::Host);
    let gate = loop_app_core::tool_gate::ToolGate::new_for_env(rules.clone(), &repo, host).await;
    let case = |k: usize| Case {
        id: format!("k={k}"),
        rules: rules.clone(),
        mode: PermissionMode::Ask,
        tool: "read",
        args: json!({ "path": "link/../x" }),
        workspace: repo.clone(),
        cwd: repo.clone(),
        home: t.join("home"),
        host_only: false,
        uses_home: false,
    };
    let mut slowest = std::time::Duration::ZERO;
    for k in 0..=320 {
        let _ = std::fs::remove_file(repo.join("l1"));
        symlink(format!("d/{}", "b/".repeat(k)), repo.join("l1")).unwrap();
        // The host facts (the sync code) deny it through the kernel form.
        let roots = [t.as_path()];
        let want = sync_line(&case(k), &roots);
        assert!(want.contains("=> deny"), "{want}");
        let started = std::time::Instant::now();
        let got = gate.decide("read", &json!({ "path": "link/../x" })).await;
        slowest = slowest.max(started.elapsed());
        assert!(matches!(got, Decision::Deny(_)), "k={k}: {got:?}");
    }
    eprintln!("slowest decision at the limit: {slowest:?}");
    // A decision that reaches the limit is refused with its own reason.
    let Decision::Deny(reason) = gate.decide("read", &json!({ "path": "x" })).await else {
        panic!("a path past the limit must be refused");
    };
    assert_eq!(
        reason,
        "`read` is refused: the path is too deep or has too many links to check."
    );
}

/// A guest env whose `canonical_path` fails (`Io`) for one path. The
/// review's two cases gave Allow where the host facts give Deny.
#[tokio::test]
async fn a_guest_env_that_cannot_resolve_one_path_never_loosens() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(t.join("secret/sub")).unwrap();
    std::fs::write(t.join("secret/x"), "s").unwrap();
    let repo = t.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("x"), "own").unwrap();
    symlink(t.join("secret/sub"), repo.join("link")).unwrap();
    symlink(t.join("secret"), t.join("plink")).unwrap();
    symlink(t.join("secret"), repo.join("s2")).unwrap();
    let host: Arc<dyn FileSystem> = Arc::new(HostExecutionEnv::new(&repo));
    let decide = |rule: String, path: &'static str, fail: PathBuf| {
        let host = host.clone();
        let repo = repo.clone();
        let t = t.clone();
        async move {
            let rules = loop_app_core::tool_approval::permissions_from_settings(
                &[(rule, "deny".to_owned())].into_iter().collect(),
            );
            let case = Case {
                id: path.to_owned(),
                rules,
                mode: PermissionMode::Ask,
                tool: "read",
                args: json!({ "path": path }),
                workspace: repo.clone(),
                cwd: repo.clone(),
                home: t.join("home"),
                host_only: false,
                uses_home: false,
            };
            let full =
                env_verdict(&GatePaths::new(host.clone(), PathNamespace::Guest), &case).await;
            let mut failing = FailOne::new(host, None);
            failing.path = Some(fail);
            failing.error = FileErrorCode::Io;
            let got = env_verdict(
                &GatePaths::new(Arc::new(failing), PathNamespace::Guest),
                &case,
            )
            .await;
            (full, got)
        }
    };
    // (A) the kernel form of `link/../x` is unknown.
    let (full, got) = decide(
        format!("read({}/secret/**)", t.display()),
        "link/../x",
        repo.join("link/../x"),
    )
    .await;
    assert!(matches!(full.decision, Decision::Deny(_)), "{full:?}");
    assert!(matches!(got.decision, Decision::Deny(_)), "(A) {got:?}");
    // (B) the deny pattern's prefix is a link the env cannot resolve.
    let (full, got) = decide(
        format!("read({}/plink/**)", t.display()),
        "s2/x",
        t.join("plink"),
    )
    .await;
    assert!(matches!(full.decision, Decision::Deny(_)), "{full:?}");
    assert!(matches!(got.decision, Decision::Deny(_)), "(B) {got:?}");
}

/// The tool lock's checks (the moved folder, the confinement, hard links)
/// with one unknown fact: a locked read is never allowed where every fact
/// refuses it.
#[tokio::test]
async fn an_unknown_fact_never_opens_the_tool_lock() {
    use loop_app_core::permissions::tool_lock::{Confinement, ToolLock};
    let tree = tree();
    let folder = tree.root.join("repo");
    let rules = loop_app_core::tool_approval::permissions_from_settings(&BTreeMap::new());
    let host: Arc<dyn FileSystem> = Arc::new(HostExecutionEnv::new(&folder));
    let mut tried = 0;
    for path in gate_matrix::PATHS {
        if path.starts_with('~') || path.contains("$DEEP") || path.starts_with("/") {
            continue;
        }
        let args = json!({ "path": gate_matrix::fill(path, &tree.root, &tree.root) });
        let gate_with = |at: Option<usize>| {
            let env = Arc::new(FailOne::new(host.clone(), at));
            let rules = rules.clone();
            let folder = folder.clone();
            let root = tree.root.clone();
            async move {
                let paths = GatePaths::new(env.clone(), PathNamespace::Host);
                let gate =
                    loop_app_core::tool_gate::ToolGate::new_for_env(rules, &folder, paths).await;
                gate.lock_tools(ToolLock::Chat);
                gate.confine(Confinement::new(&folder, &root).unwrap());
                (gate, env)
            }
        };
        let (gate, env) = gate_with(None).await;
        let before = env.calls.load(Ordering::SeqCst);
        let full = gate.verdict("read", &args).await;
        let lookups = env.calls.load(Ordering::SeqCst) - before;
        for at in 0..lookups {
            let (gate, env) = gate_with(None).await;
            let skip = env.calls.load(Ordering::SeqCst);
            drop(gate);
            let (gate, _) = gate_with(Some(skip + at)).await;
            let got = gate.verdict("read", &args).await;
            tried += 1;
            assert!(
                rank(&got) >= rank(&full),
                "{path}: lookup {at} unknown gave {:?}, all facts gave {:?}",
                got.decision,
                full.decision
            );
        }
    }
    assert!(tried > 20, "{tried}");
}
