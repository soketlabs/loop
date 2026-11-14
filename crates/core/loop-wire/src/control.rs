//! What [`crate::server::serve_with_control`] needs from a multi-Session
//! owner, so `loop-wire` does not depend on the Runtime crate.

use async_trait::async_trait;
use loop_protocol::admin;
use loop_protocol::session_workspace as sw;
use loop_protocol::rpc::{RuntimeStatus, SessionInfo};
use loop_protocol::{BackendError, BackendResult, ErrorCategory, SessionRef, UpdateEnvelope};

/// A consistent view of one Session's Event Log, for `session/attach`.
pub struct Attachment {
    /// The Session's state at `latest_seq`, when the owner can take one.
    pub snapshot: Option<loop_protocol::rpc::SessionSnapshot>,
    /// Last update `session_seq` in `replay`'s range (the high-water mark).
    pub latest_seq: u64,
    /// Generation to report and to stamp on replayed updates.
    pub generation: u64,
    /// The current branch's transcript, stamped at its high-water mark, for a
    /// client that cannot replace its transcript on BranchReset. Sent whole,
    /// before `replay`, whatever the client's cursor; earlier updates keep
    /// the client's cursor and only the last one advances it to the stamp.
    pub baseline: Vec<UpdateEnvelope>,
    /// Committed updates after the client's cursor through `latest_seq`, in
    /// contiguous order.
    pub replay: Box<dyn Iterator<Item = UpdateEnvelope> + Send>,
}

