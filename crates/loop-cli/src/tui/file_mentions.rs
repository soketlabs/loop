//! `@file` mention detection and cwd file autocomplete.

use std::path::Path;

/// An active `@…` token ending at the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtMention {
    /// Char index of `@`.
    pub start: usize,
    /// Char index of the caret (end of the query).
    pub end: usize,
    /// Text between `@` and the caret.
    pub query: String,
}

/// A file under the working directory.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Path relative to cwd, forward-slash separated.
    pub relative: String,
    /// Absolute filesystem path.
    pub absolute: String,
}

/// Find an `@file` query at `cursor` (character index), if any.
///
/// The mention starts at a word-boundary `@` and runs through non-whitespace
/// characters up to the caret. Email-like `user@host` is ignored.
pub fn find_at_mention(text: &str, cursor: usize) -> Option<AtMention> {
    let chars: Vec<char> = text.chars().collect();
    if cursor > chars.len() {
        return None;
    }

    let mut i = cursor;
    while i > 0 {
        let ch = chars[i - 1];
        if ch == '@' {
            let at = i - 1;
            if at > 0 {
                let prev = chars[at - 1];
                // Reject email / identifiers: `user@`, `foo.@`
                if prev.is_alphanumeric() || prev == '_' || prev == '.' {
                    return None;
                }
            }
            let query: String = chars[at + 1..cursor].iter().collect();
            return Some(AtMention {
                start: at,
                end: cursor,
                query,
            });
        }
        if ch.is_whitespace() {
            return None;
        }
        i -= 1;
    }
    None
}

/// Walk `cwd` (honoring gitignore) and collect files for autocomplete.
pub fn list_files(cwd: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(16))
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let relative = rel.to_string_lossy().replace('\\', "/");
        if relative.is_empty() {
            continue;
        }
        let absolute = canonicalize_display(path);
        out.push(FileEntry {
            relative,
            absolute,
        });
        if out.len() >= 8_000 {
            break;
        }
    }
    out.sort_by(|a, b| a.relative.cmp(&b.relative));
    out
}

/// Filter and rank files by a partial query (case-insensitive).
pub fn filter_files(files: &[FileEntry], query: &str, limit: usize) -> Vec<FileEntry> {
    let q = query.to_lowercase();
    let mut scored: Vec<(i32, &FileEntry)> = files
        .iter()
        .filter_map(|f| score_match(&f.relative, &q).map(|s| (s, f)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.relative.len().cmp(&b.1.relative.len()))
            .then_with(|| a.1.relative.cmp(&b.1.relative))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, f)| f.clone())
        .collect()
}

/// Text to insert for a selected file (quoted when the path has spaces).
pub fn insert_text(absolute: &str) -> String {
    if absolute.chars().any(|c| c.is_whitespace()) {
        format!("\"{absolute}\"")
    } else {
        absolute.to_string()
    }
}

fn score_match(relative: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let rel = relative.to_lowercase();
    if rel == *query {
        return Some(4);
    }
    if rel.starts_with(query) {
        return Some(3);
    }
    let file_name = Path::new(relative)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if file_name.starts_with(query) {
        return Some(3);
    }
    if file_name.contains(query) {
        return Some(2);
    }
    if rel.contains(&format!("/{query}")) {
        return Some(2);
    }
    if rel.contains(query) {
        return Some(1);
    }
    None
}

fn canonicalize_display(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn mention_at_start_and_mid() {
        assert_eq!(
            find_at_mention("@src", 4),
            Some(AtMention {
                start: 0,
                end: 4,
                query: "src".into(),
            })
        );
        assert_eq!(
            find_at_mention("see @src/app", 12),
            Some(AtMention {
                start: 4,
                end: 12,
                query: "src/app".into(),
            })
        );
        assert_eq!(find_at_mention("see @src and", 12), None);
        assert_eq!(find_at_mention("user@host", 9), None);
        assert_eq!(
            find_at_mention("@", 1),
            Some(AtMention {
                start: 0,
                end: 1,
                query: String::new(),
            })
        );
    }

    #[test]
    fn filter_narrows_by_query() {
        let files = vec![
            FileEntry {
                relative: "src/app.rs".into(),
                absolute: "/tmp/src/app.rs".into(),
            },
            FileEntry {
                relative: "src/lib.rs".into(),
                absolute: "/tmp/src/lib.rs".into(),
            },
            FileEntry {
                relative: "README.md".into(),
                absolute: "/tmp/README.md".into(),
            },
        ];
        let hits = filter_files(&files, "app", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].relative, "src/app.rs");

        let hits = filter_files(&files, "src/", 10);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn list_files_finds_nested() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("README.md"), "hi").unwrap();
        let files = list_files(dir.path());
        let rels: Vec<_> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"src/main.rs"));
        assert!(rels.contains(&"README.md"));
        assert!(files.iter().all(|f| Path::new(&f.absolute).is_absolute()));
    }
}
