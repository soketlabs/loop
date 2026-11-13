//! Pluggable sandbox execution environments.

mod drivers;
mod krun;
mod podman;
mod protect;
pub mod seatbelt;
mod tagged;
mod traits;

#[cfg(unix)]
pub use drivers::seatbelt_driver;
pub use drivers::{
    builtin_workspace_drivers, host_capabilities, podman_capabilities, podman_driver,
    seatbelt_capabilities, HostWorkspace, HostWorkspaceDriver, HOST_DRIVER_ID, HOST_KIND,
    PODMAN_DRIVER_ID, SEATBELT_DRIVER_ID,
};
pub use krun::{
    KrunExecutionEnv, KrunIsolation, KrunSandbox, KrunSandboxFactory, LocalSandboxRuntime,
    KRUN_DEFAULT_IMAGE, KRUN_DEFAULT_RUNTIME, LOCAL_DEFAULT_RUNTIME,
};
pub use loop_workspace::SandboxRegistry;
pub use podman::{
    check_krun_deps, check_local_sandbox_deps, PodmanClient, PodmanExecOpts, PodmanRunOpts,
    RealPodmanClient,
};
pub use protect::ProtectedBind;
pub use tagged::TaggedSandbox;
pub use traits::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxMode, SandboxStatus,
};
// The Workspace driver contract (see `docs/design/workspace-driver.md`).
pub use loop_workspace::{
    register_workspace_driver, registered_workspace_drivers, FactoryDriver, FileChunk, HostOs,
    Isolation, NetworkPolicy, ProcessEvent, ProcessExit, ProcessSignal, PtySize, ResourceLimits,
    SandboxWorkspace, SnapshotId, SpawnRequest, Workspace, WorkspaceCapabilities, WorkspaceDriver,
    WorkspaceDriverRegistry, WorkspaceError, WorkspacePath, WorkspaceProcess, WorkspaceSpec,
    WorkspaceStatus,
};
