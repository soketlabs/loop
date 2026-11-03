//! Async git runner for the `/diff` viewer: repo discovery, diff reads,
//! untracked-file listing and patch application.
//!
//! Every subprocess goes through `tokio::process` with `.kill_on_drop(true)`
//! and a timeout (CONTRIBUTING §subprocess handling); argv only, never a
//! shell string. Failures surface as [`GitDiffError`]; nothing here panics.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::diff_model::{
    parse_unified_diff, DiffLine, DiffLineKind, DiffModel, DiffSide, FileDiff, FileStatus, Hunk,
};

/// Failure of any [`GitDiff`] operation. The viewer renders these as a
/// banner; they are always safe to display (stderr is capped before storage).
#[derive(Debug)]
pub enum GitDiffError {
    /// The `git` executable is missing or cannot be spawned.
    NotFound(String),
    /// `git rev-parse --show-toplevel` failed: not inside a work tree.
    NotARepo,
    /// A git child process exceeded its time budget and was killed.
    Timeout,
    /// Non-zero exit; `stderr` carries git's (capped) message.
    Exit { code: i32, stderr: String },
    /// Spawn / pipe / filesystem failure.
    Io(std::io::Error),
    /// A state-changing git command exceeded its (generous) write budget and
    /// was killed. Killing a command that writes the index can leave
    /// `.git/index.lock` behind, blocking later git commands until it is
    /// removed.
    WriteTimeout,
}

impl fmt::Display for GitDiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "git executable not found: {msg}"),
            Self::NotARepo => write!(f, "not inside a git repository"),
            Self::Timeout => write!(f, "git command timed out"),
            Self::WriteTimeout => write!(
                f,
                "git command timed out; if .git/index.lock remains, check that no git command is running, then delete it"
            ),
            Self::Exit { code, stderr } => {
                let detail = stderr.trim_end();
                if detail.is_empty() {
                    write!(f, "git exited with status {code}")
                } else {
                    write!(f, "git exited with status {code}: {detail}")
                }
            }
            Self::Io(err) => write!(f, "git command failed: {err}"),
        }
    }
}

impl std::error::Error for GitDiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GitDiffError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Handle to one git repository. Every child process runs with
/// `current_dir(repo_root)` and inherits the environment plus `extra_env`.
#[derive(Debug, Clone)]
pub struct GitDiff {
    /// Repository root (`git rev-parse --show-toplevel`).
    pub repo_root: PathBuf,
    /// `(key, value)` pairs appended to every git child's env. Production
    /// leaves this empty; tests use it to isolate git config without the
    /// process-global (and racy) `std::env::set_var`.
    extra_env: Vec<(String, String)>,
}

impl GitDiff {
    /// Append key/value pairs to the environment of every future child.
    pub fn with_extra_env(&mut self, kv: impl IntoIterator<Item = (String, String)>) {
        self.extra_env.extend(kv);
    }

