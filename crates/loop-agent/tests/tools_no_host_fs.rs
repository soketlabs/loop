//! Source guard: the built-in tools reach a Workspace only through its
//! execution env (`docs/design/workspace-driver.md` section 4: "every
//! Runtime-side check on a Workspace path goes through `env()`, never
//! `std::fs`"). A clippy `disallowed_methods` rule cannot be limited to one
//! folder, so this test reads the sources of `src/harness/tools/**` and fails
//! on a host file-system call in non-test code, outside a short allowlist.
//!
//! The scan works on the code with comments and string and character
//! literals blanked, and with every `#[cfg(test)]` item removed (found by
//! its braces, wherever it stands in the file), so production code after a
//! test module is checked too.

use std::path::{Path, PathBuf};

/// Calls and names that reach the host disk, a host program or a host walk.
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
];

/// Files that may use host calls, and why.
const ALLOWED_FILES: &[(&str, &str)] = &[(
    "documents/child.rs",
    "the document reader child runs on the Runtime side by design; its host \
     calls check where the reader program is, not a Workspace file, and it \
     starts the reader process",
)];

/// Files that hold only tests; each must be declared under `#[cfg(test)]`
/// by the module named here.
const TEST_ONLY_FILES: &[(&str, &str, &str)] = &[(
    "documents/hostile_tests.rs",
    "documents/mod.rs",
    "#[cfg(test)]\nmod hostile_tests;",
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

/// `text` with comments and the insides of string and character literals
/// replaced by spaces (newlines stay, so line numbers hold).
fn blank_literals(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = b.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            let end = b[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(b.len(), |p| i + p);
            blank(&mut out, i, end);
            i = end;
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            let mut j = i;
            while j < b.len() {
                if b[j] == '/' && b.get(j + 1) == Some(&'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == '*' && b.get(j + 1) == Some(&'/') {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if c == 'r' || (c == 'b' && next == Some('r')) {
            // A raw string: r"..", r#".."#, br"..".
            let start = if c == 'b' { i + 1 } else { i };
            let mut j = start + 1;
            while b.get(j) == Some(&'#') {
                j += 1;
            }
            let prev_ident = i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_');
            if b.get(j) == Some(&'"') && !prev_ident {
                let hashes = j - start - 1;
                let close: Vec<char> = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let mut k = j + 1;
                while k + close.len() <= b.len() && b[k..k + close.len()] != close[..] {
                    k += 1;
                }
                let end = (k + close.len()).min(b.len());
                blank(&mut out, j + 1, k.min(b.len()));
                i = end;
            } else {
                i += 1;
            }
        } else if c == '"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != '"' {
                j += if b[j] == '\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1, j.min(b.len()));
            i = j + 1;
        } else if c == '\'' {
            // A character literal, not a lifetime.
            let close = if next == Some('\\') {
                b[i + 2..]
                    .iter()
                    .position(|&c| c == '\'')
                    .map(|p| i + 2 + p)
            } else if b.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match close {
                Some(end) => {
                    blank(&mut out, i + 1, end);
                    i = end + 1;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// Production code of a source file: literals blanked and every
/// `#[cfg(test)]` item (a module, function or `use`) removed.
fn production_code(text: &str) -> String {
    let blanked = blank_literals(text);
    let mut b: Vec<char> = blanked.chars().collect();
    let attr: Vec<char> = "#[cfg(test)]".chars().collect();
    let mut i = 0;
    while i + attr.len() <= b.len() {
        if b[i..i + attr.len()] != attr[..] {
            i += 1;
            continue;
        }
        // The item runs from the attribute to the `;` or the closing brace.
        let mut j = i + attr.len();
        let mut depth = 0usize;
        let mut entered = false;
        while j < b.len() {
            match b[j] {
                '{' => {
                    depth += 1;
                    entered = true;
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    if entered && depth == 0 {
                        j += 1;
                        break;
                    }
                }
                ';' if !entered => {
                    j += 1;
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        for c in &mut b[i..j] {
            if *c != '\n' {
                *c = ' ';
            }
        }
        i = j;
    }
    b.into_iter().collect()
}

/// Every forbidden name in `code`, as `(line, name)`; imports from `std` or
/// `tokio` that bring in `fs` or `process` count (`use std::{fs, io};`).
fn violations(code: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (n, line) in code.lines().enumerate() {
        for needle in FORBIDDEN {
            if line.contains(needle) {
                found.push((n + 1, (*needle).to_string()));
            }
        }
    }
    // `use` statements can span lines: check each whole statement.
    let mut rest = code;
    let mut offset = 0;
    while let Some(at) = rest.find("use ") {
        let before_ok = at == 0 || !rest[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
        let end = rest[at..].find(';').map_or(rest.len(), |e| at + e);
        let statement = &rest[at..end];
        let root = statement[4..].trim_start();
        if before_ok && (root.starts_with("std::") || root.starts_with("tokio::")) {
            let words: Vec<&str> = statement
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .collect();
            for bad in ["fs", "process"] {
                if words.contains(&bad) {
                    let line = code[..offset + at].matches('\n').count() + 1;
                    found.push((line, format!("use ... {bad}")));
                }
            }
        }
        offset += end;
        rest = &rest[end..];
    }
    found
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
        if ALLOWED_FILES.iter().any(|(name, _)| *name == rel)
            || TEST_ONLY_FILES.iter().any(|(name, _, _)| *name == rel)
        {
            continue;
        }
        let code = production_code(&std::fs::read_to_string(file).unwrap());
        for (line, needle) in violations(&code) {
            let text = code.lines().nth(line - 1).unwrap_or("").trim().to_string();
            found.push(format!("{rel}:{line}: `{needle}` in `{text}`"));
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
fn the_allowlists_name_files_that_exist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/harness/tools");
    for (name, reason) in ALLOWED_FILES {
        assert!(
            root.join(name).is_file(),
            "{name} is gone; drop it: {reason}"
        );
    }
    for (name, parent, declaration) in TEST_ONLY_FILES {
        assert!(root.join(name).is_file(), "{name} is gone; drop it");
        let text = std::fs::read_to_string(root.join(parent)).unwrap();
        assert!(
            text.contains(declaration),
            "{name} must stay declared under #[cfg(test)] in {parent}"
        );
    }
}

/// The guard must be able to fail, and must not fail on test code.
#[test]
fn the_guard_catches_a_host_call_and_skips_test_items() {
    let bad = |src: &str| !violations(&production_code(src)).is_empty();
    assert!(bad("fn f(p: &std::path::Path) -> bool { p.exists() }\n"));
    assert!(bad("use std::{fs, io};\nfn f() {}\n"));
    assert!(bad("use std::{\n    io,\n    fs::File,\n};\n"));
    assert!(bad("fn f(p: &Path) -> bool { Path::is_dir(p) }\n"));
    assert!(bad("fn f(p: &Path) -> bool { p.is_symlink() }\n"));
    assert!(bad(
        "fn f() { let _ = std::process::Command::new(\"x\"); }\n"
    ));
    assert!(bad("use ignore::WalkBuilder;\n"));
    assert!(bad("fn f() { File::options(); }\n"));
    // Production code after a test module still counts.
    assert!(bad(
        "#[cfg(test)]\nmod tests {\n    fn t() { let s = \"{\"; std::fs::read(\"x\"); }\n}\nfn after() { std::fs::read(\"y\"); }\n"
    ));
    // Test items, comments and strings do not.
    assert!(!bad(
        "fn f() {}\n#[cfg(test)]\nmod t { fn g() { std::fs::read(\"x\"); } }\n"
    ));
    assert!(!bad("#[cfg(test)]\nuse std::fs;\nfn f() {}\n"));
    assert!(!bad(
        "// std::fs is not allowed\nfn f() { let _ = \"p.exists()\"; let _ = '{'; }\n"
    ));
    assert!(!bad("fn f<'a>(x: &'a str) -> &'a str { x }\n"));
    assert!(!bad("use std::io;\nuse std::path::Path;\n"));
}
