//! A test execution environment whose root is not where the files are.
//!
//! `VirtualEnv` wraps a [`HostExecutionEnv`] rooted at a real folder, but
//! reports a virtual root (`cwd()` and every path it returns) that does not
//! exist on the host. It
//!
//! 1. records every call (method and virtual path), and
//! 2. refuses, and records as a leak, any incoming path that names the real
//!    folder, and any path outside the virtual root.
//!
//! A tool or loader that checks a Workspace path with `std::fs` instead of
//! the env looks at the virtual path, which is not on the host, and so
//! fails; one that spells a real path back to the env is a leak. Shared by
//! `loop-agent` and `loop-app-core` tests through `#[path]`, because a
//! shared crate would add a dependency edge.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use loop_agent::harness::types::{
    ExecutionError, FileError, FileErrorCode, FileIdentity, FileInfo, FileSystem, PathMetadata,
    PolicyPath, Shell, ShellExecOptions, ShellOutput,
};
use loop_agent::harness::HostExecutionEnv;

/// The virtual root. Nothing is at this path on the host.
pub const VIRTUAL_ROOT: &str = "/loop-virtual-workspace";
/// Where the env shows its temporary directories (the host puts them in the
/// system temp folder, outside the root).
pub const VIRTUAL_TMP: &str = "/loop-virtual-tmp";

pub struct VirtualEnv {
    inner: HostExecutionEnv,
    /// The folder the files are in, as the host resolves it.
    real: PathBuf,
    /// The same folder as the test spelled it (`/var/...` on macOS).
    raw: PathBuf,
    virt: PathBuf,
    /// The env's cwd: the virtual root, or a folder below it ([`Self::at`]).
    cwd: PathBuf,
    /// The system temp folder, as the host resolves it, and its virtual name.
    tmp_real: PathBuf,
    tmp_raw: PathBuf,
    tmp_virt: PathBuf,
    calls: Mutex<Vec<String>>,
    leaks: Mutex<Vec<String>>,
    /// `symlink_metadata` answers set by the test, by virtual path. The
    /// path need not exist (`/` or the virtual root itself).
    reported: Mutex<HashMap<PathBuf, PathMetadata>>,
}

