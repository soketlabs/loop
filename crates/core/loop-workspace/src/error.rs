//! Typed Workspace errors.

use thiserror::Error;

use crate::env::{ExecutionError, FileError};
use crate::platform::HostOs;
use crate::sandbox::SandboxError;

/// Why a Workspace or a Workspace driver could not do an operation.
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
    /// The driver exists but does not run on this platform.
    #[error("the `{driver}` workspace driver is not available on {platform}")]
    PlatformUnsupported {
        /// Driver id.
        driver: String,
        /// Platform in use.
        platform: HostOs,
    },
    /// The driver cannot work now: an external tool is missing, or the
    /// remote service does not answer.
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
        assert_eq!(
            e.to_string(),
            "snapshot is not supported by the `host` workspace driver"
        );
    }
}
