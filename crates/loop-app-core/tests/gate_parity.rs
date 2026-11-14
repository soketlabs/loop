//! Parity of the permission gate's path decisions (task C2b).
//!
//! A fixed tree with links, a link loop, a dangling link, a hard link, a
//! literal `~` folder and protected names. Every case (rule set, mode, cwd,
//! tool, path) gives one line: the decision, the step that decided, the
//! rule key and the refusal text, with the fixture root written `$ROOT`.
//!
//! The expected lines (`tests/data/gate_parity.<os>.golden`) were recorded
//! from the gate before it moved onto the execution env. Set
//! `LOOP_GATE_GOLDEN_WRITE=1` to record them again. A platform without a
//! golden file still runs the comparisons between the code paths.
//!
//! The same cases then run through [`GatePaths`]: over the host env (the
//! lines must be the recorded ones), and over [`VirtualEnv`], whose root is
//! not on the host (the lines must be the same, with the virtual root as
//! `$ROOT`): every fact the gate used came from the env.
#![cfg(unix)]

#[path = "../../loop-agent/tests/support/virtual_env.rs"]
mod virtual_env;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loop_agent::harness::sandbox::PathNamespace;
use loop_agent::harness::HostExecutionEnv;
use loop_app_core::permissions::tool_lock::{Confinement, ToolLock};
use loop_app_core::permissions::{
    search_may_show_in, search_may_show_with, Call, Decision, GatePaths, PermissionMode, Policy,
    ToolPermission, Verdict,
};
use loop_app_core::tool_gate::ToolGate;
use serde_json::json;
use virtual_env::{VirtualEnv, VIRTUAL_ROOT};

/// The fixture tree.
struct Tree {
    _dir: tempfile::TempDir,
    /// The root as the temp folder spelled it (`/var/...` on macOS).
    raw: PathBuf,
    /// The root with links resolved.
    root: PathBuf,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn tree() -> Tree {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().to_path_buf();
    let root = raw.canonicalize().unwrap();
    let r = |p: &str| root.join(p);
    write(&r("home/.secret/key"), "k");
    write(&r("home/.ssh/id"), "i");
    write(&r("outside/secret"), "s");
    write(&r("outside/hard-src"), "h");
    write(&r("repo/.git/config"), "c");
    write(&r("repo/.env"), "e");
    write(&r("repo/src/a.rs"), "a");
    std::fs::create_dir_all(r("repo/src/new-dir")).unwrap();
    write(&r("repo/svc/secrets/key"), "k");
    write(&r("repo/svc/.env"), "e");
    write(&r("repo/svc/main.rs"), "m");
    write(&r("repo/~/x"), "x");
    write(&r("repo/Cargo.lock"), "l");
    symlink(r("repo/src"), r("repo/link-src")).unwrap();
    symlink("src", r("repo/rel-src")).unwrap();
    symlink(r("home/.secret"), r("repo/sec")).unwrap();
    symlink(r("home/.secret/new"), r("repo/dangling")).unwrap();
    symlink("loop2", r("repo/loop1")).unwrap();
    symlink("loop1", r("repo/loop2")).unwrap();
    symlink(r("repo/svc/secrets"), r("repo/svc/sec-link")).unwrap();
    std::fs::hard_link(r("outside/hard-src"), r("repo/hard")).unwrap();
    Tree {
        _dir: dir,
        raw,
        root,
    }
}

/// Call paths. `$ROOT` is the fixture root.
const PATHS: &[&str] = &[
    "src/a.rs",
    "link-src/a.rs",
    "rel-src/a.rs",
    "sec/key",
    "sec/new",
    "dangling",
    "src/../sec/key",
    "sec/../.ssh/id",
    "link-src/../.env",
    "loop1",
    "loop1/x",
    "new.txt",
    "src/new-dir/new.rs",
    "nodir/deeper/new.rs",
    "../outside/secret",
    "../home/.ssh/id",
    "$ROOT/home/.secret/key",
    "$ROOT/outside/secret",
    "$ROOT/repo/src/a.rs",
    "~/.secret/key",
    "~/x",
    "hard",
    ".env",
    ".git/config",
    ".GIT/config",
    "src/.env.local",
    "Cargo.lock",
    "secrets/key",
    "sec-link/key",
    "svc/sec-link/key",
    ".",
    "/tmp/loop-gate-parity-x",
    "/private/tmp/loop-gate-parity-x",
];

/// Rule sets over the defaults. `$ROOT` is the fixture root as resolved,
/// `$RAW` as the temp folder spelled it.
fn rule_sets() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    vec![
        ("none", vec![]),
        (
            "abs-deny",
            vec![
                ("read($ROOT/home/.secret/**)", "deny"),
                ("grep($ROOT/home/**)", "deny"),
                ("write($ROOT/outside/*)", "ask"),
            ],
        ),
        (
            "home",
            vec![
                ("read(~/.secret/**)", "deny"),
                ("write(~/**)", "ask"),
                ("edit(~/.ssh/*)", "deny"),
                ("ls(~/x)", "deny"),
            ],
        ),
        (
            "rel",
            vec![
                ("read(secrets/**)", "deny"),
                ("write(src/**)", "allow"),
                ("edit(src/**)", "allow"),
                ("write(*.rs)", "ask"),
                ("ls(svc/**)", "deny"),
                ("glob(*)", "allow"),
            ],
        ),
        (
            "allow-abs",
            vec![
                ("read($ROOT/repo/src/**)", "allow"),
                ("write($RAW/repo/src/**)", "allow"),
                ("edit($RAW/repo/**)", "allow"),
                ("read(/tmp/**)", "allow"),
                ("write(/tmp/**)", "allow"),
                ("write(/private/tmp/**)", "allow"),
            ],
        ),
        (
            "five",
            vec![
                ("read(secrets/**)", "deny"),
                ("read(~/.ssh/**)", "deny"),
                ("edit(*.lock)", "ask"),
                ("write(src/**)", "allow"),
                ("grep(**/.env)", "deny"),
            ],
        ),
    ]
}

