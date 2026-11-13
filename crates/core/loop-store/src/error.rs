//! The one error type of every store contract (ADR-0007 §8).

use loop_protocol::SessionKey;

/// A store failure. It holds no driver types: a driver puts its own error
/// text in [`StoreError::Storage`].
///
/// Live now: `Invalid`, `Storage` and `Stopped` (the variants of the old
/// `EventLogError`, with the same text). The other variants are defined for
/// later steps of ADR-0007 and no store returns them yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Planned (B2 and later). The record, Session or generation does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The request breaks a contract rule. Nothing of it was written.
    #[error("invalid Event Log append: {0}")]
    Invalid(String),
    /// Planned (B5). An id or sequence clash, or a different row on apply.
    #[error("store conflict: {0}")]
    Conflict(String),
    /// Planned (B4). The append carried a stale lease epoch.
    #[error("stale lease fence for Session {session}")]
    Fenced {
        /// The Session whose lease moved on.
        session: SessionKey,
    },
    /// Planned (B2 and later). The store is busy; a retry is safe.
    #[error("store busy")]
    Busy,
    /// Planned (B4). The request is larger than the store accepts, in bytes.
    #[error("request too large: {0} bytes")]
    TooLarge(usize),
    /// The driver failed. The text is the driver's own message.
    #[error("Event Log storage: {0}")]
    Storage(String),
    /// The store's writer stopped. Was `EventLogError::WriterStopped`.
    #[error("Event Log writer stopped")]
    Stopped,
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// Result of every store call.
pub type Result<T> = std::result::Result<T, StoreError>;
