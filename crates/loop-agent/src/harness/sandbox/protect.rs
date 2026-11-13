//! Paths the Linux (Podman) sandbox keeps write-protected: the project's
//! `.loop`, an agent directory inside the project, and git config, hooks and
//! pointers. This is the Linux form of the Seatbelt rules in
//! [`super::seatbelt`].
//!
//! Two layers:
//! - Shell commands run in the container. [`plan`] gives the bind mounts
//!   that make these paths read-only there. A protected path that does not
//!   exist yet gets an empty stub on the host first, so a command cannot
//!   create it either; [`ProtectedMounts::remove_stubs`] removes the stubs
//!   that are still empty when the sandbox stops.
//! - File tools run on the host (partial isolation) or through the container
//!   (full isolation). [`write_denied`] refuses them on the host first, with
//!   the same rules as Seatbelt, so they also cover repositories that the
//!   mounts do not reach.

use std::path::{Path, PathBuf};

use super::seatbelt::{git_entry_write, git_metadata, resolve_path};
use super::SandboxError;
use crate::harness::types::{FileError, FileErrorCode};

/// Git metadata in a git directory that git reads as config, hooks or
/// pointers ([`super::seatbelt`]'s `GIT_METADATA`).
const GIT_METADATA_NAMES: [&str; 6] = [
    "config",
    "config.worktree",
    "commondir",
    "gitdir",
    "hooks",
    "info/attributes",
];

/// Upper bound for the git directories below `.git/modules` and
/// `.git/worktrees` that get their own mounts.
const MAX_GIT_DIRS: usize = 256;
/// Upper bound for the directory depth searched below `.git/modules`.
const MAX_MODULE_DEPTH: usize = 8;

/// One bind mount of a host path onto the same path in the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedBind {
    /// Host path; also the guest path.
    pub path: PathBuf,
    /// Read-only. A read-write bind only makes the path a mount point, so a
    /// command cannot rename or remove it (`EBUSY`).
    pub read_only: bool,
}

/// The mounts for one sandbox start, and the stubs made for them.
#[derive(Debug, Default)]
pub struct ProtectedMounts {
    /// Binds, parents before children.
    pub binds: Vec<ProtectedBind>,
    /// Empty directories created on the host as mount points, parents
    /// before children.
    pub stubs: Vec<PathBuf>,
    /// Shared lock on the project ([`project_lock`]) while this sandbox runs.
    lock: Option<std::fs::File>,
}

impl ProtectedMounts {
    /// Remove the stubs that are still empty. Never removes content.
    ///
    /// Only when no other sandbox of this project runs: a host `rmdir` of a
    /// directory that is a mount point in another container detaches that
    /// mount there, and the other sandbox would lose its protection. A
    /// stub then stays as an empty directory.
    pub fn remove_stubs(&self) {
        if self.stubs.is_empty() || !self.alone() {
            return;
        }
        for stub in self.stubs.iter().rev() {
            // `remove_dir` fails on a directory that is not empty.
            let _ = std::fs::remove_dir(stub);
        }
    }
}

impl ProtectedMounts {
    #[cfg(unix)]
    fn alone(&self) -> bool {
        // Upgrade the shared lock; it fails while another sandbox holds one.
        self.lock.as_ref().is_some_and(|lock| {
            rustix::fs::flock(lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
        })
    }

    #[cfg(not(unix))]
    fn alone(&self) -> bool {
        true
    }
}

/// A lock file for `project`, shared by the sandboxes of that project of
/// this user. The name holds a stable hash (FNV-1a) of the project path.
#[cfg(unix)]
fn project_lock(project: &Path) -> Result<std::fs::File, SandboxError> {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in project.as_os_str().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    let path = std::env::temp_dir().join(format!("loop-sandbox-{uid}-{hash:016x}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| io_refuse(&path, e))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)
        .map_err(|e| io_refuse(&path, e.into()))?;
    Ok(file)
}

fn refuse(path: &Path, why: &str) -> SandboxError {
    SandboxError::StartFailed(format!(
        "cannot start the sandbox: {} {why}, so the sandbox cannot keep it write-protected. \
         Replace it with a real file or directory, or turn the sandbox off",
        path.display()
    ))
}

fn io_refuse(path: &Path, e: std::io::Error) -> SandboxError {
    SandboxError::StartFailed(format!(
        "cannot start the sandbox: cannot prepare {}: {e}",
        path.display()
    ))
}

/// What is at `path`, without following a final symlink.
enum Entry {
    Missing,
    Symlink,
    Dir,
    Other,
}

fn entry(path: &Path) -> Result<Entry, SandboxError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Ok(Entry::Symlink),
        Ok(m) if m.is_dir() => Ok(Entry::Dir),
        Ok(_) => Ok(Entry::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Entry::Missing),
        Err(e) => Err(io_refuse(path, e)),
    }
}

fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file()
}