const MODES: &[PermissionMode] = &[
    PermissionMode::Ask,
    PermissionMode::AcceptEdits,
    PermissionMode::FullAccess,
];

const TOOLS: &[&str] = &["read", "write", "edit", "grep", "glob", "ls"];

/// Where the call runs: (name, cwd below the root, spelled raw).
const CWDS: &[(&str, &str, bool)] = &[
    ("repo", "repo", false),
    ("svc", "repo/svc", false),
    ("raw", "repo", true),
];

/// One case, with `$ROOT`, `$RAW` and `~` filled in.
#[derive(Clone)]
struct Case {
    id: String,
    rules: BTreeMap<String, ToolPermission>,
    mode: PermissionMode,
    tool: &'static str,
    args: serde_json::Value,
    workspace: PathBuf,
    cwd: PathBuf,
    home: PathBuf,
    /// The case names a path outside the fixture (`/tmp`).
    host_only: bool,
    /// The case depends on the home folder (`~`).
    uses_home: bool,
}

fn fill(text: &str, root: &Path, raw: &Path) -> String {
    text.replace("$ROOT", &root.to_string_lossy())
        .replace("$RAW", &raw.to_string_lossy())
}

/// Every case over a tree whose root is `root` (as resolved) and `raw`
/// (as spelled).
fn cases(root: &Path, raw: &Path) -> Vec<Case> {
    let mut out = Vec::new();
    for (set, entries) in rule_sets() {
        let mut settings = BTreeMap::new();
        for (key, value) in &entries {
            settings.insert(fill(key, root, raw), (*value).to_owned());
        }
        let rules = loop_app_core::tool_approval::permissions_from_settings(&settings);
        let set_uses_home = entries.iter().any(|(k, _)| k.contains('~'));
        for &(cwd_name, cwd_rel, spelled_raw) in CWDS {
            let base = if spelled_raw { raw } else { root };
            let workspace = base.join("repo");
            let cwd = base.join(cwd_rel);
            for &mode in MODES {
                for &tool in TOOLS {
                    for path in PATHS {
                        let filled = fill(path, root, raw);
                        out.push(Case {
                            id: format!("{set}|{cwd_name}|{}|{tool}|{path}", mode.as_str()),
                            rules: rules.clone(),
                            mode,
                            tool,
                            args: json!({ "path": filled }),
                            workspace: workspace.clone(),
                            cwd: cwd.clone(),
                            home: root.join("home"),
                            host_only: path.starts_with("/tmp") || path.starts_with("/private"),
                            uses_home: set_uses_home || path.starts_with('~'),
                        });
                    }
                }
            }
        }
    }
    out
}

/// One result line, with the roots written `$ROOT`.
fn line(id: &str, verdict: &Verdict, roots: &[&Path]) -> String {
    let (decision, text) = match &verdict.decision {
        Decision::Allow => ("allow", String::new()),
        Decision::Ask => ("ask", String::new()),
        Decision::Deny(text) => ("deny", text.clone()),
    };
    let mut out = format!(
        "{id} => {decision} by={:?} rule={:?} layer={:?} text={text:?}",
        verdict.by, verdict.rule, verdict.layer
    );
    // The resolved root first: on macOS it contains the spelled one.
    for root in roots {
        out = out.replace(&*root.to_string_lossy(), "$ROOT");
    }
    out
}