    /// Build a handle for an already-known repo root (used by tests and by
    /// callers that have their own root).
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            extra_env: Vec::new(),
        }
    }

    /// Spawn `git <args>`, feed `stdin` (if any), wait under a timeout, cap
    /// stdout at `stdout_cap` and stderr at 64 KiB. `kill_on_drop(true)`
    /// means a dropped child is killed, so a leaked future can never
    /// outlive us. Returns the capped stdout bytes plus `true` when the cap
    /// cut the output short.
    async fn run_git_capped(
        &self,
        args: &[&str],
        stdin: Option<&[u8]>,
        timeout: Duration,
        stdout_cap: usize,
    ) -> Result<(Vec<u8>, bool), GitDiffError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args)
            .current_dir(&self.repo_root)
            .envs(self.extra_env.iter().cloned())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                GitDiffError::NotFound(err.to_string())
            } else {
                GitDiffError::Io(err)
            }
        })?;

        // Take the pipes before awaiting anything; writing the patch and
        // reading both streams happen concurrently with the wait.
        let mut child_stdin = child.stdin.take();
        let mut child_stdout = child.stdout.take();
        let mut child_stderr = child.stderr.take();

        let write = async {
            // `take()` moves the handle out so `drop` closes the pipe: only
            // closing the fd gives git its EOF on stdin (`shutdown()` merely
            // flushes and leaves the pipe open, hanging `git apply -`).
            if let Some(mut sink) = child_stdin.take() {
                if let Some(data) = stdin {
                    // A short write or EPIPE (git rejected the patch early)
                    // is fine: the exit status below tells the real story.
                    use tokio::io::AsyncWriteExt;
                    let _ = sink.write_all(data).await;
                }
                drop(sink);
            }
        };
        let stdout = async {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let mut cut = false;
            if let Some(out) = child_stdout.as_mut() {
                // `take` caps the read at stdout_cap bytes.
                let mut capped = out.take(stdout_cap as u64);
                capped.read_to_end(&mut buf).await?;
                // Drain whatever exceeds the cap into a sink: an unread pipe
                // fills its OS buffer, git blocks on write and never exits.
                let drained =
                    tokio::io::copy(&mut capped.into_inner(), &mut tokio::io::sink()).await?;
                cut = drained > 0;
            }
            std::io::Result::Ok((buf, cut))
        };
        let stderr = async {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            if let Some(err_out) = child_stderr.as_mut() {
                let mut capped = err_out.take(MAX_GIT_STDERR as u64);
                capped.read_to_end(&mut buf).await?;
                // Same drain for stderr past its cap.
                tokio::io::copy(&mut capped.into_inner(), &mut tokio::io::sink()).await?;
            }
            std::io::Result::Ok(buf)
        };

        let joined = async { tokio::join!(write, stdout, stderr, child.wait()) };
        let (_, stdout_res, stderr_res, status_res) = with_timeout(joined, timeout).await?;

        let (stdout_bytes, stdout_cut) = stdout_res?;
        let stderr_bytes = stderr_res?;
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        let status = status_res?;

        match status.code() {
            Some(0) => Ok((stdout_bytes, stdout_cut)),
            Some(128) if stderr.contains("not a git repository") => Err(GitDiffError::NotARepo),
            Some(code) => Err(GitDiffError::Exit { code, stderr }),
            // Terminated by a signal; treat as a plain non-zero exit.
            None => Err(GitDiffError::Exit { code: -1, stderr }),
        }
    }

    /// [`run_git_capped`] with the production stdout cap; the truncation
    /// flag is dropped and the bytes are lossily converted to text, so
    /// existing callers keep their signature.
    async fn run_git(
        &self,
        args: &[&str],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<String, GitDiffError> {
        self.run_git_capped(args, stdin, timeout, MAX_GIT_STDOUT)
            .await
            .map(|(out, _)| String::from_utf8_lossy(&out).into_owned())
    }

    /// [`run_git`] with the production timeout and no stdin payload.
    pub(crate) async fn git(&self, args: &[&str]) -> Result<String, GitDiffError> {
        self.run_git(args, None, GIT_TIMEOUT).await
    }

    /// [`run_git`] for state-changing commands: a plain [`GitDiffError::Timeout`]
    /// is remapped to [`GitDiffError::WriteTimeout`], whose message names the
    /// `.git/index.lock` a killed writer may leave behind.
    async fn run_git_write(
        &self,
        args: &[&str],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<String, GitDiffError> {
        self.run_git(args, stdin, timeout)
            .await
            .map_err(as_write_error)
    }
}

/// Remap a read-path timeout into the write-path variant whose message
/// names the `.git/index.lock` a killed writer may leave behind. Any other
/// error passes through unchanged.
fn as_write_error(err: GitDiffError) -> GitDiffError {
    match err {
        GitDiffError::Timeout => GitDiffError::WriteTimeout,
        other => other,
    }
}

/// Cap on git stdout (diff output); §3.3.
const MAX_GIT_STDOUT: usize = 8 * 1024 * 1024;
/// Cap on git stderr before it goes into an error banner.
const MAX_GIT_STDERR: usize = 64 * 1024;
/// Default wall-clock budget per git child process.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for commands that change git state (`apply`, `add`, `rm`). Killing
/// a command while it holds the index lock leaves `.git/index.lock` behind,
/// which blocks every later git command until someone deletes it, so these
/// get a far more generous deadline before being force-killed.
const GIT_WRITE_TIMEOUT: Duration = Duration::from_secs(300);

/// Untracked Added [`FileDiff`] with one hunk holding `lines`. The hunk id
/// is set via [`crate::diff_model::hunk_id`], so a changed link target (or
/// file content) is detected as a stale hunk. An empty `lines` still yields
/// one hunk of zero lines so Accept/Reject stay usable.
fn untracked_added_file(path: &str, lines: Vec<DiffLine>) -> FileDiff {
    let mut file = FileDiff {
        old_path: None,
        path: path.to_string(),
        status: FileStatus::Added,
        old_mode: None,
        new_mode: None,
        untracked: true,
        hunks: Vec::new(),
    };
    let count = lines.len() as u32;
    let start = if count == 0 { 0 } else { 1 };
    let mut hunk = Hunk {
        old_start: 0,
        old_count: 0,
        new_start: start,
        new_count: count,
        section: None,
        lines,
        raw_body: Vec::new(),
        id: crate::diff_model::HunkId(0),
    };
    hunk.id = crate::diff_model::hunk_id(&file, &hunk);
    file.hunks.push(hunk);
    file
}

impl GitDiff {
    /// Find the repository containing `cwd` via
    /// `git rev-parse --show-toplevel`.
    pub async fn discover(cwd: &Path) -> Result<Self, GitDiffError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["rev-parse", "--show-toplevel"])
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                GitDiffError::NotFound(err.to_string())
            } else {
                GitDiffError::Io(err)
            }
        })?;

        // discover is called from arbitrary cwd's; a missing dir must map to
        // NotARepo rather than an Io error.
        let mut out = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            use tokio::io::AsyncReadExt;
            let mut capped = (&mut pipe).take(MAX_GIT_STDOUT as u64);
            let mut buf = Vec::new();
            let _ = capped.read_to_end(&mut buf).await;
            out = String::from_utf8_lossy(&buf).into_owned();
        }

        let status = tokio::time::timeout(GIT_TIMEOUT, child.wait())
            .await
            .map_err(|_| GitDiffError::Timeout)??;
        match status.code() {
            Some(0) => {}
            _ => return Err(GitDiffError::NotARepo),
        }

        let root = out.trim();
        if root.is_empty() {
            return Err(GitDiffError::NotARepo);
        }
        Ok(Self::new(PathBuf::from(root)))
    }

    /// Read the diff for `side` (`git diff` or `git diff --cached`) and parse
    /// it into a [`DiffModel`]. Flags mirror §3.3 exactly: they defeat user
    /// diff drivers, prefixes, colors and quoting so the parser sees stable
    /// input, and `--no-optional-locks` keeps read paths lock-free.
    pub async fn read(&self, side: DiffSide) -> Result<DiffModel, GitDiffError> {
        self.read_capped(side, MAX_GIT_STDOUT).await
    }

    /// [`read`] with an explicit stdout cap (tests use a small one to
    /// exercise truncation). When git's output is cut at the cap, the last
    /// parsed file may end mid-hunk, so it is dropped and `truncated` set.
    async fn read_capped(
        &self,
        side: DiffSide,
        stdout_cap: usize,
    ) -> Result<DiffModel, GitDiffError> {
        let mut args: Vec<&str> = vec![
            "--no-optional-locks",
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ];
        if side == DiffSide::Index {
            args.push("--cached");
        }
        let (raw_bytes, stdout_cut) = self
            .run_git_capped(&args, None, GIT_TIMEOUT, stdout_cap)
            .await?;
        // The parser takes the raw bytes: `\r` line endings and non-UTF-8
        // content must survive into the hunk bodies used for patches.
        let mut model = parse_unified_diff(raw_bytes, side);
        if stdout_cut {
            // The tail of the raw diff was discarded, so the final entry can
            // be a half-parsed hunk; drop it rather than show garbage.
            model.files.pop();
            model.truncated = true;
        }
        if side == DiffSide::WorkingTree {
            // Untracked hunk lines count toward MAX_TOTAL_LINES exactly like
            // parsed ones, so a wall of new files cannot bypass the cap.
            let mut total_lines: usize = model
                .files
                .iter()
                .map(|f| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>())
                .sum();
            for path in self.untracked_files().await? {
                if model.files.len() >= crate::diff_model::MAX_FILES {
                    model.truncated = true;
                    break;
                }
                if let Some(file) = self.synthesize_untracked(&path).await {
                    let file_lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
                    if total_lines + file_lines > crate::diff_model::MAX_TOTAL_LINES {
                        model.truncated = true;
                        break;
                    }
                    total_lines += file_lines;
                    model.files.push(file);
                }
            }
        }
        Ok(model)
    }

    /// List untracked, non-ignored files (`ls-files --others
    /// --exclude-standard -z`). Paths are repo-root-relative and never need
    /// unquoting: `-z` uses NUL separators with no C-style escaping.
    pub async fn untracked_files(&self) -> Result<Vec<String>, GitDiffError> {
        let raw = self
            .git(&[
                "--no-optional-locks",
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .await?;
        Ok(raw
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Build the synthetic all-added [`FileDiff`] for one untracked file.
    ///
    /// The entry is lstat'ed first (never followed): a symlink becomes an
    /// Added entry whose single line is the link target, without ever
    /// opening the link — following it could read the target's contents
    /// (a link to /dev/zero would never end) or expose files outside the
    /// repo. Regular files over [`MAX_UNTRACKED_BYTES`] on disk (or more
    /// than that many bytes once opened and read) become a
    /// [`FileStatus::TooLarge`] entry with no hunks, so the viewer can show
    /// and act on them without loading megabytes into memory. Files within
    /// the byte cap are read whole; a NUL byte in the first 8 KiB marks
    /// them binary (no hunk lines). More than [`MAX_UNTRACKED_LINES`] real
    /// lines also yields [`FileStatus::TooLarge`]. Entries that vanish
    /// between listing and reading are skipped (`None`); other read errors
    /// still list the file as an empty added file so the viewer can act on
    /// it. An empty file yields an Added entry with one hunk of zero lines
    /// (never a phantom "+" row), keeping Accept and Reject usable.
    async fn synthesize_untracked(&self, path: &str) -> Option<FileDiff> {
        const SNIFF_LEN: usize = 8 * 1024;
        let full = self.repo_root.join(path);

        let too_large = || {
            Some(FileDiff {
                old_path: None,
                path: path.to_string(),
                status: FileStatus::TooLarge,
                old_mode: None,
                new_mode: None,
                untracked: true,
                hunks: Vec::new(),
            })
        };

        // Lstat before opening: the link itself must never be followed.
        match tokio::fs::symlink_metadata(&full).await {
            Ok(meta) if meta.is_symlink() => {
                // Show the link target as its single content line instead
                // of reading through the link. A vanished link is skipped
                // like any other vanished file; any other read_link failure
                // falls back to listing an empty added file.
                return match tokio::fs::read_link(&full).await {
                    Ok(target) => Some(untracked_added_file(
                        path,
                        vec![DiffLine {
                            kind: DiffLineKind::Added,
                            text: target.to_string_lossy().into_owned(),
                            no_newline: true,
                            old_no: None,
                            new_no: Some(1),
                        }],
                    )),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    // Any other read_link failure falls back to listing an
                    // empty added file.
                    Err(_) => Some(untracked_added_file(path, Vec::new())),
                };
            }
            Ok(meta) if meta.len() > MAX_UNTRACKED_BYTES as u64 => return too_large(),
            // Vanished between listing and reading: skip it entirely.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            // Unreadable (permissions): still list it as an empty added
            // file so the viewer can show and act on it.
            _ => {}
        }

        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        let oversized = match tokio::fs::File::open(&full).await {
            Ok(mut file) => {
                // One extra byte past the cap is enough to tell "over" from
                // "exactly at" without ever buffering the whole file.
                let mut capped = (&mut file).take(MAX_UNTRACKED_BYTES as u64 + 1);
                // On error, clear what was read: an unreadable file lists as
                // empty and never shows partial content.
                let res = capped.read_to_end(&mut bytes).await;
                if res.is_err() {
                    bytes.clear();
                }
                res.is_ok() && bytes.len() > MAX_UNTRACKED_BYTES
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            // Same fallback as metadata: an unreadable file lists as empty.
            Err(_) => false,
        };
        if oversized {
            return too_large();
        }

        let sniff_end = bytes.len().min(SNIFF_LEN);
        let binary = bytes[..sniff_end].contains(&0u8);

        let mut lines: Vec<DiffLine> = Vec::new();
        if !binary && !bytes.is_empty() {
            // Count real lines: split on '\n' and ignore the final empty
            // part only when the content ends with '\n'. A file of exactly
            // MAX_UNTRACKED_LINES lines keeps every one of them.
            let text = String::from_utf8_lossy(&bytes);
            let mut parts = text.split('\n');
            if bytes.ends_with(b"\n") {
                parts.next_back();
            }
            for line in parts {
                if lines.len() >= MAX_UNTRACKED_LINES {
                    return too_large();
                }
                // Windows line endings: split('\n') leaves a trailing '\r'
                // on each line; display text drops exactly one so the raw
                // bytes stay recoverable from the file itself.
                let text = line.strip_suffix('\r').unwrap_or(line);
                lines.push(DiffLine {
                    kind: DiffLineKind::Added,
                    text: text.to_string(),
                    no_newline: false,
                    old_no: None,
                    new_no: Some(lines.len() as u32 + 1),
                });
            }
            // A file without a trailing newline keeps its last real line
            // flagged so patches regenerate it byte-exactly.
            if !bytes.ends_with(b"\n") {
                if let Some(last) = lines.last_mut() {
                    last.no_newline = true;
                }
            }
        }

        // Even an empty file gets one hunk with zero lines, so Accept
        // (git add) and Reject (delete) still have something to act on.
        // A binary file gets no hunks at all.
        let file = if binary {
            FileDiff {
                old_path: None,
                path: path.to_string(),
                status: FileStatus::Binary,
                old_mode: None,
                new_mode: None,
                untracked: true,
                hunks: Vec::new(),
            }
        } else {
            untracked_added_file(path, lines)
        };
        Some(file)
    }

    /// Dry-run `git apply` (adds `--check`); `Ok(())` means the patch would
    /// apply cleanly. Nothing is modified.
    pub async fn check_apply(
        &self,
        patch: &[u8],
        cached: bool,
        reverse: bool,
    ) -> Result<(), GitDiffError> {
        // `--whitespace=nowarn` overrides any user `apply.whitespace`
        // setting: `fix` would silently strip trailing spaces from staged
        // lines and `error` would reject the patch outright.
        let mut args: Vec<&str> = vec!["apply", "--check", "--whitespace=nowarn"];
        if cached {
            args.push("--cached");
        }
        if reverse {
            args.push("-R");
        }
        args.push("-");
        self.run_git(&args, Some(patch), GIT_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// Apply `patch` via stdin. `cached` targets the index, `reverse`
    /// inverts it (discard from the worktree / unstage from the index).
    pub async fn apply(
        &self,
        patch: &[u8],
        cached: bool,
        reverse: bool,
    ) -> Result<(), GitDiffError> {
        // Same override as [`check_apply`]: the user's `apply.whitespace`
        // must not rewrite or reject what we stage.
        let mut args: Vec<&str> = vec!["apply", "--whitespace=nowarn"];
        if cached {
            args.push("--cached");
        }
        if reverse {
            args.push("-R");
        }
        args.push("-");
        self.run_git_write(&args, Some(patch), GIT_WRITE_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// Stage one file (`git --literal-pathspecs add -- <path>`). The path
    /// comes from git's own output (repo-root-relative), so it is passed
    /// through verbatim; `--literal-pathspecs` stops glob metacharacters
    /// (`*`, `[...]`) in file names from matching other files.
    pub async fn add_file(&self, path: &str) -> Result<(), GitDiffError> {
        self.run_git_write(
            &["--literal-pathspecs", "add", "--", path],
            None,
            GIT_WRITE_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    /// Delete a file from the worktree and the index. Rejects paths that
    /// escape the repo root or name a directory.
    pub async fn delete_file(&self, path: &str) -> Result<(), GitDiffError> {
        let target = self.repo_root.join(path);
        // `..` components (or an absolute `path`) would let a crafted diff
        // delete outside the repository.
        if !target.starts_with(&self.repo_root)
            || Path::new(path).components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Err(GitDiffError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("refusing to delete outside the repository: {path}"),
            )));
        }
        let meta = tokio::fs::symlink_metadata(&target).await?;
        if meta.is_dir() {
            return Err(GitDiffError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("refusing to delete a directory: {path}"),
            )));
        }
        // Index first, worktree second: if `git rm --cached` fails (e.g. a
        // stale .git/index.lock), the file is still on disk so the caller's
        // backup path stays meaningful and nothing is lost.
        self.run_git_write(
            &[
                "--literal-pathspecs",
                "rm",
                "--cached",
                "--ignore-unmatch",
                "--",
                path,
            ],
            None,
            GIT_WRITE_TIMEOUT,
        )
        .await?;
        tokio::fs::remove_file(&target).await?;
        Ok(())
    }
}

/// Cap on one untracked file's synthesized content (§3.3).
const MAX_UNTRACKED_BYTES: usize = 1024 * 1024;
/// Cap on lines synthesized per untracked file.
const MAX_UNTRACKED_LINES: usize = 10_000;

/// Race `fut` against a deadline; on elapse the caller's child (owned by
/// `fut`'s future, `kill_on_drop(true)`) is dropped and killed. A separate
/// function so tests can exercise the timeout path with any future.
async fn with_timeout<T>(
    fut: impl std::future::Future<Output = T>,
    budget: Duration,
) -> Result<T, GitDiffError> {
    tokio::time::timeout(budget, fut)
        .await
        .map_err(|_| GitDiffError::Timeout)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::diff_model::{build_single_hunk_patch, DiffLineKind};
    use std::path::Path;

    /// Per-OS null device used to neutralize git's global config files.
    pub(crate) fn null_device() -> &'static str {
        if cfg!(windows) {
            "NUL"
        } else {
            "/dev/null"
        }
    }

    /// True when a usable `git` is on PATH (tests skip, not fail, without it).
    pub(crate) fn have_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// `git init` a fresh repo in `dir` with repo-local identity config only
    /// (§5.2): no global/system config is written or read.
    pub(crate) async fn init_repo(dir: &Path) -> GitDiff {
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .expect("invariant: git spawns");
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // Repo-local config only; per-child env isolates the developer's
        // real global config (never `std::env::set_var`: process-global and
        // racy across parallel tests).
        let mut gd = GitDiff::new(dir.to_path_buf());
        gd.with_extra_env([
            ("HOME".to_string(), dir.to_string_lossy().into_owned()),
            ("GIT_CONFIG_GLOBAL".to_string(), null_device().to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ]);
        gd
    }

    /// Fresh temp repo + configured [`GitDiff`]; skips the test when git is
    /// missing. Returns `(guard, handle)`; keep `guard` alive for the test.
    pub(crate) async fn temp_repo() -> Option<(tempfile::TempDir, GitDiff)> {
        if !have_git() {
            return None;
        }
        let dir = tempfile::tempdir().expect("invariant: tempdir creates");
        let gd = init_repo(dir.path()).await;
        Some((dir, gd))
    }

    /// Write `content` to `repo/<rel>` (parent dirs created).
    pub(crate) async fn write_file(repo: &Path, rel: &str, content: &str) {
        let full = repo.join(rel);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .expect("invariant: mkdir");
        }
        tokio::fs::write(&full, content)
            .await
            .expect("invariant: write");
    }

    /// `git add -A && git commit` so later diffs have a HEAD to diff against.
    pub(crate) async fn commit_all(gd: &GitDiff, msg: &str) {
        gd.git(&["add", "-A"]).await.expect("invariant: add");
        gd.git(&[
            "commit",
            "-m",
            msg,
            "--allow-empty",
            "--no-gpg-sign",
            "--no-verify",
        ])
        .await
        .expect("invariant: commit");
    }

    #[tokio::test]
    async fn discover_finds_root_from_nested_dir() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "a/b/c.txt", "x\n").await;
        commit_all(&gd, "init").await;
        let nested = gd.repo_root.join("a/b");
        let found = GitDiff::discover(&nested).await.expect("discover works");
        assert_eq!(found.repo_root, gd.repo_root);
    }

    #[tokio::test]
    async fn discover_outside_repo_is_not_a_repo() {
        if !have_git() {
            eprintln!("skipped: git not found");
            return;
        }
        // The system temp root is never inside a repository.
        match GitDiff::discover(&std::env::temp_dir()).await {
            Err(GitDiffError::NotARepo) => {}
            other => panic!("expected NotARepo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_worktree_returns_tracked_edit_hunk() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "src/app.rs", "one\ntwo\nthree\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "src/app.rs", "one\nTWO\nthree\n").await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert!(!model.truncated);
        assert_eq!(model.files.len(), 1);
        let f = &model.files[0];
        assert_eq!(f.path, "src/app.rs");
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(h.old_start, 1);
        // unified=3 on a 3-line file: context "one", -two, +TWO, context "three".
        assert_eq!(h.lines.len(), 4);
        assert_eq!(h.lines[1].kind, DiffLineKind::Removed);
        assert_eq!(h.lines[1].text, "two");
        assert_eq!(h.lines[2].kind, DiffLineKind::Added);
        assert_eq!(h.lines[2].text, "TWO");
        assert_eq!(h.lines[3].kind, DiffLineKind::Context);
        assert_eq!(h.lines[3].text, "three");
    }

    #[tokio::test]
    async fn read_worktree_includes_untracked_as_all_added() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "tracked.txt", "keep\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "brand new.txt", "hello\nworld\n").await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        let untracked: Vec<_> = model.files.iter().filter(|f| f.untracked).collect();
        assert_eq!(untracked.len(), 1, "{:?}", model.files);
        let f = untracked[0];
        assert_eq!(f.path, "brand new.txt");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.old_path, None);
        assert_eq!(f.hunks.len(), 1);
        assert!(f.hunks[0]
            .lines
            .iter()
            .all(|l| l.kind == DiffLineKind::Added));
        let texts: Vec<&str> = f.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["hello", "world"]);
        assert!(!f.hunks[0].lines[0].no_newline);
    }

    #[tokio::test]
    async fn read_index_reflects_staged_changes_only() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "a.txt", "v1\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "a.txt", "v2\n").await; // unstaged
        write_file(&gd.repo_root, "b.txt", "new\n").await;
        gd.add_file("b.txt").await.expect("add b");

        let staged = gd.read(DiffSide::Index).await.expect("read index");
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].path, "b.txt");
        assert_eq!(staged.files[0].status, FileStatus::Added);

        // Worktree side: `git diff` shows only the unstaged edit of a.txt.
        // b.txt is fully staged and tracked, so it has neither unstaged hunks
        // nor an untracked entry — exactly what the viewer must show.
        let worktree = gd.read(DiffSide::WorkingTree).await.expect("read worktree");
        let paths: Vec<&str> = worktree
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["a.txt"]);
    }

    #[tokio::test]
    async fn untracked_files_lists_others_excluding_ignored() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "tracked.txt", "x\n").await;
        // A committed ignore rule must keep matching files out of the list.
        write_file(&gd.repo_root, ".gitignore", "*.log\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "plain.txt", "new\n").await;
        write_file(&gd.repo_root, "nested/dir/deep.txt", "new\n").await;
        write_file(&gd.repo_root, "build.log", "noise\n").await;

        let others = gd.untracked_files().await.expect("untracked");
        assert_eq!(others.len(), 2, "{others:?}");
        assert!(others.contains(&"plain.txt".to_string()), "{others:?}");
        assert!(
            others.contains(&"nested/dir/deep.txt".to_string()),
            "{others:?}"
        );
        assert!(!others.contains(&"build.log".to_string()), "{others:?}");
        assert!(!others.contains(&"tracked.txt".to_string()), "{others:?}");
    }

    #[tokio::test]
    async fn untracked_over_byte_cap_is_too_large() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // One byte past the cap: the stat alone flags it, so the file is
        // never opened or buffered.
        let big = vec![b'a'; MAX_UNTRACKED_BYTES + 1];
        tokio::fs::write(gd.repo_root.join("big.dat"), &big)
            .await
            .expect("invariant: write");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert!(!model.truncated);
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "big.dat");
        assert_eq!(f.status, FileStatus::TooLarge);
        assert!(f.untracked);
        assert!(f.hunks.is_empty());
    }

    #[tokio::test]
    async fn untracked_over_line_cap_is_too_large() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // Well under the byte cap, but one line past the line cap.
        let mut body = String::with_capacity((MAX_UNTRACKED_LINES + 1) * 6);
        for i in 0..=MAX_UNTRACKED_LINES {
            body.push_str(&format!("{i:05}\n"));
        }
        write_file(&gd.repo_root, "many.txt", &body).await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "many.txt");
        assert_eq!(f.status, FileStatus::TooLarge);
        assert!(f.hunks.is_empty());
    }

    #[tokio::test]
    async fn untracked_at_line_cap_keeps_every_line() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // Exactly at the line cap: every line survives, including the last.
        let mut body = String::with_capacity(MAX_UNTRACKED_LINES * 7);
        for i in 1..=MAX_UNTRACKED_LINES {
            body.push_str(&format!("line-{i:05}\n"));
        }
        write_file(&gd.repo_root, "edge.txt", &body).await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(h.lines.len(), MAX_UNTRACKED_LINES);
        let last = h.lines.last().expect("last line kept");
        assert_eq!(last.text, format!("line-{:05}", MAX_UNTRACKED_LINES));
        assert_eq!(last.new_no, Some(MAX_UNTRACKED_LINES as u32));
        assert!(!last.no_newline);
    }

    #[tokio::test]
    async fn empty_untracked_file_has_no_lines() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // Zero bytes: Added with one hunk of zero lines — never a phantom
        // empty "+" line — so Accept (add) and Reject (delete) still work.
        write_file(&gd.repo_root, "empty.txt", "").await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "empty.txt");
        assert_eq!(f.status, FileStatus::Added);
        assert!(f.untracked);
        assert_eq!(f.hunks.len(), 1);
        assert!(f.hunks[0].lines.is_empty());
    }

    #[tokio::test]
    async fn untracked_lines_count_toward_total_cap() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // 26 files x 10,000 lines: 25 fill MAX_TOTAL_LINES (250,000)
        // exactly; the 26th trips the cap and must not be pushed.
        let body: String = (1..=MAX_UNTRACKED_LINES)
            .map(|i| format!("l{i:05}\n"))
            .collect();
        for i in 0..26 {
            write_file(&gd.repo_root, &format!("u{i:02}.txt"), &body).await;
        }

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert!(model.truncated);
        assert!(
            model.files.len() < 26,
            "expected fewer than 26 files, got {}",
            model.files.len()
        );
        let total: usize = model
            .files
            .iter()
            .flat_map(|f| f.hunks.iter().map(|h| h.lines.len()))
            .sum();
        assert!(total <= crate::diff_model::MAX_TOTAL_LINES, "total {total}");
    }

    /// Repo with two separate hunks in one tracked file, dirty worktree.
    /// Returns `(tempdir guard, handle, model, patch for hunk 1)`.
    async fn two_hunk_dirty_repo(
        path: &str,
    ) -> Option<(
        tempfile::TempDir,
        GitDiff,
        crate::diff_model::DiffModel,
        Vec<u8>,
    )> {
        let (dir, gd) = temp_repo().await?;
        let body = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\n";
        write_file(&gd.repo_root, path, body).await;
        commit_all(&gd, "init").await;
        let edited = body.replace("l2\n", "H2\n").replace("l11\n", "H11\n");
        write_file(&gd.repo_root, path, &edited).await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].hunks.len(), 2, "precondition: two hunks");
        let patch = build_single_hunk_patch(&model.files[0], 1).expect("patch builds");
        Some((dir, gd, model, patch))
    }

    /// `git diff --cached --numstat` counts as `(added, deleted, path)`.
    async fn cached_numstat(gd: &GitDiff) -> Vec<String> {
        let out = gd
            .git(&["diff", "--cached", "--numstat"])
            .await
            .expect("invariant: numstat");
        out.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    #[tokio::test]
    async fn apply_cached_stages_exactly_one_hunk_leaving_other_unstaged() {
        let Some((_dir, gd, _model, patch)) = two_hunk_dirty_repo("root.txt").await else {
            eprintln!("skipped: git not found");
            return;
        };
        gd.check_apply(&patch, true, false).await.expect("check");
        gd.apply(&patch, true, false).await.expect("apply");

        // Hunk 2 changed one line: exactly 1 add + 1 del staged.
        assert_eq!(cached_numstat(&gd).await, ["1\t1\troot.txt"]);

        // The other hunk is still unstaged.
        let after = gd.read(DiffSide::WorkingTree).await.expect("re-read");
        assert_eq!(after.files.len(), 1);
        assert_eq!(after.files[0].hunks.len(), 1);
        assert_eq!(after.files[0].hunks[0].lines[1].text, "l2");
    }

    #[tokio::test]
    async fn apply_reverse_restores_discarded_lines_in_worktree() {
        let Some((_dir, gd, _model, patch)) = two_hunk_dirty_repo("root.txt").await else {
            eprintln!("skipped: git not found");
            return;
        };
        gd.apply(&patch, false, true).await.expect("reverse apply");
        let content = tokio::fs::read_to_string(gd.repo_root.join("root.txt"))
            .await
            .expect("invariant: read");
        // Hunk 2's edit is gone, hunk 1's remains.
        assert!(content.contains("l11\n"), "{content}");
        assert!(content.contains("H2\n"), "{content}");
        assert!(!content.contains("H11\n"), "{content}");

        let after = gd.read(DiffSide::WorkingTree).await.expect("re-read");
        assert_eq!(after.files.len(), 1);
        assert_eq!(after.files[0].hunks.len(), 1);
    }

    #[tokio::test]
    async fn apply_cached_reverse_unstages_one_hunk() {
        let Some((_dir, gd, _model, patch)) = two_hunk_dirty_repo("root.txt").await else {
            eprintln!("skipped: git not found");
            return;
        };
        // Stage everything first, then unstage exactly hunk 2.
        gd.git(&["add", "-A"]).await.expect("invariant: add");
        gd.check_apply(&patch, true, true).await.expect("check");
        gd.apply(&patch, true, true).await.expect("unstage");

        // Only hunk 1 remains staged (1/1); hunk 2 is back in the worktree.
        assert_eq!(cached_numstat(&gd).await, ["1\t1\troot.txt"]);
        let staged = gd.read(DiffSide::Index).await.expect("staged read");
        let hunk = &staged.files[0].hunks[0];
        assert!(hunk.lines.iter().any(|l| l.text == "H2"));
        assert!(!hunk.lines.iter().any(|l| l.text == "H11"));

        let worktree = gd.read(DiffSide::WorkingTree).await.expect("worktree read");
        assert!(worktree.files[0]
            .hunks
            .iter()
            .any(|h| h.lines.iter().any(|l| l.text == "H11")));
    }

    #[tokio::test]
    async fn check_apply_failure_modifies_nothing() {
        let Some((_dir, gd, _model, patch)) = two_hunk_dirty_repo("root.txt").await else {
            eprintln!("skipped: git not found");
            return;
        };
        // Forward-applying an unstaged hunk onto a dirty worktree must fail:
        // the worktree already holds the post-image.
        let err = gd
            .check_apply(&patch, false, false)
            .await
            .expect_err("must refuse");
        match err {
            GitDiffError::Exit { code, stderr } => {
                assert_ne!(code, 0);
                assert!(!stderr.is_empty());
            }
            other => panic!("expected Exit, got {other:?}"),
        }
        // Nothing changed: both hunks still unstaged, index empty.
        assert!(cached_numstat(&gd).await.is_empty());
        let after = gd.read(DiffSide::WorkingTree).await.expect("re-read");
        assert_eq!(after.files[0].hunks.len(), 2);
    }

    #[tokio::test]
    async fn single_hunk_patches_apply_for_tricky_paths() {
        // Root-level file, nested file, file with a space, staged new file.
        for (path, staged_new) in [
            ("plain.txt", false),
            ("nested/dir/file.txt", false),
            ("with space.txt", false),
            ("brand-new.txt", true),
        ] {
            let Some((_dir, gd, model, patch)) = two_hunk_dirty_repo(path).await else {
                eprintln!("skipped: git not found");
                return;
            };
            if staged_new {
                // Replace the scenario: stage the whole (already committed)
                // file's edit, then build the patch from the *index* side so
                // it applies to the index like a staged new file's hunk.
                gd.git(&["add", "-A"]).await.expect("invariant: add");
                let staged = gd.read(DiffSide::Index).await.expect("staged read");
                assert_eq!(staged.files.len(), 1, "path {path}");
                let p = build_single_hunk_patch(&staged.files[0], 0).expect("patch builds");
                // Unstage it again so the worktree is dirty as in the viewer.
                gd.apply(&p, true, true).await.expect("unstage for setup");
                gd.check_apply(&p, true, false).await.expect("check cached");
                gd.check_apply(&p, false, true)
                    .await
                    .expect("check reverse");
                continue;
            }
            assert_eq!(model.files[0].path, path);
            gd.check_apply(&patch, true, false)
                .await
                .unwrap_or_else(|e| panic!("check cached failed for {path}: {e}"));
            gd.apply(&patch, true, false)
                .await
                .unwrap_or_else(|e| panic!("apply cached failed for {path}: {e}"));
            assert_eq!(
                cached_numstat(&gd).await,
                [format!("1\t1\t{path}")],
                "path {path}"
            );
        }
    }

    #[tokio::test]
    async fn hunk_action_works_on_path_with_space_b_slash() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        // The name itself contains " b/", which used to cut the
        // `diff --git` header at the first " b/" and corrupt the path.
        let path = "plan b/notes.txt";
        write_file(&gd.repo_root, path, "a\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, path, "b\n").await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, path);
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch builds");
        gd.check_apply(&patch, true, false).await.expect("check");
        gd.apply(&patch, true, false).await.expect("apply");

        let staged = gd
            .git(&["diff", "--cached", "--name-only"])
            .await
            .expect("name-only");
        let paths: Vec<&str> = staged
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(paths, [path], "{paths:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hunk_action_works_on_path_with_tab() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        // A tab makes git quote both names in the header; the second one
        // must be unquoted too, or the patch targets the wrong name.
        let path = "a\tb.txt";
        write_file(&gd.repo_root, path, "a\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, path, "b\n").await;

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, path);
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch builds");
        gd.check_apply(&patch, true, false).await.expect("check");
        gd.apply(&patch, true, false).await.expect("apply");

        // -z: NUL separators, so the tab in the name is not C-quoted.
        let staged = gd
            .git(&["diff", "--cached", "--name-only", "-z"])
            .await
            .expect("name-only -z");
        let paths: Vec<&str> = staged.split('\0').filter(|p| !p.is_empty()).collect();
        assert_eq!(paths, [path], "{paths:?}");
    }

    #[tokio::test]
    async fn add_file_makes_untracked_appear_in_index_read() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "fresh dir/fresh file.txt", "new\ncontent\n").await;

        gd.add_file("fresh dir/fresh file.txt").await.expect("add");
        let staged = gd.read(DiffSide::Index).await.expect("read index");
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].path, "fresh dir/fresh file.txt");
        assert_eq!(staged.files[0].status, FileStatus::Added);

        // And it disappears from the untracked listing.
        let others = gd.untracked_files().await.expect("untracked");
        assert!(others.is_empty(), "{others:?}");
    }

    #[tokio::test]
    async fn add_file_treats_brackets_literally() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "app/i/page.tsx", "one\n").await;
        commit_all(&gd, "init").await;
        // Unstaged edit to the single-letter dir plus an untracked file
        // whose `[id]` would glob-match it without literal pathspecs.
        write_file(&gd.repo_root, "app/i/page.tsx", "two\n").await;
        write_file(&gd.repo_root, "app/[id]/page.tsx", "new\n").await;

        gd.add_file("app/[id]/page.tsx").await.expect("add");
        let staged = gd
            .git(&["diff", "--cached", "--name-only"])
            .await
            .expect("invariant: name-only");
        let paths: Vec<&str> = staged
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(paths, ["app/[id]/page.tsx"], "{paths:?}");
    }

    #[tokio::test]
    async fn delete_file_treats_brackets_literally() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "app/i/page.tsx", "one\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "app/[id]/page.tsx", "new\n").await;

        gd.delete_file("app/[id]/page.tsx").await.expect("delete");
        assert!(
            tokio::fs::try_exists(gd.repo_root.join("app/[id]/page.tsx"))
                .await
                .is_ok_and(|e| !e)
        );
        assert!(tokio::fs::try_exists(gd.repo_root.join("app/i/page.tsx"))
            .await
            .is_ok_and(|e| e));
        let status = gd.git(&["status", "--porcelain"]).await.expect("status");
        assert!(status.trim().is_empty(), "{status}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hunk_actions_leave_file_mode_alone() {
        use std::os::unix::fs::PermissionsExt;

        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "s.sh", "one\n").await;
        commit_all(&gd, "init").await;
        // Make the tracked file executable and edit its content, so the
        // unstaged diff carries both a mode change and a text hunk.
        std::fs::set_permissions(
            gd.repo_root.join("s.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("invariant: chmod");
        write_file(&gd.repo_root, "s.sh", "two\n").await;

        // Reject: reverse-apply the single-hunk patch to the worktree. The
        // content must revert while the 0o755 mode survives.
        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch builds");
        gd.apply(&patch, false, true).await.expect("reverse apply");
        let content = tokio::fs::read_to_string(gd.repo_root.join("s.sh"))
            .await
            .expect("invariant: read");
        assert_eq!(content, "one\n");
        let mode = std::fs::metadata(gd.repo_root.join("s.sh"))
            .expect("invariant: stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "mode is {mode:o}");

        // Accept: restore the edit, rebuild the patch from a fresh read and
        // stage it. The index entry must stay a plain non-executable blob.
        write_file(&gd.repo_root, "s.sh", "two\n").await;
        let model = gd.read(DiffSide::WorkingTree).await.expect("re-read");
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch rebuilds");
        gd.apply(&patch, true, false).await.expect("cached apply");
        let ls = gd.git(&["ls-files", "-s", "s.sh"]).await.expect("ls-files");
        assert!(ls.trim_start().starts_with("100644"), "index entry: {ls}");
    }

    #[tokio::test]
    async fn apply_ignores_user_whitespace_config() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "w.txt", "a\nb\n").await;
        commit_all(&gd, "init").await;
        // Trailing spaces on the changed line are exactly what a user's
        // `apply.whitespace fix` would strip and `error` would reject.
        write_file(&gd.repo_root, "w.txt", "a\nb   \n").await;
        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch builds");

        for setting in ["fix", "error"] {
            gd.git(&["config", "apply.whitespace", setting])
                .await
                .expect("invariant: config");
            gd.check_apply(&patch, true, false)
                .await
                .unwrap_or_else(|e| panic!("check_apply failed under {setting}: {e}"));
            gd.apply(&patch, true, false)
                .await
                .unwrap_or_else(|e| panic!("apply failed under {setting}: {e}"));
            let staged = gd.git(&["show", ":w.txt"]).await.expect("show :w.txt");
            assert_eq!(
                staged, "a\nb   \n",
                "staged under apply.whitespace={setting}"
            );
            gd.git(&["reset"]).await.expect("invariant: reset");
        }
    }

    #[tokio::test]
    async fn crlf_hunk_accept_and_reject_keep_line_endings() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        gd.git(&["config", "core.autocrlf", "false"])
            .await
            .expect("invariant: config");
        let committed: &[u8] = b"one\r\ntwo\r\nthree\r\n";
        let edited: &[u8] = b"one\r\nTWO\r\nthree\r\n";
        std::fs::write(gd.repo_root.join("c.txt"), committed).expect("invariant: write");
        commit_all(&gd, "init").await;
        std::fs::write(gd.repo_root.join("c.txt"), edited).expect("invariant: write");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        // Display strips the `\r`; the raw body keeps it for the patch.
        for line in &model.files[0].hunks[0].lines {
            assert!(!line.text.contains('\r'), "{:?}", line.text);
        }
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch builds");

        // Accept: the staged blob keeps the exact CRLF bytes.
        gd.check_apply(&patch, true, false).await.expect("check");
        gd.apply(&patch, true, false).await.expect("apply");
        let (staged, _) = gd
            .run_git_capped(&["show", ":c.txt"], None, GIT_TIMEOUT, MAX_GIT_STDOUT)
            .await
            .expect("show :c.txt");
        assert_eq!(staged, edited);

        // Reject: discarding the hunk restores the committed CRLF bytes.
        gd.git(&["reset"]).await.expect("invariant: reset");
        gd.apply(&patch, false, true).await.expect("reverse apply");
        let on_disk = tokio::fs::read(gd.repo_root.join("c.txt"))
            .await
            .expect("invariant: read");
        assert_eq!(on_disk, committed);
    }

    #[tokio::test]
    async fn non_utf8_hunk_accept_and_reject_keep_bytes() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        gd.git(&["config", "core.autocrlf", "false"])
            .await
            .expect("invariant: config");
        let committed: &[u8] = b"caf\xe9\nx\n";
        let edited: &[u8] = b"caf\xe9\ny\n";
        std::fs::write(gd.repo_root.join("n.txt"), committed).expect("invariant: write");
        commit_all(&gd, "init").await;
        std::fs::write(gd.repo_root.join("n.txt"), edited).expect("invariant: write");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1);
        let patch = build_single_hunk_patch(&model.files[0], 0).expect("patch builds");

        // Accept: the staged blob keeps the exact non-UTF-8 bytes.
        gd.check_apply(&patch, true, false).await.expect("check");
        gd.apply(&patch, true, false).await.expect("apply");
        let (staged, _) = gd
            .run_git_capped(&["show", ":n.txt"], None, GIT_TIMEOUT, MAX_GIT_STDOUT)
            .await
            .expect("show :n.txt");
        assert_eq!(staged, edited);

        // Reject: discarding the hunk restores the committed bytes.
        gd.git(&["reset"]).await.expect("invariant: reset");
        gd.apply(&patch, false, true).await.expect("reverse apply");
        let on_disk = tokio::fs::read(gd.repo_root.join("n.txt"))
            .await
            .expect("invariant: read");
        assert_eq!(on_disk, committed);
    }

    #[tokio::test]
    async fn delete_file_removes_untracked_file_and_rejects_bad_paths() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "keep.txt", "x\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "doomed.txt", "bye\n").await;

        gd.delete_file("doomed.txt").await.expect("delete");
        assert!(tokio::fs::try_exists(gd.repo_root.join("doomed.txt"))
            .await
            .is_ok_and(|e| !e));
        assert!(gd.untracked_files().await.expect("untracked").is_empty());

        // Escaping the repo root and deleting a directory are refused.
        for bad in ["../outside.txt", "/etc/hosts"] {
            let res = gd.delete_file(bad).await;
            assert!(
                matches!(res, Err(GitDiffError::Io(_))),
                "path {bad}: {res:?}"
            );
        }
        assert!(matches!(
            gd.delete_file(".").await,
            Err(GitDiffError::Io(_))
        ));
        // A tracked file is a legitimate delete target (viewer offers discard
        // on untracked files only, but the runner stays general).
        gd.delete_file("keep.txt").await.expect("delete tracked");
    }

    #[tokio::test]
    async fn delete_file_failure_keeps_the_file() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        write_file(&gd.repo_root, "doomed.txt", "bye\n").await;
        // A stale index lock makes every index-writing git command fail.
        let lock = gd.repo_root.join(".git/index.lock");
        std::fs::write(&lock, b"").expect("invariant: write lock");

        // The delete must fail…
        assert!(gd.delete_file("doomed.txt").await.is_err());
        // …and the worktree file must survive it: removing it before git
        // succeeded would lose data with no backup path to show.
        assert!(tokio::fs::try_exists(gd.repo_root.join("doomed.txt"))
            .await
            .is_ok_and(|e| e));

        // Remove the lock so later git commands (and the tempdir cleanup)
        // are not blocked by it.
        tokio::fs::remove_file(&lock)
            .await
            .expect("invariant: remove lock");
    }

    #[tokio::test]
    async fn crlf_untracked_file_has_no_cr_in_text() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // CRLF content: each display line must drop its trailing '\r'.
        std::fs::write(gd.repo_root.join("crlf.txt"), b"a\r\nb\r\n").expect("invariant: write");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "crlf.txt");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.hunks.len(), 1);
        let texts: Vec<&str> = f.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["a", "b"]);
    }

    #[tokio::test]
    async fn read_capped_marks_truncated_instead_of_timing_out() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        // Two committed files, then ~3000 changed lines (~40 chars each) in
        // each so the raw worktree diff is several hundred KB — far past the
        // 200-byte cap and bigger than the OS pipe buffer, so without the
        // drain git would block on write and never exit.
        for name in ["a.txt", "b.txt"] {
            let old_body: String = (1..=3000)
                .map(|i| format!("l{i:04} {}\n", "x".repeat(38)))
                .collect();
            write_file(&gd.repo_root, name, &old_body).await;
        }
        commit_all(&gd, "init").await;
        for name in ["a.txt", "b.txt"] {
            let new_body: String = (1..=3000)
                .map(|i| format!("L{i:04} {}\n", "y".repeat(38)))
                .collect();
            write_file(&gd.repo_root, name, &new_body).await;
        }

        // Regression: before the drain, git blocked on the full stdout pipe,
        // child.wait() never returned and this call ended as Timeout.
        let model = gd
            .read_capped(DiffSide::WorkingTree, 200)
            .await
            .expect("read_capped returns, not Timeout");
        assert!(model.truncated);
        // The cut tail may hold a half-parsed hunk; the last entry is
        // dropped, so at most one (possibly partial) file remains.
        assert!(model.files.len() < 2, "{:?}", model.files);
    }

    #[tokio::test]
    async fn write_timeout_names_the_lock_file() {
        // Pure mapping check, no git process: a read-path Timeout must
        // become WriteTimeout, whose message names .git/index.lock.
        let err = as_write_error(GitDiffError::Timeout);
        assert!(matches!(err, GitDiffError::WriteTimeout), "{err:?}");
        assert!(err.to_string().contains("index.lock"), "message: {err}");

        // Anything else passes through unchanged.
        assert!(matches!(
            as_write_error(GitDiffError::NotARepo),
            GitDiffError::NotARepo
        ));
    }

    #[tokio::test]
    async fn binary_untracked_file_has_no_hunks() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // A NUL byte marks the file binary: it is listed (status Binary)
        // but never gets hunk lines.
        std::fs::write(gd.repo_root.join("blob.bin"), b"a\0b").expect("invariant: write");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "blob.bin");
        assert_eq!(f.status, FileStatus::Binary);
        assert!(f.untracked);
        assert!(f.hunks.is_empty());
    }

    #[tokio::test]
    async fn empty_untracked_file_can_be_accepted() {
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // Zero bytes on disk: Accept (git add) must still stage it.
        std::fs::write(gd.repo_root.join("empty.txt"), b"").expect("invariant: write");

        gd.add_file("empty.txt").await.expect("add");
        let staged = gd
            .git(&["diff", "--cached", "--name-only"])
            .await
            .expect("name-only");
        let paths: Vec<&str> = staged
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(paths, ["empty.txt"], "{paths:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_symlink_shows_target_not_contents() {
        use std::os::unix::fs::symlink;

        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // The target lives outside the repo; its content must never leak
        // into the viewer.
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("data.txt"), "secret").expect("invariant: write");
        let target = outside.path().join("data.txt");
        symlink(&target, gd.repo_root.join("link.txt")).expect("invariant: symlink");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "link.txt");
        assert_eq!(f.status, FileStatus::Added);
        assert!(f.untracked);
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].lines.len(), 1);
        let line = &f.hunks[0].lines[0];
        assert_eq!(line.text, target.to_string_lossy());
        assert!(line.no_newline);
        for l in &f.hunks[0].lines {
            assert!(!l.text.contains("secret"), "{}", l.text);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_dangling_symlink_is_listed() {
        use std::os::unix::fs::symlink;

        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // The target does not exist; the link is still listed with its
        // target as the single content line.
        let missing = "/nonexistent/nowhere.txt";
        symlink(missing, gd.repo_root.join("dangling.txt")).expect("invariant: symlink");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "dangling.txt");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].lines.len(), 1);
        assert_eq!(f.hunks[0].lines[0].text, missing);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_symlink_to_dev_zero_returns() {
        use std::os::unix::fs::symlink;

        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        // Following this link would read forever; it must list instantly
        // with the target path as its line.
        symlink("/dev/zero", gd.repo_root.join("zero.link")).expect("invariant: symlink");

        let model = gd.read(DiffSide::WorkingTree).await.expect("read");
        assert_eq!(model.files.len(), 1, "{:?}", model.files);
        let f = &model.files[0];
        assert_eq!(f.path, "zero.link");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].lines.len(), 1);
        assert_eq!(f.hunks[0].lines[0].text, "/dev/zero");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_symlink_id_changes_with_target() {
        use std::os::unix::fs::symlink;

        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;

        symlink("/a", gd.repo_root.join("link.txt")).expect("invariant: symlink");
        let first = gd.read(DiffSide::WorkingTree).await.expect("read 1");
        let f = &first.files[0];
        assert_eq!(f.hunks.len(), 1);
        let id_one = f.hunks[0].id;

        tokio::fs::remove_file(gd.repo_root.join("link.txt"))
            .await
            .expect("invariant: remove");
        symlink("/b", gd.repo_root.join("link.txt")).expect("invariant: symlink");
        let second = gd.read(DiffSide::WorkingTree).await.expect("read 2");
        let f = &second.files[0];
        assert_eq!(f.hunks.len(), 1);
        let id_two = f.hunks[0].id;

        // A changed link target must change the hunk id (stale-hunk
        // detection), and neither may be the placeholder HunkId(0).
        assert_ne!(id_one, id_two);
        assert_ne!(id_one, crate::diff_model::HunkId(0));
        assert_ne!(id_two, crate::diff_model::HunkId(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_symlink_can_be_accepted() {
        use std::os::unix::fs::symlink;

        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "committed.txt", "x\n").await;
        commit_all(&gd, "init").await;
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("t.txt"), "t\n").expect("invariant: write");
        symlink(
            outside.path().join("t.txt"),
            gd.repo_root.join("stage-me.link"),
        )
        .expect("invariant: symlink");

        gd.add_file("stage-me.link").await.expect("add");
        let ls = gd
            .git(&["ls-files", "-s", "stage-me.link"])
            .await
            .expect("ls-files -s");
        assert!(ls.trim_start().starts_with("120000"), "index entry: {ls}");
    }

    #[tokio::test]
    async fn timeout_wrapper_reports_timeout_on_pending_future() {
        // §5.2: test the timeout wrapper itself against a future that never
        // completes, at millisecond scale. No child process, no hooks, no
        // platform-specific sleep binary.
        let start = std::time::Instant::now();
        let res = with_timeout(std::future::pending::<()>(), Duration::from_millis(25)).await;
        assert!(matches!(res, Err(GitDiffError::Timeout)));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn merge_conflict_is_listed_on_both_sides() {
        // A conflicted file must not vanish from the viewer: the working
        // tree side prints `diff --cc`, the staged side `* Unmerged path`.
        let Some((_dir, gd)) = temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        write_file(&gd.repo_root, "f.txt", "base\n").await;
        commit_all(&gd, "init").await;
        gd.git(&["switch", "-c", "other"])
            .await
            .expect("invariant: branch");
        write_file(&gd.repo_root, "f.txt", "theirs\n").await;
        commit_all(&gd, "other").await;
        gd.git(&["switch", "-"])
            .await
            .expect("invariant: back to main");
        write_file(&gd.repo_root, "f.txt", "ours\n").await;
        commit_all(&gd, "ours").await;

        // The merge fails with a content conflict in f.txt.
        let merge = gd.git(&["merge", "other"]).await;
        match merge {
            Err(GitDiffError::Exit { code, .. }) => assert_ne!(code, 0),
            other => panic!("expected a failed merge, got {other:?}"),
        }
        let wt = gd.read(DiffSide::WorkingTree).await.expect("worktree read");
        assert_eq!(wt.files.len(), 1, "{:?}", wt.files);
        let f = &wt.files[0];
        assert_eq!(f.path, "f.txt");
        assert_eq!(f.status, FileStatus::Unmerged);
        assert!(f.hunks.is_empty());

        let idx = gd.read(DiffSide::Index).await.expect("index read");
        assert_eq!(idx.files.len(), 1, "{:?}", idx.files);
        let f = &idx.files[0];
        assert_eq!(f.path, "f.txt");
        assert_eq!(f.status, FileStatus::Unmerged);
        assert!(f.hunks.is_empty());
    }
}
