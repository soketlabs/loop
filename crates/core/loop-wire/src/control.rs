//! What [`crate::server::serve_with_control`] needs from a multi-Session
//! owner, so `loop-wire` does not depend on the Runtime crate.

use async_trait::async_trait;
use loop_protocol::rpc::SessionInfo;
use loop_protocol::{BackendResult, SessionRef, UpdateEnvelope};

/// A consistent view of one Session's Event Log, for `session/attach`.
pub struct Attachment {
    /// Last update `session_seq` in `replay`'s range (the high-water mark).
    pub latest_seq: u64,
    /// Generation to report and to stamp on replayed updates.
    pub generation: u64,
    /// Committed updates after the client's cursor through `latest_seq`, in
    /// contiguous order.
    pub replay: Box<dyn Iterator<Item = UpdateEnvelope> + Send>,
}

/// Session management for a Runtime shared by many connections.
#[async_trait]
pub trait RuntimeControl: Send + Sync {
    /// Every known Session, live or not. Does not subscribe to any of them.
    async fn list(&self) -> BackendResult<Vec<SessionInfo>>;
    /// Close a Session. A busy Session needs `force`.
    async fn close(&self, session: &SessionRef, force: bool) -> BackendResult<()>;
    /// Read the Event Log after `after_seq` up to a fixed high-water mark.
    ///
    /// The server subscribes to live updates *before* calling this, so any
    /// update committed after the snapshot is delivered live; the overlap is
    /// removed by sequence number.
    async fn attach(&self, session: &SessionRef, after_seq: u64) -> BackendResult<Attachment>;
}