/// The sync API: a `Call` resolved on the Runtime host.
fn sync_line(case: &Case, roots: &[&Path]) -> String {
    let kind = loop_agent::protocol_events::tool_kind(case.tool);
    let call = Call {
        home: Some(case.home.clone()),
        ..Call::resolve_in(case.tool, kind, &case.args, &case.workspace, &case.cwd)
    };
    let verdict = Policy::default().decide_why(case.mode, &case.rules, &call);
    line(&case.id, &verdict, roots)
}

/// The same decision through `paths` (the env), with every fact from it.
async fn env_line(paths: &GatePaths, case: &Case, roots: &[&Path]) -> String {
    let kind = loop_agent::protocol_events::tool_kind(case.tool);
    let verdict = paths
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
        .await;
    line(&case.id, &verdict, roots)
}

/// Walked paths for the search filter, below the root.
const WALKED: [&str; 12] = [
    "repo/src/a.rs",
    "repo/link-src/a.rs",
    "repo/sec/key",
    "repo/svc/secrets/key",
    "repo/svc/sec-link/key",
    "repo/svc/.env",
    "repo/.env",
    "repo/~/x",
    "repo/hard",
    "repo/loop1",
    "home/.ssh/id",
    "home/.secret/key",
];

/// What a search tool may show, for walked paths, as lines: on the Runtime
/// host, or through `paths`.
async fn search_lines(root: &Path, raw: &Path, paths: Option<&GatePaths>) -> Vec<String> {
    let mut out = Vec::new();
    for (set, entries) in rule_sets() {
        let mut settings = BTreeMap::new();
        for (key, value) in &entries {
            settings.insert(fill(key, root, raw), (*value).to_owned());
        }
        let rules = loop_app_core::tool_approval::permissions_from_settings(&settings);
        for &(cwd_name, cwd_rel, _) in CWDS.iter().take(2) {
            let workspace = root.join("repo");
            let cwd = root.join(cwd_rel);
            let home = root.join("home");
            for tool in ["grep", "glob", "ls"] {
                for walked in WALKED {
                    let path = root.join(walked);
                    let shown = match paths {
                        None => search_may_show_in(
                            &rules,
                            tool,
                            &path,
                            &workspace,
                            &cwd,
                            &[],
                            Some(&home),
                        ),
                        Some(paths) => {
                            paths
                                .run(|facts| {
                                    search_may_show_with(
                                        facts,
                                        &rules,
                                        tool,
                                        &path,
                                        &workspace,
                                        &cwd,
                                        &[],
                                        Some(&home),
                                    )
                                })
                                .await
                        }
                    };
                    out.push(format!(
                        "search|{set}|{cwd_name}|{tool}|{walked} => {shown}"
                    ));
                }
            }
        }
    }
    out
}

/// The tool lock: what a locked read may reach, through the gate.
async fn lock_lines(root: &Path) -> Vec<String> {
    let folder = root.join("repo");
    let rules = loop_app_core::tool_approval::permissions_from_settings(&BTreeMap::new());
    let gate = ToolGate::new_for_workspace(rules, &folder);
    gate.lock_tools(ToolLock::Chat);
    gate.confine(Confinement::new(&folder, root).unwrap());
    let mut out = Vec::new();
    for tool in ["read", "grep", "glob", "ls", "write"] {
        for path in PATHS {
            if path.starts_with('~') || path.starts_with("/tmp") || path.starts_with("/private") {
                continue;
            }
            let args = json!({ "path": fill(path, root, root) });
            let verdict = gate.verdict(tool, &args).await;
            out.push(line(&format!("lock|{tool}|{path}"), &verdict, &[root]));
        }
    }
    let filter = gate.search_filter();
    for walked in [
        "repo/src/a.rs",
        "repo/hard",
        "repo/sec/key",
        "repo/link-src/a.rs",
        "outside/secret",
    ] {
        let shown = filter("grep", &root.join(walked));
        out.push(format!("lock-search|{walked} => {shown}"));
    }
    out
}

