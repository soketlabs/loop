//! Unified-diff helpers.
//!
//! Diffs are produced from two directory trees, so the agent never writes a
//! diff by hand. Walking a tree never follows symlinks and skips anything that
//! is not a small regular UTF-8 text file, because the workspace is written by
//! code the agent ran.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

/// Files larger than this are never read or diffed.
pub const MAX_FILE_BYTES: u64 = 1_000_000;
/// Directories never walked.
pub const SKIP_DIRS: &[&str] = &[".git", ".defensive", "vendor", "node_modules"];

const TEST_DIRS: &[&str] = &[
    "test", "tests", "__tests__", "spec", "specs", "testdata", "testing", "e2e", "cypress",
];

static TEST_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^test_.*\.py$|_test\.(py|go|rb|rs|exs?|c|cc|cpp)$|^conftest\.py$|\.(test|spec)\.[cm]?[jt]sx?$|_spec\.rb$|Tests?\.(java|kt|scala|php|cs)$|Spec\.(kt|scala|groovy)$)",
    )
    .unwrap()
});

/// Dependency manifests and lockfiles: a fix may not change them (there is no
/// network to install from).
pub const MANIFESTS: &[&str] = &[
    "go.mod", "go.sum", "package.json", "package-lock.json", "yarn.lock", "pnpm-lock.yaml",
    "requirements.txt", "Pipfile", "Pipfile.lock", "poetry.lock", "pyproject.toml", "setup.py",
    "setup.cfg", "Cargo.toml", "Cargo.lock", "pom.xml", "build.gradle", "build.gradle.kts",
    "settings.gradle", "settings.gradle.kts", "Gemfile", "Gemfile.lock", "composer.json",
    "composer.lock", "uv.lock",
];

/// Whether `path` is a test file of `lang` (`go`, or any other profile).
pub fn is_test_file(path: &str, lang: &str) -> bool {
    if lang == "go" {
        return path.ends_with("_test.go") || format!("/{path}").contains("/testdata/");
    }
    let parts: Vec<&str> = path.split('/').collect();
    let (dirs, name) = parts.split_at(parts.len().saturating_sub(1));
    dirs.iter().any(|d| TEST_DIRS.contains(d)) || name.first().is_some_and(|n| TEST_NAME.is_match(n))
}

/// Whether `path` is a source file a fix may change.
pub fn is_code_file(path: &str, lang: &str) -> bool {
    if lang == "go" {
        return path.ends_with(".go") && !is_test_file(path, "go");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    !is_test_file(path, lang)
        && !MANIFESTS.contains(&name)
        && !path.starts_with(".defensive/")
        && !path.starts_with(".git/")
}

/// A normalized relative POSIX path, or `None` if it escapes the root.
pub fn safe_relpath(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('\0') || path.starts_with('/') || path.contains('\\') {
        return None;
    }
    let mut parts = Vec::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    // Python's PurePosixPath drops "." but keeps ".."; an empty part means "//".
    if parts.is_empty() || path.split('/').any(|p| p == "..") {
        return None;
    }
    Some(parts.join("/"))
}

/// A regular, small, UTF-8 text file's content; `None` otherwise.
pub fn read_text(p: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(p).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let data = fs::read(p).ok()?;
    if data.contains(&0) {
        return None;
    }
    String::from_utf8(data).ok()
}

/// Regular files under `root`, keyed by relative path. No symlinks followed.
pub fn list_files(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let walker = walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        e.depth() == 0
            || !(e.file_type().is_dir() && SKIP_DIRS.contains(&e.file_name().to_str().unwrap_or("")))
    });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            if let Some(rel) = rel.to_str() {
                out.insert(rel.replace('\\', "/"), entry.path().to_path_buf());
            }
        }
    }
    out
}

/// One file's unified diff (`a/` and `b/` prefixes, `/dev/null` for adds and
/// deletes); empty when the contents are equal.
pub fn file_diff(rel: &str, old: Option<&str>, new: Option<&str>) -> String {
    let a = old.unwrap_or("");
    let b = new.unwrap_or("");
    if a == b {
        return String::new();
    }
    let from = if old.is_none() { "/dev/null".to_owned() } else { format!("a/{rel}") };
    let to = if new.is_none() { "/dev/null".to_owned() } else { format!("b/{rel}") };
    let diff = similar::TextDiff::from_lines(a, b);
    let mut udiff = diff.unified_diff();
    udiff.context_radius(3).missing_newline_hint(true).header(&from, &to);
    let mut text = udiff.to_string();
    if !text.ends_with('\n') && !text.is_empty() {
        text.push('\n');
    }
    text
}

