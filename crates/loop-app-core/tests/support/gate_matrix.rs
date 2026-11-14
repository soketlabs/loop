//! The gate's path-decision matrix (task C2b), shared by `gate_parity.rs`
//! (the recorded decisions; it builds against staging too) and
//! `gate_parity_env.rs` (the same cases through the env). Only the sync API
//! and `ToolGate` are used here.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loop_app_core::permissions::tool_lock::{Confinement, ToolLock};
use loop_app_core::permissions::{
    search_may_show_in, Call, Decision, PermissionMode, Policy, ToolPermission, Verdict,
};
use loop_app_core::tool_gate::ToolGate;
use serde_json::json;

/// The fixture tree.
pub struct Tree {
    _dir: tempfile::TempDir,
    /// The root as the temp folder spelled it (`/var/...` on macOS).
    pub raw: PathBuf,
    /// The root with links resolved.
    pub root: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        // `repo/locked` has mode 000; give it back so the folder can go.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            self.root.join("repo/locked"),
            std::fs::Permissions::from_mode(0o755),
        );
    }
}

pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub fn tree() -> Tree {
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
    // A folder nobody may read (EACCES on the parent of `locked/f`).
    write(&r("repo/locked/f"), "f");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(r("repo/locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // A name that is not UTF-8, reached through a link. APFS refuses such
    // a name (EILSEQ); there the name has a newline and an escape instead.
    {
        use std::os::unix::ffi::OsStrExt;
        let mut odd = r("repo").join(std::ffi::OsStr::from_bytes(b"odd-\xff"));
        if std::fs::create_dir(&odd).is_err() {
            odd = r("repo/odd-\n\u{1b}[31m");
        }
        write(&odd.join("f"), "f");
        symlink(&odd, r("repo/nonutf")).unwrap();
    }
    Tree {
        _dir: dir,
        raw,
        root,
    }
}

/// Call paths. `$ROOT` is the fixture root.
pub const PATHS: &[&str] = &[
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
    "locked/f",
    "locked/new",
    "nonutf/f",
    "$DEEP",
];

/// A path deeper than the gate checks: 300 components under the cwd.
pub fn deep() -> String {
    "d/".repeat(300) + "f"
}

/// Rule sets over the defaults. `$ROOT` is the fixture root as resolved,
/// `$RAW` as the temp folder spelled it.
pub fn rule_sets() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
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

pub const MODES: &[PermissionMode] = &[
    PermissionMode::Ask,
    PermissionMode::AcceptEdits,
    PermissionMode::FullAccess,
];

pub const TOOLS: &[&str] = &["read", "write", "edit", "grep", "glob", "ls"];

/// Where the call runs: (name, cwd below the root, spelled raw).
pub const CWDS: &[(&str, &str, bool)] = &[
    ("repo", "repo", false),
    ("svc", "repo/svc", false),
    ("raw", "repo", true),
];

/// One case, with `$ROOT`, `$RAW` and `~` filled in.
#[derive(Clone)]
pub struct Case {
    pub id: String,
    pub rules: BTreeMap<String, ToolPermission>,
    pub mode: PermissionMode,
    pub tool: &'static str,
    pub args: serde_json::Value,
    pub workspace: PathBuf,
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// The case names a path outside the fixture (`/tmp`).
    pub host_only: bool,
    /// The case depends on the home folder (`~`).
    pub uses_home: bool,
}

pub fn fill(text: &str, root: &Path, raw: &Path) -> String {
    text.replace("$ROOT", &root.to_string_lossy())
        .replace("$RAW", &raw.to_string_lossy())
        .replace("$DEEP", &deep())
}

/// Every case over a tree whose root is `root` (as resolved) and `raw`
/// (as spelled).
pub fn cases(root: &Path, raw: &Path) -> Vec<Case> {
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
pub fn line(id: &str, verdict: &Verdict, roots: &[&Path]) -> String {
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
pub fn sync_line(case: &Case, roots: &[&Path]) -> String {
    let kind = loop_agent::protocol_events::tool_kind(case.tool);
    let call = Call {
        home: Some(case.home.clone()),
        ..Call::resolve_in(case.tool, kind, &case.args, &case.workspace, &case.cwd)
    };
    let verdict = Policy::default().decide_why(case.mode, &case.rules, &call);
    line(&case.id, &verdict, roots)
}

/// Walked paths for the search filter, below the root.
pub const WALKED: [&str; 13] = [
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
    "repo/locked/f",
];

/// One search-filter case: a walked file, rules and folders.
pub struct SearchCase {
    pub id: String,
    pub rules: BTreeMap<String, ToolPermission>,
    pub tool: &'static str,
    pub path: PathBuf,
    pub workspace: PathBuf,
    pub cwd: PathBuf,
    pub home: PathBuf,
}

/// The search-filter cases over a tree at `root` (as resolved) and `raw`.
pub fn search_cases(root: &Path, raw: &Path) -> Vec<SearchCase> {
    let mut out = Vec::new();
    for (set, entries) in rule_sets() {
        let mut settings = BTreeMap::new();
        for (key, value) in &entries {
            settings.insert(fill(key, root, raw), (*value).to_owned());
        }
        let rules = loop_app_core::tool_approval::permissions_from_settings(&settings);
        for &(cwd_name, cwd_rel, _) in CWDS.iter().take(2) {
            for tool in ["grep", "glob", "ls"] {
                for walked in WALKED {
                    out.push(SearchCase {
                        id: format!("search|{set}|{cwd_name}|{tool}|{walked}"),
                        rules: rules.clone(),
                        tool,
                        path: root.join(walked),
                        workspace: root.join("repo"),
                        cwd: root.join(cwd_rel),
                        home: root.join("home"),
                    });
                }
            }
        }
    }
    out
}

/// What a search tool may show on the Runtime host, as lines.
pub fn search_lines(root: &Path, raw: &Path) -> Vec<String> {
    search_cases(root, raw)
        .iter()
        .map(|c| {
            let shown = search_may_show_in(
                &c.rules,
                c.tool,
                &c.path,
                &c.workspace,
                &c.cwd,
                &[],
                Some(&c.home),
            );
            format!("{} => {shown}", c.id)
        })
        .collect()
}

/// The tool lock: what a locked read may reach, through the gate.
pub async fn lock_lines(root: &Path) -> Vec<String> {
    let folder = root.join("repo");
    let rules = loop_app_core::tool_approval::permissions_from_settings(&BTreeMap::new());
    let gate = ToolGate::new_for_workspace(rules, &folder);
    gate.lock_tools(ToolLock::Chat);
    gate.confine(Confinement::new(&folder, root).unwrap());
    let mut out = Vec::new();
    for tool in ["read", "grep", "glob", "ls", "write"] {
        for path in PATHS {
            if path.starts_with('~')
                || path.starts_with("/tmp")
                || path.starts_with("/private")
                || path.starts_with("locked")
            {
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
pub async fn swapped_lines(root: &Path) -> Vec<String> {
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
pub async fn gate_lines(cases: &[Case], roots: &[&Path]) -> Vec<String> {
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

pub fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/data/gate_parity.{}.golden",
        std::env::consts::OS
    ))
}

/// How strict a line's decision is: allow < ask < deny (search: shown <
/// hidden).
pub fn strictness(line: &str) -> u8 {
    let verdict = line.split(" => ").nth(1).unwrap_or("");
    match verdict.split(' ').next().unwrap_or("") {
        "allow" | "true" => 0,
        "ask" => 1,
        _ => 2,
    }
}

/// Compare `got` with the golden file, or write it. A line whose id has a
/// marker of `stricter_ok` may differ when it is stricter; those lines are
/// returned.
pub fn check_golden(got: &[String], stricter_ok: &[&str]) -> Vec<String> {
    let path = golden_path();
    let text = got.join("\n") + "\n";
    if std::env::var_os("LOOP_GATE_GOLDEN_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return Vec::new();
    }
    let Ok(want) = std::fs::read_to_string(&path) else {
        eprintln!("no golden file for this platform: {}", path.display());
        return Vec::new();
    };
    let want: Vec<&str> = want.lines().collect();
    let got: Vec<&str> = text.lines().collect();
    let mut changed = Vec::new();
    let diff: Vec<String> = want
        .iter()
        .zip(&got)
        .filter(|(w, g)| w != g)
        .filter(|(w, g)| {
            let id = w.split(" => ").next().unwrap_or("");
            let excused =
                stricter_ok.iter().any(|m| id.contains(m)) && strictness(g) >= strictness(w);
            if excused {
                changed.push(format!("was {w}\n now {g}"));
            }
            !excused
        })
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
    changed
}

/// The golden lines from the sync API, the gate, the search filter and the
/// tool lock.
pub async fn all_lines(tree: &Tree) -> Vec<String> {
    let roots = [tree.root.as_path(), tree.raw.as_path()];
    let cases = cases(&tree.root, &tree.raw);
    let mut lines: Vec<String> = cases.iter().map(|c| sync_line(c, &roots)).collect();
    lines.extend(gate_lines(&cases, &roots).await);
    lines.extend(search_lines(&tree.root, &tree.raw));
    lines.extend(lock_lines(&tree.root).await);
    lines.extend(swapped_lines(&tree.root).await);
    lines
}
