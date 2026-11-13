//! Pluggable sandbox execution environments.

mod krun;
mod podman;
mod protect;
pub mod seatbelt;
mod tagged;
mod traits;

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