/// The diff between a baseline tree and a workspace, split by file role.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TreeDiff {
    /// Non-test source files: the candidate fix.
    pub code_patch: String,
    /// Test files: the agent's own tests.
    pub test_patch: String,
    /// Files in `code_patch`.
    pub code_files: Vec<String>,
    /// Files in `test_patch`.
    pub test_files: Vec<String>,
    /// Other changed files (manifests, ...), in neither patch.
    pub ignored_files: Vec<String>,
}

/// Diff `work` against `base`.
pub fn diff_trees(base: &Path, work: &Path, lang: &str) -> TreeDiff {
    let b = list_files(base);
    let w = list_files(work);
    let all: BTreeSet<&String> = b.keys().chain(w.keys()).collect();
    let mut td = TreeDiff::default();
    for rel in all {
        let old = b.get(rel).map(|p| read_text(p));
        let new = w.get(rel).map(|p| read_text(p));
        // Binary, oversized or unreadable: never part of a patch.
        if matches!(old, Some(None)) || matches!(new, Some(None)) {
            continue;
        }
        let (old, new) = (old.flatten(), new.flatten());
        if old == new {
            continue;
        }
        let d = file_diff(rel, old.as_deref(), new.as_deref());
        if d.is_empty() {
            continue;
        }
        if is_code_file(rel, lang) {
            td.code_patch.push_str(&d);
            td.code_files.push(rel.clone());
        } else if is_test_file(rel, lang) {
            td.test_patch.push_str(&d);
            td.test_files.push(rel.clone());
        } else {
            td.ignored_files.push(rel.clone());
        }
    }
    td
}

static DIFF_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:---|\+\+\+) (?:[ab]/)?(\S+)").unwrap());

/// Target paths named in a unified diff (excluding `/dev/null`).
pub fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(m) = DIFF_PATH.captures(line) {
            let p = &m[1];
            if p != "/dev/null" && !paths.iter().any(|x| x == p) {
                paths.push(p.to_owned());
            }
        }
    }
    paths
}

/// Check a patch's format and paths: `(ok, detail, paths)`.
pub fn validate_patch(patch: &str, allow: impl Fn(&str) -> bool) -> (bool, String, Vec<String>) {
    if patch.trim().is_empty() {
        return (false, "empty patch".into(), vec![]);
    }
    if !patch.contains("@@") {
        return (false, "no hunks".into(), vec![]);
    }
    let paths = patch_paths(patch);
    if paths.is_empty() {
        return (false, "no target paths".into(), vec![]);
    }
    for p in &paths {
        if safe_relpath(p).as_deref() != Some(p.as_str()) {
            return (false, format!("unsafe path '{p}'"), paths);
        }
        if !allow(p) {
            return (false, format!("path not allowed '{p}'"), paths);
        }
    }
    let n = paths.len();
    (true, format!("{n} file(s)"), paths)
}

/// Hex SHA-256 of `text`.
pub fn sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

static TEST_FUNC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bfunc (Test\w+)\s*\(").unwrap());

/// Go test functions a diff adds or modifies (from `+` lines and hunk headers).
pub fn changed_test_funcs(patch: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in patch.lines() {
        if (line.starts_with('+') && !line.starts_with("+++")) || line.starts_with("@@") {
            for m in TEST_FUNC.captures_iter(line) {
                if !names.iter().any(|n| n == &m[1]) {
                    names.push(m[1].to_owned());
                }
            }
        }
    }
    names
}

/// Per-file chunks of a diff from [`diff_trees`]: `(path, chunk, is_new_file)`.
pub fn split_patch(patch: &str) -> Vec<(String, String, bool)> {
    let mut chunks: Vec<String> = Vec::new();
    let lines: Vec<&str> = patch.split_inclusive('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let starts = (lines[i].starts_with("--- a/") || lines[i].starts_with("--- /dev/null"))
            && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ "));
        if starts || chunks.is_empty() {
            chunks.push(String::new());
        }
        chunks.last_mut().unwrap().push_str(lines[i]);
        i += 1;
    }
    chunks
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .filter_map(|c| {
            let path = patch_paths(&c).into_iter().next()?;
            let new = c.starts_with("--- /dev/null");
            Some((path, c, new))
        })
        .collect()
}

/// Truncate long tool output, keeping both ends.
pub fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let head = floor_char(s, max / 2);
    let tail = ceil_char(s, s.len() - max / 2);
    format!("{}\n...[truncated]...\n{}", &s[..head], &s[tail..])
}

