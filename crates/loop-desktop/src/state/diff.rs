//! Pending file edits for diff review.

use std::path::PathBuf;

/// A file edit awaiting accept/reject in the diff panel.
#[derive(Debug, Clone)]
pub struct PendingFileChange {
    pub id: String,
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
    pub added: usize,
    pub removed: usize,
    pub reviewed: bool,
}

impl PendingFileChange {
    pub fn from_paths(path: PathBuf, before: Option<String>, after: String) -> Self {
        let (added, removed) = diff_stats(before.as_deref().unwrap_or(""), &after);
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            path,
            before,
            after,
            added,
            removed,
            reviewed: false,
        }
    }
}

/// Compute line add/remove counts with similar.
pub fn diff_stats(before: &str, after: &str) -> (usize, usize) {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// One line in a chat-card diff preview.
#[derive(Debug, Clone)]
pub struct DiffPreviewLine {
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub tag: similar::ChangeTag,
    pub text: String,
}

/// Compact line-oriented preview favoring changed lines (with light equal context).
pub fn preview_diff_lines(before: &str, after: &str, max_lines: usize) -> Vec<DiffPreviewLine> {
    if max_lines == 0 {
        return Vec::new();
    }
    let diff = similar::TextDiff::from_lines(before, after);
    let mut out = Vec::new();
    let mut old_no = 1usize;
    let mut new_no = 1usize;
    let mut equal_run: Vec<DiffPreviewLine> = Vec::new();

    let flush_equal = |equal_run: &mut Vec<DiffPreviewLine>, out: &mut Vec<DiffPreviewLine>| {
        if equal_run.is_empty() {
            return;
        }
        // Keep a single trailing context line before changes.
        if let Some(last) = equal_run.pop() {
            out.push(last);
        }
        equal_run.clear();
    };

    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches('\n').to_string();
        match change.tag() {
            similar::ChangeTag::Equal => {
                equal_run.push(DiffPreviewLine {
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    tag: similar::ChangeTag::Equal,
                    text,
                });
                old_no += 1;
                new_no += 1;
            }
            similar::ChangeTag::Delete => {
                flush_equal(&mut equal_run, &mut out);
                out.push(DiffPreviewLine {
                    old_no: Some(old_no),
                    new_no: None,
                    tag: similar::ChangeTag::Delete,
                    text,
                });
                old_no += 1;
            }
            similar::ChangeTag::Insert => {
                flush_equal(&mut equal_run, &mut out);
                out.push(DiffPreviewLine {
                    old_no: None,
                    new_no: Some(new_no),
                    tag: similar::ChangeTag::Insert,
                    text,
                });
                new_no += 1;
            }
        }
        if out.len() >= max_lines {
            break;
        }
    }

    // If the file was only equal / tiny, show the start of the after file.
    if out.is_empty() {
        for (i, line) in after.lines().take(max_lines).enumerate() {
            out.push(DiffPreviewLine {
                old_no: Some(i + 1),
                new_no: Some(i + 1),
                tag: similar::ChangeTag::Equal,
                text: line.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_prefers_changed_lines() {
        let before = "a\nb\nc\n";
        let after = "a\nB\nc\n";
        let lines = preview_diff_lines(before, after, 10);
        assert!(lines.iter().any(|l| l.tag == similar::ChangeTag::Delete));
        assert!(lines.iter().any(|l| l.tag == similar::ChangeTag::Insert));
    }
}

/// Reject a pending change by restoring `before` or git checkout.
pub fn reject_change(change: &PendingFileChange) -> anyhow::Result<()> {
    if let Some(before) = &change.before {
        if let Some(parent) = change.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&change.path, before)?;
        return Ok(());
    }
    let status = std::process::Command::new("git")
        .args(["checkout", "--", change.path.to_string_lossy().as_ref()])
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => anyhow::bail!("could not revert {}", change.path.display()),
    }
}
