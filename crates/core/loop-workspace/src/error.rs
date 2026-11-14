//! Typed Workspace errors and their retry classes.

use thiserror::Error;

use crate::env::{ExecutionError, FileError, RetryClass};
use crate::platform::HostOs;
use crate::sandbox::SandboxError;

/// Why a Workspace or a Workspace driver could not do an operation.
///
/// Retry semantics for the agent loop ([`WorkspaceError::retry`]):
///
/// | Class | Errors | Agent loop |
/// |---|---|---|
/// | `Safe` | `DriverUnavailable`, `Busy`, `Changed`, file/exec `Unavailable`, `SandboxError::NotReady` | Nothing happened; retry the same call (with back-off) |
/// | `Unknown` | `OutcomeUnknown`, exec `TimedOut`/`Aborted`/`OutcomeUnknown` | The effect is unknown; do not retry blindly; check state or tell the model |
/// | `Unsafe` | everything else, including `Gone` and `Fenced` | Do not retry; `Gone` ends the turn's use of this Workspace |
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// The driver does not implement this operation. Its capabilities say so.
    #[error("{operation} is not supported by the `{driver}` workspace driver")]
    Unsupported {
        /// Driver id.
        driver: String,
        /// Operation name, such as `spawn` or `snapshot`.
        operation: &'static str,
    },
    /// No registered driver serves this kind.
    #[error("unknown workspace kind: {0}")]
    UnknownKind(String),
    /// No registered driver has this id.
    #[error("unknown workspace driver: {0}")]
    UnknownDriver(String),
    /// A driver tried to take the id of a built-in driver without
    /// `replace_builtin`.
    #[error("workspace driver `{0}` is built in; use replace_builtin to replace it")]
    BuiltinId(String),
    /// The driver exists but does not run on this platform.
    #[error("the `{driver}` workspace driver is not available on {platform}")]
    PlatformUnsupported {
        /// Driver id.
        driver: String,
        /// Platform in use.
        platform: HostOs,
    },
    /// The selected driver does not meet a restriction that a setting asks
    /// for. Selection refuses instead of running with less protection.
    #[error("the `{driver}` workspace driver does not meet {setting}: {reason}")]
    Unmet {
        /// Driver id.
        driver: String,
        /// The setting or rule, such as `sandbox.network`.
        setting: &'static str,
        /// Why not.
        reason: &'static str,
    },
    /// The driver cannot work now: an external tool is missing, or the
    /// remote service does not answer. Nothing happened.
    #[error("the `{driver}` workspace driver is not available: {reason}")]
    DriverUnavailable {
        /// Driver id.
        driver: String,
        /// Reason for people.
        reason: String,
    },
    /// The Workspace does not exist any more (destroyed, expired, or lost).
    #[error("workspace {id} is gone")]
    Gone {
        /// Workspace id.
        id: String,
    },
    /// Another operation runs; for example `hibernate` while a call is in
    /// flight. Nothing happened.
    #[error("workspace {id} is busy; {operation} did not run")]
    Busy {
        /// Workspace id.
        id: String,
        /// The refused operation.
        operation: &'static str,
    },
    /// Another Runtime cell attached the Workspace with a newer epoch. This
    /// handle must stop.
    #[error(
        "workspace {id} is driven by a newer attachment (epoch {current}, this handle {held})"
    )]
    Fenced {
        /// Workspace id.
        id: String,
        /// The epoch of the newest attachment.
        current: u64,
        /// The epoch of this handle.
        held: u64,
    },
    /// The connection was lost during the operation; its effect is unknown.
    #[error("{operation}: the connection to the workspace was lost; the result is unknown")]
    OutcomeUnknown {
        /// Operation name.
        operation: &'static str,
    },
    /// A file changed after its tag was taken (download, `if_match`).
    #[error("{path} changed")]
    Changed {
        /// Workspace path.
        path: String,
    },
    /// An upload is bigger than the driver allows, or bigger than declared.
    #[error("upload too large: {size} bytes, limit {limit}")]
    TooLarge {
        /// Size asked for.
        size: u64,
        /// Limit.
        limit: u64,
    },
    /// No such upload (never begun, committed, aborted or expired).
    #[error("unknown upload {0}")]
    UnknownUpload(String),
    /// An upload write starts after the committed offset (a gap), or the
    /// upload is not complete at commit. Nothing was written; resume from
    /// `committed` (see `Workspace::upload_committed_offset`).
    #[error("upload {upload}: offset {offset} does not match the committed offset {committed}")]
    UploadOffset {
        /// Upload id.
        upload: String,
        /// Offset of the refused write, or the size at commit.
        offset: u64,
        /// Bytes stored so far.
        committed: u64,
    },
    /// The uploaded bytes do not match the declared SHA-256.
    #[error("upload {0}: checksum mismatch")]
    ChecksumMismatch(String),
    /// `attach_process` asked for events after a `seq` the process has
    /// not reached. Nothing was attached.
    #[error("process {process}: after_seq {after_seq} is past its last event {last_seq}")]
    SeqAhead {
        /// Process id.
        process: String,
        /// The `after_seq` asked for.
        after_seq: u64,
        /// The `seq` of its newest event.
        last_seq: u64,
    },
    /// No such process.
    #[error("unknown process {0}")]
    UnknownProcess(String),
    /// A workspace path is not valid.
    #[error("invalid workspace path `{path}`: {reason}")]
    InvalidPath {
        /// The path as given.
        path: String,
        /// Why it is not valid.
        reason: &'static str,
    },
    /// A lifecycle error from a sandbox backend. The text is unchanged.
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    /// A file operation failed.
    #[error(transparent)]
    File(#[from] FileError),
    /// A command failed to run.
    #[error(transparent)]
    Exec(#[from] ExecutionError),
}

impl WorkspaceError {
    /// The error for an operation `driver` does not implement.
    pub fn unsupported(driver: impl Into<String>, operation: &'static str) -> Self {
        Self::Unsupported {
            driver: driver.into(),
            operation,
        }
    }

    /// Whether the Workspace no longer exists.
    pub fn is_gone(&self) -> bool {
        matches!(self, Self::Gone { .. })
    }

    /// Whether the operation is not implemented by the driver.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }

    /// Retry class for the agent loop (see the table on the type).
    pub fn retry(&self) -> RetryClass {
        match self {
            Self::DriverUnavailable { .. } | Self::Busy { .. } | Self::Changed { .. } => {
                RetryClass::Safe
            }
            Self::Sandbox(SandboxError::NotReady(_)) => RetryClass::Safe,
            Self::OutcomeUnknown { .. } => RetryClass::Unknown,
            Self::File(e) => e.code.retry(),
            Self::Exec(e) => e.code.retry(),
            _ => RetryClass::Unsafe,
        }
    }

    /// Whether the same call can safely run again.
    pub fn is_retryable(&self) -> bool {
        self.retry() == RetryClass::Safe
    }
}

impl From<WorkspaceError> for SandboxError {
    fn from(error: WorkspaceError) -> Self {
        match error {
            WorkspaceError::Sandbox(inner) => inner,
            other => SandboxError::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{ExecutionErrorCode, FileErrorCode};

    #[test]
    fn sandbox_error_text_is_unchanged_both_ways() {
        let original = SandboxError::StartFailed("podman missing".into());
        let text = original.to_string();
        let wrapped = WorkspaceError::from(original);
        assert_eq!(wrapped.to_string(), text);
        assert_eq!(SandboxError::from(wrapped).to_string(), text);
    }

    #[test]
    fn unsupported_names_the_driver_and_operation() {
        let e = WorkspaceError::unsupported("host", "snapshot");
        assert!(e.is_unsupported());
        assert!(!e.is_retryable());
        assert_eq!(
            e.to_string(),
            "snapshot is not supported by the `host` workspace driver"
        );
    }

    #[test]
    fn retry_classes() {
        let unavailable = WorkspaceError::DriverUnavailable {
            driver: "remote".into(),
            reason: "down".into(),
        };
        assert!(unavailable.is_retryable());
        assert_eq!(
            WorkspaceError::OutcomeUnknown { operation: "exec" }.retry(),
            RetryClass::Unknown
        );
        assert_eq!(
            WorkspaceError::Gone { id: "w".into() }.retry(),
            RetryClass::Unsafe
        );
        let timed_out =
            WorkspaceError::from(ExecutionError::new(ExecutionErrorCode::TimedOut, "t"));
        assert_eq!(timed_out.retry(), RetryClass::Unknown);
        let file = WorkspaceError::from(FileError::new(FileErrorCode::Unavailable, "u"));
        assert!(file.is_retryable());
    }
}
