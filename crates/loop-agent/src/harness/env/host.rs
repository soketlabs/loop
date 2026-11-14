//! Host (local machine) execution environment.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::harness::types::{
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileSystem, Shell,
    ShellExecOptions, ShellOutput,
};
use crate::harness::utils::sanitize_binary_output;

/// Local filesystem + shell execution environment.
pub struct HostExecutionEnv {
    cwd: PathBuf,
    /// Under a tool lock: the folder every file read must stay in, checked
    /// on the opened file ([`HostExecutionEnv::confine_reads`]).
    confined: Option<PathBuf>,
}

impl HostExecutionEnv {
    /// Create with an absolute or relative cwd (canonicalized best-effort).
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        Self {
            cwd,
            confined: None,
        }
    }

    /// Refuse every file read whose opened file is not a regular file with
    /// one link inside `folder` (resolved now). The check runs on the open
    /// file, not on its path, so a path that turns into a link to a file
    /// outside between a caller's check and the open is refused. On a
    /// platform where Loop cannot name an open file's path, every read is
    /// refused.
    pub fn confine_reads(mut self, folder: PathBuf) -> Self {
        self.confined = Some(std::fs::canonicalize(&folder).unwrap_or(folder));
        self
    }

    /// Create rooted at the process current directory.
    pub fn from_current_dir() -> Result<Self, FileError> {
        let cwd = std::env::current_dir()
            .map_err(|e| FileError::new(FileErrorCode::Io, format!("current_dir: {e}")))?;
        Ok(Self::new(cwd))
    }

    fn map_io(err: std::io::Error) -> FileError {
        let code = match err.kind() {
            std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
            std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => FileErrorCode::AlreadyExists,
            _ => FileErrorCode::Io,
        };
        FileError::new(code, err.to_string())
    }
}

/// Read `path` only if the file actually opened is a regular file with one
/// link inside `folder`.
async fn read_confined(path: PathBuf, folder: PathBuf) -> Result<Vec<u8>, FileError> {
    tokio::task::spawn_blocking(move || read_confined_now(&path, &folder))
        .await
        .map_err(|e| FileError::new(FileErrorCode::Io, e.to_string()))?
}

fn read_confined_now(path: &Path, folder: &Path) -> Result<Vec<u8>, FileError> {
    use std::io::Read;
    let (mut file, meta) = open_confined(path, folder)?;
    let mut bytes = Vec::with_capacity(meta.len().min(64 * 1024 * 1024) as usize);
    file.read_to_end(&mut bytes)
        .map_err(HostExecutionEnv::map_io)?;
    Ok(bytes)
}

/// Open `path` for reading only if the file actually opened is a regular
/// file with one link inside `folder` (the tool lock's rule). Returns the
/// file and its metadata from `fstat`. Blocking.
pub(crate) fn open_confined(
    path: &Path,
    folder: &Path,
) -> Result<(std::fs::File, std::fs::Metadata), FileError> {
    let outside = || {
        FileError::new(
            FileErrorCode::PermissionDenied,
            format!(
                "{} is outside this Session's folder, or is a link: the tool lock refuses it",
                path.display()
            ),
        )
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO or device must not block the read.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(HostExecutionEnv::map_io)?;
    let meta = file.metadata().map_err(HostExecutionEnv::map_io)?;
    if !meta.is_file() {
        return Err(outside());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(outside());
        }
    }
    match opened_path(&file) {
        Some(real) if real.starts_with(folder) => {}
        _ => return Err(outside()),
    }
    Ok((file, meta))
}

/// The path of the file `file` has open, as the kernel names it.
#[cfg(target_os = "macos")]
pub(crate) fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: `buf` holds PATH_MAX + 1 bytes, as F_GETPATH needs, and the
    // descriptor stays open for the call.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if rc == -1 {
        return None;
    }
    let len = buf.iter().position(|b| *b == 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
}

/// The path of the file `file` has open, as the kernel names it.
#[cfg(target_os = "linux")]
pub(crate) fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

/// Loop cannot name an open file's path here, so a confined read fails.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn opened_path(_file: &std::fs::File) -> Option<PathBuf> {
    None
}

