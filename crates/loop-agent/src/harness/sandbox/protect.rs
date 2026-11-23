//! Paths the Linux (Podman) sandbox keeps write-protected: the project's
//! `.loop`, git config, hooks and pointers, and the hooks directory that
//! `core.hooksPath` names. This is the Linux form of the Seatbelt rules in
//! [`super::seatbelt`].
//!
//! Two layers:
//! - Shell commands run in the container. [`plan`] gives the bind mounts
//!   that make these paths read-only there. A protected path that does not
//!   exist yet gets a placeholder on the host first (an empty directory or
//!   file, or a `commondir` that names the git directory itself), so a
//!   command cannot create it either. Every directory between the project
//!   and a protected path is a mount point of its own, so a command cannot
//!   rename a parent away and build a new one. [`ProtectedMounts::release`]
//!   removes the placeholders when the last sandbox of the project stops.
//! - File tools run on the host (partial isolation) or through the container
//!   (full isolation). [`write_denied`] refuses them on the host first, with
//!   the same rules as Seatbelt, so they also cover repositories that the
//!   mounts do not reach.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use super::seatbelt::{git_entry_write, git_metadata, resolve_path};
use super::SandboxError;
use crate::harness::types::{FileError, FileErrorCode};

/// Upper bound for the git directories below `.git/modules` and
/// `.git/worktrees` that get their own mounts.
const MAX_GIT_DIRS: usize = 256;
/// Upper bound for the directory depth below `.git/modules`.
const MAX_MODULE_DEPTH: usize = 8;
/// Upper bound for the directory entries one `.git` scan reads.
const MAX_SCAN_ENTRIES: usize = 50_000;

/// One bind mount of a host path onto the same path in the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedBind {
    /// Host path; also the guest path.
    pub path: PathBuf,
    /// Read-only. A read-write bind only makes the path a mount point, so a
    /// command cannot rename or remove it (`EBUSY`).
    pub read_only: bool,
}

/// A placeholder made on the host as a mount point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stub {
    /// An empty directory.
    Dir,
    /// An empty file (`config`, `config.worktree`, `info/attributes`).
    Empty,
    /// A `commondir` that names the git directory itself (`.`). An empty
    /// `commondir` (or a directory) makes git fail.
    CommonDir,
}

impl Stub {
    fn code(self) -> char {
        match self {
            Self::Dir => 'D',
            Self::Empty => 'E',
            Self::CommonDir => 'C',
        }
    }

    fn from_code(c: char) -> Option<Self> {
        match c {
            'D' => Some(Self::Dir),
            'E' => Some(Self::Empty),
            'C' => Some(Self::CommonDir),
            _ => None,
        }
    }

    fn content(self) -> &'static str {
        match self {
            Self::Dir | Self::Empty => "",
            Self::CommonDir => ".\n",
        }
    }

    fn create(self, path: &Path) -> std::io::Result<()> {
        match self {
            Self::Dir => std::fs::create_dir(path),
            _ => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?
                    .write_all(self.content().as_bytes())
            }
        }
    }

    /// Remove `path` if it is still this placeholder, unchanged.
    fn remove_if_unchanged(self, path: &Path) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        match self {
            // `remove_dir` fails on a directory that is not empty.
            Self::Dir if meta.is_dir() => {
                let _ = std::fs::remove_dir(path);
            }
            Self::Empty | Self::CommonDir
                if meta.is_file()
                    && std::fs::read(path).is_ok_and(|b| b == self.content().as_bytes()) =>
            {
                let _ = std::fs::remove_file(path);
            }
            _ => {}
        }
    }
}

/// The mounts for one sandbox start, and the placeholders made for them.
#[derive(Debug, Default)]
pub struct ProtectedMounts {
    /// Binds, parents before children.
    pub binds: Vec<ProtectedBind>,
    /// Placeholders this start made, parents before children.
    stubs: Vec<(PathBuf, Stub)>,
    /// `.git` entries in the project at the start ([`new_git_entries`]).
    git_entries: BTreeSet<PathBuf>,
    /// Locks shared by the sandboxes of this project, while this one runs.
    #[cfg(unix)]
    locks: Option<locks::ProjectLocks>,
}

