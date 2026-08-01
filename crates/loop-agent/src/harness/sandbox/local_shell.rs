//! Test sandbox: isolated workdir + sibling shell on the same machine.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::harness::env::HostExecutionEnv;
use crate::harness::sandbox::traits::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxStatus,
};
use crate::harness::types::{
    ExecutionError, ExecutionEnv, FileError, FileErrorCode, FileInfo, FileSystem, Shell,
    ShellExecOptions, ShellOutput,
};

/// Factory for [`LocalShellSandbox`].
pub struct LocalShellSandboxFactory;

#[async_trait]
impl SandboxFactory for LocalShellSandboxFactory {
    fn kind(&self) -> &str {
        "local-shell"
    }

    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(LocalShellSandbox::new(config)))
    }
}

/// Isolated workdir sandbox (not a security boundary — for tests / soft isolation).
pub struct LocalShellSandbox {
    id: String,
    workdir: PathBuf,
    status: RwLock<SandboxStatus>,
    env: RwLock<Option<Arc<dyn ExecutionEnv>>>,
    owns_temp: bool,
}

impl LocalShellSandbox {
    /// Create from config (workdir created on start).
    pub fn new(config: SandboxConfig) -> Self {
        let owns_temp = config.workdir.as_os_str().is_empty()
            || config
                .options
                .get("temp")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        let workdir = if owns_temp || config.workdir.as_os_str().is_empty() {
            std::env::temp_dir().join(format!("loop-sandbox-{}", loop_ai::new_id()))
        } else {
            config.workdir
        };
        Self {
            id: loop_ai::new_id(),
            workdir,
            status: RwLock::new(SandboxStatus::Created),
            env: RwLock::new(None),
            owns_temp,
        }
    }

    /// Workdir root.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }
}

#[async_trait]
impl Sandbox for LocalShellSandbox {
    fn kind(&self) -> &str {
        "local-shell"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> SandboxStatus {
        *self.status.read()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env
            .read()
            .clone()
            .expect("sandbox env available only after start")
    }

    async fn start(&self) -> Result<(), SandboxError> {
        *self.status.write() = SandboxStatus::Starting;
        tokio::fs::create_dir_all(&self.workdir)
            .await
            .map_err(|e| SandboxError::StartFailed(e.to_string()))?;
        let host = HostExecutionEnv::new(&self.workdir);
        let jailed: Arc<dyn ExecutionEnv> = Arc::new(JailedEnv {
            inner: host,
            root: self.workdir.clone(),
        });
        *self.env.write() = Some(jailed);
        *self.status.write() = SandboxStatus::Ready;
        Ok(())
    }

    async fn stop(&self) -> Result<(), SandboxError> {
        *self.status.write() = SandboxStatus::Stopping;
        *self.env.write() = None;
        *self.status.write() = SandboxStatus::Stopped;
        Ok(())
    }

    async fn destroy(&self) -> Result<(), SandboxError> {
        let _ = self.stop().await;
        if self.owns_temp {
            let _ = tokio::fs::remove_dir_all(&self.workdir).await;
        }
        Ok(())
    }
}

struct JailedEnv {
    inner: HostExecutionEnv,
    root: PathBuf,
}

impl JailedEnv {
    fn assert_in_root(&self, path: &Path) -> Result<PathBuf, FileError> {
        let abs = self.inner.absolute_path(path)?;
        // Reject .. components in the relative form
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                "path escape (..) rejected",
            ));
        }
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let canon = std::fs::canonicalize(&abs).unwrap_or(abs.clone());
        if !canon.starts_with(&root) && !abs.starts_with(&self.root) {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                format!("path escapes sandbox root: {}", abs.display()),
            ));
        }
        Ok(abs)
    }
}

#[async_trait]
impl FileSystem for JailedEnv {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }

    fn absolute_path(&self, path: &Path) -> Result<std::path::PathBuf, FileError> {
        self.assert_in_root(path)
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.inner.join_path(base, child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.read_text_file(&path).await
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.read_binary_file(&path).await
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.write_file(&path, data).await
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.append_file(&path, data).await
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.file_info(&path).await
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.list_dir(&path).await
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.exists(&path).await
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.create_dir(&path).await
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.remove(&path).await
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        let path = self.assert_in_root(path)?;
        self.inner.canonical_path(&path).await
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        // Temp dirs must stay inside root
        let name = format!("{prefix}-{}", loop_ai::new_id());
        let path = self.root.join(name);
        self.inner.create_dir(&path).await?;
        Ok(path)
    }
}

#[async_trait]
impl Shell for JailedEnv {
    async fn exec(
        &self,
        command: &str,
        mut options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let cwd = options.cwd.clone().unwrap_or_else(|| self.root.clone());
        if cwd.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ExecutionError::new(
                crate::harness::types::ExecutionErrorCode::Other,
                "cwd path escape rejected",
            ));
        }
        let abs = if cwd.is_absolute() {
            cwd
        } else {
            self.root.join(cwd)
        };
        if !abs.starts_with(&self.root) {
            return Err(ExecutionError::new(
                crate::harness::types::ExecutionErrorCode::Other,
                "cwd escapes sandbox root",
            ));
        }
        options.cwd = Some(abs);
        self.inner.exec(command, options).await
    }
}
