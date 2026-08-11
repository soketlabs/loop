//! Local Podman + crun-krun sandbox (`kind` = `local`).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use parking_lot::RwLock;
use serde_json::json;

use crate::harness::env::HostExecutionEnv;
use crate::harness::sandbox::podman::{
    PodmanClient, PodmanExecOpts, PodmanRunOpts, RealPodmanClient,
};
use crate::harness::sandbox::traits::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus,
};
use crate::harness::types::{
    ExecutionError, ExecutionErrorCode, ExecutionEnv, FileError, FileErrorCode, FileInfo,
    FileSystem, Shell, ShellExecOptions, ShellOutput,
};

/// Default OCI image.
pub const KRUN_DEFAULT_IMAGE: &str = "fedora:latest";
/// Default OCI runtime for `/sandbox local` (rootless containers).
pub const LOCAL_DEFAULT_RUNTIME: &str = "runc";
/// Legacy alias — prefer [`LOCAL_DEFAULT_RUNTIME`].
pub const KRUN_DEFAULT_RUNTIME: &str = LOCAL_DEFAULT_RUNTIME;

/// OCI runtime used by the local Podman sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSandboxRuntime {
    /// Rootless containers via `crun`.
    Crun,
    /// Rootless containers via `runc` (default).
    Runc,
    /// gVisor user-space kernel (`runsc`).
    Runsc,
    /// libkrun microVM (`krun`).
    Krun,
}

impl LocalSandboxRuntime {
    /// Parse from settings / CLI (`crun`, `runc`, `runsc`/`gvisor`, `krun`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "crun" => Some(Self::Crun),
            "runc" => Some(Self::Runc),
            "runsc" | "gvisor" => Some(Self::Runsc),
            "krun" => Some(Self::Krun),
            _ => None,
        }
    }

    /// Value passed to `podman --runtime`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crun => "crun",
            Self::Runc => "runc",
            Self::Runsc => "runsc",
            Self::Krun => "krun",
        }
    }
}

/// Isolation level for the local sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KrunIsolation {
    /// FS + shell via `podman exec`.
    Full,
    /// Host FS (jailed); shell via `podman exec`.
    Partial,
}

impl KrunIsolation {
    /// Parse from settings / options string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "partial" => Some(Self::Partial),
            _ => None,
        }
    }

    /// Stable string for settings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
        }
    }
}

/// Factory for [`KrunSandbox`] (`kind` = `local`).
pub struct KrunSandboxFactory;

#[async_trait]
impl SandboxFactory for KrunSandboxFactory {
    fn kind(&self) -> &str {
        "local"
    }

    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(KrunSandbox::new(config)))
    }
}

/// Local Podman sandbox (`kind` = `local`) with selectable OCI runtime.
pub struct KrunSandbox {
    id: String,
    workdir: PathBuf,
    isolation: KrunIsolation,
    image: String,
    runtime: String,
    cpus: String,
    ram_mib: String,
    client: Arc<dyn PodmanClient>,
    status: RwLock<SandboxStatus>,
    container_id: RwLock<Option<String>>,
    env: RwLock<Option<Arc<dyn ExecutionEnv>>>,
}

impl KrunSandbox {
    /// Create from config (container started on [`Sandbox::start`]).
    pub fn new(config: SandboxConfig) -> Self {
        Self::with_client(config, Arc::new(RealPodmanClient::new()))
    }

    /// Create with an injectable [`PodmanClient`] (tests).
    pub fn with_client(config: SandboxConfig, client: Arc<dyn PodmanClient>) -> Self {
        let isolation = config
            .options
            .get("isolation")
            .and_then(|v| v.as_str())
            .and_then(KrunIsolation::parse)
            .unwrap_or(KrunIsolation::Full);
        let image = config
            .options
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or(KRUN_DEFAULT_IMAGE)
            .to_string();
        let runtime = config
            .options
            .get("runtime")
            .and_then(|v| v.as_str())
            .and_then(LocalSandboxRuntime::parse)
            .unwrap_or(LocalSandboxRuntime::Runc)
            .as_str()
            .to_string();
        let cpus = config
            .options
            .get("cpus")
            .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())))
            .unwrap_or_else(|| "2".into());
        let ram_mib = config
            .options
            .get("ram_mib")
            .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())))
            .unwrap_or_else(|| "2048".into());
        let workdir = if config.workdir.as_os_str().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            config.workdir
        };
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        Self {
            id: loop_ai::new_id(),
            workdir,
            isolation,
            image,
            runtime,
            cpus,
            ram_mib,
            client,
            status: RwLock::new(SandboxStatus::Created),
            container_id: RwLock::new(None),
            env: RwLock::new(None),
        }
    }

    /// Host workdir bind-mounted into the guest.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Isolation level.
    pub fn isolation(&self) -> KrunIsolation {
        self.isolation
    }

    /// OCI runtime id (`crun`, `runc`, `runsc`, `krun`).
    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    /// Build a [`SandboxConfig`] for CLI/settings.
    pub fn config_for(
        workdir: PathBuf,
        isolation: KrunIsolation,
        runtime: LocalSandboxRuntime,
    ) -> SandboxConfig {
        SandboxConfig {
            workdir,
            options: json!({
                "isolation": isolation.as_str(),
                "runtime": runtime.as_str(),
            }),
            labels: Default::default(),
        }
    }
}