struct Planner {
    out: ProtectedMounts,
    git_dirs: usize,
}

impl Planner {
    fn bind(&mut self, path: PathBuf, read_only: bool) {
        self.out.binds.push(ProtectedBind { path, read_only });
    }

    fn stub(&mut self, path: &Path) -> Result<(), SandboxError> {
        std::fs::create_dir(path).map_err(|e| io_refuse(path, e))?;
        self.out.stubs.push(path.to_path_buf());
        self.bind(path.to_path_buf(), true);
        Ok(())
    }

    /// Protect one git directory: make it a mount point, make its metadata
    /// read-only, then do the same for the git directories of its
    /// submodules and linked worktrees.
    fn git_dir(&mut self, dir: &Path, stub_hooks: bool) -> Result<(), SandboxError> {
        self.git_dirs += 1;
        if self.git_dirs > MAX_GIT_DIRS {
            return Err(SandboxError::StartFailed(format!(
                "cannot start the sandbox: the repository has more than {MAX_GIT_DIRS} \
                 submodule and worktree git directories to protect"
            )));
        }
        self.bind(dir.to_path_buf(), false);
        if let Entry::Symlink = entry(&dir.join("info"))? {
            return Err(refuse(&dir.join("info"), "is a symlink"));
        }
        for name in GIT_METADATA_NAMES {
            let path = dir.join(name);
            match entry(&path)? {
                Entry::Symlink => return Err(refuse(&path, "is a symlink")),
                Entry::Dir | Entry::Other => self.bind(path, true),
                // An empty hooks directory is what git itself creates. Other
                // names cannot get a stub: an empty `commondir` or `gitdir`
                // file breaks git on the host.
                Entry::Missing if name == "hooks" && stub_hooks => self.stub(&path)?,
                Entry::Missing => {}
            }
        }
        self.modules(&dir.join("modules"), 0)?;
        let worktrees = dir.join("worktrees");
        if let Entry::Dir = entry(&worktrees)? {
            for child in sorted_dirs(&worktrees)? {
                if is_git_dir(&child) {
                    self.git_dir(&child, false)?;
                }
            }
        }
        Ok(())
    }