/// A locked Session whose folder is swapped for a link after it opened.
async fn swapped_lines(root: &Path) -> Vec<String> {
    let folder = root.join("repo/svc");
    let rules = loop_app_core::tool_approval::permissions_from_settings(&BTreeMap::new());
    let gate = ToolGate::new_for_workspace(rules, &folder);
    gate.lock_tools(ToolLock::Chat);
    gate.confine(Confinement::new(&folder, &root.join("repo")).unwrap());
    let args = json!({ "path": "main.rs" });
    let mut out = vec![line(
        "swap|before",
        &gate.verdict("read", &args).await,
        &[root],
    )];
    std::fs::rename(&folder, root.join("repo/svc-old")).unwrap();
    std::os::unix::fs::symlink(root.join("outside"), &folder).unwrap();
    out.push(line(
        "swap|after",
        &gate.verdict("read", &args).await,
        &[root],
    ));
    let filter = gate.search_filter();
    out.push(format!(
        "swap-search|after => {}",
        filter("grep", &folder.join("secret"))
    ));
    out
}

/// The gate's own decision (Session paths, real home), for the cases that
/// do not depend on the home folder.
async fn gate_lines(cases: &[Case], roots: &[&Path]) -> Vec<String> {
    let mut out = Vec::new();
    let mut gates: BTreeMap<(String, PathBuf), Arc<ToolGate>> = BTreeMap::new();
    for case in cases.iter().filter(|c| !c.uses_home) {
        let set = case.id.split('|').next().unwrap_or_default().to_owned();
        let gate = gates
            .entry((set, case.cwd.clone()))
            .or_insert_with(|| ToolGate::new_for_workspace(case.rules.clone(), &case.cwd))
            .clone();
        gate.set_mode(case.mode);
        let verdict = gate.verdict(case.tool, &case.args).await;
        out.push(line(&format!("gate|{}", case.id), &verdict, roots));
    }
    out
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/data/gate_parity.{}.golden",
        std::env::consts::OS
    ))
}

/// Compare `got` with the golden file, or write it.
fn check_golden(got: &[String]) {
    let path = golden_path();
    let text = got.join("\n") + "\n";
    if std::env::var_os("LOOP_GATE_GOLDEN_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let Ok(want) = std::fs::read_to_string(&path) else {
        eprintln!("no golden file for this platform: {}", path.display());
        return;
    };
    let want: Vec<&str> = want.lines().collect();
    let got: Vec<&str> = text.lines().collect();
    let diff: Vec<String> = want
        .iter()
        .zip(&got)
        .filter(|(w, g)| w != g)
        .take(20)
        .map(|(w, g)| format!("want {w}\n got {g}"))
        .collect();
    assert!(
        diff.is_empty() && want.len() == got.len(),
        "{} lines differ from {} ({} wanted, {} got):\n{}",
        diff.len(),
        path.display(),
        want.len(),
        got.len(),
        diff.join("\n")
    );
}

/// The golden lines from the sync API, the gate, the search filter and the
/// tool lock.
async fn all_lines(tree: &Tree) -> Vec<String> {
    let roots = [tree.root.as_path(), tree.raw.as_path()];
    let cases = cases(&tree.root, &tree.raw);
    let mut lines: Vec<String> = cases.iter().map(|c| sync_line(c, &roots)).collect();
    lines.extend(gate_lines(&cases, &roots).await);
    lines.extend(search_lines(&tree.root, &tree.raw, None).await);
    lines.extend(lock_lines(&tree.root).await);
    lines.extend(swapped_lines(&tree.root).await);
    lines
}

#[tokio::test]
async fn gate_path_decisions_match_the_recorded_ones() {
    let tree = tree();
    let lines = all_lines(&tree).await;
    assert!(lines.len() > 5000, "{} lines", lines.len());
    check_golden(&lines);
}

/// The host env gives every decision the sync code gives (and so the
/// recorded one).
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
        if want != got {
            differ.push(format!("want {want}\n got {got}"));
        }
    }
    let want = search_lines(&tree.root, &tree.raw, None).await;
    let got = search_lines(&tree.root, &tree.raw, Some(&paths)).await;
    for (w, g) in want.iter().zip(&got) {
        if w != g {
            differ.push(format!("want {w}\n got {g}"));
        }
    }
    assert!(
        differ.is_empty(),
        "{}:\n{}",
        differ.len(),
        differ.join("\n")
    );
}

/// An env whose root is not on the host gives the same decisions: every
/// link, `..` and existence fact came from the env.
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
        if *want != got {
            differ.push(format!("want {want}\n got {got}"));
        }
    }
    assert!(checked > 3000, "{checked}");
    let want = search_lines(&tree.root, &tree.root, None).await;
    let got = search_lines(virt, virt, Some(&paths)).await;
    for (w, g) in want.iter().zip(&got) {
        if w != g {
            differ.push(format!("want {w}\n got {g}"));
        }
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
