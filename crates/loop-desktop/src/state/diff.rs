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
