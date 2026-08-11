//! Pluggable sandbox execution environments.

mod krun;
mod podman;
mod registry;
mod traits;

pub use krun::{
    KrunExecutionEnv, KrunIsolation, KrunSandbox, KrunSandboxFactory, LocalSandboxRuntime,
    KRUN_DEFAULT_IMAGE, KRUN_DEFAULT_RUNTIME, KRUN_GUEST_WORKDIR, LOCAL_DEFAULT_RUNTIME,
};
pub use podman::{
    check_krun_deps, check_local_sandbox_deps, PodmanClient, PodmanExecOpts, PodmanRunOpts,
    RealPodmanClient,
};
pub use registry::SandboxRegistry;
pub use traits::{
    Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxMode, SandboxStatus,
};