#[async_trait]
impl FileSystem for HostExecutionEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.cwd.join(path))
        }
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        let path = self.absolute_path(path)?;
        if let Some(folder) = &self.confined {
            let bytes = read_confined(path, folder.clone()).await?;
            return String::from_utf8(bytes).map_err(|_| {
                FileError::new(FileErrorCode::Io, "stream did not contain valid UTF-8")
            });
        }
        tokio::fs::read_to_string(&path).await.map_err(Self::map_io)
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        let path = self.absolute_path(path)?;
        if let Some(folder) = &self.confined {
            return read_confined(path, folder.clone()).await;
        }
        tokio::fs::read(&path).await.map_err(Self::map_io)
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(Self::map_io)?;
        }
        tokio::fs::write(&path, data).await.map_err(Self::map_io)
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        use tokio::io::AsyncWriteExt;
        let path = self.absolute_path(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(Self::map_io)?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(Self::map_io)?;
        file.write_all(data).await.map_err(Self::map_io)?;
        // A tokio file hands the bytes to a blocking task. Without a flush,
        // the call can return before the bytes are in the file.
        file.flush().await.map_err(Self::map_io)
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(Self::map_io)?;
        Ok(FileInfo {
            path,
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            size: meta.len(),
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let path = self.absolute_path(path)?;
        let mut rd = tokio::fs::read_dir(&path).await.map_err(Self::map_io)?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(Self::map_io)? {
            match entry.metadata().await {
                Ok(meta) => out.push(FileInfo {
                    path: entry.path(),
                    is_dir: meta.is_dir(),
                    is_file: meta.is_file(),
                    size: meta.len(),
                }),
                // Removed between the listing and the stat: not there.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // One entry whose metadata cannot be read (a folder without
                // search permission) does not fail the listing: it is listed
                // with the kind its directory entry gives and size 0, as
                // `ls -l` shows such an entry with unknown details.
                Err(_) => {
                    let kind = entry.file_type().await.ok();
                    out.push(FileInfo {
                        path: entry.path(),
                        is_dir: kind.is_some_and(|k| k.is_dir()),
                        is_file: kind.is_some_and(|k| k.is_file()),
                        size: 0,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        let path = self.absolute_path(path)?;
        Ok(tokio::fs::try_exists(&path).await.map_err(Self::map_io)?)
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::create_dir_all(&path).await.map_err(Self::map_io)
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(Self::map_io)?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await.map_err(Self::map_io)
        } else {
            tokio::fs::remove_file(&path).await.map_err(Self::map_io)
        }
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::canonicalize(&path).await.map_err(Self::map_io)
    }

    async fn file_identity(
        &self,
        path: &Path,
    ) -> Result<Option<crate::harness::types::FileIdentity>, FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(Self::map_io)?;
        Ok(meta
            .is_file()
            .then(|| crate::harness::types::FileIdentity::from_metadata(&meta)))
    }

    async fn symlink_metadata(
        &self,
        path: &Path,
    ) -> Result<crate::harness::types::PathMetadata, FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(Self::map_io)?;
        Ok(path_metadata(&meta))
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::read_link(&path).await.map_err(Self::map_io)
    }

    async fn resolve_for_policy(
        &self,
        path: &Path,
    ) -> Result<crate::harness::types::PolicyPath, FileError> {
        let absolute = loop_workspace::lexical(&self.absolute_path(path)?);
        // Walk the components with lstat: any link on the way is reported.
        let mut has_link = false;
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            prefix.push(component.as_os_str());
            match tokio::fs::symlink_metadata(&prefix).await {
                Ok(meta) if meta.file_type().is_symlink() => has_link = true,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(Self::map_io(e)),
            }
        }
        match tokio::fs::canonicalize(&absolute).await {
            Ok(resolved) => {
                let meta = tokio::fs::metadata(&resolved).await.map_err(Self::map_io)?;
                let info = path_metadata(&meta);
                Ok(crate::harness::types::PolicyPath {
                    resolved,
                    exists: true,
                    has_link,
                    uid: info.uid,
                    nlink: info.nlink,
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
            Err(e) => Err(Self::map_io(e)),
        }
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        let base = std::env::temp_dir();
        let name = format!("{prefix}-{}", loop_ai::new_id());
        let path = base.join(name);
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(Self::map_io)?;
        Ok(path)
    }
}

/// Gate facts of `meta` (from `lstat` or `stat`): kind, size, owner, link
/// count and permission bits.
pub(crate) fn path_metadata(meta: &std::fs::Metadata) -> crate::harness::types::PathMetadata {
    use crate::harness::types::{PathKind, PathMetadata};
    let kind = if meta.file_type().is_symlink() {
        PathKind::Symlink
    } else if meta.is_file() {
        PathKind::File
    } else if meta.is_dir() {
        PathKind::Dir
    } else {
        PathKind::Other
    };
    #[cfg(unix)]
    let (uid, nlink, mode) = {
        use std::os::unix::fs::MetadataExt;
        (
            Some(meta.uid()),
            Some(meta.nlink()),
            Some(meta.mode() & 0o7777),
        )
    };
    #[cfg(not(unix))]
    let (uid, nlink, mode) = (None, None, None);
    PathMetadata {
        kind,
        size: meta.len(),
        uid,
        nlink,
        mode,
    }
}

/// Minimum interval between live output callbacks.
const OUTPUT_EMIT_INTERVAL: Duration = Duration::from_millis(50);

#[async_trait]
impl Shell for HostExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let cwd = options.cwd.clone().unwrap_or_else(|| self.cwd.clone());

        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd.current_dir(&cwd);
        if !options.inherit_env {
            cmd.env_clear();
        }
        if let Some(env) = &options.env {
            cmd.envs(env);
        }
        for name in &options.remove_env {
            cmd.env_remove(name);
        }
        exec_command(cmd, options, false, None).await
    }
}

/// Shared process-group, cancellation and bounded-output runner.
pub(crate) async fn exec_command(
    mut cmd: Command,
    options: ShellExecOptions,
    stop_background: bool,
    _marker: Option<PathBuf>,
) -> Result<ShellOutput, ExecutionError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if stop_background {
        // SAFETY: setsid is async-signal-safe and no allocator/locks are used
        // between fork and exec. Do not setpgid first: a group leader cannot setsid.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    } else {
        cmd.process_group(0);
    }

    #[cfg(target_os = "macos")]
    let descendants = _marker
        .as_deref()
        .map(super::process_tree::Descendants::new)
        .transpose()
        .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
    if options.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Aborted,
            "operation aborted",
        ));
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| ExecutionError::new(ExecutionErrorCode::SpawnFailed, e.to_string()))?;
    // Kills the tree if this future is dropped before the command ends.
    let mut tree = TreeKiller {
        pid: child.id(),
        #[cfg(target_os = "macos")]
        descendants,
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecutionError::new(ExecutionErrorCode::Io, "missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecutionError::new(ExecutionErrorCode::Io, "missing stderr pipe"))?;

    let combined = Arc::new(Mutex::new(String::new()));
    let stdout_acc = Arc::new(Mutex::new(String::new()));
    let stderr_acc = Arc::new(Mutex::new(String::new()));
    let last_emit = Arc::new(Mutex::new(
        Instant::now()
            .checked_sub(OUTPUT_EMIT_INTERVAL)
            .unwrap_or_else(Instant::now),
    ));
    let on_output = options.on_output.clone();
    let cut = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut stdout_task = PipeTask({
        let combined = Arc::clone(&combined);
        let stdout_acc = Arc::clone(&stdout_acc);
        let last_emit = Arc::clone(&last_emit);
        let on_output = on_output.clone();
        let cut = Arc::clone(&cut);
        tokio::spawn(async move {
            read_stream(stdout, combined, stdout_acc, last_emit, on_output, cut).await;
        })
    });
    let mut stderr_task = PipeTask({
        let combined = Arc::clone(&combined);
        let stderr_acc = Arc::clone(&stderr_acc);
        let last_emit = Arc::clone(&last_emit);
        let on_output = on_output.clone();
        let cut = Arc::clone(&cut);
        tokio::spawn(async move {
            read_stream(stderr, combined, stderr_acc, last_emit, on_output, cut).await;
        })
    });

    let status = match (&options.cancel, options.timeout_ms) {
        (Some(cancel), Some(ms)) => {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let cleanup = tree.kill();
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
                    drain(&mut stdout_task, &mut stderr_task).await;
                    cleanup?;
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Aborted,
                        "operation aborted",
                    ));
                }
                _ = tokio::time::sleep(Duration::from_millis(ms)) => {
                    let cleanup = tree.kill();
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
                    drain(&mut stdout_task, &mut stderr_task).await;
                    cleanup?;
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::TimedOut,
                        "command timed out",
                    ));
                }
                res = child.wait() => res.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                })?,
            }
        }
        (Some(cancel), None) => {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let cleanup = tree.kill();
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
                    drain(&mut stdout_task, &mut stderr_task).await;
                    cleanup?;
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Aborted,
                        "operation aborted",
                    ));
                }
                res = child.wait() => res.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                })?,
            }
        }
        (None, Some(ms)) => {
            match tokio::time::timeout(Duration::from_millis(ms), child.wait()).await {
                Ok(res) => {
                    res.map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?
                }
                Err(_) => {
                    let cleanup = tree.kill();
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
                    drain(&mut stdout_task, &mut stderr_task).await;
                    cleanup?;
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::TimedOut,
                        "command timed out",
                    ));
                }
            }
        }
        (None, None) => child
            .wait()
            .await
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?,
    };

    let cleanup = if stop_background {
        tree.kill()
    } else {
        tree.pid = None;
        Ok(())
    };
    drain(&mut stdout_task, &mut stderr_task).await;
    cleanup?;

    let stdout_text = stdout_acc.lock().await.clone();
    let stderr_text = stderr_acc.lock().await.clone();
    let combined_text = combined.lock().await.clone();

    if let Some(cb) = &on_output {
        cb(&combined_text);
    }

    Ok(ShellOutput {
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code: status.code().unwrap_or(-1),
        truncated: cut.load(std::sync::atomic::Ordering::Acquire),
    })
}