/// The last `n` bytes of `s` (on a char boundary).
pub fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    s[ceil_char(s, s.len() - n)..].to_owned()
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_and_code_files_by_language() {
        assert!(is_test_file("pkg/a_test.go", "go"));
        assert!(is_test_file("pkg/testdata/x.json", "go"));
        assert!(is_code_file("pkg/a.go", "go"));
        assert!(!is_code_file("pkg/a_test.go", "go"));
        assert!(is_test_file("test/ssrf.test.js", "node"));
        assert!(is_test_file("src/a.spec.ts", "node"));
        assert!(is_test_file("tests/test_x.py", "python"));
        assert!(is_test_file("app/FooTest.java", "maven"));
        assert!(is_code_file("src/v1/libs/websearch.client.js", "node"));
        assert!(!is_code_file("package.json", "node"));
        assert!(!is_code_file(".defensive/report.json", "node"));
    }

    #[test]
    fn safe_relpath_rejects_escapes() {
        assert_eq!(safe_relpath("a/./b"), Some("a/b".into()));
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a\\b", "a\0b"] {
            assert_eq!(safe_relpath(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn diff_trees_splits_code_and_tests_and_skips_binaries() {
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(base.path().join("a.go"), "package a\n\nfunc A() int { return 1 }\n").unwrap();
        fs::write(work.path().join("a.go"), "package a\n\nfunc A() int { return 2 }\n").unwrap();
        fs::write(work.path().join("a_test.go"), "package a\n\nfunc TestRepro(t *testing.T) {}\n").unwrap();
        fs::write(work.path().join("go.mod"), "module a\n").unwrap();
        fs::write(work.path().join("blob.go"), b"\x00\x01").unwrap();
        fs::create_dir(work.path().join(".defensive")).unwrap();
        fs::write(work.path().join(".defensive/report.json"), "{}").unwrap();
        let d = diff_trees(base.path(), work.path(), "go");
        assert_eq!(d.code_files, vec!["a.go"]);
        assert_eq!(d.test_files, vec!["a_test.go"]);
        assert_eq!(d.ignored_files, vec!["go.mod"]);
        assert!(d.code_patch.starts_with("--- a/a.go\n+++ b/a.go\n@@ "));
        assert!(d.test_patch.starts_with("--- /dev/null\n+++ b/a_test.go\n"));
        assert_eq!(changed_test_funcs(&d.test_patch), vec!["TestRepro"]);
        let (ok, detail, paths) = validate_patch(&d.code_patch, |p| is_code_file(p, "go"));
        assert!(ok, "{detail}");
        assert_eq!(paths, vec!["a.go"]);
        assert!(!validate_patch(&d.test_patch, |p| is_code_file(p, "go")).0);
    }

    #[test]
    fn missing_trailing_newline_is_marked() {
        let d = file_diff("x.txt", Some("a\nb"), Some("a\nc"));
        assert!(d.contains("\\ No newline at end of file"), "{d}");
    }

    #[test]
    fn split_patch_marks_new_files() {
        let patch = format!(
            "{}{}",
            file_diff("a.js", Some("x\n"), Some("y\n")),
            file_diff("b.js", None, Some("new\n"))
        );
        let chunks = split_patch(&patch);
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].0.as_str(), chunks[0].2), ("a.js", false));
        assert_eq!((chunks[1].0.as_str(), chunks[1].2), ("b.js", true));
    }

    #[test]
    fn validate_rejects_empty_and_unsafe() {
        assert_eq!(validate_patch("", |_| true).1, "empty patch");
        assert_eq!(validate_patch("--- a/x\n+++ b/x\n", |_| true).1, "no hunks");
        let bad = "--- a/../x\n+++ b/../x\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(validate_patch(bad, |_| true).1.starts_with("unsafe path"));
    }

    #[test]
    fn trunc_and_tail_respect_char_boundaries() {
        let s = "é".repeat(100);
        assert!(trunc(&s, 51).contains("[truncated]"));
        assert!(tail(&s, 3).chars().all(|c| c == 'é'));
    }
}
