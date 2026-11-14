//! Source guard: the permission gate checks Workspace paths only through
//! the Session's env (`docs/design/workspace-driver.md` sections 4 and 6:
//! "every Runtime-side check on a Workspace path goes through `env()`,
//! never `std::fs`"). This test reads the production code of
//! `src/permissions/**`, `src/tool_gate.rs` and `src/tool_approval.rs` and
//! fails on a host file-system call, or a use of the Runtime host's path
//! facts, outside the Runtime-side functions named in [`ALLOWED`].

#[path = "../../loop-agent/tests/support/source_guard.rs"]
mod source_guard;

use std::path::{Path, PathBuf};

use source_guard::{function_at, functions, production_code};

/// Calls and names that reach the host disk, a host program or a host
/// walk, and the gate's own host fallbacks.
const FORBIDDEN: &[&str] = &[
    "std::fs",
    "tokio::fs",
    "fs::read",
    "fs::write",
    "fs::metadata",
    "fs::canonicalize",
    "fs::create_dir",
    "fs::remove",
    "fs::File",
    "File::open",
    "File::create",
    "File::options",
    "OpenOptions",
    "DirBuilder",
    ".exists()",
    ".is_file()",
    ".is_dir()",
    ".is_symlink()",
    ".canonicalize()",
    ".metadata()",
    ".symlink_metadata()",
    ".read_link()",
    ".read_dir()",
    ".try_exists()",
    "Path::exists(",
    "Path::is_file(",
    "Path::is_dir(",
    "Path::is_symlink(",
    "Path::canonicalize(",
    "Path::metadata(",
    "current_dir()",
    "walkdir",
    "WalkBuilder",
    "std::process::Command",
    "tokio::process",
    "Command::new",
    // Other host facts: the home folder, the temp folder, system calls.
    "dirs::home_dir",
    "std::env::temp_dir",
    "libc::",
    // The gate's host fallbacks: only the functions below may pick them.
    "PathFacts::runtime_host(",
    "host_canonical(",
    "host_read_link(",
    "host_lstat(",
    "permissions::workspace_root(",
    "HostExecutionEnv",
];

/// The gate's sync wrappers that take host facts by default. Banned in
/// `tool_gate.rs` and `tool_approval.rs`: a gate decision uses the
/// `_with` / `_in` forms with facts from `GatePaths`.
const HOST_WRAPPERS: &[&str] = &[
    "Call::new(",
    "Call::resolve(",
    "Call::resolve_in(",
    "search_may_show(",
    "search_may_show_in(",
    ".allows(",
    ".moved()",
    "inside_workspace(",
    "inside_folder(",
];

