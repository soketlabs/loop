//! Pluggable sandbox execution environments.

mod drivers;
#[cfg(unix)]
mod host_ws;
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
#[cfg(unix)]
pub use host_ws::leaked_host_processes;
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
    register_workspace_driver, registered_workspace_drivers, replace_builtin_workspace_driver,
    reset_registered_workspace_drivers, Attached, ExitState, FactoryDriver, FileChunk, FileTag,
    HostOs, Isolation, NetworkPolicy, PathNamespace, ProcessEvent, ProcessEventKind, ProcessId,
    ProcessInfo, ProcessSignal, PtySize, ResourceLimits, SandboxWorkspace, SnapshotId,
    SpawnRequest, StreamId, UploadId, UploadRequest, Workspace, WorkspaceCapabilities,
    WorkspaceDriver, WorkspaceDriverRegistry, WorkspaceError, WorkspacePath, WorkspaceProcess,
    WorkspaceSource, WorkspaceSpec, WorkspaceStatus,
};

/// Whether `sandbox` and `workspace` are the same object (address only).
pub fn same_sandbox(
    sandbox: &std::sync::Arc<dyn Sandbox>,
    workspace: &std::sync::Arc<dyn Workspace>,
) -> bool {
    std::ptr::addr_eq(
        std::sync::Arc::as_ptr(sandbox),
        std::sync::Arc::as_ptr(workspace),
    )
}
