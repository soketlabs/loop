//! The built-in Workspace drivers.
//!
//! | Driver id  | Kind    | Platforms | Backend |
//! |------------|---------|-----------|---------|
//! | `podman`   | `local` | Linux     | [`KrunSandboxFactory`] (Podman with runc, crun, runsc or krun) |
//! | `seatbelt` | `local` | macOS     | [`SeatbeltSandboxFactory`] |
//! | `host`     | `off`   | all       | [`HostExecutionEnv`] |
//!
//! `podman` and `seatbelt` are adapters ([`FactoryDriver`]) over the
//! existing factories; the backends do not change. The `host` driver wraps
//! the host environment that tools use with the sandbox off. The app does
//! not route the sandbox-off path through it yet (task C4 does).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use loop_workspace::{
    FactoryDriver, HostOs, Isolation, NetworkPolicy, PathNamespace, ResourceLimits, Workspace,
    WorkspaceCapabilities, WorkspaceDriver, WorkspaceError, WorkspaceSpec,
};
use parking_lot::Mutex;

use super::krun::{KrunIsolation, KrunSandboxFactory};
#[cfg(unix)]
use super::seatbelt::SeatbeltSandboxFactory;
use super::{Sandbox, SandboxError, SandboxInfo, SandboxStatus};
use crate::harness::env::HostExecutionEnv;
use crate::harness::types::ExecutionEnv;

/// Driver id of the Podman sandbox.
pub const PODMAN_DRIVER_ID: &str = "podman";
/// Driver id of the macOS Seatbelt sandbox.
pub const SEATBELT_DRIVER_ID: &str = "seatbelt";
/// Driver id of the host (no sandbox).
pub const HOST_DRIVER_ID: &str = "host";
/// Kind served by the host driver (`sandbox.mode = off`).
pub const HOST_KIND: &str = "off";

/// Bytes of output the host shell keeps per command (see `env/host.rs`).
const HOST_OUTPUT_CAP: u64 = 1024 * 1024;

fn option_str<'a>(spec: &'a WorkspaceSpec, key: &str) -> Option<&'a str> {
    spec.options.get(key).and_then(|v| v.as_str())
}

fn option_u64(spec: &WorkspaceSpec, key: &str) -> Option<u64> {
    spec.options.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

/// A CPU count (`2`, `"1.5"`, `0.5`) in thousandths of a CPU.
fn option_cpu_millis(spec: &WorkspaceSpec, key: &str) -> Option<u32> {
    let cpus = spec.options.get(key).and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    })?;
    (cpus.is_finite() && cpus > 0.0)
        .then(|| (cpus * 1000.0).round().min(f64::from(u32::MAX)) as u32)
}