/// Runtime-side functions that may use host calls: `(file, function,
/// reason)`. A function in an `impl` is named `Type::function`. A gate decision on a Workspace path never runs them.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "permissions/paths.rs",
        "host_canonical",
        "the Runtime host source of path facts (PathFacts::runtime_host), used only by the functions below",
    ),
    (
        "permissions/paths.rs",
        "host_read_link",
        "the Runtime host source of path facts (PathFacts::runtime_host)",
    ),
    (
        "permissions/paths.rs",
        "host_lstat",
        "the Runtime host source of path facts (PathFacts::runtime_host)",
    ),
    (
        "permissions/paths.rs",
        "GatePaths::host",
        "the host env for a gate that no Session bound to a Workspace env (tests, hosts that only gate agents); still read through the env",
    ),
    (
        "permissions/paths.rs",
        "HostLookup::canonical",
        "PathFacts::runtime_host: the Runtime host facts",
    ),
    (
        "permissions/paths.rs",
        "HostLookup::read_link",
        "PathFacts::runtime_host: the Runtime host facts",
    ),
    (
        "permissions/paths.rs",
        "HostLookup::lstat",
        "PathFacts::runtime_host: the Runtime host facts",
    ),
    (
        "permissions/paths.rs",
        "SearchMemo::canonical",
        "the sync search filter's memo for one search: host facts only for a host-namespace env (GatePaths::sync_facts); SearchFilter is sync (tools/search.rs) and cannot wait for the env",
    ),
    (
        "permissions/paths.rs",
        "SearchMemo::read_link",
        "the sync search filter's memo for one search: host facts only for a host-namespace env (GatePaths::sync_facts); SearchFilter is sync (tools/search.rs) and cannot wait for the env",
    ),
    (
        "permissions/paths.rs",
        "SearchMemo::lstat",
        "the sync search filter's memo for one search: host facts only for a host-namespace env (GatePaths::sync_facts); SearchFilter is sync (tools/search.rs) and cannot wait for the env",
    ),
    (
        "permissions/policy.rs",
        "Call::new",
        "the sync API (tests, the E3 probe, external agents in agent_gate.rs): Runtime host facts; banned in tool_gate.rs and tool_approval.rs",
    ),
    (
        "permissions/policy.rs",
        "Call::resolve_in",
        "the sync API (tests, the E3 probe), on the Runtime host",
    ),
    (
        "permissions/policy.rs",
        "Call::resolve_with",
        "reads the HOME variable (not the disk), and only when the facts know the home (host namespace)",
    ),
    (
        "permissions/policy.rs",
        "inside_workspace",
        "the sync API (tests, the E3 probe), on the Runtime host",
    ),
    (
        "permissions/policy.rs",
        "workspace_root",
        "the Runtime's own name for a Session folder (loop-runtime sessions.rs) and bare gates; a Session's gate uses GatePaths::workspace_root",
    ),
    (
        "permissions/rules.rs",
        "search_may_show_in",
        "the sync API (tests, the E3 probe), on the Runtime host",
    ),
    (
        "permissions/tool_lock.rs",
        "RuntimeToolLock::new",
        "the tool lock's root is admitted on the Runtime host at Runtime start",
    ),
    (
        "permissions/tool_lock.rs",
        "RuntimeToolLock::admit",
        "the tool lock admits a Session folder under its host root at Session open",
    ),
    (
        "permissions/tool_lock.rs",
        "Confinement::new",
        "the folder and root resolve on the Runtime host at Session open; the lock needs a host-namespace driver",
    ),
    (
        "permissions/tool_lock.rs",
        "Confinement::moved",
        "the sync API on the Runtime host; the gate uses moved_in with env facts",
    ),
    (
        "permissions/tool_lock.rs",
        "Confinement::allows",
        "the sync API on the Runtime host; the gate uses allows_in with env facts",
    ),
    (
        "permissions/tool_lock.rs",
        "inside_folder",
        "the sync API on the Runtime host; the gate uses inside_folder_in with env facts",
    ),
    (
        "permissions/tool_lock.rs",
        "skill_runs_code",
        "skill folders are checked at Session open under the tool lock, which needs a host-namespace driver; global skills are Runtime files",
    ),
    (
        "permissions/decision.rs",
        "local_user",
        "the OS user of the Runtime process, for decision records",
    ),
    (
        "permissions/decision.rs",
        "rotate",
        "decision records go to the audit folder in the agent directory (Runtime side)",
    ),
    (
        "permissions/decision.rs",
        "open_private",
        "decision records go to the audit folder in the agent directory (Runtime side)",
    ),
    (
        "permissions/decision.rs",
        "append_private",
        "decision records go to the audit folder in the agent directory (Runtime side)",
    ),
    (
        "permissions/decision.rs",
        "<outside a function>",
        "the record sink's open audit file (a struct field type)",
    ),
    (
        "tool_gate.rs",
        "ToolGate::new_for_workspace",
        "a gate without a Session env (tests, hosts that only gate agents): its folder is a Runtime host folder",
    ),
    (
        "tool_gate.rs",
        "ToolGate::install",
        "fallback git root for a gate without a Session env (host folder of the harness)",
    ),
    (
        "tool_gate.rs",
        "ToolGate::available",
        "no path argument (Null args): no path fact is read",
    ),
    (
        "tool_gate.rs",
        "ToolGate::record",
        "no path argument (Null args): only the kind and the Capability of the call",
    ),
    (
        "tool_gate.rs",
        "ToolGate::workspace",
        "fallback to the process cwd for a gate without a folder (not a file-system read)",
    ),
    (
        "tool_gate.rs",
        "ToolGate::agent_call_denied_by",
        "external agents run on the Runtime host, in their own host folder",
    ),
    (
        "tool_gate.rs",
        "ToolGate::search_filter",
        "reads the HOME variable (not the disk); a guest Session's filter has no facts, so `~` rules fail closed there",
    ),
    (
        "tool_approval.rs",
        "open_diff",
        "starts the user's diff editor on the Runtime host",
    ),
    (
        "tool_approval.rs",
        "which_ok",
        "looks for the user's diff editor on the Runtime host",
    ),
    (
        "tool_approval.rs",
        "<outside a function>",
        "the import of `std::process` for the diff editor",
    ),
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn gate_files() -> (PathBuf, Vec<PathBuf>) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src.join("permissions"), &mut files);
    files.push(src.join("tool_gate.rs"));
    files.push(src.join("tool_approval.rs"));
    // External agents' calls pass the same gate.
    files.push(src.join("agent_gate.rs"));
    (src, files)
}

