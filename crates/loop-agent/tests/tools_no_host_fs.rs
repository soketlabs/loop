//! Source guard: the built-in tools reach a Workspace only through its
//! execution env (`docs/design/workspace-driver.md` section 4: "every
//! Runtime-side check on a Workspace path goes through `env()`, never
//! `std::fs`"). A clippy `disallowed_methods` rule cannot be limited to one
//! folder, so this test reads the sources of `src/harness/tools/**` and fails
//! on a host file-system call in non-test code, outside a short allowlist.

use std::path::{Path, PathBuf};

/// Calls that look at the host disk or the host process directory.
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
    "OpenOptions",
    ".exists()",
    ".is_file()",
    ".is_dir()",
    ".canonicalize()",
    ".metadata()",
    ".symlink_metadata()",
    ".read_link()",
    ".read_dir()",
    ".try_exists()",
    "current_dir()",
];

/// Files that may use host calls, and why.
const ALLOWED_FILES: &[(&str, &str)] = &[(
    "documents/child.rs",
    "the document reader child runs on the Runtime side by design; its host \
     calls check where the reader program is, not a Workspace file",
)];

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

/// The non-test code of a source file: everything before its first
/// `#[cfg(test)]` (test modules close the files in this folder), without
/// comment lines.
fn production_code(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim() == "#[cfg(test)]" {
            break;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[test]
fn tools_use_no_host_file_calls() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/harness/tools");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(files.len() > 10, "the guard found no sources in {root:?}");

    let mut found = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // Files that hold only tests.
        if rel.ends_with("hostile_tests.rs") {
            continue;
        }
        if ALLOWED_FILES.iter().any(|(name, _)| *name == rel) {
            continue;
        }
        let code = production_code(&std::fs::read_to_string(file).unwrap());
        for (n, line) in code.lines().enumerate() {
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    found.push(format!("{rel}:{}: `{needle}` in `{}`", n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "host file-system calls in the tools; go through the ExecutionEnv instead \
         (or add the file to ALLOWED_FILES with a reason):\n{}",
        found.join("\n")
    );
}

#[test]
fn the_allowlist_names_files_that_exist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/harness/tools");
    for (name, reason) in ALLOWED_FILES {
        assert!(
            root.join(name).is_file(),
            "{name} is gone; drop it: {reason}"
        );
    }
}

/// The guard must be able to fail: it sees a violation in a sample.
#[test]
fn the_guard_catches_a_host_call() {
    let sample = "fn f(p: &std::path::Path) -> bool { p.exists() }\n";
    let code = production_code(sample);
    assert!(FORBIDDEN.iter().any(|n| code.contains(n)));
    let tests_only = "fn f() {}\n#[cfg(test)]\nmod t { fn g() { std::fs::read(\"x\"); } }\n";
    let code = production_code(tests_only);
    assert!(!FORBIDDEN.iter().any(|n| code.contains(n)));
}
