//! Pure per-hunk action decisions for the `/diff` viewer (M6, §3.6/§5.4).
//!
//! Everything here is a total function of `(model, selection, expected
//! HunkId, action, busy)` — no I/O, no git, no terminal — so every rule is
//! unit-testable without a repo. The viewer applies the returned plan and
//! turns `Disabled`/`Blocked`/`Refreshed` into notice banners.

use crate::diff_model::{build_single_hunk_patch, DiffModel, DiffSide, FileStatus, HunkId};
use crate::git_diff::GitDiff;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Stage this hunk (`a` / enter).
    Accept,
    /// Discard this hunk from the worktree (`r` / backspace).
    Reject,
    /// Unstage this hunk from the index (`u` / backspace in staged view).
    Unstage,
}

/// What the viewer should do for an [`Action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPlan {
    /// The file cannot take this action; carries the notice text.
    Disabled(String),
    /// The agent is mid-turn; mutations are blocked.
    Blocked(String),
    /// The hunk changed under us; the caller re-reads and refreshes.
    Refreshed,
    /// Run `git apply` with this single-hunk patch (raw bytes; file content
    /// need not be UTF-8).
    Apply {
        patch: Vec<u8>,
        cached: bool,
        reverse: bool,
    },
    /// `git add -- <path>` for an untracked file.
    AddFile(String),
    /// Delete an untracked file after backing it up.
    DeleteUntracked(String),
}

/// Decide what `action` on `(file_idx, hunk_idx)` means, given the `expected`
/// [`HunkId`] captured when the hunk was last rendered (staleness check) and
/// whether the agent is busy. See §3.6 for the pipeline this feeds.
pub fn plan_action(
    model: &DiffModel,
    file_idx: usize,
    hunk_idx: usize,
    expected: HunkId,
    action: Action,
    busy: bool,
) -> ActionPlan {
    let Some(file) = model.files.get(file_idx) else {
        return ActionPlan::Disabled("no file selected".into());
    };
    // Guard: statuses that carry no applicable hunk.
    match file.status {
        FileStatus::Binary => {
            return ActionPlan::Disabled(format!(
                "{} is binary — no text hunks to act on",
                file.path
            ));
        }
        FileStatus::ModeOnly => {
            return ActionPlan::Disabled(format!("{} only changes the file mode", file.path));
        }
        FileStatus::Renamed | FileStatus::Copied => {
            return ActionPlan::Disabled(format!(
                "{} is a rename/copy — use git directly",
                file.path
            ));
        }
        FileStatus::TooLarge => {
            return ActionPlan::Disabled(format!(
                "{} is too large to review — use git add directly",
                file.path
            ));
        }
        FileStatus::Unmerged => {
            return ActionPlan::Disabled(format!(
                "{} has a merge conflict — resolve it first",
                file.path
            ));
        }
        FileStatus::Deleted => {
            return ActionPlan::Disabled(format!(
                "{} is deleted — nothing to {}",
                file.path,
                match action {
                    Action::Accept => "stage",
                    Action::Reject => "discard",
                    Action::Unstage => "unstage",
                }
            ));
        }
        FileStatus::Modified | FileStatus::Added => {}
    }

    // Busy guard comes after the status guard so a binary file still reports
    // "binary" rather than "busy" (the more specific reason wins).
    if busy {
        return ActionPlan::Blocked("agents are working — navigation only".into());
    }

    // Staleness: the hunk the user is looking at must still be there.
    let Some(hunk) = file.hunks.get(hunk_idx) else {
        return ActionPlan::Refreshed;
    };
    if hunk.id != expected {
        return ActionPlan::Refreshed;
    }

    // Side/action matrix (§3.6 step 5).
    match (action, model.side) {
        (Action::Accept, DiffSide::WorkingTree) => {
            if file.untracked {
                ActionPlan::AddFile(file.path.clone())
            } else {
                match build_single_hunk_patch(file, hunk_idx) {
                    Some(patch) => ActionPlan::Apply {
                        patch,
                        cached: true,
                        reverse: false,
                    },
                    None => ActionPlan::Disabled("cannot build a patch for this hunk".into()),
                }
            }
        }
        (Action::Reject, DiffSide::WorkingTree) => {
            if file.untracked {
                ActionPlan::DeleteUntracked(file.path.clone())
            } else {
                match build_single_hunk_patch(file, hunk_idx) {
                    Some(patch) => ActionPlan::Apply {
                        patch,
                        cached: false,
                        reverse: true,
                    },
                    None => ActionPlan::Disabled("cannot build a patch for this hunk".into()),
                }
            }
        }
        (Action::Unstage, DiffSide::Index) => match build_single_hunk_patch(file, hunk_idx) {
            Some(patch) => ActionPlan::Apply {
                patch,
                cached: true,
                reverse: true,
            },
            None => ActionPlan::Disabled("cannot build a patch for this hunk".into()),
        },
        (Action::Accept, DiffSide::Index) => {
            ActionPlan::Disabled("already staged — use the unstaged view to stage".into())
        }
        (Action::Reject, DiffSide::Index) => {
            ActionPlan::Disabled("use u (unstage) in the staged view".into())
        }
        (Action::Unstage, DiffSide::WorkingTree) => {
            ActionPlan::Disabled("this hunk is not staged — open /diff --staged".into())
        }
    }
}

