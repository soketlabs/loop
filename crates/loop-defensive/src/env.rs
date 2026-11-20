//! Loop's own `read`, `write`, `edit` and `bash` tools, bound to the run's
//! container instead of the host.
//!
//! [`SandboxEnv`] is an [`ExecutionEnv`] that forwards every call to a
//! full-isolation [`KrunExecutionEnv`] for the container the session has
//! started: file reads and writes are `podman exec` inside it, so a symlink
//! the agent planted resolves in the container's filesystem, never the
//! host's. Before `prepare_task` there is no container and every call says so.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loop_agent::harness::sandbox::{KrunExecutionEnv, KrunIsolation, RealPodmanClient};
use loop_agent::harness::types::{
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileSystem, Shell,
    ShellExecOptions, ShellOutput,
};
use parking_lot::Mutex;

use crate::session::Session;

/// An [`ExecutionEnv`](loop_agent::harness::ExecutionEnv) inside the
/// session's current container, rooted at `root` (`/work` or the audit source).
pub struct SandboxEnv {
    session: Arc<Session>,
    root: PathBuf,
    current: Mutex<Option<(String, Arc<KrunExecutionEnv>)>>,
}

impl SandboxEnv {
    /// An env for `session`'s containers, with `root` as the working directory.
    pub fn new(session: Arc<Session>, root: impl Into<PathBuf>) -> Self {
        Self {
            session,
            root: root.into(),
            current: Mutex::new(None),
        }
    }

    fn inner(&self) -> Option<Arc<KrunExecutionEnv>> {
        let name = self.session.container_name()?;
        let mut current = self.current.lock();
        if let Some((cached, env)) = current.as_ref() {
            if *cached == name {
                return Some(Arc::clone(env));
            }
        }
        let env = Arc::new(KrunExecutionEnv::new(
            Arc::new(RealPodmanClient::new()),
            name.clone(),
            self.root.clone(),
            KrunIsolation::Full,
        ));
        *current = Some((name, Arc::clone(&env)));
        Some(env)
    }

    fn file(&self) -> Result<Arc<KrunExecutionEnv>, FileError> {
        self.inner().ok_or_else(|| FileError::new(FileErrorCode::Io, NO_SANDBOX))
    }
}

const NO_SANDBOX: &str = "no sandbox is running: call prepare_task (or prepare_audit) first";

#[async_trait]
impl FileSystem for SandboxEnv {
    fn cwd(&self) -> &Path {
        &self.root
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        match self.inner() {
            Some(env) => env.absolute_path(path),
            None => Ok(if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) }),
        }
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.file()?.read_text_file(path).await
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.file()?.read_binary_file(path).await
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.file()?.write_file(path, data).await
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.file()?.append_file(path, data).await
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.file()?.file_info(path).await
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.file()?.list_dir(path).await
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.file()?.exists(path).await
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.file()?.create_dir(path).await
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.file()?.remove(path).await
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.file()?.canonical_path(path).await
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.file()?.create_temp_dir(prefix).await
    }
}

#[async_trait]
impl Shell for SandboxEnv {
    async fn exec(&self, command: &str, options: ShellExecOptions) -> Result<ShellOutput, ExecutionError> {
        match self.inner() {
            Some(env) => env.exec(command, options).await,
            None => Err(ExecutionError::new(ExecutionErrorCode::Other, NO_SANDBOX)),
        }
    }
}
