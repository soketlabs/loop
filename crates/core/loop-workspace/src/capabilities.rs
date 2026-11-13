//! What a Workspace driver can do and which limits it enforces.

use serde::{Deserialize, Serialize};

/// How strongly the Workspace separates commands from the Runtime host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Commands run as the Runtime user on the host.
    #[default]
    None,
    /// Some host access is denied (Seatbelt, a container with host file tools).
    Partial,
    /// Files and commands are in a separate system (container, microVM, remote).
    Full,
}

/// The network access of commands in the Workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "policy", content = "hosts")]
pub enum NetworkPolicy {
    /// The driver does not limit the network (the host, or a backend that
    /// does not enforce `sandbox.network` yet).
    #[default]
    NotEnforced,
    /// All network access is allowed, by decision.
    Allowed,
    /// All network access is denied.
    Denied,
    /// Only these hosts can be reached, through an egress proxy.
    Allowlist(Vec<String>),
}

/// Resource limits the driver enforces. `None` means no limit from the driver.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU limit in thousandths of a CPU.
    pub cpu_millis: Option<u32>,
    /// Memory limit in MiB.
    pub memory_mib: Option<u64>,
    /// Disk limit in MiB.
    pub disk_mib: Option<u64>,
    /// Bytes of combined output one `exec` keeps; more is cut and
    /// `ShellOutput::truncated` is set.
    pub max_output_bytes: Option<u64>,
    /// Longest `exec` the driver allows, in milliseconds.
    pub max_exec_timeout_ms: Option<u64>,
}

/// What a driver (or one Workspace of it) can do.
///
/// A `false` flag means the matching trait method returns
/// `WorkspaceError::Unsupported`. The conformance kit checks that the flags
/// and the methods agree.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceCapabilities {
    /// Isolation strength.
    pub isolation: Isolation,
    /// Files and commands are on another machine; paths are not host paths.
    pub remote: bool,
    /// Files stay after `stop` and after the Runtime process exits.
    pub persistent: bool,
    /// The Workspace keeps running after the Runtime process exits unless
    /// it is destroyed (`Sandbox::outlives_process`).
    pub outlives_process: bool,
    /// File reads outside the Workspace (and its allowed read paths) fail.
    pub confines_reads: bool,
    /// File writes and command writes outside the Workspace (and
    /// `sandbox.allowWrite`) fail.
    pub confines_writes: bool,
    /// Commands get the driver's own minimal environment; `tools.passEnv`
    /// and the host environment do not apply (`Shell::sandboxed`).
    pub own_environment: bool,
    /// Network policy for commands.
    pub network: NetworkPolicy,
    /// Enforced limits.
    pub limits: ResourceLimits,
    /// `Workspace::spawn` without a PTY works.
    pub streaming_exec: bool,
    /// `Workspace::spawn` with a PTY works.
    pub pty: bool,
    /// `snapshot`, `hibernate` and `wake` work.
    pub snapshot: bool,
    /// `read_chunk` and `write_chunk` work.
    pub chunked_transfer: bool,
    /// `WorkspaceDriver::attach` can reopen a Workspace by id after a
    /// Runtime restart.
    pub attach: bool,
}