struct PipeTask(tokio::task::JoinHandle<()>);

impl Drop for PipeTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn drain(stdout: &mut PipeTask, stderr: &mut PipeTask) {
    if tokio::time::timeout(Duration::from_millis(250), async {
        let _ = tokio::join!(&mut stdout.0, &mut stderr.0);
    })
    .await
    .is_err()
    {
        stdout.0.abort();
        stderr.0.abort();
    }
}

/// Kills a process and everything in its process group, unless disarmed. Without it, a grandchild such as the `sleep`
/// in `sh -c "sleep 60; echo"` keeps the output pipes open after a timeout.
struct TreeKiller {
    pid: Option<u32>,
    #[cfg(target_os = "macos")]
    descendants: Option<super::process_tree::Descendants>,
}

impl TreeKiller {
    fn kill(&mut self) -> Result<(), ExecutionError> {
        #[cfg(target_os = "macos")]
        let result = self.descendants.take().map(|d| d.kill()).transpose();
        let Some(pid) = self.pid.take() else {
            return Ok(());
        };
        // SAFETY: child PIDs fit pid_t; a negative PID targets its dedicated
        // process group. Never launch a PATH-resolved cleanup helper on the host.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        #[cfg(target_os = "macos")]
        result.map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        Ok(())
    }
}