fn option_bool(spec: &WorkspaceSpec, key: &str) -> bool {
    spec.options
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Capabilities of a Podman Workspace for `spec` (the defaults of
/// [`super::KrunSandbox::new`]).
pub fn podman_capabilities(spec: &WorkspaceSpec) -> WorkspaceCapabilities {
    let isolation = match option_str(spec, "isolation").and_then(KrunIsolation::parse) {
        Some(KrunIsolation::Partial) => Isolation::Partial,
        _ => Isolation::Full,
    };
    WorkspaceCapabilities {
        isolation,
        persistent: true,
        outlives_process: true,
        confines_reads: true,
        confines_writes: true,
        confines_shell: true,
        own_environment: true,
        // `exec` passes no variable from the caller into the container.
        env_allowlist: Some(Vec::new()),
        path_namespace: PathNamespace::Host,
        protected_paths_enforced: true,
        // `sandbox.network` and `sandbox.allowWrite` are not enforced on
        // Linux yet (docs/configuration.md).
        honours_allow_write: false,
        network: NetworkPolicy::NotEnforced,
        limits: ResourceLimits {
            cpu_millis: Some(option_cpu_millis(spec, "cpus").unwrap_or(2000)),
            memory_mib: Some(option_u64(spec, "ram_mib").unwrap_or(2048)),
            ..ResourceLimits::default()
        },
        ..WorkspaceCapabilities::default()
    }
}

/// Capabilities of a Seatbelt Workspace for `spec`.
pub fn seatbelt_capabilities(spec: &WorkspaceSpec) -> WorkspaceCapabilities {
    WorkspaceCapabilities {
        isolation: Isolation::Partial,
        persistent: true,
        // Reads are allowed except credential paths (and, with
        // `denyHomeReads`, most of the home folder): not confined to the
        // Workspace.
        confines_reads: false,
        confines_writes: true,
        confines_shell: true,
        own_environment: true,
        env_allowlist: Some(
            super::seatbelt::SEATBELT_ENV_ALLOWLIST
                .iter()
                .map(|n| (*n).to_owned())
                .collect(),
        ),
        path_namespace: PathNamespace::Host,
        protected_paths_enforced: true,
        honours_allow_write: true,
        network: if option_bool(spec, "network") {
            NetworkPolicy::Allowed
        } else {
            NetworkPolicy::Denied
        },
        limits: ResourceLimits {
            max_output_bytes: Some(HOST_OUTPUT_CAP),
            ..ResourceLimits::default()
        },
        ..WorkspaceCapabilities::default()
    }
}

/// Capabilities of a host Workspace for `spec`.
pub fn host_capabilities(spec: &WorkspaceSpec) -> WorkspaceCapabilities {
    WorkspaceCapabilities {
        isolation: Isolation::None,
        persistent: true,
        confines_reads: option_bool(spec, "confineReads"),
        confines_writes: false,
        own_environment: false,
        path_namespace: PathNamespace::Host,
        protected_paths_enforced: false,
        honours_allow_write: false,
        network: NetworkPolicy::NotEnforced,
        limits: ResourceLimits {
            max_output_bytes: Some(HOST_OUTPUT_CAP),
            ..ResourceLimits::default()
        },
        ..WorkspaceCapabilities::default()
    }
}

/// The Podman driver (`sandbox.mode = local` on Linux).
pub fn podman_driver() -> Arc<dyn WorkspaceDriver> {
    Arc::new(
        FactoryDriver::new(Arc::new(KrunSandboxFactory))
            .with_id(PODMAN_DRIVER_ID)
            .with_platforms(&[HostOs::Linux])
            .with_capabilities(podman_capabilities),
    )
}

/// The Seatbelt driver (`sandbox.mode = local` on macOS).
#[cfg(unix)]
pub fn seatbelt_driver() -> Arc<dyn WorkspaceDriver> {
    Arc::new(
        FactoryDriver::new(Arc::new(SeatbeltSandboxFactory))
            .with_id(SEATBELT_DRIVER_ID)
            .with_platforms(&[HostOs::MacOs])
            .with_capabilities(seatbelt_capabilities),
    )
}

/// Every built-in driver, in selection order. Platform support is data on
/// each driver; selection checks it at run time.
pub fn builtin_workspace_drivers() -> Vec<Arc<dyn WorkspaceDriver>> {
    let mut drivers = vec![podman_driver()];
    #[cfg(unix)]
    drivers.push(seatbelt_driver());
    drivers.push(Arc::new(HostWorkspaceDriver));
    drivers
}

/// The host driver: tools run on the Runtime host in the project folder.
///
/// Spec options: `confineReads` (bool) confines every file read to the
/// project folder, as the tool lock does
/// ([`HostExecutionEnv::confine_reads`]).
pub struct HostWorkspaceDriver;

#[async_trait]
impl WorkspaceDriver for HostWorkspaceDriver {
    fn id(&self) -> &str {
        HOST_DRIVER_ID
    }
    fn kind(&self) -> &str {
        HOST_KIND
    }
    fn capabilities(&self) -> WorkspaceCapabilities {
        host_capabilities(&WorkspaceSpec::default())
    }
    async fn create(&self, spec: WorkspaceSpec) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let capabilities = host_capabilities(&spec);
        let folder = spec.host_folder().ok_or(WorkspaceError::Unmet {
            driver: HOST_DRIVER_ID.into(),
            setting: "workspace source",
            reason: "the host driver uses a host folder in place",
        })?;
        let workdir = if folder.as_os_str().is_empty() {
            std::env::current_dir().map_err(|e| {
                WorkspaceError::Sandbox(SandboxError::StartFailed(format!("current_dir: {e}")))
            })?
        } else {
            folder.to_path_buf()
        };
        let mut env = HostExecutionEnv::new(workdir.clone());
        if capabilities.confines_reads {
            env = env.confine_reads(workdir);
        }
        Ok(Arc::new(HostWorkspace {
            id: loop_ai::new_id(),
            env: Arc::new(env),
            capabilities,
            status: Mutex::new(SandboxStatus::Created),
            destroyed: AtomicBool::new(false),
        }))
    }
}

/// A host Workspace. Start, stop and destroy only change its status; destroy
/// never removes the project folder.
pub struct HostWorkspace {
    id: String,
    env: Arc<HostExecutionEnv>,
    capabilities: WorkspaceCapabilities,
    status: Mutex<SandboxStatus>,
    destroyed: AtomicBool,
}

impl HostWorkspace {
    /// The project folder.
    pub fn workdir(&self) -> PathBuf {
        use crate::harness::types::FileSystem;
        self.env.cwd().to_path_buf()
    }
}

#[async_trait]
impl Sandbox for HostWorkspace {
    fn kind(&self) -> &str {
        HOST_KIND
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn status(&self) -> SandboxStatus {
        *self.status.lock()
    }
    fn info(&self) -> SandboxInfo {
        SandboxInfo::off()
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }
    async fn start(&self) -> Result<(), SandboxError> {
        if self.destroyed.load(Ordering::Acquire) {
            return Err(SandboxError::NotReady("workspace was destroyed".into()));
        }
        *self.status.lock() = SandboxStatus::Ready;
        Ok(())
    }
    async fn stop(&self) -> Result<(), SandboxError> {
        *self.status.lock() = SandboxStatus::Stopped;
        Ok(())
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.destroyed.store(true, Ordering::Release);
        *self.status.lock() = SandboxStatus::Stopped;
        Ok(())
    }
}

#[async_trait]
impl Workspace for HostWorkspace {
    fn driver_id(&self) -> &str {
        HOST_DRIVER_ID
    }
    fn capabilities(&self) -> WorkspaceCapabilities {
        self.capabilities.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_workspace::WorkspaceDriverRegistry;
    use serde_json::json;

    #[test]
    fn local_kind_selects_by_platform() {
        let reg = WorkspaceDriverRegistry::new();
        for d in builtin_workspace_drivers() {
            reg.register_builtin(d);
        }
        assert_eq!(
            reg.select("local", HostOs::Linux).unwrap().id(),
            PODMAN_DRIVER_ID
        );
        #[cfg(unix)]
        assert_eq!(
            reg.select("local", HostOs::MacOs).unwrap().id(),
            SEATBELT_DRIVER_ID
        );
        assert!(reg.select("local", HostOs::Other).is_err());
        assert_eq!(
            reg.select("off", HostOs::Other).unwrap().id(),
            HOST_DRIVER_ID
        );
    }

    #[test]
    fn capabilities_follow_options() {
        let spec = |options| WorkspaceSpec {
            options,
            ..Default::default()
        };
        let partial = podman_capabilities(&spec(json!({"isolation": "partial", "cpus": 4})));
        assert_eq!(partial.isolation, Isolation::Partial);
        assert_eq!(partial.limits.cpu_millis, Some(4000));
        for (cpus, millis) in [(json!("1.5"), 1500), (json!(0.25), 250), (json!("2"), 2000)] {
            assert_eq!(
                podman_capabilities(&spec(json!({ "cpus": cpus })))
                    .limits
                    .cpu_millis,
                Some(millis)
            );
        }
        assert_eq!(
            podman_capabilities(&spec(json!({"cpus": "x"})))
                .limits
                .cpu_millis,
            Some(2000)
        );
        assert_eq!(
            podman_capabilities(&spec(json!({}))).isolation,
            Isolation::Full
        );
        assert_eq!(
            seatbelt_capabilities(&spec(json!({"network": true}))).network,
            NetworkPolicy::Allowed
        );
        assert!(host_capabilities(&spec(json!({"confineReads": true}))).confines_reads);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn host_env_reports_links_owner_and_link_count() {
        use crate::harness::types::{FileSystem, PathKind};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("f"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("f"), root.join("l")).unwrap();
        std::fs::hard_link(root.join("f"), root.join("h")).unwrap();
        let env = HostExecutionEnv::new(&root);

        let link = env.symlink_metadata(&root.join("l")).await.unwrap();
        assert_eq!(link.kind, PathKind::Symlink);
        assert_eq!(
            env.read_link(&root.join("l")).await.unwrap(),
            root.join("f")
        );

        let through = env.resolve_for_policy(&root.join("l")).await.unwrap();
        assert!(through.exists && through.has_link);
        assert_eq!(through.resolved, root.join("f"));
        assert_eq!(through.nlink, Some(2));
        // SAFETY: geteuid has no arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        assert_eq!(through.uid, Some(uid));

        let plain = env.resolve_for_policy(&root.join("f")).await.unwrap();
        assert!(!plain.has_link);
        let missing = env
            .resolve_for_policy(&root.join("l/../none"))
            .await
            .unwrap();
        assert!(!missing.exists);
    }
}
