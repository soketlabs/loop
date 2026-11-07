//! What [`crate::server::serve_with_control`] needs from a multi-Session
//! owner, so `loop-wire` does not depend on the Runtime crate.

use async_trait::async_trait;
use loop_protocol::rpc::{RuntimeStatus, SessionInfo};
use loop_protocol::{BackendError, BackendResult, ErrorCategory, SessionRef, UpdateEnvelope};

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
    /// Register an interactive connection after replay setup, or remove it on detach.
    async fn recipient(
        &self,
        _session: &SessionRef,
        _connection: u64,
        _recipient: Option<std::sync::Arc<dyn EscalationRecipient>>,
    ) -> BackendResult<()> {
        Ok(())
    }
    /// Record whether `connection` is attached to `session` (watching it,
    /// whether or not it answers Escalations), with its Escalation
    /// `recipient` when it answers them. A Runtime keeps watched Sessions
    /// loaded. The default only registers the recipient.
    async fn register(
        &self,
        session: &SessionRef,
        connection: u64,
        _attached: bool,
        recipient: Option<std::sync::Arc<dyn EscalationRecipient>>,
    ) -> BackendResult<()> {
        self.recipient(session, connection, recipient).await
    }
    /// Remove a disconnected connection from every Session.
    async fn disconnected(&self, _connection: u64) {}
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
    /// [`Self::attach`] for `connection`. A Runtime records the connection
    /// as watching the Session before the snapshot, so it is not unloaded
    /// meanwhile, and loads it first if it was unloaded; when it cannot, the
    /// attach fails. The default is a plain attach.
    async fn attach_watching(
        &self,
        _connection: u64,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<Attachment> {
        self.attach(session, after_seq).await
    }
    /// Copy a Session into a new, unloaded one (`session/fork`).
    async fn fork(&self, _params: loop_protocol::rpc::ForkParams) -> BackendResult<SessionRef> {
        Err(unsupported("session/fork"))
    }
    /// The Runtime process and its Session counts (`runtime/status`).
    async fn status(&self) -> BackendResult<RuntimeStatus> {
        Err(unsupported("runtime/status"))
    }
    /// Ask the process that owns the Runtime to shut down (`runtime/stop`).
    /// Returns once shutdown is requested, before it runs, so the caller can
    /// still answer.
    async fn stop(&self) -> BackendResult<()> {
        Err(unsupported("runtime/stop"))
    }
}

fn unsupported(method: &str) -> BackendError {
    BackendError::failed(
        ErrorCategory::Protocol,
        format!("{method} is not supported by this server"),
    )
}

/// An interactive connection that can receive a Session's Escalations.
#[async_trait]
pub trait EscalationRecipient: Send + Sync {
    /// Queue withdrawal synchronously in the connection's ordered output.
    /// The Runtime calls this before removing the recipient or redispatching.
    fn withdraw(&self, params: loop_protocol::rpc::InteractWithdrawnParams);
    /// Ask the Surface, or return an error when it disconnects. The signal
    /// clears pending JSON-RPC bookkeeping; notification is queued by `withdraw`.
    /// Receiving `Ok(())` withdraws; a dropped sender means this recipient won.
    async fn interact(
        &self,
        params: loop_protocol::rpc::InteractParams,
        withdrawn: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<loop_protocol::InteractionOutcome, ()>;
}