impl Drop for TreeKiller {
    fn drop(&mut self) {
        if let Err(error) = self.kill() {
            tracing::error!(%error, "process cleanup failed while dropping execution");
        }
    }
}

// Keep draining after the cap, so a noisy command never blocks on a full pipe.
const OUTPUT_CAP: usize = 1024 * 1024;

/// Append what fits under the cap; false when part of `chunk` was dropped.
fn push_capped(acc: &mut String, chunk: &str) -> bool {
    let mut end = chunk.len().min(OUTPUT_CAP.saturating_sub(acc.len()));
    while !chunk.is_char_boundary(end) {
        end -= 1;
    }
    acc.push_str(&chunk[..end]);
    end == chunk.len()
}

async fn read_stream<R: AsyncReadExt + Unpin>(
    mut reader: R,
    combined: Arc<Mutex<String>>,
    stream_acc: Arc<Mutex<String>>,
    last_emit: Arc<Mutex<Instant>>,
    on_output: Option<crate::harness::types::ShellOutputCallback>,
    cut: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let chunk = sanitize_binary_output(&buf[..n]);
                {
                    let mut acc = stream_acc.lock().await;
                    if !push_capped(&mut acc, &chunk) {
                        cut.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                let snapshot = {
                    let mut c = combined.lock().await;
                    let _ = push_capped(&mut c, &chunk);
                    c.clone()
                };
                if let Some(cb) = &on_output {
                    let should_emit = {
                        let mut last = last_emit.lock().await;
                        if last.elapsed() >= OUTPUT_EMIT_INTERVAL {
                            *last = Instant::now();
                            true
                        } else {
                            false
                        }
                    };
                    if should_emit {
                        cb(&snapshot);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    async fn run(
        command: &str,
        timeout_ms: Option<u64>,
        cancel: Option<CancellationToken>,
    ) -> Result<ShellOutput, ExecutionError> {
        HostExecutionEnv::new(std::env::temp_dir())
            .exec(
                command,
                ShellExecOptions {
                    timeout_ms,
                    cancel,
                    ..Default::default()
                },
            )
            .await
    }

    #[tokio::test]
    async fn a_timeout_stops_what_the_command_started() {
        let started = Instant::now();
        let err = run("sleep 5; echo done", Some(200), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, ExecutionErrorCode::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_cancel_stops_what_the_command_started() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let err = run("sleep 5; echo done", Some(60_000), Some(cancel))
            .await
            .unwrap_err();
        assert_eq!(err.code, ExecutionErrorCode::Aborted);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn commands_do_not_read_loops_stdin() {
        let out = run("cat; echo end", Some(5_000), None).await.unwrap();
        assert_eq!(out.stdout.trim(), "end");
    }
}

#[cfg(all(test, unix))]
mod list_tests {
    use super::*;

    /// In a folder that can be read but not searched, every `stat` fails.
    /// The listing still names each entry, with the kind from the entry
    /// and size 0, instead of failing as a whole.
    #[tokio::test]
    async fn an_entry_without_metadata_does_not_fail_the_listing() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no arguments or preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the permission bits
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(locked.join("child")).unwrap();
        std::fs::write(locked.join("file.txt"), "12345").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
        let env = HostExecutionEnv::new(dir.path());
        let listed = env.list_dir(&locked).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut listed: Vec<_> = listed
            .expect("the listing must not fail")
            .into_iter()
            .map(|i| {
                (
                    i.path.file_name().unwrap().to_string_lossy().into_owned(),
                    i.is_dir,
                    i.size,
                )
            })
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                ("child".to_owned(), true, 0),
                ("file.txt".to_owned(), false, 0)
            ]
        );
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod confined_tests {
    use super::*;

    /// The check runs on the opened file: a link out, a hard link and a
    /// FIFO are refused even when nothing checked the path first.
    #[tokio::test]
    async fn a_confined_read_checks_the_opened_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("own.txt"), "OWN").unwrap();
        std::fs::write(base.join("secret.txt"), "SECRET").unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), ws.join("link.txt")).unwrap();
        std::fs::hard_link(base.join("secret.txt"), ws.join("hard.txt")).unwrap();
        let env = HostExecutionEnv::new(&ws).confine_reads(ws.clone());
        assert_eq!(
            env.read_text_file(Path::new("own.txt")).await.unwrap(),
            "OWN"
        );
        for path in ["link.txt", "hard.txt", "../secret.txt", "sub"] {
            let error = env.read_binary_file(Path::new(path)).await.unwrap_err();
            assert_ne!(error.code, FileErrorCode::NotFound, "{path}: {error:?}");
        }
        // The folder swapped for a link after the env was made.
        std::fs::rename(ws.join("sub"), base.join("sub-old")).unwrap();
        std::fs::create_dir_all(base.join("bob")).unwrap();
        std::fs::write(base.join("bob/diary.txt"), "BOB").unwrap();
        std::os::unix::fs::symlink(base.join("bob"), ws.join("sub")).unwrap();
        assert!(env
            .read_text_file(Path::new("sub/diary.txt"))
            .await
            .is_err());
        // Unconfined, the same reads work.
        let open = HostExecutionEnv::new(&ws);
        assert_eq!(
            open.read_text_file(Path::new("link.txt")).await.unwrap(),
            "SECRET"
        );
    }
}
