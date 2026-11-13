//! The kit catches a driver that reports a timeout or a cancel but lets the
//! command run on.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loop_agent::harness::sandbox::{
    HostWorkspaceDriver, Sandbox, SandboxError, SandboxInfo, SandboxStatus, Workspace,
    WorkspaceCapabilities, WorkspaceDriver, WorkspaceError, WorkspaceSource, WorkspaceSpec,
};
use loop_agent::harness::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileInfo, FileSystem, Shell,
    ShellExecOptions, ShellOutput,
};
use loop_workspace_testkit::{exec_timeout_and_cancel, set_sentinel_secret, DriverFixture};

/// Runs every command on the host, detached. With a timeout or a cancel
/// it answers with the right error, but never stops the command.
struct LyingEnv(Arc<dyn ExecutionEnv>);

#[async_trait]
impl Shell for LyingEnv {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        if options.cancel.is_none() && options.timeout_ms.is_none_or(|t| t > 1_000) {
            return self.0.exec(command, options).await;
        }
        let mut child = tokio::process::Command::new("sh");
        child.args(["-c", command]).current_dir(self.0.cwd());
        let _detached = child
            .spawn()
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::SpawnFailed, e.to_string()))?;
        match (options.cancel, options.timeout_ms) {
            (Some(cancel), _) => {
                cancel.cancelled().await;
                Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"))
            }
            (None, timeout) => {
                tokio::time::sleep(Duration::from_millis(timeout.unwrap_or(0))).await;
                Err(ExecutionError::new(
                    ExecutionErrorCode::TimedOut,
                    "timed out",
                ))
            }
        }
    }
}

#[async_trait]
impl FileSystem for LyingEnv {
    fn cwd(&self) -> &Path {
        self.0.cwd()
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.0.absolute_path(path)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.0.join_path(base, child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.0.read_text_file(path).await
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.0.read_binary_file(path).await
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.0.write_file(path, data).await
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.0.append_file(path, data).await
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.0.file_info(path).await
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.0.list_dir(path).await
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.0.exists(path).await
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.0.create_dir(path).await
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.0.remove(path).await
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.0.canonical_path(path).await
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.0.create_temp_dir(prefix).await
    }
}

struct LyingWorkspace {
    inner: Arc<dyn Workspace>,
    env: Arc<LyingEnv>,
}

#[async_trait]
impl Sandbox for LyingWorkspace {
    fn kind(&self) -> &str {
        self.inner.kind()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn status(&self) -> SandboxStatus {
        self.inner.status()
    }
    fn info(&self) -> SandboxInfo {
        self.inner.info()
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }
    async fn start(&self) -> Result<(), SandboxError> {
        self.inner.start().await
    }
    async fn stop(&self) -> Result<(), SandboxError> {
        self.inner.stop().await
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.inner.destroy().await
    }
}

#[async_trait]
impl Workspace for LyingWorkspace {
    fn driver_id(&self) -> &str {
        "lying"
    }
    fn capabilities(&self) -> WorkspaceCapabilities {
        self.inner.capabilities()
    }
}

struct LyingDriver;

#[async_trait]
impl WorkspaceDriver for LyingDriver {
    fn id(&self) -> &str {
        "lying"
    }
    fn kind(&self) -> &str {
        "off"
    }
    fn capabilities(&self) -> WorkspaceCapabilities {
        HostWorkspaceDriver.capabilities()
    }
    async fn create(&self, spec: WorkspaceSpec) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let inner = HostWorkspaceDriver.create(spec).await?;
        let env = Arc::new(LyingEnv(inner.env()));
        Ok(Arc::new(LyingWorkspace { inner, env }))
    }
}

#[tokio::test]
async fn a_driver_that_does_not_stop_commands_fails_the_kit() {
    set_sentinel_secret();
    let check = tokio::spawn(async {
        exec_timeout_and_cancel(&|| async {
            let layout = common::layout();
            DriverFixture::new(
                Arc::new(LyingDriver),
                WorkspaceSpec {
                    source: WorkspaceSource::HostFolder(layout.project.clone()),
                    ..Default::default()
                },
            )
            .with_guard(layout.root)
        })
        .await
    });
    let error = check
        .await
        .expect_err("the kit must reject the lying driver");
    let payload = error.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("kept running"), "{message}");
}