impl ProtectedMounts {
    /// `.git` entries in the project at the start.
    pub fn git_entries(&self) -> &BTreeSet<PathBuf> {
        &self.git_entries
    }

    #[cfg(test)]
    /// Paths this start created as placeholders.
    pub fn stub_paths(&self) -> Vec<PathBuf> {
        self.stubs.iter().map(|(p, _)| p.clone()).collect()
    }

    /// Release this sandbox's hold on the project. Call only after its
    /// container is gone. When no other sandbox of the project runs, every
    /// placeholder in the project's manifest that is still unchanged is
    /// removed: a host `rmdir` of a mount point of another container would
    /// detach that mount there.
    pub fn release(mut self) {
        #[cfg(unix)]
        if let Some(locks) = self.locks.take() {
            locks.release();
            return;
        }
        for (path, stub) in self.stubs.iter().rev() {
            stub.remove_if_unchanged(path);
        }
    }

    /// Keep this sandbox's hold, but forget it (the container may still
    /// run): the placeholders stay, and the next start that runs alone
    /// removes them.
    pub fn abandon(mut self) {
        // The shared run lock stays held until this process exits, so no
        // other sandbox removes placeholders the container may still mount.
        #[cfg(unix)]
        std::mem::forget(self.locks.take());
        self.stubs.clear();
    }

    #[cfg(test)]
    /// `.git` entries in the project that were not there at the start: a
    /// repository a command made, whose hooks and config host git would use
    /// in it, or below it when its parent repository records it as a
    /// gitlink. Also says when the scan stopped early.
    pub fn new_git_entries(&self, project: &Path) -> (Vec<PathBuf>, bool) {
        let (now, complete) = scan_git_entries(project);
        let added = now
            .into_iter()
            .filter(|p| !self.git_entries.contains(p))
            .collect();
        (added, complete)
    }
}

/// `.git` entries below `project` (not inside another `.git`), and whether
/// the scan read every directory within [`MAX_SCAN_ENTRIES`].
pub fn scan_git_entries(project: &Path) -> (BTreeSet<PathBuf>, bool) {
    let mut found = BTreeSet::new();
    let mut todo = vec![project.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = todo.pop() {
        let Ok(items) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in items.flatten() {
            seen += 1;
            if seen > MAX_SCAN_ENTRIES {
                return (found, false);
            }
            let path = item.path();
            if item.file_name() == ".git" {
                if path != project.join(".git") {
                    found.insert(path);
                }
                continue;
            }
            if item.file_type().is_ok_and(|t| t.is_dir()) {
                todo.push(path);
            }
        }
    }
    (found, true)
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

/// `path` with `.` and `..` resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// The kind of git directory [`Planner::git_dir`] protects.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GitDir {
    /// A full git directory: the project's, a submodule's or a pointer
    /// target's.
    Full,
    /// The admin directory of a linked worktree (`.git/worktrees/<id>`).
    Worktree,
}

struct Planner<'a> {
    project: &'a Path,
    /// Path to read-only; a path asked for both ways is read-only.
    binds: BTreeMap<PathBuf, bool>,
    stubs: Vec<(PathBuf, Stub)>,
    git_dirs: usize,
}