#[async_trait]
impl Sandbox for KrunSandbox {
    fn kind(&self) -> &str {
        "local"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> SandboxStatus {
        *self.status.read()
    }

    fn info(&self) -> SandboxInfo {
        let container = self
            .container_id
            .read()
            .clone()
            .unwrap_or_else(|| "—".into());
        SandboxInfo::enabled(
            self.kind(),
            vec![
                ("Status".into(), self.status().to_string()),
                ("Isolation".into(), self.isolation.as_str().into()),
                ("Runtime".into(), self.runtime.clone()),
                ("Image".into(), self.image.clone()),
                ("Workdir".into(), self.workdir.display().to_string()),
                ("Container".into(), container),
                ("CPUs".into(), self.cpus.clone()),
                ("RAM".into(), format!("{} MiB", self.ram_mib)),
                ("Id".into(), self.id.clone()),
            ],
        )
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env
            .read()
            .clone()
            .expect("sandbox env available only after start")
    }

    async fn start(&self) -> Result<(), SandboxError> {
        if *self.status.read() == SandboxStatus::Ready {
            return Ok(());
        }
        *self.status.write() = SandboxStatus::Starting;

        self.client.preflight(&self.runtime).await?;

        // Use the full id (no hyphens). UUID v7's first 8 hex chars are timestamp
        // bits and collide across instances started within ~65s.
        let name = format!("loop-sandbox-{}", self.id.replace('-', ""));
        // Mount at the same absolute path inside the guest so host and container
        // paths match (e.g. `/home/u/proj/README.md` → `/home/u/proj/README.md`).
        let guest_workdir = self.workdir.to_string_lossy().into_owned();
        let container_id = self
            .client
            .run(PodmanRunOpts {
                name,
                image: self.image.clone(),
                host_workdir: self.workdir.clone(),
                guest_workdir,
                runtime: self.runtime.clone(),
                cpus: self.cpus.clone(),
                ram_mib: self.ram_mib.clone(),
            })
            .await
            .map_err(|e| {
                *self.status.write() = SandboxStatus::Failed;
                e
            })?;

        let env: Arc<dyn ExecutionEnv> = Arc::new(KrunExecutionEnv::new(
            Arc::clone(&self.client),
            container_id.clone(),
            self.workdir.clone(),
            self.isolation,
        ));

        *self.container_id.write() = Some(container_id);
        *self.env.write() = Some(env);
        *self.status.write() = SandboxStatus::Ready;
        Ok(())
    }

    async fn stop(&self) -> Result<(), SandboxError> {
        *self.status.write() = SandboxStatus::Stopping;
        *self.env.write() = None;
        let id = self.container_id.write().take();
        if let Some(id) = id {
            let _ = self.client.stop(&id).await;
        }
        *self.status.write() = SandboxStatus::Stopped;
        Ok(())
    }

    async fn destroy(&self) -> Result<(), SandboxError> {
        *self.env.write() = None;
        let id = self.container_id.write().take();
        if let Some(id) = id {
            let _ = self.client.stop(&id).await;
            let _ = self.client.rm(&id).await;
        }
        *self.status.write() = SandboxStatus::Stopped;
        Ok(())
    }
}

/// Execution environment backed by a krun microVM (and optionally host FS).
pub struct KrunExecutionEnv {
    client: Arc<dyn PodmanClient>,
    container_id: String,
    host_workdir: PathBuf,
    #[allow(dead_code)]
    isolation: KrunIsolation,
    /// Present when isolation is Partial.
    host_fs: Option<JailedHostFs>,
}

impl KrunExecutionEnv {
    /// Create env for a running container.
    pub fn new(
        client: Arc<dyn PodmanClient>,
        container_id: String,
        host_workdir: PathBuf,
        isolation: KrunIsolation,
    ) -> Self {
        let host_fs = match isolation {
            KrunIsolation::Partial => Some(JailedHostFs {
                inner: HostExecutionEnv::new(&host_workdir),
                root: host_workdir.clone(),
            }),
            KrunIsolation::Full => None,
        };
        Self {
            client,
            container_id,
            host_workdir,
            isolation,
            host_fs,
        }
    }