/// Unique backup path: `<stem>-<unix-nanos>.<ext>` under `backup_dir`.
fn backup_path(backup_dir: &Path, file_path: &str, ext: &str) -> PathBuf {
    let stem = Path::new(file_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    backup_dir.join(format!("{stem}-{nanos}.{ext}"))
}

/// Path a notice should name for an [`ActionPlan::Apply`] patch: the `+++`
/// line's path (repo-root-relative), or "the hunk" if unreadable.
fn patch_path(patch: &[u8]) -> String {
    String::from_utf8_lossy(patch)
        .lines()
        .find(|l| l.starts_with("+++ "))
        .and_then(|l| l.strip_prefix("+++ b/").or_else(|| l.strip_prefix("+++ ")))
        .map(str::to_string)
        .unwrap_or_else(|| "the hunk".into())
}

/// Backups older than this are pruned on the next Reject/delete, so the
/// folder cannot grow without bound across sessions.
const BACKUP_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Create `dir` for backups and keep it private: 0o700 on unix even when it
/// already exists (a shared temp dir may have been created world-readable),
/// then prune stale backups. Fails with the caller's existing message.
async fn prepare_backup_dir(dir: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("cannot create backup dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| format!("cannot create backup dir: {e}"))?;
    }
    prune_old_backups(dir, BACKUP_MAX_AGE).await;
    Ok(())
}

/// Make a freshly written backup readable only by its owner (0o600 on
/// unix); other systems have no portable mode to set.
async fn make_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    #[cfg(not(unix))]
    {
        // No portable mode on this platform; the file stays as written.
        let _ = path;
    }
    Ok(())
}