impl Planner<'_> {
    fn bind(&mut self, path: PathBuf, read_only: bool) {
        let ro = self.binds.entry(path).or_insert(read_only);
        *ro |= read_only;
    }

    /// Bind `path`, making the placeholder `stub` first when it is missing.
    fn ensure(&mut self, path: &Path, stub: Stub, read_only: bool) -> Result<(), SandboxError> {
        match entry(path)? {
            Entry::Symlink => return Err(refuse(path, "is a symlink")),
            Entry::Missing => {
                stub.create(path).map_err(|e| io_refuse(path, e))?;
                self.stubs.push((path.to_path_buf(), stub));
            }
            Entry::Dir | Entry::Other => {}
        }
        self.bind(path.to_path_buf(), read_only);
        Ok(())
    }

    /// Make every directory between the project and `target` a mount point,
    /// so none of them can be renamed away (a placeholder when missing). Each
    /// must be a real directory.
    fn anchor(&mut self, target: &Path) -> Result<(), SandboxError> {
        let rel = target
            .strip_prefix(self.project)
            .map_err(|_| refuse(target, "is outside the project"))?;
        let mut dir = self.project.to_path_buf();
        let parts: Vec<_> = rel.components().collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            dir.push(part.as_os_str());
            match entry(&dir)? {
                Entry::Dir | Entry::Missing => self.ensure(&dir.clone(), Stub::Dir, false)?,
                Entry::Symlink => return Err(refuse(&dir, "is a symlink")),
                Entry::Other => return Err(refuse(&dir, "is not a directory")),
            }
        }
        Ok(())
    }

    /// Protect one git directory: it is a mount point; its hooks, `info`,
    /// `modules` and `worktrees` directories and its `config`,
    /// `config.worktree` and `commondir` are read-only (placeholders when
    /// missing); then the same for the git directories of its submodules and
    /// linked worktrees.
    fn git_dir(&mut self, dir: &Path, kind: GitDir) -> Result<(), SandboxError> {
        self.git_dirs += 1;
        if self.git_dirs > MAX_GIT_DIRS {
            return Err(SandboxError::StartFailed(format!(
                "cannot start the sandbox: the repository has more than {MAX_GIT_DIRS} \
                 submodule and worktree git directories to protect"
            )));
        }
        self.bind(dir.to_path_buf(), false);
        self.ensure(&dir.join("config.worktree"), Stub::Empty, true)?;
        self.ensure(&dir.join("commondir"), Stub::CommonDir, true)?;
        if let Entry::Symlink = entry(&dir.join("gitdir"))? {
            return Err(refuse(&dir.join("gitdir"), "is a symlink"));
        }
        if dir.join("gitdir").exists() {
            self.bind(dir.join("gitdir"), true);
        }
        if kind == GitDir::Worktree {
            // Git reads hooks and config from the common directory.
            return Ok(());
        }
        self.ensure(&dir.join("config"), Stub::Empty, true)?;
        self.ensure(&dir.join("hooks"), Stub::Dir, true)?;
        // `info` stays writable (gc writes `info/refs`); it is a mount point
        // so `info/attributes` cannot be replaced by a rename.
        self.ensure(&dir.join("info"), Stub::Dir, false)?;
        self.ensure(&dir.join("info/attributes"), Stub::Empty, true)?;
        let modules = dir.join("modules");
        self.ensure(&modules, Stub::Dir, true)?;
        self.modules(&modules, 0)?;
        let worktrees = dir.join("worktrees");
        self.ensure(&worktrees, Stub::Dir, true)?;
        for child in children(&worktrees)? {
            if is_git_dir(&child) {
                self.git_dir(&child, GitDir::Worktree)?;
            }
        }
        Ok(())
    }

    /// Submodule git directories, named by their (possibly nested) path
    /// below `modules`.
    fn modules(&mut self, dir: &Path, depth: usize) -> Result<(), SandboxError> {
        if depth > MAX_MODULE_DEPTH {
            return Err(refuse(dir, "is nested too deep"));
        }
        for child in children(dir)? {
            if is_git_dir(&child) {
                self.git_dir(&child, GitDir::Full)?;
            } else {
                self.modules(&child, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// The directories in `dir`, sorted. A symlink refuses the start: a mount
/// below it would follow it.
fn children(dir: &Path) -> Result<Vec<PathBuf>, SandboxError> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir).map_err(|e| io_refuse(dir, e))? {
        let item = item.map_err(|e| io_refuse(dir, e))?;
        let kind = item.file_type().map_err(|e| io_refuse(dir, e))?;
        if kind.is_symlink() {
            return Err(refuse(&item.path(), "is a symlink"));
        }
        if kind.is_dir() {
            out.push(item.path());
        }
    }
    out.sort();
    Ok(out)
}

/// `core.hooksPath` from a git config file, if it sets one. Only the plain
/// `[core]` section is read; `include` is not followed.
fn hooks_path_in(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_core = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_start_matches('[').trim_end_matches(']').trim();
            in_core = name.eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("hookspath") {
            let mut value = value.trim();
            if let Some(i) = value.find([';', '#']).filter(|_| !value.starts_with('"')) {
                value = value[..i].trim();
            }
            found = Some(value.trim_matches('"').to_owned());
        }
    }
    found
}

/// The hooks directory `core.hooksPath` names for a project whose common git
/// directory is `common`, from the user's global config and the repository's
/// config (the repository wins).
fn hooks_path(project: &Path, common: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let files = [
        xdg.map(|x| x.join("git/config")),
        home.as_ref().map(|h| h.join(".gitconfig")),
        Some(common.join("config")),
    ];
    let value = files
        .iter()
        .flatten()
        .filter_map(|f| hooks_path_in(f))
        .next_back()?;
    let path = match value.strip_prefix("~/") {
        Some(rest) => home?.join(rest),
        None => project.join(value),
    };
    Some(normalize(&path))
}

/// Bind mounts that keep the protected paths of `project` read-only in the
/// container, and the placeholders made for them. Takes the project's
/// locks; the caller must later [`ProtectedMounts::release`] or
/// [`ProtectedMounts::abandon`] the result. Blocking.
///
/// A symlinked protected entry is refused: a mount follows it, and the link
/// itself would stay replaceable. So is an agent directory inside the
/// project but outside its `.loop`, and a `.git` pointer to a directory in
/// the project that is not a git directory on a `.git` path.
pub fn plan(project: &Path, agent_dir: Option<&Path>) -> Result<ProtectedMounts, SandboxError> {
    #[cfg(unix)]
    let locks = locks::ProjectLocks::acquire(project, agent_dir)?;
    let mut p = Planner {
        project,
        binds: BTreeMap::new(),
        stubs: Vec::new(),
        git_dirs: 0,
    };
    let result = plan_into(&mut p, project, agent_dir);
    if result.is_err() {
        for (path, stub) in p.stubs.iter().rev() {
            stub.remove_if_unchanged(path);
        }
    }
    result?;
    #[cfg(unix)]
    let locks = {
        let mut locks = locks;
        locks.record(&p.stubs).map_err(|e| io_refuse(project, e))?;
        locks.planned();
        locks
    };
    Ok(ProtectedMounts {
        binds: p
            .binds
            .into_iter()
            .map(|(path, read_only)| ProtectedBind { path, read_only })
            .collect(),
        stubs: p.stubs,
        git_entries: scan_git_entries(project).0,
        #[cfg(unix)]
        locks: Some(locks),
    })
}

fn plan_into(p: &mut Planner, project: &Path, agent: Option<&Path>) -> Result<(), SandboxError> {
    let dot_git = project.join(".git");
    let mut common = None;
    match entry(&dot_git)? {
        Entry::Symlink => return Err(refuse(&dot_git, "is a symlink")),
        // `git init` cannot run in a read-only empty directory.
        Entry::Missing => p.ensure(&dot_git, Stub::Dir, true)?,
        Entry::Other => {
            p.bind(dot_git.clone(), true);
            common = pointer_target(p, project, &dot_git)?;
        }
        Entry::Dir if is_git_dir(&dot_git) => {
            p.git_dir(&dot_git, GitDir::Full)?;
            common = Some(dot_git.clone());
        }
        // Not a repository (a placeholder kept for another sandbox).
        Entry::Dir => p.bind(dot_git.clone(), true),
    }
    let dot_loop = project.join(".loop");
    p.ensure(&dot_loop, Stub::Dir, true)?;
    if let Some(common) = common {
        if let Some(hooks) = hooks_path(project, &common) {
            if hooks.starts_with(project) && hooks != project {
                p.anchor(&hooks)?;
                p.ensure(&hooks, Stub::Dir, true)?;
            }
        }
    }
    if let Some(agent) = agent {
        let agent = resolve_path(agent).unwrap_or_else(|_| agent.to_path_buf());
        if agent.starts_with(project) && !agent.starts_with(&dot_loop) {
            return Err(SandboxError::StartFailed(format!(
                "cannot start the sandbox: Loop's agent directory {} is inside the project \
                 but not in its .loop, so the sandbox cannot keep it write-protected. Move it, \
                 or turn the sandbox off",
                agent.display()
            )));
        }
    }
    Ok(())
}

/// For a `.git` pointer file: protect a target inside the project, and
/// return the git directory whose config applies. A target outside the
/// project is not mounted, so git does not work in the container there.
fn pointer_target(
    p: &mut Planner,
    project: &Path,
    dot_git: &Path,
) -> Result<Option<PathBuf>, SandboxError> {
    let text = std::fs::read_to_string(dot_git).map_err(|e| io_refuse(dot_git, e))?;
    let Some(dir) = text.trim().strip_prefix("gitdir:") else {
        return Err(refuse(dot_git, "is a file that is not a git pointer"));
    };
    let target = normalize(&project.join(dir.trim()));
    if !target.starts_with(project) {
        return Ok(None);
    }
    let on_git_path = target
        .strip_prefix(project)
        .is_ok_and(|r| r.components().any(|c| c.as_os_str() == ".git"));
    if !on_git_path || !is_git_dir(&target) {
        return Err(refuse(
            dot_git,
            "points to a directory in the project that is not a git directory on a .git path",
        ));
    }
    p.anchor(&target)?;
    p.git_dir(&target, GitDir::Full)?;
    Ok(Some(target))
}

/// The project's locks and placeholder manifest, in a directory of this
/// user: the agent directory's `run/`, `$XDG_RUNTIME_DIR/loop`, or a 0700
/// directory in the temp directory.
///
/// - `<id>.plan`: held exclusive while one sandbox plans or releases, so
///   two starts never both make the same placeholder.
/// - `<id>.run`: held shared by every running sandbox; whoever can take it
///   exclusive (under the plan lock) runs alone.
/// - `<id>.stubs`: the placeholders made by the sandboxes of the project.
#[cfg(unix)]
mod locks {
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use rustix::fs::{flock, FlockOperation};

    use super::{io_refuse, Stub};
    use crate::harness::sandbox::SandboxError;

    #[derive(Debug)]
    pub(super) struct ProjectLocks {
        plan: File,
        run: File,
        manifest: PathBuf,
    }

    fn lock_dir(agent: Option<&Path>) -> Result<PathBuf, SandboxError> {
        // SAFETY: geteuid has no arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        let dir = match (agent, std::env::var_os("XDG_RUNTIME_DIR")) {
            (Some(agent), _) if agent.is_dir() => agent.join("run/sandbox"),
            (_, Some(xdg)) if !xdg.is_empty() => PathBuf::from(xdg).join("loop-sandbox"),
            _ => std::env::temp_dir().join(format!("loop-sandbox-{uid}")),
        };
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {}
            Err(e) => return Err(io_refuse(&dir, e)),
        }
        let meta = std::fs::symlink_metadata(&dir).map_err(|e| io_refuse(&dir, e))?;
        if !meta.is_dir() || meta.uid() != uid {
            return Err(SandboxError::StartFailed(format!(
                "cannot start the sandbox: {} is not a directory of this user",
                dir.display()
            )));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| io_refuse(&dir, e))?;
        }
        Ok(dir)
    }

    fn open(path: &Path) -> Result<File, SandboxError> {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| io_refuse(path, e))
    }

    fn lock(file: &File, op: FlockOperation, path: &Path) -> Result<(), SandboxError> {
        flock(file, op).map_err(|e| io_refuse(path, e.into()))
    }

    impl ProjectLocks {
        /// Take the plan lock and a shared run lock. Alone, first remove the
        /// unchanged placeholders that an earlier sandbox left (a crash).
        /// The base path of `project`'s lock files.
        fn base(project: &Path, agent: Option<&Path>) -> Result<PathBuf, SandboxError> {
            use std::os::unix::ffi::OsStrExt;
            // FNV-1a: stable across builds.
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in project.as_os_str().as_bytes() {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
            }
            Ok(lock_dir(agent)?.join(format!("{hash:016x}")))
        }

        /// Wait until no sandbox holds `project`'s run lock (at most
        /// `limit`). A closed descriptor releases its lock only when no
        /// process holds a copy: a process that this test process starts at
        /// the same moment holds one until its exec. A real crash is a dead
        /// process and holds nothing.
        #[cfg(test)]
        pub(super) fn wait_until_free(
            project: &Path,
            agent: Option<&Path>,
            limit: std::time::Duration,
        ) -> bool {
            let Ok(run_path) = Self::base(project, agent).map(|b| b.with_extension("run")) else {
                return false;
            };
            let Ok(run) = open(&run_path) else {
                return false;
            };
            let deadline = std::time::Instant::now() + limit;
            loop {
                if flock(&run, FlockOperation::NonBlockingLockExclusive).is_ok() {
                    let _ = flock(&run, FlockOperation::Unlock);
                    return true;
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        pub(super) fn acquire(project: &Path, agent: Option<&Path>) -> Result<Self, SandboxError> {
            let base = Self::base(project, agent)?;
            let plan_path = base.with_extension("plan");
            let run_path = base.with_extension("run");
            let plan = open(&plan_path)?;
            lock(&plan, FlockOperation::LockExclusive, &plan_path)?;
            let run = open(&run_path)?;
            let this = Self {
                plan,
                run,
                manifest: base.with_extension("stubs"),
            };
            if flock(&this.run, FlockOperation::NonBlockingLockExclusive).is_ok() {
                this.remove_all();
            }
            lock(&this.run, FlockOperation::LockShared, &run_path)?;
            Ok(this)
        }

        /// Add `stubs` to the manifest.
        pub(super) fn record(&mut self, stubs: &[(PathBuf, Stub)]) -> std::io::Result<()> {
            if stubs.is_empty() {
                return Ok(());
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&self.manifest)?;
            let mut text = String::new();
            for (path, stub) in stubs {
                let Some(p) = path.to_str().filter(|p| !p.contains('\n')) else {
                    continue;
                };
                text.push(stub.code());
                text.push('\t');
                text.push_str(p);
                text.push('\n');
            }
            file.write_all(text.as_bytes())
        }

        /// Planning is over: let other sandboxes plan.
        pub(super) fn planned(&self) {
            let _ = flock(&self.plan, FlockOperation::Unlock);
        }

        /// Remove every placeholder in the manifest that is unchanged, and
        /// empty the manifest. Only while no other sandbox runs.
        fn remove_all(&self) {
            let Ok(text) = std::fs::read_to_string(&self.manifest) else {
                return;
            };
            for line in text.lines().rev() {
                let mut chars = line.chars();
                let (Some(code), Some('\t')) = (chars.next(), chars.next()) else {
                    continue;
                };
                if let Some(stub) = Stub::from_code(code) {
                    stub.remove_if_unchanged(Path::new(chars.as_str()));
                }
            }
            let _ = std::fs::remove_file(&self.manifest);
        }

        /// This sandbox stops (its container is gone). The last one removes
        /// the placeholders.
        pub(super) fn release(self) {
            if flock(&self.plan, FlockOperation::LockExclusive).is_err() {
                return;
            }
            if flock(&self.run, FlockOperation::NonBlockingLockExclusive).is_ok() {
                self.remove_all();
            }
        }
    }
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

    fn names(dir: &Path) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            out.insert(e.file_name().to_string_lossy().into_owned());
            if e.path().is_dir() {
                for f in std::fs::read_dir(e.path()).unwrap().flatten() {
                    out.insert(format!(
                        "{}/{}",
                        e.file_name().to_string_lossy(),
                        f.file_name().to_string_lossy()
                    ));
                }
            }
        }
        out
    }

    #[test]
    fn a_repository_gets_read_only_metadata_and_placeholders_that_go_away() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let before = names(&project.join(".git"));
        let m = plan(&project, None).unwrap();
        assert_eq!(
            binds(&m, &project),
            vec![
                (".git".into(), false),
                (".git/commondir".into(), true),
                (".git/config".into(), true),
                (".git/config.worktree".into(), true),
                (".git/hooks".into(), true),
                (".git/info".into(), false),
                (".git/info/attributes".into(), true),
                (".git/modules".into(), true),
                (".git/worktrees".into(), true),
                (".loop".into(), true),
            ]
        );
        // A commondir placeholder names the git directory itself; an empty
        // one would make git fail.
        assert_eq!(
            std::fs::read_to_string(project.join(".git/commondir")).unwrap(),
            ".\n"
        );
        assert_eq!(
            std::fs::read(project.join(".git/config.worktree")).unwrap(),
            b""
        );
        // The existing config is not touched.
        assert_eq!(
            std::fs::read_to_string(project.join(".git/config")).unwrap(),
            "[core]\n"
        );
        m.release();
        assert_eq!(names(&project.join(".git")), before);
    }

    #[test]
    fn missing_git_and_loop_get_read_only_placeholders_that_go_away() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let m = plan(&project, None).unwrap();
        assert_eq!(
            binds(&m, &project),
            vec![(".git".into(), true), (".loop".into(), true)]
        );
        m.release();
        assert!(!project.join(".git").exists() && !project.join(".loop").exists());
    }

    #[test]
    fn a_changed_placeholder_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        std::fs::remove_dir_all(project.join(".loop")).unwrap();
        let m = plan(&project, None).unwrap();
        std::fs::write(project.join(".loop/settings.json"), "{}").unwrap();
        std::fs::write(project.join(".git/commondir"), "elsewhere\n").unwrap();
        m.release();
        assert!(project.join(".loop/settings.json").is_file());
        assert!(project.join(".git/commondir").is_file());
        assert!(!project.join(".git/config.worktree").exists());
    }

    #[cfg(unix)]
    #[test]
    fn placeholders_stay_while_another_sandbox_runs_and_a_crash_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let first = plan(&project, None).unwrap();
        let second = plan(&project, None).unwrap();
        first.release();
        assert!(
            project.join(".git").is_dir(),
            "removed under a running sandbox"
        );
        // The last one removes all placeholders, also those of the first.
        second.release();
        assert!(!project.join(".git").exists() && !project.join(".loop").exists());
        // A sandbox that never released (a crash): the next start alone
        // removes its placeholders first.
        let crashed = plan(&project, None).unwrap();
        drop(crashed);
        assert!(project.join(".git").is_dir());
        // The crashed holder is gone once no process holds a copy of its
        // descriptor (another test may start a process right now).
        assert!(locks::ProjectLocks::wait_until_free(
            &project,
            None,
            std::time::Duration::from_secs(10)
        ));
        let next = plan(&project, None).unwrap();
        assert!(
            !next.stub_paths().is_empty(),
            "the next start makes its own"
        );
        next.release();
        assert!(!project.join(".git").exists());
    }

    #[test]
    fn an_empty_git_directory_is_not_treated_as_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(project.join(".git")).unwrap();
        let m = plan(&project, None).unwrap();
        assert!(binds(&m, &project).contains(&(".git".into(), true)));
        assert!(!project.join(".git/hooks").exists());
        m.release();
        assert!(project.join(".git").is_dir(), "not ours to remove");
    }

    #[test]
    fn a_pointer_outside_the_project_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap().join("p");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join(".git"), "gitdir: ../elsewhere\n").unwrap();
        let m = plan(&project, None).unwrap();
        assert!(binds(&m, &project).contains(&(".git".into(), true)));
        m.release();
    }

    #[test]
    fn a_pointer_inside_the_project_protects_its_target_and_the_way_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let target = project.join("vendor/.git/real");
        std::fs::create_dir_all(target.join("objects")).unwrap();
        std::fs::write(target.join("HEAD"), "x").unwrap();
        std::fs::write(project.join(".git"), "gitdir: vendor/.git/real\n").unwrap();
        let m = plan(&project, None).unwrap();
        let b = binds(&m, &project);
        for want in [
            (".git", true),
            ("vendor", false),
            ("vendor/.git", false),
            ("vendor/.git/real", false),
            ("vendor/.git/real/hooks", true),
            ("vendor/.git/real/commondir", true),
        ] {
            assert!(b.contains(&(want.0.into(), want.1)), "{want:?} in {b:?}");
        }
        m.release();
        // Not on a .git path, or not a git directory: refused.
        std::fs::write(project.join(".git"), "gitdir: ./realgit\n").unwrap();
        std::fs::create_dir_all(project.join("realgit")).unwrap();
        std::fs::write(project.join("realgit/HEAD"), "x").unwrap();
        let err = plan(&project, None).unwrap_err().to_string();
        assert!(err.contains("not a git directory on a .git path"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_protected_entries_refuse_the_start_and_leave_no_placeholder() {
        for link in [
            ".git",
            ".loop",
            ".git/hooks",
            ".git/config",
            ".git/info",
            ".git/modules",
            ".git/worktrees",
        ] {
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
            assert!(
                !project.join(".git/commondir").exists(),
                "{link}: placeholder left"
            );
            if link == ".loop" {
                assert!(!project.join(".git").exists(), "placeholder left behind");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_modules_or_worktrees_refuses_the_start() {
        for parent in [".git/modules", ".git/worktrees"] {
            let dir = tempfile::tempdir().unwrap();
            let project = repo(dir.path());
            std::fs::create_dir_all(project.join(parent)).unwrap();
            std::os::unix::fs::symlink(project.join("x"), project.join(parent).join("a")).unwrap();
            let err = plan(&project, None).unwrap_err().to_string();
            assert!(err.contains("is a symlink"), "{parent}: {err}");
        }
    }

    #[test]
    fn modules_nested_too_deep_refuse_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let deep = (0..=MAX_MODULE_DEPTH + 1)
            .fold(project.join(".git/modules"), |p, i| p.join(format!("d{i}")));
        std::fs::create_dir_all(&deep).unwrap();
        let err = plan(&project, None).unwrap_err().to_string();
        assert!(err.contains("nested too deep"), "{err}");
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
        let wt = git.join("worktrees/w1");
        std::fs::create_dir_all(&wt).unwrap();
        for f in ["HEAD", "commondir", "gitdir"] {
            std::fs::write(wt.join(f), "../..\n").unwrap();
        }
        let m = plan(&project, None).unwrap();
        let b = binds(&m, &project);
        for want in [
            (".git/modules", true),
            (".git/modules/libs/a", false),
            (".git/modules/libs/a/config", true),
            (".git/modules/libs/a/hooks", true),
            (".git/modules/libs/a/commondir", true),
            (".git/modules/libs/a/modules", true),
            (".git/worktrees", true),
            (".git/worktrees/w1", false),
            (".git/worktrees/w1/commondir", true),
            (".git/worktrees/w1/config.worktree", true),
            (".git/worktrees/w1/gitdir", true),
        ] {
            assert!(b.contains(&(want.0.into(), want.1)), "{want:?} in {b:?}");
        }
        // Worktree admin directories get no hooks or config.
        assert!(!wt.join("hooks").exists() && !wt.join("config").exists());
        assert_eq!(
            std::fs::read_to_string(wt.join("commondir")).unwrap(),
            "../..\n"
        );
        m.release();
        assert!(!sub.join("commondir").exists());
    }

    #[test]
    fn hooks_path_in_the_project_is_read_only_with_its_parents_as_mount_points() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        std::fs::write(
            project.join(".git/config"),
            "[core]\n\tbare = false\n\thooksPath = .husky/_ ; husky\n",
        )
        .unwrap();
        let m = plan(&project, None).unwrap();
        let b = binds(&m, &project);
        assert!(b.contains(&(".husky".into(), false)), "{b:?}");
        assert!(b.contains(&(".husky/_".into(), true)), "{b:?}");
        m.release();
        assert!(!project.join(".husky").exists());
        assert_eq!(
            hooks_path_in(&project.join(".git/config")).as_deref(),
            Some(".husky/_")
        );
    }

    #[test]
    fn an_agent_directory_inside_the_project_outside_loop_refuses_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        let agent = project.join("home/agent");
        std::fs::create_dir_all(&agent).unwrap();
        let err = plan(&project, Some(&agent)).unwrap_err().to_string();
        assert!(err.contains("agent directory"), "{err}");
        assert!(!project.join(".git/commondir").exists());
        let inside_loop = project.join(".loop/agent");
        std::fs::create_dir_all(&inside_loop).unwrap();
        plan(&project, Some(&inside_loop)).unwrap().release();
        let outside = tempfile::tempdir().unwrap();
        plan(&project, Some(outside.path())).unwrap().release();
    }

    #[test]
    fn a_new_repository_in_the_project_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let project = repo(dir.path());
        std::fs::create_dir_all(project.join("old/.git")).unwrap();
        let m = plan(&project, None).unwrap();
        std::fs::create_dir_all(project.join("sub/.git")).unwrap();
        std::fs::create_dir_all(project.join("a/b/.git")).unwrap();
        let (added, complete) = m.new_git_entries(&project);
        assert!(complete);
        assert_eq!(
            added,
            vec![project.join("a/b/.git"), project.join("sub/.git")]
        );
        m.release();
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
        // The `.git` placeholder is a real directory, so only the mount rule
        // refuses `.git/HEAD` in it.
        write_denied(&project, None, Path::new(".git/HEAD")).unwrap();
        assert!(under_read_only(&project, &ro, Path::new(".git/HEAD")).is_err());
        under_read_only(&project, &ro, Path::new("src/a.rs")).unwrap();
        m.release();
    }
}
