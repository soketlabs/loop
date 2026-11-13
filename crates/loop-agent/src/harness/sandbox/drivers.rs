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
    FactoryDriver, HostOs, Isolation, NetworkPolicy, ResourceLimits, Workspace,
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
        own_environment: true,
        // `sandbox.network` and `sandbox.allowWrite` are not enforced on
        // Linux yet (docs/configuration.md).
        network: NetworkPolicy::NotEnforced,
        limits: ResourceLimits {
            cpu_millis: Some(
                option_u64(spec, "cpus")
                    .unwrap_or(2)
                    .saturating_mul(1000)
                    .min(u64::from(u32::MAX)) as u32,
            ),
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
        own_environment: true,
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
        let workdir = if spec.workdir.as_os_str().is_empty() {
            std::env::current_dir().map_err(|e| {
                WorkspaceError::Sandbox(SandboxError::StartFailed(format!("current_dir: {e}")))
            })?
        } else {
            spec.workdir.clone()
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
            reg.register_driver(d);
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
}
