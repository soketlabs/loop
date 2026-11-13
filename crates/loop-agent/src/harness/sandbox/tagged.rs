//! A sandbox double with its own identity, for tests of code that swaps
//! sandboxes. Tools run in it never touch the machine: every command is
//! recorded under the sandbox's tag and answered with that tag.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use super::{Sandbox, SandboxError, SandboxInfo, SandboxStatus};
use crate::harness::env::HostExecutionEnv;
use crate::harness::types::{
    ExecutionEnv, ExecutionError, FileError, FileInfo, FileSystem, Shell, ShellExecOptions,
    ShellOutput,
};

/// Where a [`TaggedSandbox`] and its environment report to.
#[derive(Default)]
struct Shared {
    commands: parking_lot::Mutex<Vec<String>>,
    destroyed: AtomicUsize,
    /// Held commands wait for a permit; absent means they never wait.
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

/// A fake sandbox identified by `tag`.
#[derive(Clone)]
pub struct TaggedSandbox {
    tag: String,
    shared: Arc<Shared>,
    env: Arc<TaggedEnv>,
}

impl TaggedSandbox {
    /// A sandbox whose commands finish at once.
    pub fn new(tag: &str) -> Self {
        Self::build(tag, None)
    }

    /// A sandbox whose commands wait until `gate` has a permit.
    pub fn gated(tag: &str, gate: Arc<tokio::sync::Semaphore>) -> Self {
        Self::build(tag, Some(gate))
    }

    fn build(tag: &str, gate: Option<Arc<tokio::sync::Semaphore>>) -> Self {
        let shared = Arc::new(Shared {
            gate,
            ..Shared::default()
        });
        let env = Arc::new(TaggedEnv {
            tag: tag.to_owned(),
            host: HostExecutionEnv::new(std::env::temp_dir()),
            shared: Arc::clone(&shared),
        });
        Self {
            tag: tag.to_owned(),
            shared,
            env,
        }
    }

    /// The tag its commands are answered with.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Commands that reached this sandbox, in order.
    pub fn commands(&self) -> Vec<String> {
        self.shared.commands.lock().clone()
    }

    /// How many times it was destroyed.
    pub fn destroy_count(&self) -> usize {
        self.shared.destroyed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Sandbox for TaggedSandbox {
    fn kind(&self) -> &str {
        "tagged"
    }
    fn id(&self) -> &str {
        &self.tag
    }
    fn status(&self) -> SandboxStatus {
        SandboxStatus::Ready
    }
    fn info(&self) -> SandboxInfo {
        SandboxInfo::enabled("tagged", vec![("Tag".into(), self.tag.clone())])
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }
    async fn start(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.shared.destroyed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl loop_workspace::Workspace for TaggedSandbox {
    fn driver_id(&self) -> &str {
        "tagged"
    }
    fn capabilities(&self) -> loop_workspace::WorkspaceCapabilities {
        loop_workspace::WorkspaceCapabilities::default()
    }
}

struct TaggedEnv {
    tag: String,
    host: HostExecutionEnv,
    shared: Arc<Shared>,
}

#[async_trait]
impl Shell for TaggedEnv {
    async fn exec(
        &self,
        command: &str,
        _options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        self.shared.commands.lock().push(command.to_owned());
        if let Some(gate) = &self.shared.gate {
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
        }
        Ok(ShellOutput {
            stdout: format!("{}\n", self.tag),
            stderr: String::new(),
            exit_code: 0,
            truncated: false,
        })
    }
}

#[async_trait]
impl FileSystem for TaggedEnv {
    fn cwd(&self) -> &Path {
        self.host.cwd()
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.host.absolute_path(path)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.host.join_path(base, child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.host.read_text_file(path).await
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.host.read_binary_file(path).await
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.host.write_file(path, data).await
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.host.append_file(path, data).await
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.host.file_info(path).await
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.host.list_dir(path).await
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.host.exists(path).await
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.host.create_dir(path).await
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.host.remove(path).await
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.host.canonical_path(path).await
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.host.create_temp_dir(prefix).await
    }
    async fn file_identity(
        &self,
        path: &Path,
    ) -> Result<Option<crate::harness::types::FileIdentity>, FileError> {
        self.host.file_identity(path).await
    }
}