/// Delete regular `.patch`/`.bak` files under `dir` whose modified time is
/// older than `max_age`. Symlinks are never followed and every error is
/// ignored: pruning must never block a Reject.
async fn prune_old_backups(dir: &Path, max_age: Duration) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        // Lstat, never follow: a symlink named *.patch must not be pruned
        // through to its target.
        let Ok(meta) = tokio::fs::symlink_metadata(entry.path()).await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.ends_with(".patch") || name.ends_with(".bak")) {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = SystemTime::now().duration_since(modified) else {
            continue;
        };
        if age > max_age {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Run an [`ActionPlan`] against the repo and return the notice text:
/// `Ok(success)` or `Err(failure)`. §3.6 step 5-7: backups are written
/// BEFORE any mutation, `--check` guards every apply, nothing is touched
/// when a guard fails.
pub async fn execute(git: &GitDiff, plan: ActionPlan, backup_dir: &Path) -> Result<String, String> {
    match plan {
        ActionPlan::Disabled(note) | ActionPlan::Blocked(note) => Err(note),
        ActionPlan::Refreshed => Err("hunk changed, refreshed".into()),
        ActionPlan::AddFile(path) => {
            git.add_file(&path).await.map_err(|e| e.to_string())?;
            Ok(format!("staged new file {path}"))
        }
        ActionPlan::DeleteUntracked(path) => {
            // Back up first: a failed copy must leave the file untouched.
            let target = backup_path(backup_dir, &path, "bak");
            if let Some(parent) = target.parent() {
                prepare_backup_dir(parent).await?;
            }
            let src = git.repo_root.join(&path);
            // Lstat, never follow: copying through a symlink would copy the
            // target's content (or hang on /dev/zero). A link backs up as
            // its target path; every other entry copies as today.
            match tokio::fs::symlink_metadata(&src).await {
                Ok(meta) if meta.is_symlink() => {
                    let dest = tokio::fs::read_link(&src)
                        .await
                        .map_err(|e| format!("cannot back up {path}: {e}"))?;
                    // The backup holds the link target path, not the
                    // target's content.
                    tokio::fs::write(&target, dest.as_os_str().as_encoded_bytes())
                        .await
                        .map_err(|e| format!("cannot back up {path}: {e}"))?;
                    make_private(&target)
                        .await
                        .map_err(|e| format!("cannot back up {path}: {e}"))?;
                }
                _ => {
                    tokio::fs::copy(&src, &target)
                        .await
                        .map_err(|e| format!("cannot back up {path}: {e}"))?;
                    make_private(&target)
                        .await
                        .map_err(|e| format!("cannot back up {path}: {e}"))?;
                }
            }
            git.delete_file(&path).await.map_err(|e| e.to_string())?;
            Ok(format!("deleted {path} · backup: \"{}\"", target.display()))
        }
        ActionPlan::Apply {
            patch,
            cached,
            reverse,
        } => {
            let path = patch_path(&patch);
            if !cached && reverse {
                // Reject: write the backup patch before touching anything so
                // even a failed apply leaves the user a recoverable patch.
                let target = backup_path(backup_dir, &path, "patch");
                if let Some(parent) = target.parent() {
                    prepare_backup_dir(parent).await?;
                }
                tokio::fs::write(&target, &patch)
                    .await
                    .map_err(|e| format!("cannot write backup patch: {e}"))?;
                make_private(&target)
                    .await
                    .map_err(|e| format!("cannot write backup patch: {e}"))?;
                git.check_apply(&patch, cached, reverse)
                    .await
                    .map_err(|e| e.to_string())?;
                git.apply(&patch, cached, reverse)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(format!(
                    "discarded 1 hunk in {path} · undo: git -C \"{}\" apply \"{}\"",
                    git.repo_root.display(),
                    target.display()
                ))
            } else {
                // Accept / unstage: check first, then apply.
                git.check_apply(&patch, cached, reverse)
                    .await
                    .map_err(|e| e.to_string())?;
                git.apply(&patch, cached, reverse)
                    .await
                    .map_err(|e| e.to_string())?;
                if cached && reverse {
                    Ok(format!("unstaged 1 hunk in {path}"))
                } else {
                    Ok(format!("staged 1 hunk in {path}"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_model::{DiffLine, DiffLineKind, FileDiff, Hunk};

    /// One modified file with `hunks` one-line hunks; ids are stable hashes
    /// via `hunk_id`, exactly like production.
    fn tracked_file(path: &str, hunks: usize, untracked: bool) -> FileDiff {
        let mut file = FileDiff {
            old_path: None,
            path: path.to_string(),
            status: FileStatus::Modified,
            old_mode: None,
            new_mode: None,
            untracked,
            hunks: Vec::new(),
        };
        for i in 0..hunks {
            let hunk = Hunk {
                old_start: 1 + i as u32,
                old_count: 1,
                new_start: 1 + i as u32,
                new_count: 1,
                section: None,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Removed,
                        text: format!("old {i}"),
                        no_newline: false,
                        old_no: Some(1 + i as u32),
                        new_no: None,
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        text: format!("new {i}"),
                        no_newline: false,
                        old_no: None,
                        new_no: Some(1 + i as u32),
                    },
                ],
                raw_body: format!("-old {i}\n+new {i}\n").into_bytes(),
                id: HunkId(0),
            };
            let id = crate::diff_model::hunk_id(&file, &hunk);
            let mut hunk = hunk;
            hunk.id = id;
            file.hunks.push(hunk);
        }
        file
    }

    fn model(side: DiffSide, files: Vec<FileDiff>) -> DiffModel {
        DiffModel {
            side,
            files,
            truncated: false,
        }
    }

    fn worktree(files: Vec<FileDiff>) -> DiffModel {
        model(DiffSide::WorkingTree, files)
    }

    fn index(files: Vec<FileDiff>) -> DiffModel {
        model(DiffSide::Index, files)
    }

    fn hunk_id_at(m: &DiffModel, f: usize, h: usize) -> HunkId {
        m.files[f].hunks[h].id
    }

    #[test]
    fn accept_on_worktree_stages_the_hunk() {
        let m = worktree(vec![tracked_file("a.rs", 2, false)]);
        let id = hunk_id_at(&m, 0, 1);
        match plan_action(&m, 0, 1, id, Action::Accept, false) {
            ActionPlan::Apply {
                patch,
                cached,
                reverse,
            } => {
                assert!(cached && !reverse);
                let text = String::from_utf8_lossy(&patch);
                assert!(text.contains("@@"), "{text}");
                assert!(text.contains("-old 1"), "{text}");
                assert!(text.contains("+new 1"), "{text}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn reject_on_worktree_reverses_into_the_worktree() {
        let m = worktree(vec![tracked_file("a.rs", 1, false)]);
        let id = hunk_id_at(&m, 0, 0);
        match plan_action(&m, 0, 0, id, Action::Reject, false) {
            ActionPlan::Apply {
                cached, reverse, ..
            } => assert!(!cached && reverse),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn unstage_on_index_reverses_from_the_index() {
        let m = index(vec![tracked_file("a.rs", 1, false)]);
        let id = hunk_id_at(&m, 0, 0);
        match plan_action(&m, 0, 0, id, Action::Unstage, false) {
            ActionPlan::Apply {
                cached, reverse, ..
            } => assert!(cached && reverse),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn accept_on_untracked_adds_the_whole_file() {
        let m = worktree(vec![tracked_file("new.txt", 1, true)]);
        let id = hunk_id_at(&m, 0, 0);
        assert_eq!(
            plan_action(&m, 0, 0, id, Action::Accept, false),
            ActionPlan::AddFile("new.txt".into())
        );
    }

    #[test]
    fn reject_on_untracked_deletes_it() {
        let m = worktree(vec![tracked_file("new.txt", 1, true)]);
        let id = hunk_id_at(&m, 0, 0);
        assert_eq!(
            plan_action(&m, 0, 0, id, Action::Reject, false),
            ActionPlan::DeleteUntracked("new.txt".into())
        );
    }

    #[test]
    fn too_large_file_blocks_every_action() {
        // A TooLarge entry carries no hunks at all, exactly like production.
        let mut file = tracked_file("big.log", 0, true);
        file.status = FileStatus::TooLarge;
        let m = worktree(vec![file]);
        for action in [Action::Accept, Action::Reject, Action::Unstage] {
            match plan_action(&m, 0, 0, HunkId(0), action, false) {
                ActionPlan::Disabled(msg) => {
                    assert!(msg.contains("too large to review"), "{action:?}: {msg}")
                }
                other => panic!("{action:?}: expected Disabled, got {other:?}"),
            }
        }
    }

    #[test]
    fn unmerged_file_blocks_every_action() {
        // A conflicted file carries no hunks at all, exactly like production.
        let mut file = tracked_file("f.txt", 0, false);
        file.status = FileStatus::Unmerged;
        let m = worktree(vec![file]);
        for action in [Action::Accept, Action::Reject, Action::Unstage] {
            match plan_action(&m, 0, 0, HunkId(0), action, false) {
                ActionPlan::Disabled(msg) => assert!(
                    msg == "f.txt has a merge conflict — resolve it first",
                    "{action:?}: {msg}"
                ),
                other => panic!("{action:?}: expected Disabled, got {other:?}"),
            }
        }
    }

    #[test]
    fn busy_blocks_every_mutation() {
        let m = worktree(vec![tracked_file("a.rs", 1, false)]);
        let id = hunk_id_at(&m, 0, 0);
        for action in [Action::Accept, Action::Reject, Action::Unstage] {
            match plan_action(&m, 0, 0, id, action, true) {
                ActionPlan::Blocked(msg) => {
                    assert!(msg.contains("agents are working"), "{msg}")
                }
                other => panic!("{action:?}: expected Blocked, got {other:?}"),
            }
        }
    }

    #[test]
    fn stale_or_missing_hunk_refreshes() {
        let m = worktree(vec![tracked_file("a.rs", 2, false)]);
        // Wrong id: the hunk changed since it was rendered.
        assert_eq!(
            plan_action(&m, 0, 0, HunkId(999), Action::Accept, false),
            ActionPlan::Refreshed
        );
        // Out-of-range hunk index: it vanished (e.g. already staged).
        let id = hunk_id_at(&m, 0, 0);
        assert_eq!(
            plan_action(&m, 0, 5, id, Action::Accept, false),
            ActionPlan::Refreshed
        );
        // Out-of-range file index.
        assert!(matches!(
            plan_action(&m, 3, 0, id, Action::Accept, false),
            ActionPlan::Disabled(_)
        ));
    }

    #[test]
    fn special_statuses_are_disabled_with_notes() {
        let mut binary = tracked_file("b.bin", 0, false);
        binary.status = FileStatus::Binary;
        let m = worktree(vec![binary]);
        match plan_action(&m, 0, 0, HunkId(0), Action::Accept, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("binary"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }

        let mut mode_only = tracked_file("m.sh", 0, false);
        mode_only.status = FileStatus::ModeOnly;
        let m = worktree(vec![mode_only]);
        match plan_action(&m, 0, 0, HunkId(0), Action::Reject, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("mode"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }

        let mut renamed = tracked_file("r.rs", 0, false);
        renamed.status = FileStatus::Renamed;
        let m = worktree(vec![renamed]);
        match plan_action(&m, 0, 0, HunkId(0), Action::Accept, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("rename"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }

        let mut deleted = tracked_file("d.rs", 0, false);
        deleted.status = FileStatus::Deleted;
        let m = worktree(vec![deleted]);
        match plan_action(&m, 0, 0, HunkId(0), Action::Unstage, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("deleted"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    #[test]
    fn wrong_side_actions_are_disabled_with_notes() {
        let wt = worktree(vec![tracked_file("a.rs", 1, false)]);
        let id = hunk_id_at(&wt, 0, 0);
        match plan_action(&wt, 0, 0, id, Action::Accept, false) {
            ActionPlan::Apply { .. } => {}
            other => panic!("sanity: accept works on worktree, got {other:?}"),
        }
        // Unstage makes no sense on the unstaged side.
        match plan_action(&wt, 0, 0, id, Action::Unstage, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("/diff --staged"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }

        let idx = index(vec![tracked_file("a.rs", 1, false)]);
        let id = hunk_id_at(&idx, 0, 0);
        match plan_action(&idx, 0, 0, id, Action::Accept, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("staged"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }
        match plan_action(&idx, 0, 0, id, Action::Reject, false) {
            ActionPlan::Disabled(note) => assert!(note.contains("unstage"), "{note}"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    // ---- execute() integration tests (temp repos, §5.4) ----

    use crate::git_diff::tests::{commit_all, temp_repo, write_file};

    /// Dirty worktree repo with one tracked file of two separate hunks.
    /// Returns `(dir_guard, git, model, hunk-1 patch)`.
    async fn two_hunk_repo() -> Option<(tempfile::TempDir, GitDiff, DiffModel, Vec<u8>)> {
        let (dir, git) = temp_repo().await?;
        let body = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\n";
        write_file(dir.path(), "root.txt", body).await;
        commit_all(&git, "init").await;
        let edited = body.replace("l2\n", "H2\n").replace("l11\n", "H11\n");
        write_file(dir.path(), "root.txt", &edited).await;

        let model = git.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].hunks.len(), 2);
        let patch = build_single_hunk_patch(&model.files[0], 1).expect("patch builds");
        Some((dir, git, model, patch))
    }

    #[tokio::test]
    async fn execute_accept_stages_exactly_one_hunk() {
        let Some((_dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        let backup = tempfile::tempdir().expect("backup dir");
        let plan = ActionPlan::Apply {
            patch,
            cached: true,
            reverse: false,
        };
        let notice = execute(&git, plan, backup.path()).await.expect("accept");
        assert!(notice.contains("staged 1 hunk in root.txt"), "{notice}");

        let numstat = git
            .git(&["diff", "--cached", "--numstat"])
            .await
            .expect("numstat");
        assert_eq!(numstat.trim(), "1\t1\troot.txt", "{numstat}");
        // The other hunk is still unstaged.
        let after = git.read(DiffSide::WorkingTree).await.expect("re-read");
        assert_eq!(after.files[0].hunks.len(), 1);
    }

    #[tokio::test]
    async fn execute_reject_backs_up_and_undo_restores() {
        let Some((dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        let backup = tempfile::tempdir().expect("backup dir");
        let plan = ActionPlan::Apply {
            patch,
            cached: false,
            reverse: true,
        };
        let notice = execute(&git, plan, backup.path()).await.expect("reject");
        assert!(notice.contains("discarded 1 hunk in root.txt"), "{notice}");
        assert!(
            notice.contains(&format!(
                "undo: git -C \"{}\" apply",
                git.repo_root.display()
            )),
            "{notice}"
        );

        // Exactly one backup patch exists…
        let mut entries: Vec<_> = std::fs::read_dir(backup.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.sort();
        let undo = &entries[0];
        assert!(undo.to_string_lossy().ends_with(".patch"), "{undo:?}");

        // …and re-applying it brings the discarded change back.
        let content = tokio::fs::read_to_string(dir.path().join("root.txt"))
            .await
            .expect("content");
        assert!(!content.contains("H11\n"), "{content}");
        let undo_text = tokio::fs::read(undo).await.expect("patch bytes");
        git.apply(&undo_text, false, false)
            .await
            .expect("undo apply");
        let restored = tokio::fs::read_to_string(dir.path().join("root.txt"))
            .await
            .expect("restored");
        assert!(restored.contains("H11\n"), "{restored}");
    }

    #[tokio::test]
    async fn rejected_hunk_notice_has_repo_rooted_undo() {
        let Some((_dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        let backup = tempfile::tempdir().expect("backup dir");
        let plan = ActionPlan::Apply {
            patch,
            cached: false,
            reverse: true,
        };
        let notice = execute(&git, plan, backup.path()).await.expect("reject");

        // The undo command is rooted at the repo so it works from any cwd…
        assert!(notice.contains(" · undo: git -C \""), "{notice}");
        // …and names the backup path in quotes.
        let mut entries: Vec<_> = std::fs::read_dir(backup.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            notice.contains(&format!("\"{}\"", entries[0].display())),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn undo_command_restores_from_a_subfolder() {
        let Some((dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        let backup = tempfile::tempdir().expect("backup dir");
        let plan = ActionPlan::Apply {
            patch,
            cached: false,
            reverse: true,
        };
        execute(&git, plan, backup.path()).await.expect("reject");

        // The hunk is gone…
        let content = tokio::fs::read_to_string(dir.path().join("root.txt"))
            .await
            .expect("content");
        assert!(!content.contains("H11\n"), "{content}");

        // …and the printed undo command still works from a subfolder of
        // the repo because it carries `git -C <repo_root>`.
        let mut entries: Vec<_> = std::fs::read_dir(backup.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 1, "{entries:?}");
        let sub = dir.path().join("nested/deeper");
        tokio::fs::create_dir_all(&sub).await.expect("subfolder");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&git.repo_root)
            .arg("apply")
            .arg(&entries[0])
            .current_dir(&sub)
            .output()
            .expect("invariant: git spawns");
        assert!(
            out.status.success(),
            "undo from subfolder failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let restored = tokio::fs::read_to_string(dir.path().join("root.txt"))
            .await
            .expect("restored");
        assert!(restored.contains("H11\n"), "{restored}");
    }

    #[tokio::test]
    async fn execute_unstage_removes_one_hunk_from_index() {
        let Some((_dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        // Stage everything first, then unstage exactly hunk 2.
        git.git(&["add", "-A"]).await.expect("add");
        let backup = tempfile::tempdir().expect("backup dir");
        let plan = ActionPlan::Apply {
            patch,
            cached: true,
            reverse: true,
        };
        let notice = execute(&git, plan, backup.path()).await.expect("unstage");
        assert!(notice.contains("unstaged 1 hunk in root.txt"), "{notice}");

        let numstat = git
            .git(&["diff", "--cached", "--numstat"])
            .await
            .expect("numstat");
        assert_eq!(numstat.trim(), "1\t1\troot.txt", "{numstat}");
    }

    #[tokio::test]
    async fn execute_add_file_stages_new_file() {
        let Some((dir, git)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(dir.path(), "fresh.txt", "hello\n").await;
        let backup = tempfile::tempdir().expect("backup dir");
        let notice = execute(&git, ActionPlan::AddFile("fresh.txt".into()), backup.path())
            .await
            .expect("add");
        assert!(notice.contains("staged new file fresh.txt"), "{notice}");
        let staged = git.read(DiffSide::Index).await.expect("index read");
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].path, "fresh.txt");
    }

    #[tokio::test]
    async fn execute_delete_untracked_leaves_matching_backup() {
        let Some((dir, git)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(dir.path(), "keep.txt", "x\n").await;
        commit_all(&git, "init").await;
        write_file(dir.path(), "doomed.txt", "bye\nbye\n").await;
        let backup = tempfile::tempdir().expect("backup dir");

        let notice = execute(
            &git,
            ActionPlan::DeleteUntracked("doomed.txt".into()),
            backup.path(),
        )
        .await
        .expect("delete");
        assert!(notice.contains("deleted doomed.txt"), "{notice}");

        // File gone from the worktree…
        assert!(tokio::fs::try_exists(dir.path().join("doomed.txt"))
            .await
            .is_ok_and(|e| !e));
        // …and the backup holds the exact original content.
        let mut entries: Vec<_> = std::fs::read_dir(backup.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.sort();
        // The notice names the backup path in quotes so a copy-paste works
        // even when the temp dir contains spaces.
        assert!(
            notice.contains(&format!("backup: \"{}\"", entries[0].display())),
            "{notice}"
        );
        let copy = tokio::fs::read_to_string(&entries[0])
            .await
            .expect("backup content");
        assert_eq!(copy, "bye\nbye\n");
        assert!(
            entries[0].to_string_lossy().ends_with(".bak"),
            "{entries:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reject_backup_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let Some((_dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        // A backup dir that already exists world-readable, like a shared
        // temp folder: prepare_backup_dir must tighten it to 0o700.
        let backup_root = tempfile::tempdir().expect("backup root");
        let backup_dir = backup_root.path().join("backups");
        tokio::fs::create_dir_all(&backup_dir)
            .await
            .expect("invariant: mkdir");
        tokio::fs::set_permissions(&backup_dir, std::fs::Permissions::from_mode(0o755))
            .await
            .expect("invariant: chmod");

        let plan = ActionPlan::Apply {
            patch,
            cached: false,
            reverse: true,
        };
        execute(&git, plan, &backup_dir).await.expect("reject");

        let dir_mode = tokio::fs::metadata(&backup_dir)
            .await
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "dir mode {dir_mode:o}");

        let mut entries: Vec<_> = std::fs::read_dir(&backup_dir)
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            entries[0].to_string_lossy().ends_with(".patch"),
            "{entries:?}"
        );
        let file_mode = tokio::fs::metadata(&entries[0])
            .await
            .expect("file metadata")
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600, "file mode {file_mode:o}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_backup_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let Some((dir, git)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(dir.path(), "keep.txt", "x\n").await;
        commit_all(&git, "init").await;
        write_file(dir.path(), "doomed.txt", "bye\n").await;
        // The source is world-readable; the backup must not inherit that.
        tokio::fs::set_permissions(
            dir.path().join("doomed.txt"),
            std::fs::Permissions::from_mode(0o644),
        )
        .await
        .expect("invariant: chmod");

        let backup = tempfile::tempdir().expect("backup dir");
        execute(
            &git,
            ActionPlan::DeleteUntracked("doomed.txt".into()),
            backup.path(),
        )
        .await
        .expect("delete");

        let mut entries: Vec<_> = std::fs::read_dir(backup.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            entries[0].to_string_lossy().ends_with(".bak"),
            "{entries:?}"
        );
        let mode = tokio::fs::metadata(&entries[0])
            .await
            .expect("backup metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "backup mode {mode:o}");
    }

    #[tokio::test]
    async fn prune_removes_only_old_backups() {
        let dir = tempfile::tempdir().expect("backup dir");
        let old_when = std::time::SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
        for name in ["old.patch", "old.bak", "new.patch", "old.txt"] {
            tokio::fs::write(dir.path().join(name), "x\n")
                .await
                .expect("invariant: write");
        }
        for name in ["old.patch", "old.bak", "old.txt"] {
            let f = std::fs::File::options()
                .write(true)
                .open(dir.path().join(name))
                .expect("invariant: open");
            f.set_modified(old_when).expect("invariant: set_modified");
        }

        prune_old_backups(dir.path(), BACKUP_MAX_AGE).await;

        let left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!left.contains(&"old.patch".to_string()), "{left:?}");
        assert!(!left.contains(&"old.bak".to_string()), "{left:?}");
        assert!(left.contains(&"new.patch".to_string()), "{left:?}");
        assert!(left.contains(&"old.txt".to_string()), "{left:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reject_untracked_symlink_keeps_its_target() {
        use std::os::unix::fs::symlink;

        let Some((dir, git)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(dir.path(), "keep.txt", "x\n").await;
        commit_all(&git, "init").await;
        // The link points outside the repo; rejecting it must delete the
        // link itself while leaving the target's content untouched.
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("data.txt"), "precious\n").expect("invariant: write");
        let target = outside.path().join("data.txt");
        symlink(&target, dir.path().join("doomed.link")).expect("invariant: symlink");

        let backup = tempfile::tempdir().expect("backup dir");
        execute(
            &git,
            ActionPlan::DeleteUntracked("doomed.link".into()),
            backup.path(),
        )
        .await
        .expect("delete link");

        // The link is gone…
        assert!(tokio::fs::try_exists(dir.path().join("doomed.link"))
            .await
            .is_ok_and(|e| !e));
        // …the target still holds its content…
        let content = tokio::fs::read_to_string(&target)
            .await
            .expect("target content");
        assert_eq!(content, "precious\n");
        // …and the backup holds the link target path, not the target bytes.
        let mut entries: Vec<_> = std::fs::read_dir(backup.path())
            .expect("backup dir readable")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.sort();
        let backed_up = tokio::fs::read_to_string(&entries[0])
            .await
            .expect("backup content");
        assert_eq!(backed_up, target.to_string_lossy());
        assert!(
            entries[0].to_string_lossy().ends_with(".bak"),
            "{entries:?}"
        );
    }

    #[tokio::test]
    async fn execute_stale_patch_fails_without_touching_anything() {
        let Some((dir, git, _model, patch)) = two_hunk_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        let backup = tempfile::tempdir().expect("backup dir");
        // The user stages a different edit after the plan was built: the
        // patch's context (l11) no longer matches the index (EDITED), so
        // `git apply --cached --check` must refuse.
        let body = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nEDITED\nl12\n";
        write_file(dir.path(), "root.txt", body).await;
        git.add_file("root.txt").await.expect("stage edit");

        let plan = ActionPlan::Apply {
            patch,
            cached: true,
            reverse: false,
        };
        let err = execute(&git, plan, backup.path())
            .await
            .expect_err("stale patch must fail");
        assert!(!err.is_empty(), "{err}");

        // Nothing changed: the index still holds exactly the user's EDITED
        // change, never the planned H11 hunk.
        let staged = git.read(DiffSide::Index).await.expect("index read");
        assert_eq!(staged.files.len(), 1);
        let texts: Vec<&str> = staged.files[0]
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.text.as_str()))
            .collect();
        assert!(texts.contains(&"EDITED"), "{texts:?}");
        assert!(!texts.contains(&"H11"), "{texts:?}");
        // And the accept path never wrote a backup.
        assert!(
            std::fs::read_dir(backup.path())
                .expect("backup dir readable")
                .count()
                == 0
        );
    }
}
