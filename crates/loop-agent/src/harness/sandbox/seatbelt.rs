//! macOS Seatbelt execution and filesystem confinement.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::{Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus};
use crate::harness::env::exec_command;
use crate::harness::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo,
    FileSystem, Shell, ShellExecOptions, ShellOutput,
};

fn io_error(e: impl Into<std::io::Error>) -> FileError {
    let e = e.into();
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        _ => FileErrorCode::Io,
    };
    FileError::new(code, e.to_string())
}

fn denied(path: &Path) -> FileError {
    FileError::new(
        FileErrorCode::PermissionDenied,
        format!("sandbox denies access to {}", path.display()),
    )
}

// Resolve existing ancestors too: canonicalize(path).unwrap_or(path) would let
// a symlink followed by a not-yet-created child escape the write jail.
pub(super) fn resolve_path(path: &Path) -> Result<PathBuf, FileError> {
    let absolute = std::path::absolute(path).map_err(io_error)?;
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => {
                out.push(part.as_os_str());
                match std::fs::symlink_metadata(&out) {
                    Ok(_) => out = std::fs::canonicalize(&out).map_err(io_error)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_error(e)),
                }
            }
        }
    }
    Ok(out)
}

/// Git metadata that can make git run a command or trust another directory:
/// config, hooks and pointers, in any repository's `.git`, including
/// submodules (`.git/modules/**`) and worktrees (`.git/worktrees/**`).
/// The same expression is used in the Seatbelt profile and by
/// [`JailedHostFs`], so paths created later are covered too.
const GIT_METADATA: &str = r"/\.git/((modules|worktrees)/.+/)?(config|config\.worktree|commondir|gitdir|hooks|info/attributes)(/|$)";

/// A `.git` entry itself. Commands may not create, rewrite, replace or remove
/// one, so they cannot start a repository whose config they wrote or point an
/// existing pointer file elsewhere.
const GIT_ENTRY: &str = r"/\.git$";

pub(super) fn git_metadata(path: &Path) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(GIT_METADATA).expect("valid pattern"));
    path.to_str().is_none_or(|p| RE.is_match(p))
}

/// Whether writing `path` would create, replace or remove a `.git` entry, or
/// write below a `.git` that does not exist yet as a real directory.
pub(super) fn git_entry_write(path: &Path) -> bool {
    if path.file_name().is_some_and(|n| n == ".git") {
        return true;
    }
    path.ancestors().skip(1).any(|a| {
        a.file_name().is_some_and(|n| n == ".git")
            && !std::fs::symlink_metadata(a).is_ok_and(|m| m.is_dir())
    })
}

/// Check the project's `.git` and return the git directories outside the
/// project that commits and branches need to write.
///
/// Accepted: a `.git` directory; a pointer to a git directory inside the
/// project within a `.git` path; a linked worktree, pointing to
/// `<common>/worktrees/<name>` whose `commondir` names `<common>` and whose
/// `gitdir` names this project; and a submodule checkout, pointing to
/// `<superproject>/.git/modules/<name>`. Every accepted git directory lies on a
/// `.git` path, so [`GIT_METADATA`] keeps its config, hooks and pointers
/// write-protected. A symlinked `.git` and any other pointer are refused.
fn project_git_writes(project: &Path) -> Result<Vec<PathBuf>, FileError> {
    let dot_git = project.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
        return Ok(vec![]);
    };
    let refuse = |why: String| {
        Err(FileError::new(
            FileErrorCode::PermissionDenied,
            format!(
                "{why}; the sandbox cannot protect that repository's hooks and config. \
                 Run Loop in the repository's main checkout, or turn the sandbox off"
            ),
        ))
    };
    if meta.file_type().is_symlink() {
        return refuse(format!(
            "the project's .git is a symlink ({})",
            dot_git.display()
        ));
    }
    if !meta.is_file() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(&dot_git).map_err(io_error)?;
    let Some(dir) = text.trim().strip_prefix("gitdir:") else {
        return refuse("the project's .git is a file that is not a git pointer".into());
    };
    let target = resolve_path(&project.join(dir.trim()))?;
    let on_git_path = |p: &Path| p.components().any(|c| c.as_os_str() == ".git");
    if target.starts_with(project) && on_git_path(&target) {
        return Ok(vec![]);
    }
    let unsupported = || {
        refuse(format!(
            "the project's .git points to {}, which is not a linked worktree or submodule \
             of a repository with a .git directory",
            target.display()
        ))
    };
    let is_git_dir = |p: &Path| p.join("HEAD").is_file() && p.join("objects").is_dir();
    if !on_git_path(&target) || !target.join("HEAD").is_file() {
        return unsupported();
    }
    let parent = target.parent();
    if parent
        .and_then(Path::file_name)
        .is_some_and(|n| n == "worktrees")
    {
        // Linked worktree: its admin directory names the shared git directory
        // and points back at this checkout.
        let common = parent.and_then(Path::parent);
        let named = std::fs::read_to_string(target.join("commondir"))
            .ok()
            .and_then(|c| resolve_path(&target.join(c.trim())).ok());
        let back = std::fs::read_to_string(target.join("gitdir"))
            .ok()
            .and_then(|g| resolve_path(&target.join(g.trim())).ok());
        let (Some(common), Some(named), Some(back)) = (common, named, back) else {
            return unsupported();
        };
        if named != common
            || !is_git_dir(common)
            || !common.join("config").is_file()
            || back != resolve_path(&dot_git)?
        {
            return unsupported();
        }
        return Ok(vec![
            target.clone(),
            common.join("objects"),
            common.join("refs"),
            common.join("logs"),
            common.join("packed-refs"),
            common.join("packed-refs.lock"),
        ]);
    }
    // Submodule checkout: a complete git directory below `.git/modules`.
    let in_modules = target
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == ".git" && w[1] == "modules");
    if in_modules
        && is_git_dir(&target)
        && target.join("config").is_file()
        && !target.join("commondir").exists()
    {
        return Ok(vec![target]);
    }
    unsupported()
}

/// Lock file inside a Seatbelt temporary directory, held while the
/// directory is in use. A directory whose lock can be taken is stale.
const TEMP_LOCK: &str = ".loop-lock";