/// Every forbidden name in `code`, as `(line, name)`; imports of `fs` or
/// `process` from `std` or `tokio` count.
fn violations(code: &str, gate_file: bool) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let wrappers: &[&str] = if gate_file { HOST_WRAPPERS } else { &[] };
    for (n, line) in code.lines().enumerate() {
        for needle in FORBIDDEN.iter().chain(wrappers) {
            if line.contains(needle) {
                found.push((n + 1, (*needle).to_string()));
            }
        }
        // The `nix` crate (not `unix::`).
        if line
            .match_indices("nix::")
            .any(|(at, _)| !line[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
        {
            found.push((n + 1, "nix::".into()));
        }
        let trimmed = line.trim_start();
        if (trimmed.starts_with("use std::") || trimmed.starts_with("use tokio::"))
            && trimmed
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == "fs" || w == "process")
        {
            found.push((n + 1, "use ... fs/process".into()));
        }
    }
    found
}

#[test]
fn the_gate_uses_no_host_file_calls() {
    let (src, files) = gate_files();
    assert!(files.len() >= 10, "the guard found no sources in {src:?}");
    let mut found = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let code = production_code(&std::fs::read_to_string(file).unwrap());
        let fns = functions(&code);
        let gate_file = rel == "tool_gate.rs" || rel == "tool_approval.rs";
        for (line, needle) in violations(&code, gate_file) {
            let function = function_at(&fns, line).unwrap_or("<outside a function>");
            if let Some(at) = ALLOWED
                .iter()
                .position(|(f, name, _)| *f == rel && *name == function)
            {
                used.insert(at);
                continue;
            }
            let text = code.lines().nth(line - 1).unwrap_or("").trim().to_string();
            found.push(format!(
                "{rel}:{line} in `{function}`: `{needle}` in `{text}`"
            ));
        }
    }
    assert!(
        found.is_empty(),
        "host file-system calls in the gate; go through GatePaths / the Session env \
         (or name a Runtime-side function in ALLOWED with a reason):\n{}",
        found.join("\n")
    );
    let stale: Vec<_> = ALLOWED
        .iter()
        .enumerate()
        .filter(|(at, _)| !used.contains(at))
        .map(|(_, (file, function, _))| format!("{file}: {function}"))
        .collect();
    assert!(
        stale.is_empty(),
        "allowed but no host call any more: {stale:?}"
    );
}

#[test]
fn every_allowed_function_exists() {
    let (src, _) = gate_files();
    for (file, function, reason) in ALLOWED {
        if *function == "<outside a function>" {
            continue;
        }
        let code = production_code(&std::fs::read_to_string(src.join(file)).unwrap());
        assert!(
            functions(&code).iter().any(|(_, _, name)| name == function),
            "{file}: `{function}` is gone; drop it from ALLOWED ({reason})"
        );
    }
}

/// The guard must fail on a host call in a gate function, and must not on
/// an allowed one or on test code.
#[test]
fn the_guard_catches_a_host_call_in_a_gate_function() {
    let found = |src: &str| -> Vec<String> {
        let code = production_code(src);
        let fns = functions(&code);
        violations(&code, true)
            .into_iter()
            .map(|(line, _)| function_at(&fns, line).unwrap_or("").to_owned())
            .collect()
    };
    assert_eq!(
        found("fn decide(p: &Path) -> bool {\n    p.exists()\n}\n"),
        ["decide"]
    );
    assert_eq!(
        found("impl X {\n    fn a(&self) {}\n    fn b(&self) { let _ = PathFacts::runtime_host(); }\n}\n"),
        ["X::b"]
    );
    assert_eq!(
        found("impl<T: Send> Tr for a::Y<T> where T: Sync {\n    fn c() { let _ = dirs::home_dir(); }\n}\n"),
        ["Y::c"]
    );
    assert_eq!(
        found("fn f() {\n    let c = Call::resolve_in(t, k, a, w, c);\n}\n"),
        ["f"]
    );
    assert!(
        found("#[cfg(all(test, unix))]\nmod t { fn g() { std::fs::read(\"x\"); } }\n").is_empty()
    );
    assert!(
        found("#[cfg(test)]\nmod t { fn g() { std::fs::read(\"x\"); } }\nfn f() {}\n").is_empty()
    );
}