    /// Resolve a tool path to the absolute path used inside the guest.
    ///
    /// The host workdir is bind-mounted at the same absolute path, so guest and
    /// host paths are identical (no `/workspace` rewrite).
    fn guest_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                "path escape (..) rejected",
            ));
        }
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.host_workdir.join(path)
        };
        let root =
            std::fs::canonicalize(&self.host_workdir).unwrap_or_else(|_| self.host_workdir.clone());
        // File may not exist yet; check prefix without requiring canonicalize.
        if !abs.starts_with(&root) && !abs.starts_with(&self.host_workdir) {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                format!("path escapes sandbox root: {}", abs.display()),
            ));
        }
        Ok(abs)
    }

    fn guest_cwd(&self, options: &ShellExecOptions) -> Result<String, ExecutionError> {
        let cwd = options
            .cwd
            .clone()
            .unwrap_or_else(|| self.host_workdir.clone());
        if cwd.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Other,
                "cwd path escape rejected",
            ));
        }
        let guest = self.guest_path(&cwd).map_err(|e| {
            ExecutionError::new(ExecutionErrorCode::Other, e.to_string())
        })?;
        Ok(guest.to_string_lossy().into_owned())
    }

    async fn exec_argv(
        &self,
        argv: Vec<String>,
        workdir: Option<String>,
        stdin: Option<Vec<u8>>,
        timeout_ms: Option<u64>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<ShellOutput, ExecutionError> {
        self.client
            .exec(PodmanExecOpts {
                container: self.container_id.clone(),
                workdir,
                argv,
                stdin,
                timeout_ms,
                cancel,
            })
            .await
    }

    async fn exec_sh(
        &self,
        script: &str,
        workdir: Option<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<ShellOutput, ExecutionError> {
        self.exec_argv(
            vec!["sh".into(), "-c".into(), script.into()],
            workdir,
            stdin,
            None,
            None,
        )
        .await
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn map_exec_file(err: ExecutionError) -> FileError {
    FileError::new(FileErrorCode::Io, err.to_string())
}

#[async_trait]
impl FileSystem for KrunExecutionEnv {
    fn cwd(&self) -> &Path {
        &self.host_workdir
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.absolute_path(path);
        }
        let _ = self.guest_path(path)?;
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.host_workdir.join(path))
        }
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.read_text_file(path).await;
        }
        let guest = self.guest_path(path)?;
        let out = self
            .exec_sh(&format!("cat -- {}", sh_quote(&guest.to_string_lossy())), None, None)
            .await
            .map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::NotFound,
                out.stderr.trim().to_string(),
            ));
        }
        Ok(out.stdout)
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.read_binary_file(path).await;
        }
        let guest = self.guest_path(path)?;
        let out = self
            .exec_sh(
                &format!("base64 -w0 -- {}", sh_quote(&guest.to_string_lossy())),
                None,
                None,
            )
            .await
            .map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::NotFound,
                out.stderr.trim().to_string(),
            ));
        }
        base64::engine::general_purpose::STANDARD
            .decode(out.stdout.trim())
            .map_err(|e| FileError::new(FileErrorCode::Io, e.to_string()))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.write_file(path, data).await;
        }
        let guest = self.guest_path(path)?;
        let guest_s = guest.to_string_lossy();
        let parent = guest
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.host_workdir.to_string_lossy().into_owned());
        let script = format!(
            "mkdir -p {} && cat > {}",
            sh_quote(&parent),
            sh_quote(&guest_s)
        );
        let out = self
            .exec_sh(&script, None, Some(data.to_vec()))
            .await
            .map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::Io,
                out.stderr.trim().to_string(),
            ));
        }
        Ok(())
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.append_file(path, data).await;
        }
        let guest = self.guest_path(path)?;
        let guest_s = guest.to_string_lossy();
        let parent = guest
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.host_workdir.to_string_lossy().into_owned());
        let script = format!(
            "mkdir -p {} && cat >> {}",
            sh_quote(&parent),
            sh_quote(&guest_s)
        );
        let out = self
            .exec_sh(&script, None, Some(data.to_vec()))
            .await
            .map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::Io,
                out.stderr.trim().to_string(),
            ));
        }
        Ok(())
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.file_info(path).await;
        }
        let guest = self.guest_path(path)?;
        let host_abs = self.absolute_path(path)?;
        let script = format!(
            "if [ ! -e {p} ]; then echo MISSING; exit 1; fi; \
             if [ -d {p} ]; then echo DIR; elif [ -f {p} ]; then echo FILE; else echo OTHER; fi; \
             stat -c %s -- {p} 2>/dev/null || wc -c < {p}",
            p = sh_quote(&guest.to_string_lossy())
        );
        let out = self.exec_sh(&script, None, None).await.map_err(map_exec_file)?;
        if out.exit_code != 0 || out.stdout.contains("MISSING") {
            return Err(FileError::new(
                FileErrorCode::NotFound,
                format!("not found: {}", path.display()),
            ));
        }
        let mut lines = out.stdout.lines();
        let kind = lines.next().unwrap_or("").trim();
        let size: u64 = lines
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .unwrap_or(0);
        Ok(FileInfo {
            path: host_abs,
            is_dir: kind == "DIR",
            is_file: kind == "FILE",
            size,
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.list_dir(path).await;
        }
        let guest = self.guest_path(path)?;
        let script = format!(
            "if [ ! -d {p} ]; then echo NOTDIR >&2; exit 1; fi; \
             find {p} -mindepth 1 -maxdepth 1 -printf '%y\\t%s\\t%P\\n'",
            p = sh_quote(&guest.to_string_lossy())
        );
        let out = self.exec_sh(&script, None, None).await.map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::NotADirectory,
                out.stderr.trim().to_string(),
            ));
        }
        let base = self.absolute_path(path)?;
        let mut entries = Vec::new();
        for line in out.stdout.lines() {
            let mut parts = line.splitn(3, '\t');
            let kind = parts.next().unwrap_or("");
            let size: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
            let name = parts.next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            entries.push(FileInfo {
                path: base.join(name),
                is_dir: kind == "d",
                is_file: kind == "f",
                size,
            });
        }
        Ok(entries)
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.exists(path).await;
        }
        let guest = self.guest_path(path)?;
        let out = self
            .exec_sh(
                &format!(
                    "if [ -e {} ]; then echo 1; else echo 0; fi",
                    sh_quote(&guest.to_string_lossy())
                ),
                None,
                None,
            )
            .await
            .map_err(map_exec_file)?;
        Ok(out.stdout.trim() == "1")
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.create_dir(path).await;
        }
        let guest = self.guest_path(path)?;
        let out = self
            .exec_sh(
                &format!("mkdir -p -- {}", sh_quote(&guest.to_string_lossy())),
                None,
                None,
            )
            .await
            .map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::Io,
                out.stderr.trim().to_string(),
            ));
        }
        Ok(())
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.remove(path).await;
        }
        let guest = self.guest_path(path)?;
        let out = self
            .exec_sh(
                &format!("rm -rf -- {}", sh_quote(&guest.to_string_lossy())),
                None,
                None,
            )
            .await
            .map_err(map_exec_file)?;
        if out.exit_code != 0 {
            return Err(FileError::new(
                FileErrorCode::Io,
                out.stderr.trim().to_string(),
            ));
        }
        Ok(())
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        if let Some(fs) = &self.host_fs {
            return fs.canonical_path(path).await;
        }
        // Best-effort: resolve under host workdir without requiring the file to exist on host.
        self.absolute_path(path)
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        if let Some(fs) = &self.host_fs {
            let name = format!("{prefix}-{}", loop_ai::new_id());
            let path = self.host_workdir.join(name);
            fs.create_dir(&path).await?;
            return Ok(path);
        }
        let name = format!("{prefix}-{}", loop_ai::new_id());
        let host_path = self.host_workdir.join(&name);
        self.create_dir(&host_path).await?;
        Ok(host_path)
    }
}

#[async_trait]
impl Shell for KrunExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let workdir = self.guest_cwd(&options)?;
        self.exec_argv(
            vec!["sh".into(), "-c".into(), command.to_string()],
            Some(workdir),
            None,
            options.timeout_ms,
            options.cancel,
        )
        .await
    }
}

/// Path-jailed host FS for partial isolation.
struct JailedHostFs {
    inner: HostExecutionEnv,
    root: PathBuf,
}

impl JailedHostFs {
    fn assert_in_root(&self, path: &Path) -> Result<PathBuf, FileError> {
        let abs = self.inner.absolute_path(path)?;
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
impl FileSystem for JailedHostFs {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
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
        let name = format!("{prefix}-{}", loop_ai::new_id());
        let path = self.root.join(name);
        self.inner.create_dir(&path).await?;
        Ok(path)
    }
}