/// Create and hold the lock of `dir`, a fresh temporary directory.
#[cfg(unix)]
fn hold_lock(dir: &Path) -> std::io::Result<std::fs::File> {
    // Publish only an already-held lock. Otherwise another sandbox's cleanup
    // can see the new file before flock and remove this live directory.
    let file = tempfile::NamedTempFile::new_in(dir)?;
    rustix::fs::flock(
        file.as_file(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )?;
    file.persist_noclobber(dir.join(TEMP_LOCK))
        .map_err(|error| error.error)
}

/// The Seatbelt rule that lets xcrun write its cache files (`xcrun_db` and
/// the `xcrun_db-XXXXXX` file it renames into place) in the user temp dir,
/// `confstr(_CS_DARWIN_USER_TEMP_DIR)`. `None` when the dir is unknown or its
/// path has characters the rule does not escape.
#[cfg(target_os = "macos")]
fn xcrun_cache_rule() -> Option<String> {
    let mut buf = [0 as libc::c_char; libc::PATH_MAX as usize];
    // SAFETY: `buf` is valid for `buf.len()` bytes; confstr writes at most that
    // many, NUL-terminated.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    // SAFETY: confstr NUL-terminated the buffer.
    let dir = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .ok()?;
    // Seatbelt matches resolved paths: `/var` is a link to `/private/var`.
    let dir = std::fs::canonicalize(dir).ok()?;
    let dir = dir.to_str()?.trim_end_matches('/');
    if dir.is_empty()
        || !dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
    {
        return None;
    }
    let dir = dir.replace('.', "\\.");
    Some(format!(
        "(allow file-write* (regex #\"^{dir}/xcrun_db(-[A-Za-z0-9]+)?$\"))\n"
    ))
}

#[cfg(not(target_os = "macos"))]
fn xcrun_cache_rule() -> Option<String> {
    None
}

/// Remove `/tmp/loop-seatbelt-*` and `/tmp/loop-command-*` directories this
/// user left behind: their lock is free (its process ended), or, without a
/// lock, they are more than a day old (an older build). Never follows links.
#[cfg(unix)]
pub fn clean_stale_temp_dirs() {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    let Ok(entries) = std::fs::read_dir("/private/tmp") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("loop-seatbelt-") || name.starts_with("loop-command-")) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.uid() != uid {
            continue;
        }
        let stale = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join(TEMP_LOCK))
        {
            Ok(lock) => {
                rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                    .is_ok()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(24 * 60 * 60)),
            Err(_) => false,
        };
        if stale {
            if let Err(error) = std::fs::remove_dir_all(&path) {
                tracing::debug!(%error, path = %path.display(), "could not remove a stale Seatbelt directory");
            }
        }
    }
}

/// A command's identity directory and the lock that marks it in use.
struct Marker {
    dir: tempfile::TempDir,
    _lock: std::fs::File,
}

/// Paths under `$HOME` that stay readable with `sandbox.denyHomeReads`, so
/// git and the Rust toolchain keep working. Credential files below them are
/// still denied.
const HOME_READ_DEFAULTS: [&str; 5] = [
    ".gitconfig",
    ".config/git",
    ".cargo",
    ".rustup",
    ".gitignore_global",
];

/// Shared shell and file-tool policy. Paths are resolved before granting access.
#[derive(Clone)]
pub struct SeatbeltPolicy {
    project: PathBuf,
    writes: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    write_denied: Vec<PathBuf>,
    database_dirs: Vec<PathBuf>,
    #[cfg(unix)]
    read_ids: std::collections::HashSet<(u64, u64)>,
    #[cfg(unix)]
    write_ids: std::collections::HashSet<(u64, u64)>,
    network: bool,
    temp_root: Option<PathBuf>,
    /// With `sandbox.denyHomeReads`: `$HOME` (configured and resolved) and
    /// the paths below it that stay readable.
    home_reads: Option<(Vec<PathBuf>, Vec<PathBuf>)>,
}

/// The secrets below the home directory (and the system keychains) that
/// the Seatbelt profile denies to sandboxed commands, whatever the Policy:
/// credential stores, shell histories and other agents' configuration. The
/// edge listener's tool-lock root check reads the same list.
pub fn sensitive_home_paths(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".git-credentials"),
        home.join(".config/git/credentials"),
        home.join(".cargo/credentials"),
        home.join(".cargo/credentials.toml"),
        home.join(".gem/credentials"),
        home.join(".terraform.d/credentials.tfrc.json"),
        home.join(".azure"),
        home.join(".config/hub"),
        home.join(".bash_history"),
        home.join(".zsh_history"),
        home.join(".zhistory"),
        home.join(".sh_history"),
        home.join(".history"),
        home.join(".local/share/fish/fish_history"),
        home.join(".python_history"),
        home.join(".node_repl_history"),
        home.join(".psql_history"),
        home.join(".mysql_history"),
        home.join(".sqlite_history"),
        home.join(".lesshst"),
        home.join(".netrc"),
        home.join(".docker/config.json"),
        home.join(".kube/config"),
        home.join(".npmrc"),
        home.join(".pypirc"),
        home.join(".config/gcloud"),
        home.join("Library/Cookies"),
        home.join(".codex"),
        home.join(".claude"),
        home.join(".claude.json"),
        home.join(".gemini"),
        home.join(".qwen"),
        home.join(".copilot"),
        home.join(".config/github-copilot"),
        home.join(".cursor"),
        home.join(".continue"),
        home.join(".aider.conf.yml"),
        home.join(".config/opencode"),
        home.join(".local/share/opencode"),
        home.join(".config/goose"),
        home.join(".config/amp"),
        home.join(".local/share/amp"),
        home.join(".ssh"),
        home.join(".aws"),
        home.join(".gnupg"),
        home.join(".config/gh"),
        home.join("Library/Keychains"),
        PathBuf::from("/Library/Keychains"),
    ]
}

/// [`sensitive_home_paths`] and more folders that hold credentials or
/// personal data: whole credential stores and browser profiles. The Edge
/// listener's tool-lock root check uses this wider list; the Seatbelt
/// profile does not deny these, so tools such as `docker` and `kubectl`
/// keep working inside the sandbox.
pub fn sensitive_home_paths_extended(home: &Path) -> Vec<PathBuf> {
    let mut paths = sensitive_home_paths(home);
    paths.extend([
        home.join(".password-store"),
        home.join(".local/share/keyrings"),
        home.join(".kube"),
        home.join(".docker"),
        home.join(".mozilla"),
        home.join(".config/google-chrome"),
        home.join(".config/chromium"),
        home.join(".config/BraveSoftware"),
        home.join("Library/Application Support/Google/Chrome"),
        home.join("Library/Application Support/Firefox"),
        home.join("Library/Application Support/BraveSoftware"),
        home.join("Library/Application Support/Microsoft Edge"),
        home.join("Library/Safari"),
    ]);
    paths
}