    /// Submodule git directories, named by their (possibly nested) path
    /// below `modules`.
    fn modules(&mut self, dir: &Path, depth: usize) -> Result<(), SandboxError> {
        if depth > MAX_MODULE_DEPTH {
            return Ok(());
        }
        match entry(dir)? {
            Entry::Dir => {}
            Entry::Symlink => return Err(refuse(dir, "is a symlink")),
            _ => return Ok(()),
        }
        for child in sorted_dirs(dir)? {
            if is_git_dir(&child) {
                self.git_dir(&child, true)?;
            } else {
                self.modules(&child, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// Real directories (not symlinks) in `dir`, sorted.
fn sorted_dirs(dir: &Path) -> Result<Vec<PathBuf>, SandboxError> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir).map_err(|e| io_refuse(dir, e))? {
        let item = item.map_err(|e| io_refuse(dir, e))?;
        if item.file_type().is_ok_and(|t| t.is_dir()) {
            out.push(item.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Bind mounts that keep the protected paths of `project` read-only in the
/// container. Creates empty stubs for a missing `.git` (so `git init`
/// cannot run), a missing `.git/hooks` and a missing `.loop`.
///
/// A symlinked `.git`, `.loop` or git metadata entry is refused: a mount
/// follows it, and the link itself would stay replaceable.
pub fn plan(project: &Path, agent_dir: Option<&Path>) -> Result<ProtectedMounts, SandboxError> {
    let mut p = Planner {
        out: ProtectedMounts::default(),
        git_dirs: 0,
    };
    // Before any stub is made: a sandbox that stops now keeps its stubs.
    #[cfg(unix)]
    {
        p.out.lock = Some(project_lock(project)?);
    }
    let result = plan_into(&mut p, project, agent_dir);
    if result.is_err() {
        p.out.remove_stubs();
    }
    result.map(|()| p.out)
}

fn plan_into(p: &mut Planner, project: &Path, agent: Option<&Path>) -> Result<(), SandboxError> {
    let dot_git = project.join(".git");
    match entry(&dot_git)? {
        Entry::Symlink => return Err(refuse(&dot_git, "is a symlink")),
        Entry::Missing => p.stub(&dot_git)?,
        // A pointer of a linked worktree or submodule checkout.
        Entry::Other => p.bind(dot_git, true),
        Entry::Dir if is_git_dir(&dot_git) => p.git_dir(&dot_git, true)?,
        // Not a repository (an empty stub left by another session):
        // keep it read-only so no repository can start in it.
        Entry::Dir => p.bind(dot_git, true),
    }
    let dot_loop = project.join(".loop");
    match entry(&dot_loop)? {
        Entry::Symlink => return Err(refuse(&dot_loop, "is a symlink")),
        Entry::Missing => p.stub(&dot_loop)?,
        Entry::Dir | Entry::Other => p.bind(dot_loop.clone(), true),
    }
    if let Some(agent) = agent {
        let agent = resolve_path(agent).unwrap_or_else(|_| agent.to_path_buf());
        if agent.starts_with(project)
            && agent != project
            && !agent.starts_with(&dot_loop)
            && agent.exists()
        {
            p.bind(agent, true);
        }
    }
    p.out.binds.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// Whether a file tool may not write, create or remove `path` in a sandbox
/// for `project`: git metadata in any repository, a `.git` entry, project
/// `.loop`, or `agent_dir`. Both the path as given and its resolved form
/// are checked, so a symlink cannot lead into a protected path.
pub fn write_denied(
    project: &Path,
    agent_dir: Option<&Path>,
    path: &Path,
) -> Result<(), FileError> {
    let absolute = project.join(path);
    let resolved = resolve_path(&absolute)?;
    let mut roots = vec![project.join(".loop")];
    if let Ok(r) = resolve_path(&project.join(".loop")) {
        roots.push(r);
    }
    if let Some(agent) = agent_dir {
        roots.push(agent.to_path_buf());
        if let Ok(r) = resolve_path(agent) {
            roots.push(r);
        }
    }
    let protected = [&absolute, &resolved]
        .iter()
        .any(|p| git_metadata(p) || git_entry_write(p) || roots.iter().any(|r| p.starts_with(r)));
    if protected {
        return Err(FileError::new(
            FileErrorCode::PermissionDenied,
            format!(
                "sandbox denies writing {}: git config, hooks and pointers, project .loop \
                 and the agent directory are write-protected",
                absolute.display()
            ),
        ));
    }
    Ok(())
}

/// Refuse `path` when it lies in one of `read_only` (the container's
/// read-only mounts), as given or resolved.
pub fn under_read_only(
    project: &Path,
    read_only: &[PathBuf],
    path: &Path,
) -> Result<(), FileError> {
    let absolute = project.join(path);
    let resolved = resolve_path(&absolute)?;
    if let Some(root) = read_only
        .iter()
        .find(|r| absolute.starts_with(r) || resolved.starts_with(r))
    {
        return Err(FileError::new(
            FileErrorCode::PermissionDenied,
            format!(
                "sandbox denies writing {}: {} is read-only in the sandbox",
                absolute.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

/// [`write_denied`] for a removal: also refuses removing a directory that
/// holds the project's `.git` or `.loop`.
pub fn remove_denied(
    project: &Path,
    agent_dir: Option<&Path>,
    path: &Path,
) -> Result<(), FileError> {
    write_denied(project, agent_dir, path)?;
    let absolute = project.join(path);
    let resolved = resolve_path(&absolute)?;
    for held in [project.join(".git"), project.join(".loop")] {
        if held.starts_with(&absolute) || held.starts_with(&resolved) {
            return Err(FileError::new(
                FileErrorCode::PermissionDenied,
                format!(
                    "sandbox denies removing {}: it holds the write-protected {}",
                    absolute.display(),
                    held.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(root: &Path) -> PathBuf {
        let project = std::fs::canonicalize(root).unwrap();
        let git = project.join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("config"), "[core]\n").unwrap();
        std::fs::write(git.join("hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(project.join(".loop")).unwrap();
        std::fs::write(project.join(".loop/settings.json"), "{}").unwrap();
        project
    }

    fn binds(m: &ProtectedMounts, project: &Path) -> Vec<(String, bool)> {
        m.binds
            .iter()
            .map(|b| {
                (
                    b.path.strip_prefix(project).unwrap().display().to_string(),
                    b.read_only,
                )
            })
            .collect()
    }

    #[test]
    fn an_existing_repository_gets_read_only_metadata_below_a_mount_point() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let m = plan(&project, None).unwrap();
        assert_eq!(
            binds(&m, &project),
            vec![
                (".git".into(), false),
                (".git/config".into(), true),
                (".git/hooks".into(), true),
                (".loop".into(), true),
            ]
        );
        assert!(m.stubs.is_empty());
    }

    #[test]
    fn missing_git_and_loop_get_empty_read_only_stubs_that_are_removed_later() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let m = plan(&project, None).unwrap();
        assert_eq!(
            binds(&m, &project),
            vec![(".git".into(), true), (".loop".into(), true)]
        );
        assert!(project.join(".git").is_dir() && project.join(".loop").is_dir());
        m.remove_stubs();
        assert!(!project.join(".git").exists() && !project.join(".loop").exists());
    }

    #[test]
    fn a_stub_with_content_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let m = plan(&project, None).unwrap();
        std::fs::write(project.join(".loop/settings.json"), "{}").unwrap();
        m.remove_stubs();
        assert!(project.join(".loop/settings.json").is_file());
        assert!(!project.join(".git").exists());
    }

    #[cfg(unix)]
    #[test]
    fn stubs_stay_while_another_sandbox_of_the_project_runs() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let first = plan(&project, None).unwrap();
        // The second sandbox finds the first one's empty stubs.
        let second = plan(&project, None).unwrap();
        assert!(second.stubs.is_empty());
        first.remove_stubs();
        assert!(
            project.join(".git").is_dir(),
            "removed under a running sandbox"
        );
        drop(first);
        second.remove_stubs();
        drop(second);
        // A later sandbox alone removes its own stubs.
        std::fs::remove_dir(project.join(".git")).unwrap();
        std::fs::remove_dir(project.join(".loop")).unwrap();
        let third = plan(&project, None).unwrap();
        third.remove_stubs();
        assert!(!project.join(".git").exists());
    }

    #[test]
    fn a_missing_hooks_directory_gets_a_stub() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        std::fs::remove_dir_all(project.join(".git/hooks")).unwrap();
        let m = plan(&project, None).unwrap();
        assert!(binds(&m, &project).contains(&(".git/hooks".into(), true)));
        assert_eq!(m.stubs, vec![project.join(".git/hooks")]);
        m.remove_stubs();
        assert!(!project.join(".git/hooks").exists());
        assert!(project.join(".git/config").exists());
    }

    #[test]
    fn an_empty_git_directory_is_not_treated_as_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(project.join(".git")).unwrap();
        let m = plan(&project, None).unwrap();
        assert!(binds(&m, &project).contains(&(".git".into(), true)));
        assert!(!project.join(".git/hooks").exists());
    }

    #[test]
    fn a_git_pointer_file_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(project.join(".git"), "gitdir: /elsewhere\n").unwrap();
        let m = plan(&project, None).unwrap();
        assert!(binds(&m, &project).contains(&(".git".into(), true)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_protected_entries_refuse_the_start_and_leave_no_stub() {
        for link in [".git", ".loop", ".git/hooks", ".git/config", ".git/info"] {
            let dir = tempfile::tempdir().unwrap();
            let project = repo(dir.path());
            if link == ".loop" {
                std::fs::remove_dir_all(project.join(".git")).unwrap();
            }
            let target = project.join("elsewhere");
            std::fs::create_dir_all(&target).unwrap();
            let _ = std::fs::remove_dir_all(project.join(link));
            let _ = std::fs::remove_file(project.join(link));
            std::os::unix::fs::symlink(&target, project.join(link)).unwrap();
            let err = plan(&project, None).unwrap_err().to_string();
            assert!(err.contains("is a symlink"), "{link}: {err}");
            if link == ".loop" {
                assert!(!project.join(".git").exists(), "stub left behind");
            }
        }
    }

    #[test]
    fn submodule_and_worktree_git_directories_are_protected() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let git = project.join(".git");
        let sub = git.join("modules/libs/a");
        std::fs::create_dir_all(sub.join("hooks")).unwrap();
        std::fs::write(sub.join("HEAD"), "x").unwrap();
        std::fs::write(sub.join("config"), "x").unwrap();
        let nested = sub.join("modules/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("HEAD"), "x").unwrap();
        let wt = git.join("worktrees/w1");
        std::fs::create_dir_all(&wt).unwrap();
        for f in ["HEAD", "commondir", "gitdir"] {
            std::fs::write(wt.join(f), "x").unwrap();
        }
        let m = plan(&project, None).unwrap();
        let b = binds(&m, &project);
        for want in [
            (".git/modules/libs/a", false),
            (".git/modules/libs/a/config", true),
            (".git/modules/libs/a/hooks", true),
            (".git/modules/libs/a/modules/b", false),
            (".git/modules/libs/a/modules/b/hooks", true),
            (".git/worktrees/w1", false),
            (".git/worktrees/w1/commondir", true),
            (".git/worktrees/w1/gitdir", true),
        ] {
            assert!(b.contains(&(want.0.into(), want.1)), "{want:?} in {b:?}");
        }
        // Worktree admin directories get no hooks stub.
        assert!(!wt.join("hooks").exists());
        // Parents come before children.
        let mut sorted = m.binds.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(sorted, m.binds);
    }

    #[test]
    fn an_agent_directory_inside_the_project_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let agent = project.join("home/.loop/agent");
        std::fs::create_dir_all(&agent).unwrap();
        let m = plan(&project, Some(&agent)).unwrap();
        assert!(binds(&m, &project).contains(&("home/.loop/agent".into(), true)));
        let outside = tempfile::tempdir().unwrap();
        let m = plan(&project, Some(outside.path())).unwrap();
        assert!(!m.binds.iter().any(|b| b.path.starts_with(outside.path())));
    }

    #[test]
    fn file_tools_may_not_write_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let agent = project.join("agent");
        for path in [
            ".git/hooks/pre-commit",
            ".git/hooks/post-checkout",
            ".git/config",
            ".git/commondir",
            ".git/info/attributes",
            ".git",
            ".loop/settings.json",
            ".loop",
            "agent/auth.json",
            "nested/.git",
            "nested/.git/hooks/pre-commit",
            "sub/.git/config",
        ] {
            let err = write_denied(&project, Some(&agent), Path::new(path)).expect_err(path);
            assert_eq!(err.code, FileErrorCode::PermissionDenied, "{path}");
            let abs = project.join(path);
            assert!(
                write_denied(&project, Some(&agent), &abs).is_err(),
                "{path}"
            );
        }
        for path in [
            "src/main.rs",
            ".git/objects/ab",
            ".git/index",
            "loop.txt",
            ".gitignore",
        ] {
            write_denied(&project, Some(&agent), Path::new(path)).expect(path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_a_protected_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        std::os::unix::fs::symlink(project.join(".git/hooks"), project.join("h")).unwrap();
        std::os::unix::fs::symlink(project.join(".loop"), project.join("cfg")).unwrap();
        assert!(write_denied(&project, None, Path::new("h/pre-commit")).is_err());
        assert!(write_denied(&project, None, Path::new("cfg/settings.json")).is_err());
    }

    #[test]
    fn removing_a_directory_that_holds_git_or_loop_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        assert!(remove_denied(&project, None, Path::new(".")).is_err());
        assert!(remove_denied(&project, None, &project).is_err());
        assert!(remove_denied(&project, None, Path::new(".loop")).is_err());
        std::fs::create_dir(project.join("src")).unwrap();
        remove_denied(&project, None, Path::new("src")).unwrap();
    }

    #[test]
    fn writes_below_a_read_only_mount_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let m = plan(&project, None).unwrap();
        let ro: Vec<PathBuf> = m
            .binds
            .iter()
            .filter(|b| b.read_only)
            .map(|b| b.path.clone())
            .collect();
        // The stub `.git` is a real directory, so only the mount rule
        // refuses `.git/HEAD` in it.
        write_denied(&project, None, Path::new(".git/HEAD")).unwrap();
        assert!(under_read_only(&project, &ro, Path::new(".git/HEAD")).is_err());
        under_read_only(&project, &ro, Path::new("src/a.rs")).unwrap();
        m.remove_stubs();
    }
}