fn fold(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl VirtualEnv {
    /// An env over the files in `dir`, shown at [`VIRTUAL_ROOT`].
    pub fn new(dir: &Path) -> Self {
        let real = std::fs::canonicalize(dir).expect("canonical test dir");
        let virt = PathBuf::from(VIRTUAL_ROOT);
        assert!(
            !virt.exists(),
            "the virtual root must not exist on the host: {}",
            virt.display()
        );
        Self {
            inner: HostExecutionEnv::new(&real),
            real,
            raw: dir.to_path_buf(),
            cwd: virt.clone(),
            virt,
            tmp_real: std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir"),
            tmp_raw: std::env::temp_dir(),
            tmp_virt: PathBuf::from(VIRTUAL_TMP),
            calls: Mutex::new(Vec::new()),
            leaks: Mutex::new(Vec::new()),
            reported: Mutex::new(HashMap::new()),
        }
    }

    /// Answer `symlink_metadata` for the virtual `path` with `meta` (an
    /// owner, mode or link count the host files do not have).
    pub fn report_metadata(&self, path: &Path, meta: PathMetadata) {
        lock(&self.reported).insert(fold(path), meta);
    }

    /// Forget the calls recorded so far.
    pub fn clear_calls(&self) {
        lock(&self.calls).clear();
    }

    /// The same env with its cwd at `sub` below the virtual root.
    pub fn at(mut self, sub: &str) -> Self {
        self.cwd = self.virt.join(sub);
        self
    }

    /// The virtual root path.
    pub fn root(&self) -> &Path {
        &self.virt
    }

    /// Every call so far, as `method path`.
    pub fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }

    /// Calls that named the real folder instead of the virtual root.
    pub fn leaks(&self) -> Vec<String> {
        lock(&self.leaks).clone()
    }

    /// Whether `method` was called at all.
    pub fn called(&self, method: &str) -> bool {
        self.calls()
            .iter()
            .any(|c| c.split(' ').next() == Some(method))
    }

    /// Fail unless no call named the real folder.
    pub fn assert_no_leaks(&self) {
        let leaks = self.leaks();
        assert!(leaks.is_empty(), "host paths reached the env: {leaks:?}");
    }

    /// Replace the real folder with the virtual root in `text`.
    fn show(&self, text: &str) -> String {
        let text = text.replace(&*self.real.to_string_lossy(), VIRTUAL_ROOT);
        let text = text.replace(&*self.raw.to_string_lossy(), VIRTUAL_ROOT);
        let text = text.replace(&*self.tmp_real.to_string_lossy(), VIRTUAL_TMP);
        text.replace(&*self.tmp_raw.to_string_lossy(), VIRTUAL_TMP)
    }

    fn to_virtual(&self, path: &Path) -> PathBuf {
        if let Ok(rest) = path.strip_prefix(&self.real) {
            return self.virt.join(rest);
        }
        for tmp in [&self.tmp_real, &self.tmp_raw] {
            if let Ok(rest) = path.strip_prefix(tmp) {
                return self.tmp_virt.join(rest);
            }
        }
        path.to_path_buf()
    }

    fn shown(&self, error: FileError) -> FileError {
        FileError::new(error.code, self.show(&error.message))
    }

    /// The real path of a virtual one, recording the call.
    fn to_real(&self, method: &str, path: &Path) -> Result<PathBuf, FileError> {
        lock(&self.calls).push(format!("{method} {}", path.display()));
        let abs = fold(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        });
        if abs.starts_with(&self.real)
            || abs.starts_with(&self.raw)
            || abs.starts_with(&self.tmp_real)
            || abs.starts_with(&self.tmp_raw)
        {
            lock(&self.leaks).push(format!("{method} {}", path.display()));
            return Err(FileError::new(
                FileErrorCode::Other,
                format!("host path used: {}", path.display()),
            ));
        }
        if let Ok(rest) = abs.strip_prefix(&self.tmp_virt) {
            return Ok(self.tmp_real.join(rest));
        }
        if !abs.starts_with(&self.virt) {
            return Err(FileError::new(
                FileErrorCode::NotFound,
                format!("no such file: {}", path.display()),
            ));
        }
        // The `..` parts go to the host as written, so a `..` after a link
        // leaves the link's target, as the kernel does (`link/..`). The
        // folded form above already refused a path that leaves the root.
        let unfolded = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        match unfolded.strip_prefix(&self.virt) {
            Ok(rest) => Ok(self.real.join(rest)),
            Err(_) => Ok(self.real.join(abs.strip_prefix(&self.virt).unwrap_or(&abs))),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[async_trait]
impl FileSystem for VirtualEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        lock(&self.calls).push(format!("absolute_path {}", path.display()));
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        })
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        lock(&self.calls).push(format!("join_path {}", base.display()));
        base.join(child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        let real = self.to_real("read_text_file", path)?;
        self.inner
            .read_text_file(&real)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        let real = self.to_real("read_binary_file", path)?;
        self.inner
            .read_binary_file(&real)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let real = self.to_real("write_file", path)?;
        self.inner
            .write_file(&real, data)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let real = self.to_real("append_file", path)?;
        self.inner
            .append_file(&real, data)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let real = self.to_real("file_info", path)?;
        let mut info = self
            .inner
            .file_info(&real)
            .await
            .map_err(|e| self.shown(e))?;
        info.path = self.to_virtual(&info.path);
        Ok(info)
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let real = self.to_real("list_dir", path)?;
        let mut out = self
            .inner
            .list_dir(&real)
            .await
            .map_err(|e| self.shown(e))?;
        for info in &mut out {
            info.path = self.to_virtual(&info.path);
        }
        Ok(out)
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        match self.to_real("exists", path) {
            Ok(real) => self.inner.exists(&real).await.map_err(|e| self.shown(e)),
            // Outside the virtual root nothing exists (a parent folder).
            Err(e) if e.code == FileErrorCode::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        let real = self.to_real("create_dir", path)?;
        self.inner
            .create_dir(&real)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let real = self.to_real("remove", path)?;
        self.inner.remove(&real).await.map_err(|e| self.shown(e))
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        let real = self.to_real("canonical_path", path)?;
        let canonical = self
            .inner
            .canonical_path(&real)
            .await
            .map_err(|e| self.shown(e))?;
        Ok(self.to_virtual(&canonical))
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        lock(&self.calls).push(format!("create_temp_dir {prefix}"));
        let dir = self
            .inner
            .create_temp_dir(prefix)
            .await
            .map_err(|e| self.shown(e))?;
        Ok(self.to_virtual(&dir))
    }

    async fn file_identity(&self, path: &Path) -> Result<Option<FileIdentity>, FileError> {
        let real = self.to_real("file_identity", path)?;
        self.inner
            .file_identity(&real)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn symlink_metadata(&self, path: &Path) -> Result<PathMetadata, FileError> {
        if let Some(meta) = lock(&self.reported).get(&fold(path)).cloned() {
            lock(&self.calls).push(format!("symlink_metadata {}", path.display()));
            return Ok(meta);
        }
        let real = self.to_real("symlink_metadata", path)?;
        self.inner
            .symlink_metadata(&real)
            .await
            .map_err(|e| self.shown(e))
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf, FileError> {
        let real = self.to_real("read_link", path)?;
        let target = self
            .inner
            .read_link(&real)
            .await
            .map_err(|e| self.shown(e))?;
        Ok(self.to_virtual(&target))
    }

    async fn resolve_for_policy(&self, path: &Path) -> Result<PolicyPath, FileError> {
        let real = self.to_real("resolve_for_policy", path)?;
        let mut policy = self
            .inner
            .resolve_for_policy(&real)
            .await
            .map_err(|e| self.shown(e))?;
        policy.resolved = self.to_virtual(&policy.resolved);
        Ok(policy)
    }
}

#[async_trait]
impl Shell for VirtualEnv {
    async fn exec(
        &self,
        command: &str,
        mut options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        lock(&self.calls).push(format!("exec {command}"));
        if let Some(cwd) = &options.cwd {
            options.cwd = Some(self.to_real("exec_cwd", cwd).map_err(|e| {
                ExecutionError::new(
                    loop_agent::harness::types::ExecutionErrorCode::SpawnFailed,
                    e.message,
                )
            })?);
        }
        let mut out = self.inner.exec(command, options).await?;
        out.stdout = self.show(&out.stdout);
        out.stderr = self.show(&out.stderr);
        Ok(out)
    }
}