impl SeatbeltPolicy {
    /// Build a policy with explicit home, agent directory and Runtime endpoints.
    pub fn new(
        project: &Path,
        home: &Path,
        agent: &Path,
        allow_write: &[PathBuf],
        network: bool,
        runtime_paths: &[PathBuf],
    ) -> Result<Self, FileError> {
        let project = std::fs::canonicalize(project).map_err(io_error)?;
        let mut writes = vec![project.clone()];
        for path in allow_write {
            writes.push(resolve_path(&project.join(path))?);
        }
        // Commits and branches in a linked worktree or submodule checkout
        // write its git directory outside the project.
        writes.extend(project_git_writes(&project)?);
        let mut denied_paths = vec![];
        for path in sensitive_home_paths(home)
            .into_iter()
            .chain([
                agent.join("auth.json"),
                agent.join("audit"),
                agent.join("run"),
                agent.join("mcp-auth"),
                agent.join("agents"),
                agent.join("history.json"),
                agent.join("history"),
                agent.join("logs"),
                agent.join("usage"),
            ])
            .chain(runtime_paths.iter().cloned())
        {
            // Protect both the configured name and its resolved target.
            denied_paths.push(std::path::absolute(&path).map_err(io_error)?);
            denied_paths.push(resolve_path(&path)?);
        }
        let mut database_dirs = vec![
            std::path::absolute(agent).map_err(io_error)?,
            resolve_path(agent)?,
        ];
        // Session storage can live outside the agent directory. Protect its
        // SQLite files and sidecars too, without granting access to that root.
        if let Some(dir) =
            std::env::var_os("LOOP_CODING_AGENT_SESSION_DIR").filter(|v| !v.is_empty())
        {
            let dir = PathBuf::from(dir);
            database_dirs.push(std::path::absolute(&dir).map_err(io_error)?);
            database_dirs.push(resolve_path(&dir)?);
        }
        for dir in &database_dirs {
            for name in [
                "sessions.db",
                "sessions.db-wal",
                "sessions.db-shm",
                "events.db",
                "events.db-wal",
                "events.db-shm",
            ] {
                denied_paths.push(dir.join(name));
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("sessions.db") || name.starts_with("events.db") {
                        denied_paths.push(entry.path());
                    }
                }
            }
        }
        let mut write_denied = denied_paths.clone();
        // Git metadata is covered by [`GIT_METADATA`] wherever it is.
        for path in [agent.to_path_buf(), project.join(".loop")] {
            write_denied.push(std::path::absolute(&path).map_err(io_error)?);
            write_denied.push(resolve_path(&path)?);
        }
        if writes
            .iter()
            .chain(denied_paths.iter())
            .chain(write_denied.iter())
            .any(|p| p.to_str().is_none())
        {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                "Seatbelt paths must be valid UTF-8",
            ));
        }
        #[cfg(unix)]
        let (read_ids, write_ids) = {
            use std::os::unix::fs::MetadataExt;
            let ids = |paths: &[PathBuf]| {
                paths
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).ok())
                    .map(|m| (m.dev(), m.ino()))
                    .collect()
            };
            (ids(&denied_paths), ids(&write_denied))
        };
        Ok(Self {
            #[cfg(unix)]
            read_ids,
            #[cfg(unix)]
            write_ids,
            project,
            writes,
            denied: denied_paths,
            write_denied,
            database_dirs,
            network,
            temp_root: None,
            home_reads: None,
        })
    }

    /// Deny reads under `home` except the project, the writable paths,
    /// `allow_read` and [`HOME_READ_DEFAULTS`] (`sandbox.denyHomeReads`).
    pub fn deny_home_reads(
        &mut self,
        home: &Path,
        allow_read: &[PathBuf],
    ) -> Result<(), FileError> {
        let homes = vec![
            std::path::absolute(home).map_err(io_error)?,
            resolve_path(home)?,
        ];
        let mut readable = vec![self.project.clone()];
        readable.extend(self.writes.iter().cloned());
        for path in allow_read
            .iter()
            .map(|p| self.project.join(p))
            .chain(HOME_READ_DEFAULTS.iter().map(|p| home.join(p)))
        {
            readable.push(std::path::absolute(&path).map_err(io_error)?);
            readable.push(resolve_path(&path)?);
        }
        if homes
            .iter()
            .chain(readable.iter())
            .any(|p| p.to_str().is_none())
        {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                "Seatbelt paths must be valid UTF-8",
            ));
        }
        self.home_reads = Some((homes, readable));
        Ok(())
    }

    fn home_read_denied(&self, absolute: &Path, resolved: &Path) -> bool {
        let Some((homes, readable)) = &self.home_reads else {
            return false;
        };
        let under = |p: &Path, roots: &[PathBuf]| roots.iter().any(|r| p.starts_with(r));
        [absolute, resolved]
            .iter()
            .any(|p| under(p, homes) && !under(p, readable))
    }

    fn resolve(&self, path: &Path, write: bool) -> Result<PathBuf, FileError> {
        let absolute = self.project.join(path);
        let resolved = resolve_path(&absolute)?;
        if absolute.starts_with("/dev") || resolved.starts_with("/dev") {
            return Err(denied(&absolute));
        }
        if self.db_path(&absolute) || self.db_path(&resolved) {
            return Err(denied(&absolute));
        }
        if self.home_read_denied(&absolute, &resolved) {
            return Err(denied(&absolute));
        }
        if write
            && (git_metadata(&absolute)
                || git_metadata(&resolved)
                || git_entry_write(&absolute)
                || git_entry_write(&resolved))
        {
            return Err(denied(&absolute));
        }
        if self
            .denied
            .iter()
            .any(|p| absolute.starts_with(p) || resolved.starts_with(p))
            || (write
                && (self
                    .write_denied
                    .iter()
                    .any(|p| absolute.starts_with(p) || resolved.starts_with(p))
                    || !self.writes.iter().any(|p| resolved.starts_with(p))))
        {
            return Err(denied(&absolute));
        }
        Ok(resolved)
    }

    fn db_path(&self, path: &Path) -> bool {
        self.database_dirs.iter().any(|root| {
            path.strip_prefix(root).ok().is_some_and(|relative| {
                relative.components().any(|c| {
                    let name = c.as_os_str().to_string_lossy();
                    name.starts_with("sessions.db") || name.starts_with("events.db")
                })
            })
        })
    }

    #[cfg(unix)]
    fn denied_identities(&self, write: bool) -> std::collections::HashSet<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let mut paths = self.denied.clone();
        if write {
            paths.extend(self.write_denied.iter().cloned());
        }
        for agent in &self.database_dirs {
            if let Ok(entries) = std::fs::read_dir(agent) {
                paths.extend(
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| self.db_path(p)),
                );
            }
        }
        let mut ids = self.read_ids.clone();
        if write {
            ids.extend(&self.write_ids);
        }
        ids.extend(
            paths
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| (m.dev(), m.ino())),
        );
        ids
    }

    fn profile(&self) -> (String, Vec<String>) {
        let mut profile = String::from(
            r#"(version 1)
(deny default)
(deny system-mac-syscall (mac-policy-name "Sandbox") (mac-syscall-number 0))
(allow process-exec process-fork)
(allow signal (target self))
(allow sysctl-read)
; xcrun (including /usr/bin/git) asks dirhelper for per-user cache paths.
(allow mach-lookup (global-name "com.apple.bsd.dirhelper"))
(allow file-read*)
(allow mach-lookup (global-name "com.apple.system.logger") (global-name "com.apple.system.notification_center") (global-name "com.apple.system.opendirectoryd.libinfo") (global-name "com.apple.system.configd") (global-name "com.apple.cfprefsd.daemon") (global-name "com.apple.cfprefsd.agent"))
(allow file-write* (literal (param "DEV_NULL")))
"#,
        );
        // xcrun (the `/usr/bin/git` shim) caches tool lookups in `xcrun_db` in
        // the user temp dir, through a `xcrun_db-XXXXXX` file it renames. Without
        // this it prints "couldn't create cache file" on every run when the
        // cache is missing. Only those two names are writable, not the dir.
        if let Some(rule) = xcrun_cache_rule() {
            profile.push_str(&rule);
        }
        let mut params = vec!["DEV_NULL=/dev/null".into()];
        for (i, path) in self.writes.iter().enumerate() {
            profile.push_str(&format!(
                "(allow file-write* (subpath (param \"WRITE_{i}\")))\n"
            ));
            params.push(format!("WRITE_{i}={}", path.display()));
        }
        if let Some(root) = &self.temp_root {
            profile.push_str("(deny file-write-mode file-write-owner file-write-flags (literal (param \"TMP_ROOT\")))\n");
            params.push(format!("TMP_ROOT={}", root.display()));
            profile.push_str("(deny file-write* (literal (param \"TMP_LOCK\")))\n");
            params.push(format!("TMP_LOCK={}", root.join(TEMP_LOCK).display()));
        }
        profile.push_str(&format!(
            "(deny file-write* (regex #\"{GIT_METADATA}\"))\n(deny file-write* (regex #\"{GIT_ENTRY}\"))\n"
        ));
        if let Some((homes, readable)) = &self.home_reads {
            // Contents only: lookups and stat of ancestors keep working.
            // Seatbelt prefers an operation-specific rule over a wildcard one,
            // whatever their order, so every secret deny below also names
            // `file-read-data`; otherwise these allows would reopen it.
            for (i, path) in homes.iter().enumerate() {
                profile.push_str(&format!(
                    "(deny file-read-data (subpath (param \"HOME_{i}\")))\n"
                ));
                params.push(format!("HOME_{i}={}", path.display()));
            }
            for (i, path) in readable.iter().enumerate() {
                profile.push_str(&format!(
                    "(allow file-read-data (subpath (param \"READ_{i}\")))\n"
                ));
                params.push(format!("READ_{i}={}", path.display()));
            }
        }
        if self.network {
            profile.push_str(r#"(allow network*)
(allow mach-lookup (global-name "com.apple.mDNSResponder") (global-name "com.apple.trustd.agent") (global-name "com.apple.trustd"))
(deny network* (require-all (remote unix-socket) (require-not (literal (param "DNS_SOCKET")))))
"#);
            params.push("DNS_SOCKET=/private/var/run/mDNSResponder".into());
        } else {
            profile.push_str("(deny network* (remote unix-socket))\n");
        }
        // The only pathname Unix exception is the system-owned DNS resolver
        // when networking is on. Runtime and custom Unix sockets stay denied.
        for (i, path) in self.denied.iter().enumerate() {
            profile.push_str(&format!(
                "(deny file-read* file-read-data file-write* (subpath (param \"DENY_{i}\")))\n"
            ));
            params.push(format!("DENY_{i}={}", path.display()));
        }
        for (i, path) in self.write_denied.iter().enumerate() {
            profile.push_str(&format!(
                "(deny file-write* (subpath (param \"WRITE_DENY_{i}\")))\n"
            ));
            params.push(format!("WRITE_DENY_{i}={}", path.display()));
        }
        for (i, path) in self.database_dirs.iter().enumerate() {
            profile.push_str(&format!("(deny file-read* file-read-data file-write* (require-all (subpath (param \"AGENT_{i}\")) (regex #\"/(sessions|events)\\.db[^/]*(/|$)\")))\n"));
            params.push(format!("AGENT_{i}={}", path.display()));
        }
        // A pathname deny can otherwise be bypassed by renaming an allowed
        // ancestor (e.g. the whole agent dir) and reading auth.json there.
        let mut locked = std::collections::BTreeSet::new();
        for path in self.denied.iter().chain(self.write_denied.iter()) {
            for ancestor in path.ancestors().skip(1) {
                locked.insert(ancestor.to_path_buf());
            }
        }
        for (i, path) in locked.iter().enumerate() {
            profile.push_str(&format!(
                "(deny file-write-unlink (literal (param \"LOCK_{i}\")))\n"
            ));
            params.push(format!("LOCK_{i}={}", path.display()));
        }
        (profile, params)
    }

    fn command(&self, program: &str, args: &[&str]) -> Result<(Command, Marker), ExecutionError> {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        builder.permissions(std::fs::Permissions::from_mode(0o700));
        let marker = builder
            .prefix("loop-command-")
            .tempdir_in("/tmp")
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let lock = hold_lock(marker.path())
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let marker = Marker {
            dir: marker,
            _lock: lock,
        };
        let root = std::fs::canonicalize(marker.dir.path())
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        if root.to_str().is_none() {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Io,
                "Seatbelt command identity path must be UTF-8",
            ));
        }
        for name in ["deny", "allow"] {
            std::fs::write(root.join(name), "command identity")
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        }
        let (mut profile, mut params) = self.profile();
        profile.push_str("(deny file-read* (literal (param \"COMMAND_DENY\")))\n(deny file-write* (subpath (param \"COMMAND_ROOT\")))\n");
        params.push(format!("COMMAND_DENY={}", root.join("deny").display()));
        params.push(format!("COMMAND_ROOT={}", root.display()));
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile);
        for param in params {
            cmd.arg("-D").arg(param);
        }
        cmd.arg(program).args(args);
        Ok((cmd, marker))
    }
}