/// Session management for a Runtime shared by many connections.
#[async_trait]
pub trait RuntimeControl: Send + Sync {
    /// Current public Session snapshot.
    async fn config(
        &self,
        _params: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<loop_protocol::rpc::SessionSnapshot> {
        Err(BackendError::Unsupported("session/config".into()))
    }

    /// Activate a Session skill.
    async fn skill(
        &self,
        _params: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<loop_protocol::MutationResult> {
        Err(BackendError::Unsupported("session/skill".into()))
    }
    /// Start a workflow and return its identity; progress uses Session updates.
    async fn workflow(
        &self,
        _params: loop_protocol::session_services::WorkflowParams,
    ) -> BackendResult<loop_protocol::session_services::WorkflowResult> {
        Err(BackendError::Unsupported("session/workflow".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn tree(
        &self,
        _params: loop_protocol::rpc::TreeParams,
    ) -> BackendResult<loop_protocol::rpc::TreeResult> {
        Err(BackendError::Unsupported("session/tree".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn view(
        &self,
        _params: loop_protocol::rpc::ViewParams,
    ) -> BackendResult<loop_protocol::rpc::ViewResult> {
        Err(BackendError::Unsupported("session/view".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn navigate(
        &self,
        _params: loop_protocol::rpc::NavigateParams,
    ) -> BackendResult<loop_protocol::rpc::NavigateResult> {
        Err(BackendError::Unsupported("session/navigate".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn rename(
        &self,
        _params: loop_protocol::rpc::RenameParams,
    ) -> BackendResult<loop_protocol::rpc::MutationResult> {
        Err(BackendError::Unsupported("session/rename".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn fork(
        &self,
        _params: loop_protocol::rpc::ForkParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        Err(BackendError::Unsupported("session/fork".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn clone_session(
        &self,
        _params: loop_protocol::rpc::CloneParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        Err(BackendError::Unsupported("session/clone_session".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn export(
        &self,
        _params: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<loop_protocol::rpc::ExportResult> {
        Err(BackendError::Unsupported("session/export".into()))
    }
    /// Session-tree service; unsupported unless advertised by the backend.
    async fn import(
        &self,
        _params: loop_protocol::rpc::ImportParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        Err(BackendError::Unsupported("session/import".into()))
    }
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
    /// Extended attach for Surfaces that understand immutable transcript views.
    async fn attach_snapshot_watching(
        &self,
        connection: u64,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<Attachment> {
        self.attach_watching(connection, session, after_seq).await
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
    /// Stop only if no Session is loaded; admission is fenced by the owner.
    async fn stop_empty(&self) -> BackendResult<()> {
        Err(unsupported("runtime/stop_empty"))
    }
    /// Workspace admin and host-only services, when this Runtime serves them.
    fn admin(&self) -> Option<&dyn RuntimeAdmin> {
        None
    }
    /// The files and terminals of Sessions (`session/workspace`,
    /// `session/files/*`, `session/terminal/*`), when this Runtime serves
    /// them.
    fn session_workspace(&self) -> Option<&dyn SessionWorkspace> {
        None
    }
}

/// One event of a terminal, for one connection's stream
/// (`session/terminal/output`). Live only: never in the Event Log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Output bytes.
    Output {
        /// The event's sequence number.
        seq: u64,
        /// The bytes.
        data: Vec<u8>,
    },
    /// This reader lost `bytes` of output; `seq` is the last lost event.
    Dropped {
        /// The last lost event.
        seq: u64,
        /// Bytes lost.
        bytes: u64,
    },
    /// The process ended. No event follows.
    Exited {
        /// The event's sequence number.
        seq: u64,
        /// How it ended.
        exit: loop_protocol::session_workspace::TerminalEnd,
    },
}

/// A terminal's events for one connection, from an attach. Dropping it
/// ends this connection's stream; the terminal runs on.
#[async_trait]
pub trait TerminalFeed: Send + Sync {
    /// The next event, waiting for one; `None` after `Exited`, or when the
    /// terminal is gone.
    async fn next(&self) -> Option<TerminalEvent>;
}

/// The files and terminals of a Session's workspace. Each call checks the
/// caller's access to the Session first, then the tool lock, the sandbox
/// and the permission gate. See `docs/design/files-and-terminal-protocol.md`.
#[async_trait]
pub trait SessionWorkspace: Send + Sync {
    /// `session/workspace`.
    async fn info(
        &self,
        params: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<sw::SessionWorkspaceInfo>;
    /// `session/files/list`.
    async fn list(&self, params: sw::FilesListParams) -> BackendResult<sw::FilesListResult>;
    /// `session/files/stat`.
    async fn stat(&self, params: sw::FilePathParams) -> BackendResult<sw::FileEntry>;
    /// `session/files/read`.
    async fn read(&self, params: sw::FilesReadParams) -> BackendResult<sw::FilesReadResult>;
    /// `session/files/upload_begin`.
    async fn upload_begin(
        &self,
        params: sw::UploadBeginParams,
    ) -> BackendResult<sw::UploadBeginResult>;
    /// `session/files/upload_write`.
    async fn upload_write(&self, params: sw::UploadWriteParams)
        -> BackendResult<sw::UploadProgress>;
    /// `session/files/upload_status`.
    async fn upload_status(&self, params: sw::UploadParams) -> BackendResult<sw::UploadProgress>;
    /// `session/files/upload_commit`.
    async fn upload_commit(
        &self,
        params: sw::UploadParams,
    ) -> BackendResult<sw::UploadCommitResult>;
    /// `session/files/upload_abort`.
    async fn upload_abort(&self, params: sw::UploadParams) -> BackendResult<()>;
    /// `session/files/delete`.
    async fn delete(&self, params: sw::FilesDeleteParams) -> BackendResult<()>;
    /// `session/files/rename`.
    async fn rename(&self, params: sw::FilesRenameParams) -> BackendResult<()>;
    /// `session/files/mkdir`.
    async fn mkdir(&self, params: sw::FilePathParams) -> BackendResult<()>;
    /// `session/terminal/open`.
    async fn terminal_open(
        &self,
        params: sw::TerminalOpenParams,
    ) -> BackendResult<sw::TerminalOpenResult>;
    /// `session/terminal/attach`: the events after `after_seq`, for this
    /// connection.
    async fn terminal_attach(
        &self,
        params: sw::TerminalAttachParams,
    ) -> BackendResult<(sw::TerminalAttachResult, Box<dyn TerminalFeed>)>;
    /// `session/terminal/detach`: checks the caller may use the terminal.
    async fn terminal_detach(&self, params: sw::TerminalParams) -> BackendResult<()>;
    /// `session/terminal/input`.
    async fn terminal_input(&self, params: sw::TerminalInputParams) -> BackendResult<()>;
    /// `session/terminal/resize`.
    async fn terminal_resize(&self, params: sw::TerminalResizeParams) -> BackendResult<()>;
    /// `session/terminal/signal`.
    async fn terminal_signal(&self, params: sw::TerminalSignalParams) -> BackendResult<()>;
    /// `session/terminal/close`.
    async fn terminal_close(
        &self,
        params: sw::TerminalParams,
    ) -> BackendResult<sw::TerminalCloseResult>;
    /// `session/terminal/list`.
    async fn terminal_list(
        &self,
        params: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<sw::TerminalListResult>;
}

/// Workspace admin, Session sandbox and permission mode, and host-only
/// methods (design `tui-client.md` §3). Each answers Unsupported unless the
/// owner serves it and advertises its capability. The server refuses
/// host-only calls without host-local authority before calling these.
#[async_trait]
pub trait RuntimeAdmin: Send + Sync {
    /// Host-only provider credential management.
    async fn providers(
        &self,
        _p: loop_protocol::host::ProvidersParams,
    ) -> BackendResult<loop_protocol::host::ProvidersResult> {
        Err(BackendError::Unsupported("runtime/providers".into()))
    }
    /// Host-only telemetry configuration.
    async fn tracing(
        &self,
        _p: loop_protocol::host::TracingParams,
    ) -> BackendResult<loop_protocol::host::TracingResult> {
        Err(BackendError::Unsupported("runtime/tracing".into()))
    }
    /// Host-only MCP OAuth.
    async fn mcp_auth(
        &self,
        _p: loop_protocol::host::McpAuthParams,
    ) -> BackendResult<loop_protocol::host::McpAuthResult> {
        Err(BackendError::Unsupported("runtime/mcp_auth".into()))
    }

    /// Redacted workspace settings.
    async fn settings(
        &self,
        _params: admin::SettingsParams,
    ) -> BackendResult<admin::SettingsResult> {
        Err(BackendError::Unsupported("workspace/settings".into()))
    }

    /// The admin and host-only capability flags this service serves; the
    /// server advertises these (host-only ones only host-locally) and
    /// ignores the backend's own admin flags.
    fn offered(&self) -> loop_protocol::Capabilities;
    /// `workspace/mcp`.
    async fn mcp(&self, _params: admin::McpParams) -> BackendResult<admin::McpResult> {
        Err(BackendError::Unsupported("workspace/mcp".into()))
    }
    /// `workspace/web_search`.
    async fn web_search(
        &self,
        _params: admin::WebSearchParams,
    ) -> BackendResult<admin::WebSearchResult> {
        Err(BackendError::Unsupported("workspace/web_search".into()))
    }
    /// `workspace/skills`.
    async fn skills(&self, _params: admin::SkillsParams) -> BackendResult<admin::SkillsResult> {
        Err(BackendError::Unsupported("workspace/skills".into()))
    }
    /// `workspace/templates`.
    async fn templates(
        &self,
        _params: admin::TemplatesParams,
    ) -> BackendResult<admin::TemplatesResult> {
        Err(BackendError::Unsupported("workspace/templates".into()))
    }
    /// `workspace/reload`.
    async fn reload(&self, _params: admin::ReloadParams) -> BackendResult<admin::ReloadResult> {
        Err(BackendError::Unsupported("workspace/reload".into()))
    }
    /// `workspace/trust`. The server only passes a write (`trusted`) on a
    /// host-local connection.
    async fn trust(&self, _params: admin::TrustParams) -> BackendResult<admin::TrustResult> {
        Err(BackendError::Unsupported("workspace/trust".into()))
    }
    /// `session/sandbox`.
    async fn sandbox(&self, _params: admin::SandboxParams) -> BackendResult<admin::SandboxResult> {
        Err(BackendError::Unsupported("session/sandbox".into()))
    }
    /// `session/permission_mode`.
    async fn permission_mode(
        &self,
        _params: admin::PermissionModeParams,
    ) -> BackendResult<admin::PermissionModeResult> {
        Err(BackendError::Unsupported("session/permission_mode".into()))
    }
    /// `runtime/sessions`.
    async fn sessions(&self) -> BackendResult<admin::RuntimeSessionsResult> {
        Err(BackendError::Unsupported("runtime/sessions".into()))
    }
    /// `runtime/usage` (host-only).
    async fn usage(&self, _params: admin::UsageParams) -> BackendResult<admin::UsageResult> {
        Err(BackendError::Unsupported("runtime/usage".into()))
    }
    /// `runtime/defaults` (host-only).
    async fn defaults(
        &self,
        _params: admin::DefaultsParams,
    ) -> BackendResult<admin::DefaultsResult> {
        Err(BackendError::Unsupported("runtime/defaults".into()))
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
    /// Who the connection acts for, fixed when it was accepted; `None` for
    /// an in-process recipient. The Runtime sends a user from outside the
    /// host ([`loop_protocol::Caller::is_host_user`] is false) only the
    /// Escalations of turns that user started, and records the caller that
    /// answered.
    fn caller(&self) -> Option<loop_protocol::Caller>;
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