/// The only variable names Seatbelt commands get (`LC_*` is a prefix).
pub const SEATBELT_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "COLORTERM",
    "TMPDIR",
    "TZ",
    "NO_COLOR",
    "CLICOLOR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "DEVELOPER_DIR",
    "SDKROOT",
    "LC_*",
];

fn safe_env_key(key: &str) -> bool {
    SEATBELT_ENV_ALLOWLIST
        .iter()
        .any(|name| match name.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == *name,
        })
}

/// Host filesystem access with canonical policy checks and no-follow opens.
/// Descriptor-relative operations fail closed on a symlink replacement race.
pub struct JailedHostFs {
    policy: SeatbeltPolicy,
}

impl JailedHostFs {
    /// Bind file tools to the same policy as shell commands.
    pub fn new(policy: SeatbeltPolicy) -> Self {
        Self { policy }
    }
}

#[cfg(unix)]
impl JailedHostFs {
    fn open(
        &self,
        path: &Path,
        write: bool,
        flags: rustix::fs::OFlags,
        create_parents: bool,
    ) -> Result<std::fs::File, FileError> {
        use rustix::fs::{mkdirat, open, openat, Mode, OFlags};
        let path = self.policy.resolve(path, write)?;
        let denied_ids = self.policy.denied_identities(write);
        let mut fd = open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_error)?;
        let parts: Vec<_> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(p) => Some(p),
                _ => None,
            })
            .collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let opts = if last {
                flags
            } else {
                OFlags::RDONLY | OFlags::DIRECTORY
            } | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
                | OFlags::NONBLOCK;
            if create_parents && !last {
                match mkdirat(&fd, *part, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(io_error(e)),
                }
            }
            fd = openat(&fd, *part, opts, Mode::from_raw_mode(0o644)).map_err(io_error)?;
            let meta = std::fs::File::from(fd.try_clone().map_err(io_error)?)
                .metadata()
                .map_err(io_error)?;
            use std::os::unix::fs::MetadataExt;
            if denied_ids.contains(&(meta.dev(), meta.ino())) {
                return Err(denied(&path));
            }
        }
        let file = std::fs::File::from(fd);
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(denied(&path));
        }
        // Refuse aliases whose other names may be outside the jail.
        use std::os::unix::fs::MetadataExt;
        if metadata.is_file() && metadata.nlink() > 1 {
            return Err(denied(&path));
        }
        Ok(file)
    }

    /// `path` with its folder resolved in the jail and the final component
    /// kept (not followed), after the read policy accepted the path (a
    /// denied path, or a link to one, is refused).
    fn unfollowed(&self, path: &Path) -> Result<PathBuf, FileError> {
        let resolved = self.policy.resolve(path, false)?;
        let absolute = loop_workspace::lexical(&self.policy.project.join(path));
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(name)) => Ok(self.policy.resolve(parent, false)?.join(name)),
            _ => Ok(resolved),
        }
    }

    fn info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let file = self.open(path, false, rustix::fs::OFlags::RDONLY, false)?;
        let meta = file.metadata().map_err(io_error)?;
        Ok(FileInfo {
            path: self.policy.project.join(path),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            size: meta.len(),
        })
    }

    fn write(&self, path: &Path, data: &[u8], append: bool) -> Result<(), FileError> {
        use rustix::fs::OFlags;
        use std::io::Write;
        // Don't truncate until after the regular-file and link-count checks.
        let mut file = self.open(
            path,
            true,
            OFlags::WRONLY
                | OFlags::CREATE
                | if append {
                    OFlags::APPEND
                } else {
                    OFlags::empty()
                },
            true,
        )?;
        if !append {
            file.set_len(0).map_err(io_error)?;
        }
        file.write_all(data).map_err(io_error)
    }
}

#[cfg(unix)]
#[async_trait]
impl FileSystem for JailedHostFs {
    fn cwd(&self) -> &Path {
        &self.policy.project
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.policy.resolve(path, false)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        String::from_utf8(self.read_binary_file(path).await?)
            .map_err(|e| FileError::new(FileErrorCode::Io, e.to_string()))
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        use std::io::Read;
        let mut bytes = vec![];
        self.open(path, false, rustix::fs::OFlags::RDONLY, false)?
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        Ok(bytes)
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.write(path, data, false)
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.write(path, data, true)
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.info(path)
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let file = self.open(
            path,
            false,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            false,
        )?;
        let dir = rustix::fs::Dir::read_from(&file).map_err(io_error)?;
        let mut entries = vec![];
        for entry in dir {
            let entry = entry.map_err(io_error)?;
            if entry.file_name() == c"." || entry.file_name() == c".." {
                continue;
            }
            use std::os::unix::ffi::OsStrExt;
            let child = self
                .policy
                .project
                .join(path)
                .join(std::ffi::OsStr::from_bytes(entry.file_name().to_bytes()));
            // Re-open every child under the policy. A denied entry is hidden.
            if let Ok(info) = self.info(&child) {
                entries.push(info);
            }
        }
        Ok(entries)
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        match self.info(path) {
            Ok(_) => Ok(true),
            Err(e) if e.code == FileErrorCode::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        // A sentinel final component lets open() create every directory relative
        // to pinned descriptors without creating or following the sentinel.
        let path = self.policy.resolve(path, true)?;
        let probe = path.join(format!(".loop-probe-{}", loop_ai::new_id()));
        match self.open(&probe, true, rustix::fs::OFlags::RDONLY, true) {
            Err(e) if e.code == FileErrorCode::NotFound => Ok(()),
            Ok(_) => Err(denied(&probe)),
            Err(e) => Err(e),
        }
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let path = self.policy.resolve(path, true)?;
        let parent = path.parent().ok_or_else(|| denied(&path))?;
        let fd = self.open(
            parent,
            true,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            false,
        )?;
        let info = self.info(&path)?;
        let flags = if info.is_dir {
            rustix::fs::AtFlags::REMOVEDIR
        } else {
            rustix::fs::AtFlags::empty()
        };
        rustix::fs::unlinkat(&fd, path.file_name().ok_or_else(|| denied(&path))?, flags)
            .map_err(io_error)
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.info(path)?;
        self.policy.resolve(path, false)
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        if prefix.contains(['/', '\\']) || prefix == ".." {
            return Err(denied(Path::new(prefix)));
        }
        let path = self
            .policy
            .project
            .join(format!("{prefix}-{}", loop_ai::new_id()));
        self.create_dir(&path).await?;
        Ok(path)
    }
    async fn file_identity(
        &self,
        path: &Path,
    ) -> Result<Option<crate::harness::types::FileIdentity>, FileError> {
        // `open` uses O_NOFOLLOW on every component: a symlink is refused.
        let file = match self.open(path, false, rustix::fs::OFlags::RDONLY, false) {
            Ok(file) => file,
            Err(e) if e.code == FileErrorCode::NotFound => return Err(e),
            Err(_) => return Ok(None),
        };
        let meta = file.metadata().map_err(io_error)?;
        Ok(meta
            .is_file()
            .then(|| crate::harness::types::FileIdentity::from_metadata(&meta)))
    }
    async fn symlink_metadata(
        &self,
        path: &Path,
    ) -> Result<crate::harness::types::PathMetadata, FileError> {
        let target = self.unfollowed(path)?;
        let meta = std::fs::symlink_metadata(&target).map_err(io_error)?;
        Ok(crate::harness::env::path_metadata(&meta))
    }
    async fn read_link(&self, path: &Path) -> Result<PathBuf, FileError> {
        let target = self.unfollowed(path)?;
        std::fs::read_link(&target).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidInput {
                FileError::new(
                    FileErrorCode::InvalidPath,
                    format!("not a link: {}", target.display()),
                )
            } else {
                io_error(e)
            }
        })
    }
    async fn resolve_for_policy(
        &self,
        path: &Path,
    ) -> Result<crate::harness::types::PolicyPath, FileError> {
        let resolved = self.policy.resolve(path, false)?;
        let absolute = loop_workspace::lexical(&self.policy.project.join(path));
        let mut has_link = false;
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            prefix.push(component.as_os_str());
            match std::fs::symlink_metadata(&prefix) {
                Ok(meta) if meta.file_type().is_symlink() => has_link = true,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(io_error(e)),
            }
        }
        match std::fs::metadata(&resolved) {
            Ok(meta) => {
                let facts = crate::harness::env::path_metadata(&meta);
                Ok(crate::harness::types::PolicyPath {
                    resolved,
                    exists: true,
                    has_link,
                    uid: facts.uid,
                    nlink: facts.nlink,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(crate::harness::types::PolicyPath {
                    resolved: absolute,
                    exists: false,
                    has_link,
                    uid: None,
                    nlink: None,
                })
            }
            Err(e) => Err(io_error(e)),
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl Shell for JailedHostFs {
    fn sandboxed(&self) -> bool {
        true
    }

    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let cwd = self
            .policy
            .resolve(
                options.cwd.as_deref().unwrap_or(&self.policy.project),
                false,
            )
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let mut env: HashMap<String, String> = if options.inherit_env {
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .filter(|(k, _)| safe_env_key(k))
                .collect()
        } else {
            HashMap::new()
        };
        if let Some(overrides) = &options.env {
            env.extend(
                overrides
                    .iter()
                    .filter(|(k, _)| safe_env_key(k))
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        env.retain(|name, _| !options.remove_env.contains(name));
        let (mut cmd, marker) = self.policy.command("/bin/sh", &["-c", command])?;
        cmd.current_dir(cwd).env_clear().envs(env);
        let marker_path = std::fs::canonicalize(marker.dir.path())
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let result = exec_command(cmd, options, true, Some(marker_path)).await;
        drop(marker);
        result
    }
}

/// Factory registered as the local sandbox on macOS.
pub struct SeatbeltSandboxFactory;

#[cfg(unix)]
#[async_trait]
impl SandboxFactory for SeatbeltSandboxFactory {
    fn kind(&self) -> &str {
        "local"
    }
    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let path = |key: &str| {
            config
                .options
                .get(key)
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .ok_or_else(|| SandboxError::Policy(format!("Seatbelt requires {key}")))
        };
        let home = path("homeDir")?;
        let agent = path("agentDir")?;
        let allow = config
            .options
            .get("allowWrite")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let runtime = config
            .options
            .get("runtimePaths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut policy = SeatbeltPolicy::new(
            &config.workdir,
            &home,
            &agent,
            &allow,
            config
                .options
                .get("network")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            &runtime,
        )
        .map_err(|e| SandboxError::Policy(e.to_string()))?;
        if let Some(protected) = policy
            .denied
            .iter()
            .chain(&policy.write_denied)
            .find(|path| policy.project.starts_with(path))
        {
            return Err(SandboxError::Policy(format!(
                "Cannot start the sandbox: project {} is inside protected folder {}. \
                 The sandbox blocks this folder to protect credentials and private agent data. \
                 Move the project outside this folder to use the sandbox",
                policy.project.display(),
                protected.display()
            )));
        }
        if config
            .options
            .get("denyHomeReads")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let allow_read: Vec<PathBuf> = config
                .options
                .get("allowRead")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default();
            policy
                .deny_home_reads(&home, &allow_read)
                .map_err(|e| SandboxError::Policy(e.to_string()))?;
        }
        clean_stale_temp_dirs();
        // Use a trusted parent, never the ambient TMPDIR. tempfile creates
        // this directory exclusively; verify its owner, mode and resolved path.
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .prefix("loop-seatbelt-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .map_err(|e| SandboxError::Policy(e.to_string()))?;
        let temp_path =
            std::fs::canonicalize(temp.path()).map_err(|e| SandboxError::Policy(e.to_string()))?;
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&temp_path)
            .map_err(|e| SandboxError::Policy(e.to_string()))?;
        // SAFETY: geteuid has no arguments or preconditions.
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || meta.uid() != unsafe { libc::geteuid() }
            || temp_path.to_str().is_none()
        {
            return Err(SandboxError::Policy(
                "Seatbelt requires a private, user-owned UTF-8 temporary directory".into(),
            ));
        }
        let temp_lock = hold_lock(&temp_path).map_err(|e| SandboxError::Policy(e.to_string()))?;
        let canary_read = temp_path.join("denied-read");
        let canary_write = temp_path.join("denied-write");
        std::fs::write(&canary_read, "Seatbelt preflight canary")
            .map_err(|e| SandboxError::Policy(e.to_string()))?;
        policy.denied.push(canary_read.clone());
        policy.write_denied.push(canary_write.clone());
        let fs = JailedHostFs::new(policy.clone());
        policy.writes.push(temp_path.clone());
        policy.temp_root = Some(temp_path.clone());
        Ok(Arc::new(SeatbeltSandbox {
            id: loop_ai::new_id(),
            env: Arc::new(SeatbeltEnv {
                fs,
                shell: JailedHostFs::new(policy),
                temp: Mutex::new(Some((temp, temp_lock))),
                temp_path,
                canary_read,
                canary_write,
                stopped: CancellationToken::new(),
                active: Mutex::new(0),
                cleanup_error: Mutex::new(None),
                idle: tokio::sync::Notify::new(),
            }),
            status: Mutex::new(SandboxStatus::Created),
        }))
    }
}

#[cfg(unix)]
struct SeatbeltEnv {
    fs: JailedHostFs,
    shell: JailedHostFs,
    temp: Mutex<Option<(tempfile::TempDir, std::fs::File)>>,
    temp_path: PathBuf,
    canary_read: PathBuf,
    canary_write: PathBuf,
    stopped: CancellationToken,
    active: Mutex<usize>,
    cleanup_error: Mutex<Option<String>>,
    idle: tokio::sync::Notify,
}

#[cfg(unix)]
struct Flight<'a>(&'a SeatbeltEnv);

#[cfg(unix)]
impl Drop for Flight<'_> {
    fn drop(&mut self) {
        *self.0.active.lock() -= 1;
        self.0.idle.notify_one();
    }
}

#[cfg(unix)]
struct SeatbeltSandbox {
    id: String,
    env: Arc<SeatbeltEnv>,
    status: Mutex<SandboxStatus>,
}

#[cfg(unix)]
#[async_trait]
impl Sandbox for SeatbeltSandbox {
    fn kind(&self) -> &str {
        "local"
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn status(&self) -> SandboxStatus {
        *self.status.lock()
    }
    fn info(&self) -> SandboxInfo {
        SandboxInfo::enabled(
            "local",
            vec![
                ("Status".into(), self.status().to_string()),
                ("Runtime".into(), "seatbelt".into()),
                ("Isolation".into(), "partial".into()),
                (
                    "Shell network".into(),
                    if self.env.shell.policy.network {
                        "on"
                    } else {
                        "off"
                    }
                    .into(),
                ),
                (
                    "Writable paths".into(),
                    self.env
                        .shell
                        .policy
                        .writes
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ],
        )
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }
    async fn start(&self) -> Result<(), SandboxError> {
        *self.status.lock() = SandboxStatus::Starting;
        let result = async {
            if !Path::new("/usr/bin/sandbox-exec").is_file() {
                return Err(SandboxError::StartFailed(
                    "/usr/bin/sandbox-exec is missing; Seatbelt is unavailable".into(),
                ));
            }
            let out = self
                .env
                .exec(
                    "/usr/bin/true",
                    ShellExecOptions {
                        timeout_ms: Some(5_000),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| SandboxError::StartFailed(e.to_string()))?;
            if out.exit_code != 0 {
                return Err(SandboxError::StartFailed(format!(
                    "Seatbelt preflight failed: {}",
                    out.stderr.trim()
                )));
            }
            let script = format!("if /bin/cat {} >/dev/null 2>&1; then exit 71; fi; if /bin/sh -c 'echo canary > \"$1\"' canary {} 2>/dev/null; then exit 72; fi", shell_quote(&self.env.canary_read), shell_quote(&self.env.canary_write));
            let canaries = self.env.exec(&script, ShellExecOptions { timeout_ms: Some(5_000), ..Default::default() }).await.map_err(|e| SandboxError::StartFailed(e.to_string()))?;
            if canaries.exit_code != 0 || self.env.canary_write.exists() {
                return Err(SandboxError::StartFailed("Seatbelt preflight failed: read/write denial canary did not hold".into()));
            }
            Ok(())
        }
        .await;
        *self.status.lock() = if result.is_ok() {
            SandboxStatus::Ready
        } else {
            SandboxStatus::Failed
        };
        result
    }
    async fn stop(&self) -> Result<(), SandboxError> {
        self.env.stopped.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let notified = self.env.idle.notified();
                if *self.env.active.lock() == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| SandboxError::Other("Seatbelt commands did not finish cleanup".into()))?;
        if let Some(error) = self.env.cleanup_error.lock().clone() {
            *self.status.lock() = SandboxStatus::Failed;
            return Err(SandboxError::Other(error));
        }
        *self.status.lock() = SandboxStatus::Stopped;
        Ok(())
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.stop().await?;
        if let Some((temp, lock)) = self.env.temp.lock().take() {
            temp.close()
                .map_err(|e| SandboxError::Other(e.to_string()))?;
            drop(lock);
        }
        Ok(())
    }
}

#[cfg(unix)]
#[async_trait]
impl FileSystem for SeatbeltEnv {
    fn cwd(&self) -> &Path {
        self.fs.cwd()
    }
    fn absolute_path(&self, p: &Path) -> Result<PathBuf, FileError> {
        self.fs.absolute_path(p)
    }
    fn join_path(&self, b: &Path, p: &Path) -> PathBuf {
        b.join(p)
    }
    async fn read_text_file(&self, p: &Path) -> Result<String, FileError> {
        self.fs.read_text_file(p).await
    }
    async fn read_binary_file(&self, p: &Path) -> Result<Vec<u8>, FileError> {
        self.fs.read_binary_file(p).await
    }
    async fn write_file(&self, p: &Path, d: &[u8]) -> Result<(), FileError> {
        self.fs.write_file(p, d).await
    }
    async fn append_file(&self, p: &Path, d: &[u8]) -> Result<(), FileError> {
        self.fs.append_file(p, d).await
    }
    async fn file_info(&self, p: &Path) -> Result<FileInfo, FileError> {
        self.fs.file_info(p).await
    }
    async fn list_dir(&self, p: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.fs.list_dir(p).await
    }
    async fn exists(&self, p: &Path) -> Result<bool, FileError> {
        self.fs.exists(p).await
    }
    async fn create_dir(&self, p: &Path) -> Result<(), FileError> {
        self.fs.create_dir(p).await
    }
    async fn remove(&self, p: &Path) -> Result<(), FileError> {
        self.fs.remove(p).await
    }
    async fn canonical_path(&self, p: &Path) -> Result<PathBuf, FileError> {
        self.fs.canonical_path(p).await
    }
    async fn create_temp_dir(&self, p: &str) -> Result<PathBuf, FileError> {
        self.fs.create_temp_dir(p).await
    }
    async fn file_identity(
        &self,
        p: &Path,
    ) -> Result<Option<crate::harness::types::FileIdentity>, FileError> {
        self.fs.file_identity(p).await
    }
    async fn symlink_metadata(
        &self,
        p: &Path,
    ) -> Result<crate::harness::types::PathMetadata, FileError> {
        self.fs.symlink_metadata(p).await
    }
    async fn read_link(&self, p: &Path) -> Result<PathBuf, FileError> {
        self.fs.read_link(p).await
    }
    async fn resolve_for_policy(
        &self,
        p: &Path,
    ) -> Result<crate::harness::types::PolicyPath, FileError> {
        self.fs.resolve_for_policy(p).await
    }
}

#[cfg(unix)]
#[async_trait]
impl Shell for SeatbeltEnv {
    fn sandboxed(&self) -> bool {
        true
    }

    async fn exec(&self, c: &str, mut o: ShellExecOptions) -> Result<ShellOutput, ExecutionError> {
        {
            let mut active = self.active.lock();
            if self.stopped.is_cancelled() {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Aborted,
                    "Seatbelt sandbox stopped",
                ));
            }
            *active += 1;
        }
        let _flight = Flight(self);
        o.env.get_or_insert_default().insert(
            "TMPDIR".into(),
            self.temp_path.to_string_lossy().into_owned(),
        );
        let caller_cancel = o.cancel.take().unwrap_or_default();
        let cancel = CancellationToken::new();
        o.cancel = Some(cancel.clone());
        let command = self.shell.exec(c, o);
        tokio::pin!(command);
        let result = tokio::select! {
            biased;
            _ = self.stopped.cancelled() => { cancel.cancel(); command.await }
            _ = caller_cancel.cancelled() => { cancel.cancel(); command.await }
            result = &mut command => result,
        };
        if self.stopped.is_cancelled() {
            if let Err(error) = &result {
                if error.code == ExecutionErrorCode::Io {
                    *self.cleanup_error.lock() = Some(error.to_string());
                }
            }
        }
        result
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paths the Seatbelt profile denies below the home directory. A
    /// change here changes what sandboxed commands can read, so it must be
    /// deliberate: the Edge listener's wider list is
    /// [`sensitive_home_paths_extended`].
    #[test]
    fn the_seatbelt_sensitive_list_is_pinned() {
        let home = Path::new("/h");
        let expected: Vec<PathBuf> = [
            ".git-credentials",
            ".config/git/credentials",
            ".cargo/credentials",
            ".cargo/credentials.toml",
            ".gem/credentials",
            ".terraform.d/credentials.tfrc.json",
            ".azure",
            ".config/hub",
            ".bash_history",
            ".zsh_history",
            ".zhistory",
            ".sh_history",
            ".history",
            ".local/share/fish/fish_history",
            ".python_history",
            ".node_repl_history",
            ".psql_history",
            ".mysql_history",
            ".sqlite_history",
            ".lesshst",
            ".netrc",
            ".docker/config.json",
            ".kube/config",
            ".npmrc",
            ".pypirc",
            ".config/gcloud",
            "Library/Cookies",
            ".codex",
            ".claude",
            ".claude.json",
            ".gemini",
            ".qwen",
            ".copilot",
            ".config/github-copilot",
            ".cursor",
            ".continue",
            ".aider.conf.yml",
            ".config/opencode",
            ".local/share/opencode",
            ".config/goose",
            ".config/amp",
            ".local/share/amp",
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gh",
            "Library/Keychains",
        ]
        .iter()
        .map(|p| home.join(p))
        .chain([PathBuf::from("/Library/Keychains")])
        .collect();
        assert_eq!(sensitive_home_paths(home), expected);
        let extended = sensitive_home_paths_extended(home);
        assert!(extended.starts_with(&expected));
        for wider in [".docker", ".kube", ".password-store", ".mozilla"] {
            assert!(!expected.contains(&home.join(wider)), "{wider}");
            assert!(extended.contains(&home.join(wider)), "{wider}");
        }
    }

    #[test]
    fn paths_are_parameters_not_profile_source() {
        let root = tempfile::tempdir().unwrap();
        let tricky = root.path().join("quote\" (allow default)");
        std::fs::create_dir(&tricky).unwrap();
        let policy =
            SeatbeltPolicy::new(&tricky, root.path(), root.path(), &[], false, &[]).unwrap();
        let (profile, params) = policy.profile();
        assert!(!profile.contains("quote"));
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(params
            .iter()
            .any(|p| p == &format!("WRITE_0={}", tricky.canonicalize().unwrap().display())));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn resolves_missing_children_but_not_symlink_escapes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let policy = SeatbeltPolicy::new(
            &project,
            root.path(),
            &root.path().join("agent"),
            &[],
            false,
            &[],
        )
        .unwrap();
        assert!(policy.resolve(&project.join("new/deep/file"), true).is_ok());
        assert!(policy.resolve(&root.path().join("outside"), true).is_err());
        assert!(policy.resolve(&project.join("../outside"), true).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), project.join("escape")).unwrap();
            assert!(policy
                .resolve(&project.join("escape/new/file"), true)
                .is_err());
        }
    }

    #[test]
    fn denied_paths_override_broad_write_grants() {
        let root = tempfile::tempdir().unwrap();
        let policy = SeatbeltPolicy::new(
            root.path(),
            root.path(),
            root.path(),
            &[root.path().to_path_buf()],
            true,
            &[],
        )
        .unwrap();
        for name in [
            ".ssh/key",
            ".aws/config",
            ".gnupg/key",
            ".config/gh/hosts.yml",
            "Library/Keychains/login.keychain-db",
            "auth.json",
            "run/runtime.sock",
        ] {
            assert!(
                policy.resolve(&root.path().join(name), false).is_err(),
                "{name}"
            );
            assert!(
                policy.resolve(&root.path().join(name), true).is_err(),
                "{name}"
            );
        }
    }

    /// The gate primitives answer in the jail: links and modes inside the
    /// project as the host sees them; a denied path (or a link to one) is
    /// refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn gate_primitives_answer_in_the_jail() {
        use crate::harness::env::HostExecutionEnv;
        use crate::harness::types::PathKind;
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let project = base.join("project");
        let home = base.join("home");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id"), "key").unwrap();
        std::fs::write(project.join("f.txt"), "x").unwrap();
        std::fs::hard_link(project.join("f.txt"), project.join("h.txt")).unwrap();
        std::os::unix::fs::symlink(project.join("f.txt"), project.join("l")).unwrap();
        std::os::unix::fs::symlink(home.join(".ssh/id"), project.join("key")).unwrap();
        let policy =
            SeatbeltPolicy::new(&project, &home, &base.join("agent"), &[], false, &[]).unwrap();
        let env = JailedHostFs::new(policy);
        let host = HostExecutionEnv::new(&project);

        assert_eq!(
            env.symlink_metadata(&project.join("l")).await.unwrap().kind,
            PathKind::Symlink
        );
        let file = env.symlink_metadata(&project.join("f.txt")).await.unwrap();
        assert_eq!(
            file,
            host.symlink_metadata(&project.join("f.txt")).await.unwrap()
        );
        assert!(file.mode.is_some());
        assert_eq!(
            env.read_link(&project.join("l")).await.unwrap(),
            project.join("f.txt")
        );
        assert_eq!(
            env.read_link(&project.join("f.txt"))
                .await
                .unwrap_err()
                .code,
            FileErrorCode::InvalidPath
        );
        for rel in ["l", "f.txt", "h.txt", "none", "sub/none", "sub"] {
            assert_eq!(
                env.resolve_for_policy(&project.join(rel)).await.unwrap(),
                host.resolve_for_policy(&project.join(rel)).await.unwrap(),
                "{rel}"
            );
        }
        // A credential path and a link to it are refused.
        for path in [home.join(".ssh/id"), project.join("key")] {
            assert!(env.symlink_metadata(&path).await.is_err(), "{path:?}");
            assert!(env.resolve_for_policy(&path).await.is_err(), "{path:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_symlink_replacement_cannot_redirect_writes() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            let inside = project.join("inside");
            let outside = root.path().join("outside");
            std::fs::create_dir_all(&inside).unwrap();
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("file"), "unchanged").unwrap();
            let policy = SeatbeltPolicy::new(
                &project,
                root.path(),
                &root.path().join("agent"),
                &[],
                false,
                &[],
            )
            .unwrap();
            let fs = JailedHostFs::new(policy);
            let alias = project.join("alias");
            let swap = project.join("swap");
            std::os::unix::fs::symlink(&inside, &alias).unwrap();
            let writer = std::thread::spawn(move || {
                for i in 0..2_000 {
                    let _ = std::fs::remove_file(&swap);
                    std::os::unix::fs::symlink(if i % 2 == 0 { &inside } else { &outside }, &swap)
                        .unwrap();
                    std::fs::rename(&swap, &alias).unwrap();
                }
            });
            for _ in 0..2_000 {
                let _ = fs.write_file(Path::new("alias/file"), b"changed").await;
                tokio::task::yield_now().await;
            }
            tokio::task::spawn_blocking(move || writer.join().unwrap())
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(root.path().join("outside/file")).unwrap(),
                "unchanged"
            );
        })
        .await
        .unwrap();
    }

    #[test]
    fn git_metadata_rule_covers_every_repository_but_not_ordinary_git_data() {
        for path in [
            "/p/.git/config",
            "/p/.git/config.worktree",
            "/p/.git/hooks",
            "/p/.git/hooks/pre-commit",
            "/p/.git/commondir",
            "/p/.git/info/attributes",
            "/p/.git/modules/a/config",
            "/p/.git/modules/a/modules/b/hooks/x",
            "/p/.git/worktrees/w/gitdir",
            "/p/vendor/lib/.git/config",
        ] {
            assert!(git_metadata(Path::new(path)), "{path}");
        }
        for path in [
            "/p/.git/objects/ab/cdef",
            "/p/.git/refs/heads/config",
            "/p/.git/logs/refs/heads/hooks",
            "/p/.git/index",
            "/p/.git/HEAD",
            "/p/.gitconfig",
            "/p/src/config",
            "/p/.git/modules/a/objects/x",
        ] {
            assert!(!git_metadata(Path::new(path)), "{path}");
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        assert!(!git_entry_write(&root.path().join(".git/objects/x")));
        assert!(git_entry_write(&root.path().join(".git")));
        assert!(git_entry_write(&root.path().join("new/.git/objects/x")));
    }

    #[test]
    fn child_environment_is_allowlisted() {
        for name in [
            "OPENAI_API_KEY",
            "LOOP_RUNTIME_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "GH_TOKEN",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CUSTOM_PASSWORD",
        ] {
            assert!(!safe_env_key(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR"] {
            assert!(safe_env_key(name), "{name}");
        }
    }
}
