//! An [`AgentBackend`] on the other end of a [`Transport`].
//!
//! [`RemoteBackend::connect`] runs `initialize`, then forwards the server's
//! `session/update`s to its own broadcast and answers `host/interact` with
//! the [`HostServices`] it was given. Everything else is a request.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use loop_protocol::admin;
use loop_protocol::rpc::{
    methods, AttachParams, AttachResult, ChoicesResult, ClientCapabilities, CloseParams,
    InitializeParams, InitializeResult, LoadParams, ModelsResult, PeerInfo, PromptParams,
    PromptResult, RpcErrorBody, RuntimeStatus, SessionInfo, SessionParams, SetEffortParams,
    SetModeParams, SetModelParams,
};
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, Compaction,
    CompactionReport, ContentBlock, ErrorCategory, ErrorInfo, HostServices, InteractionRequest,
    LocalShell, ModelChoices, ModelControl, MutationResult, SessionKey, SessionRef, SessionSpec,
    SessionStats, SessionUpdate, ShellResult, StatsSummary, TurnHandle, TurnOutcome,
    UpdateEnvelope, PROTOCOL_VERSION,
};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::connection::{codes, Connection, Incoming, RpcError};
use crate::transport::Transport;
use loop_protocol::session_workspace::{self as sw, TerminalOutputParams};

/// Updates a client keeps for a slow subscriber before it lags.
const UPDATE_BUFFER: usize = 4096;

struct Initializing(Option<Arc<Connection>>);

impl Drop for Initializing {
    fn drop(&mut self) {
        if let Some(conn) = &self.0 {
            conn.close();
        }
    }
}

/// A backend served over a connection ([`crate::serve`]).
pub struct RemoteBackend {
    conn: Arc<Connection>,
    is_websocket: bool,
    connection_notices: Mutex<Vec<String>>,
    instance: BackendInstanceId,
    capabilities: Capabilities,
    server: PeerInfo,
    updates: broadcast::Sender<UpdateEnvelope>,
    /// Sessions with a turn running, from the update stream and from
    /// prompts this client sent.
    busy: Arc<Mutex<HashSet<SessionKey>>>,
    /// Sessions with a prompt this client sent still unanswered. Unlike
    /// `busy`, observation (detach) never clears these.
    local: Arc<Mutex<HashSet<SessionKey>>>,
    /// `session/terminal/output` of the terminals this client attached.
    terminal_output: broadcast::Sender<TerminalOutputParams>,
}

impl RemoteBackend {
    /// Connect to the Runtime over an authenticated WebSocket, then initialize.
    /// The caller supplies the token; it is never included in diagnostics.
    pub async fn connect_websocket(
        url: &str,
        token: &str,
        client: PeerInfo,
        host: Arc<dyn HostServices>,
    ) -> BackendResult<Arc<Self>> {
        let transport = crate::socket::websocket_connect(url, token)
            .await
            .map_err(|error| BackendError::failed(ErrorCategory::Protocol, error.to_string()))?;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Self::connect(transport, client, host),
        )
        .await
        .map_err(|_| {
            BackendError::failed(ErrorCategory::Protocol, "Runtime initialization timed out")
        })?
    }

    /// Current public Session snapshot.
    pub async fn config(
        &self,
        p: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<loop_protocol::rpc::SessionSnapshot> {
        if !self.capabilities.session_config {
            return Err(BackendError::Unsupported(methods::SESSION_CONFIG.into()));
        }
        call(&self.conn, methods::SESSION_CONFIG, &p).await
    }

    /// Activate a Session skill.
    pub async fn skill(
        &self,
        params: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<loop_protocol::MutationResult> {
        if !self.capabilities.session_skills {
            return Err(BackendError::Unsupported(methods::SESSION_SKILL.into()));
        }
        call(&self.conn, methods::SESSION_SKILL, &params).await
    }
    /// Start a workflow; progress arrives through subscribe.
    pub async fn workflow(
        &self,
        params: loop_protocol::session_services::WorkflowParams,
    ) -> BackendResult<loop_protocol::session_services::WorkflowResult> {
        if !self.capabilities.workflow {
            return Err(BackendError::Unsupported(methods::SESSION_WORKFLOW.into()));
        }
        call(&self.conn, methods::SESSION_WORKFLOW, &params).await
    }

    /// Connect over `transport` and agree on a version. `host` answers the
    /// server's `host/interact` requests.
    pub async fn connect(
        transport: Transport,
        client: PeerInfo,
        host: Arc<dyn HostServices>,
    ) -> BackendResult<Arc<Self>> {
        Self::connect_with_capabilities(
            transport,
            client,
            host,
            ClientCapabilities {
                interact: true,
                ..Default::default()
            },
        )
        .await
    }

    /// Connect with the Surface's actual reset and interaction support.
    pub async fn connect_with_capabilities(
        transport: Transport,
        client: PeerInfo,
        host: Arc<dyn HostServices>,
        client_capabilities: ClientCapabilities,
    ) -> BackendResult<Arc<Self>> {
        let is_websocket = transport.is_websocket;
        let (conn, incoming) = Connection::new(transport);
        let mut initializing = Initializing(Some(conn.clone()));
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let (terminal_output, _) = broadcast::channel(UPDATE_BUFFER);
        let busy: Arc<Mutex<HashSet<SessionKey>>> = Arc::default();
        tokio::spawn(read(
            Arc::clone(&conn),
            incoming,
            Feeds {
                updates: updates.clone(),
                terminal_output: terminal_output.clone(),
            },
            Arc::clone(&busy),
            host,
        ));
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: client,
            client_capabilities,
        };
        let result: InitializeResult = call(&conn, methods::INITIALIZE, &params).await?;
        if result.protocol_version > PROTOCOL_VERSION || result.protocol_version == 0 {
            conn.close();
            return Err(BackendError::failed(
                ErrorCategory::Protocol,
                format!(
                    "the server speaks loop-protocol {}, and this client speaks up to {PROTOCOL_VERSION}",
                    result.protocol_version
                ),
            ));
        }
        initializing.0 = None;
        Ok(Arc::new(Self {
            conn,
            is_websocket,
            connection_notices: Mutex::new(vec![]),
            instance: result.instance,
            capabilities: result.capabilities,
            server: result.server_info,
            updates,
            busy,
            local: Arc::default(),
            terminal_output,
        }))
    }

    /// Session-tree operation over the wire.
    pub async fn tree(
        &self,
        params: loop_protocol::rpc::TreeParams,
    ) -> BackendResult<loop_protocol::rpc::TreeResult> {
        if !self.capabilities.session_tree {
            return Err(BackendError::Unsupported(methods::SESSION_TREE.into()));
        }
        call(&self.conn, methods::SESSION_TREE, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn view(
        &self,
        params: loop_protocol::rpc::ViewParams,
    ) -> BackendResult<loop_protocol::rpc::ViewResult> {
        if !self.capabilities.branch_snapshot {
            return Err(BackendError::Unsupported(methods::SESSION_VIEW.into()));
        }
        call(&self.conn, methods::SESSION_VIEW, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn navigate(
        &self,
        params: loop_protocol::rpc::NavigateParams,
    ) -> BackendResult<loop_protocol::rpc::NavigateResult> {
        if !self.capabilities.session_tree {
            return Err(BackendError::Unsupported(methods::SESSION_NAVIGATE.into()));
        }
        call(&self.conn, methods::SESSION_NAVIGATE, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn rename(
        &self,
        params: loop_protocol::rpc::RenameParams,
    ) -> BackendResult<loop_protocol::rpc::MutationResult> {
        if !self.capabilities.session_rename {
            return Err(BackendError::Unsupported(methods::SESSION_RENAME.into()));
        }
        call(&self.conn, methods::SESSION_RENAME, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn fork(
        &self,
        params: loop_protocol::rpc::ForkParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        if !self.capabilities.session_fork {
            return Err(BackendError::Unsupported(methods::SESSION_FORK.into()));
        }
        call(&self.conn, methods::SESSION_FORK, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn clone_session(
        &self,
        params: loop_protocol::rpc::CloneParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        if !self.capabilities.session_fork {
            return Err(BackendError::Unsupported(methods::SESSION_CLONE.into()));
        }
        call(&self.conn, methods::SESSION_CLONE, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn export(
        &self,
        params: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<loop_protocol::rpc::ExportResult> {
        if !self.capabilities.session_transfer {
            return Err(BackendError::Unsupported(methods::SESSION_EXPORT.into()));
        }
        call(&self.conn, methods::SESSION_EXPORT, &params).await
    }

    /// Session-tree operation over the wire.
    pub async fn import(
        &self,
        params: loop_protocol::rpc::ImportParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        if !self.capabilities.session_transfer {
            return Err(BackendError::Unsupported(methods::SESSION_IMPORT.into()));
        }
        call(&self.conn, methods::SESSION_IMPORT, &params).await
    }

    /// Whether this client sends data over WebSocket rather than a local socket.
    pub fn is_websocket(&self) -> bool {
        self.is_websocket
    }

    /// Add a Surface startup notice without sending or persisting it on the wire.
    pub fn add_connection_notice(&self, notice: String) {
        self.connection_notices.lock().push(notice);
    }
    /// Take connection startup notices once.
    pub fn take_connection_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.connection_notices.lock())
    }
    /// Wait for the connection to the Runtime to close.
    pub async fn closed(&self) {
        self.conn.closed().await;
    }

    /// Close this Surface connection without stopping the Runtime.
    pub fn disconnect(&self) {
        self.conn.close();
    }

    /// The server, as it introduced itself.
    pub fn server(&self) -> &PeerInfo {
        &self.server
    }

    /// Every Session the Runtime knows (`session/list`). Needs the
    /// `multi_session` capability.
    pub async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        call(&self.conn, methods::SESSION_LIST, &serde_json::json!({})).await
    }

    /// Watch a Session (`session/attach`). Updates after `after_seq` arrive on
    /// [`AgentBackend::subscribe`] as `origin: replay` and then live, with
    /// contiguous `seq`; subscribe *before* calling so none is missed. Needs
    /// the `attach` capability.
    pub async fn attach(
        &self,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<AttachResult> {
        let params = AttachParams {
            session: session.clone(),
            after_seq,
        };
        call(&self.conn, methods::SESSION_ATTACH, &params).await
    }

    /// Stop receiving a Session's updates. The turn keeps running.
    pub async fn detach(&self, session: &SessionRef) -> BackendResult<()> {
        let params = SessionParams {
            session: session.clone(),
        };
        call::<Value>(&self.conn, methods::SESSION_DETACH, &params).await?;
        // The `turn_ended` of a turn someone else is running will never
        // arrive now; stop believing the Session is busy because of it.
        if !self.local.lock().contains(&session.key) {
            self.busy.lock().remove(&session.key);
        }
        Ok(())
    }

    /// The Runtime process and its Session counts (`runtime/status`).
    pub async fn status(&self) -> BackendResult<RuntimeStatus> {
        call(&self.conn, methods::RUNTIME_STATUS, &serde_json::json!({})).await
    }

    /// Stop an empty owner without racing a new Session admission.
    pub async fn stop_empty(&self) -> BackendResult<()> {
        if !self.capabilities.runtime_stop_empty {
            return Err(BackendError::Unsupported(
                methods::RUNTIME_STOP_EMPTY.into(),
            ));
        }
        call::<Value>(
            &self.conn,
            methods::RUNTIME_STOP_EMPTY,
            &serde_json::json!({}),
        )
        .await
        .map(|_| ())
    }

    /// Ask the Runtime to shut down (`runtime/stop`). It answers first, then
    /// closes every connection; send nothing more on this one.
    pub async fn stop(&self) -> BackendResult<()> {
        call::<Value>(&self.conn, methods::RUNTIME_STOP, &serde_json::json!({}))
            .await
            .map(|_| ())
    }

    /// The models a Session can use, with their backends (`session/models`).
    /// Needs the `set_model` capability.
    pub async fn session_models(&self, session: &SessionRef) -> BackendResult<ModelsResult> {
        let params = SessionParams {
            session: session.clone(),
        };
        call(&self.conn, methods::SESSION_MODELS, &params).await
    }

    async fn choices(&self, method: &str, session: &SessionRef) -> BackendResult<ModelChoices> {
        let params = SessionParams {
            session: session.clone(),
        };
        call::<ChoicesResult>(&self.conn, method, &params)
            .await
            .map(ModelChoices::from)
    }

    /// Close a Session; a busy one needs `force`.
    pub async fn close(&self, session: &SessionRef, force: bool) -> BackendResult<()> {
        let params = CloseParams {
            session: session.clone(),
            force,
        };
        call::<Value>(&self.conn, methods::SESSION_CLOSE, &params)
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl AgentBackend for RemoteBackend {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }

    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        call(&self.conn, methods::SESSION_NEW, &spec).await
    }

    async fn load_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        let params = LoadParams {
            session: session.clone(),
            spec,
        };
        call(&self.conn, methods::SESSION_LOAD, &params).await
    }

    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        let params = SessionParams {
            session: session.clone(),
        };
        call(&self.conn, methods::SESSION_TRANSCRIPT, &params).await
    }

    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        if !self.busy.lock().insert(session.key.clone()) {
            return Err(BackendError::Busy);
        }
        let params = PromptParams {
            session: session.clone(),
            content: input,
        };
        let (done, handle) = TurnHandle::channel();
        let conn = Arc::clone(&self.conn);
        let busy = Arc::clone(&self.busy);
        let local = Arc::clone(&self.local);
        local.lock().insert(session.key.clone());
        let key = session.key.clone();
        // The response comes when the turn ends. A turn another client
        // started first shows up here as a failed outcome, not as `Busy`.
        tokio::spawn(async move {
            let outcome = match call::<PromptResult>(&conn, methods::SESSION_PROMPT, &params).await
            {
                Ok(result) => TurnOutcome::from(result),
                Err(BackendError::Failed(info)) if info.category == ErrorCategory::Network => {
                    TurnOutcome::Uncertain
                }
                Err(error) => TurnOutcome::Failed(match error {
                    BackendError::Failed(info) => info,
                    other => ErrorInfo::new(ErrorCategory::Agent, other.to_string()),
                }),
            };
            local.lock().remove(&key);
            busy.lock().remove(&key);
            let _ = done.send(outcome);
        });
        Ok(handle)
    }

    fn cancel(&self, session: &SessionRef) {
        let params = SessionParams {
            session: session.clone(),
        };
        let _ = self.conn.notify(
            methods::SESSION_CANCEL,
            serde_json::to_value(params).unwrap_or_default(),
        );
    }

    fn is_busy(&self, session: &SessionRef) -> bool {
        self.busy.lock().contains(&session.key)
    }

    async fn shutdown(&self) {
        let _ = self
            .conn
            .request(methods::SHUTDOWN, Value::Object(Default::default()))
            .await;
        self.conn.close();
    }

    fn model_control(&self) -> Option<&dyn ModelControl> {
        let c = self.capabilities;
        (c.set_model || c.efforts || c.modes || c.session_ttfb).then_some(self as &dyn ModelControl)
    }

    fn compaction(&self) -> Option<&dyn Compaction> {
        self.capabilities
            .compaction
            .then_some(self as &dyn Compaction)
    }

    fn session_stats(&self) -> Option<&dyn SessionStats> {
        self.capabilities
            .session_stats
            .then_some(self as &dyn SessionStats)
    }

    fn local_shell(&self) -> Option<&dyn LocalShell> {
        self.capabilities
            .local_shell
            .then_some(self as &dyn LocalShell)
    }
}

#[async_trait]
impl Compaction for RemoteBackend {
    async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<CompactionReport> {
        let params = loop_protocol::rpc::CompactParams {
            session: session.clone(),
            instructions: instructions.map(str::to_owned),
        };
        call(&self.conn, methods::SESSION_COMPACT, &params).await
    }
}

#[async_trait]
impl SessionStats for RemoteBackend {
    async fn stats(&self, session: &SessionRef) -> BackendResult<StatsSummary> {
        let params = SessionParams {
            session: session.clone(),
        };
        call::<loop_protocol::rpc::StatsResult>(&self.conn, methods::SESSION_STATS, &params)
            .await
            .map(StatsSummary::from)
    }
}

#[async_trait]
impl LocalShell for RemoteBackend {
    async fn run(
        &self,
        session: &SessionRef,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> BackendResult<ShellResult> {
        let params = loop_protocol::rpc::ShellParams {
            session: session.clone(),
            command: command.to_owned(),
            timeout_ms,
        };
        call(&self.conn, methods::SESSION_SHELL, &params).await
    }
}

/// Unsupported unless the server advertised `offered`.
fn offered(offered: bool, what: &str) -> BackendResult<()> {
    if offered {
        Ok(())
    } else {
        Err(BackendError::Unsupported(what.into()))
    }
}

#[async_trait]
impl ModelControl for RemoteBackend {
    async fn ttfb(
        &self,
        session: &SessionRef,
        change: Option<Option<u64>>,
        expected_revision: Option<u64>,
    ) -> BackendResult<loop_protocol::rpc::TtfbResult> {
        offered(
            self.capabilities.session_ttfb,
            "changing the response timeout",
        )?;
        let params = loop_protocol::rpc::TtfbParams {
            session: session.clone(),
            timeout_ms: change.flatten(),
            clear_override: matches!(change, Some(None)),
            expected_revision,
        };
        call(&self.conn, methods::SESSION_TTFB, &params).await
    }

    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        offered(self.capabilities.set_model, "choosing a model")?;
        self.session_models(session).await.map(ModelChoices::from)
    }

    async fn set_model(
        &self,
        session: &SessionRef,
        model: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        offered(self.capabilities.set_model, "choosing a model")?;
        let params = SetModelParams {
            session: session.clone(),
            model: model.into(),
            expected_revision,
        };
        call(&self.conn, methods::SESSION_SET_MODEL, &params).await
    }

    async fn efforts(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        offered(self.capabilities.efforts, "choosing a reasoning effort")?;
        self.choices(methods::SESSION_EFFORTS, session).await
    }

    async fn set_effort(
        &self,
        session: &SessionRef,
        effort: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        offered(self.capabilities.efforts, "choosing a reasoning effort")?;
        let params = SetEffortParams {
            session: session.clone(),
            effort: effort.into(),
            expected_revision,
        };
        call(&self.conn, methods::SESSION_SET_EFFORT, &params).await
    }

    async fn modes(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        offered(self.capabilities.modes, "choosing a mode")?;
        self.choices(methods::SESSION_MODES, session).await
    }

    async fn set_mode(
        &self,
        session: &SessionRef,
        mode: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        offered(self.capabilities.modes, "choosing a mode")?;
        let params = SetModeParams {
            session: session.clone(),
            mode: mode.into(),
            expected_revision,
        };
        call(&self.conn, methods::SESSION_SET_MODE, &params).await
    }
}

/// Send a request and parse its result; a remote error becomes the
/// [`BackendError`] it stands for, a lost connection a network failure.
async fn call<T: DeserializeOwned>(
    conn: &Connection,
    method: &str,
    params: &impl serde::Serialize,
) -> BackendResult<T> {
    let params = serde_json::to_value(params).unwrap_or_default();
    match conn.request(method, params).await {
        Ok(value) => serde_json::from_value(value)
            .map_err(|e| BackendError::failed(ErrorCategory::Protocol, e.to_string())),
        Err(RpcError::Remote {
            code,
            message,
            data,
        }) => Err(RpcErrorBody {
            code,
            message,
            data,
        }
        .into_backend_error()),
        Err(RpcError::Closed) => Err(BackendError::failed(
            ErrorCategory::Network,
            "lost the connection to the server",
        )),
        Err(RpcError::Invalid(why)) => Err(BackendError::failed(ErrorCategory::Protocol, why)),
    }
}

/// Where [`read`] sends what the server streams.
struct Feeds {
    updates: broadcast::Sender<UpdateEnvelope>,
    terminal_output: broadcast::Sender<TerminalOutputParams>,
}

/// Handle everything the server sends, in order.
async fn read(
    conn: Arc<Connection>,
    mut incoming: tokio::sync::mpsc::Receiver<Incoming>,
    feeds: Feeds,
    busy: Arc<Mutex<HashSet<SessionKey>>>,
    host: Arc<dyn HostServices>,
) {
    let Feeds {
        updates,
        terminal_output,
    } = feeds;
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Response { id, result } => conn.resolve(id, result).await,
            Incoming::Notification { method, params } if method == methods::SESSION_UPDATE => {
                match serde_json::from_value::<UpdateEnvelope>(params) {
                    Ok(envelope) => {
                        match &envelope.update {
                            SessionUpdate::TurnStarted { .. } => {
                                busy.lock().insert(envelope.session.clone());
                            }
                            SessionUpdate::TurnEnded { .. } => {
                                busy.lock().remove(&envelope.session);
                            }
                            _ => {}
                        }
                        let _ = updates.send(envelope);
                    }
                    Err(error) => tracing::debug!(%error, "skipping an unreadable update"),
                }
            }
            // Another Surface answered (or the turn ended): tell subscribers
            // so a still-open approval can close. Seq 0 is outside the
            // stream's numbering; consumers act on it before ordering checks.
            Incoming::Notification { method, params }
                if method == methods::HOST_INTERACT_WITHDRAWN =>
            {
                if let Ok(withdrawn) =
                    serde_json::from_value::<loop_protocol::rpc::InteractWithdrawnParams>(params)
                {
                    let _ = updates.send(UpdateEnvelope::live(
                        withdrawn.session.instance.clone(),
                        0,
                        withdrawn.session.key.clone(),
                        None,
                        0,
                        SessionUpdate::InteractionResolved {
                            id: withdrawn.id,
                            outcome: loop_protocol::InteractionOutcome::Cancelled,
                        },
                    ));
                }
            }
            Incoming::Notification { method, params }
                if method == methods::SESSION_TERMINAL_OUTPUT =>
            {
                match serde_json::from_value::<TerminalOutputParams>(params) {
                    Ok(output) => {
                        let _ = terminal_output.send(output);
                    }
                    Err(error) => tracing::debug!(%error, "skipping unreadable terminal output"),
                }
            }
            // Notifications this client does not know are ignored.
            Incoming::Notification { .. } => {}
            Incoming::Request { id, method, params } if method == methods::HOST_INTERACT => {
                let conn = Arc::clone(&conn);
                let host = Arc::clone(&host);
                // Answer on its own task: the user may take a while, and
                // updates keep flowing meanwhile.
                tokio::spawn(async move {
                    match serde_json::from_value::<InteractionRequest>(params) {
                        Ok(request) => {
                            let outcome = host.interact(request).await;
                            conn.respond(id, Ok(serde_json::to_value(outcome).unwrap_or_default()));
                        }
                        Err(error) => {
                            conn.respond(id, Err((codes::INVALID_PARAMS, error.to_string())))
                        }
                    }
                });
            }
            Incoming::Request { id, method, .. } => conn.respond(
                id,
                Err((codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
            ),
        }
    }
    conn.fail_pending().await;
    conn.close();
}

/// Workspace admin and host-only methods over the wire. A method the server
/// did not advertise answers Unsupported without a round trip; a host-only
/// one on a remote connection comes back Forbidden.
#[async_trait]
impl crate::control::RuntimeAdmin for RemoteBackend {
    async fn providers(
        &self,
        p: loop_protocol::host::ProvidersParams,
    ) -> BackendResult<loop_protocol::host::ProvidersResult> {
        self.admin_call(|c| c.host_providers, methods::RUNTIME_PROVIDERS, &p)
            .await
    }
    async fn tracing(
        &self,
        p: loop_protocol::host::TracingParams,
    ) -> BackendResult<loop_protocol::host::TracingResult> {
        self.admin_call(|c| c.host_tracing, methods::RUNTIME_TRACING, &p)
            .await
    }
    async fn mcp_auth(
        &self,
        p: loop_protocol::host::McpAuthParams,
    ) -> BackendResult<loop_protocol::host::McpAuthResult> {
        self.admin_call(|c| c.host_mcp_auth, methods::RUNTIME_MCP_AUTH, &p)
            .await
    }

    async fn settings(&self, p: admin::SettingsParams) -> BackendResult<admin::SettingsResult> {
        self.admin_call(|c| c.workspace_settings, methods::WORKSPACE_SETTINGS, &p)
            .await
    }

    fn offered(&self) -> Capabilities {
        self.capabilities()
    }
    async fn mcp(&self, params: admin::McpParams) -> BackendResult<admin::McpResult> {
        self.admin_call(|c| c.workspace_mcp, methods::WORKSPACE_MCP, &params)
            .await
    }
    async fn web_search(
        &self,
        params: admin::WebSearchParams,
    ) -> BackendResult<admin::WebSearchResult> {
        self.admin_call(
            |c| c.workspace_web_search,
            methods::WORKSPACE_WEB_SEARCH,
            &params,
        )
        .await
    }
    async fn skills(&self, params: admin::SkillsParams) -> BackendResult<admin::SkillsResult> {
        self.admin_call(|c| c.workspace_skills, methods::WORKSPACE_SKILLS, &params)
            .await
    }
    async fn templates(
        &self,
        params: admin::TemplatesParams,
    ) -> BackendResult<admin::TemplatesResult> {
        self.admin_call(
            |c| c.workspace_templates,
            methods::WORKSPACE_TEMPLATES,
            &params,
        )
        .await
    }
    async fn reload(&self, params: admin::ReloadParams) -> BackendResult<admin::ReloadResult> {
        self.admin_call(|c| c.workspace_reload, methods::WORKSPACE_RELOAD, &params)
            .await
    }
    async fn trust(&self, params: admin::TrustParams) -> BackendResult<admin::TrustResult> {
        self.admin_call(|c| c.workspace_trust, methods::WORKSPACE_TRUST, &params)
            .await
    }
    async fn sandbox(&self, params: admin::SandboxParams) -> BackendResult<admin::SandboxResult> {
        self.admin_call(|c| c.session_sandbox, methods::SESSION_SANDBOX, &params)
            .await
    }
    async fn permission_mode(
        &self,
        params: admin::PermissionModeParams,
    ) -> BackendResult<admin::PermissionModeResult> {
        self.admin_call(
            |c| c.permission_mode,
            methods::SESSION_PERMISSION_MODE,
            &params,
        )
        .await
    }
    async fn sessions(&self) -> BackendResult<admin::RuntimeSessionsResult> {
        self.admin_call(
            |c| c.runtime_sessions,
            methods::RUNTIME_SESSIONS,
            &serde_json::json!({}),
        )
        .await
    }
    async fn usage(&self, params: admin::UsageParams) -> BackendResult<admin::UsageResult> {
        self.admin_call(|c| c.host_usage, methods::RUNTIME_USAGE, &params)
            .await
    }
    async fn defaults(
        &self,
        params: admin::DefaultsParams,
    ) -> BackendResult<admin::DefaultsResult> {
        self.admin_call(|c| c.host_defaults, methods::RUNTIME_DEFAULTS, &params)
            .await
    }
}

impl RemoteBackend {
    async fn admin_call<T: DeserializeOwned>(
        &self,
        flag: fn(&Capabilities) -> bool,
        method: &str,
        params: &impl serde::Serialize,
    ) -> BackendResult<T> {
        if !flag(&self.capabilities()) {
            return Err(BackendError::Unsupported(method.into()));
        }
        call(&self.conn, method, params).await
    }
}

/// A terminal's `session/terminal/output` for this client, after an attach.
struct RemoteFeed {
    session: SessionKey,
    terminal: String,
    output: tokio::sync::Mutex<broadcast::Receiver<TerminalOutputParams>>,
}

#[async_trait]
impl crate::control::TerminalFeed for RemoteFeed {
    async fn next(&self) -> Option<crate::control::TerminalEvent> {
        use crate::control::TerminalEvent;
        let mut output = self.output.lock().await;
        loop {
            let event = match output.recv().await {
                Ok(event) => event,
                // This client fell behind its own buffer: the feed ends; attach
                // again with the last `seq`.
                Err(_) => return None,
            };
            if event.session != self.session || event.terminal != self.terminal {
                continue;
            }
            return Some(match (event.exit, event.dropped, event.data) {
                (Some(exit), _, _) => TerminalEvent::Exited {
                    seq: event.seq,
                    exit,
                },
                (None, Some(bytes), _) => TerminalEvent::Dropped {
                    seq: event.seq,
                    bytes,
                },
                (None, None, data) => TerminalEvent::Output {
                    seq: event.seq,
                    data: data
                        .and_then(|data| sw::base64::decode(&data).ok())
                        .unwrap_or_default(),
                },
            });
        }
    }
}

impl RemoteBackend {
    async fn workspace_call<T: DeserializeOwned>(
        &self,
        flag: fn(&Capabilities) -> bool,
        method: &str,
        params: &impl serde::Serialize,
    ) -> BackendResult<T> {
        if !flag(&self.capabilities) {
            return Err(BackendError::Unsupported(method.into()));
        }
        call(&self.conn, method, params).await
    }

    async fn workspace_unit(
        &self,
        flag: fn(&Capabilities) -> bool,
        method: &str,
        params: &impl serde::Serialize,
    ) -> BackendResult<()> {
        self.workspace_call::<Value>(flag, method, params)
            .await
            .map(|_| ())
    }
}

fn files(c: &Capabilities) -> bool {
    c.session_files
}
fn files_write(c: &Capabilities) -> bool {
    c.session_files_write
}
fn terminal(c: &Capabilities) -> bool {
    c.session_terminal
}

/// A Session's files and terminals over the wire. A method the server did
/// not advertise answers Unsupported without a round trip.
#[async_trait]
impl crate::control::SessionWorkspace for RemoteBackend {
    async fn info(&self, params: SessionParams) -> BackendResult<sw::SessionWorkspaceInfo> {
        self.workspace_call(files, methods::SESSION_WORKSPACE, &params)
            .await
    }
    async fn list(&self, params: sw::FilesListParams) -> BackendResult<sw::FilesListResult> {
        self.workspace_call(files, methods::SESSION_FILES_LIST, &params)
            .await
    }
    async fn stat(&self, params: sw::FilePathParams) -> BackendResult<sw::FileEntry> {
        self.workspace_call(files, methods::SESSION_FILES_STAT, &params)
            .await
    }
    async fn read(&self, params: sw::FilesReadParams) -> BackendResult<sw::FilesReadResult> {
        self.workspace_call(files, methods::SESSION_FILES_READ, &params)
            .await
    }
    async fn upload_begin(
        &self,
        params: sw::UploadBeginParams,
    ) -> BackendResult<sw::UploadBeginResult> {
        self.workspace_call(files_write, methods::SESSION_FILES_UPLOAD_BEGIN, &params)
            .await
    }
    async fn upload_write(
        &self,
        params: sw::UploadWriteParams,
    ) -> BackendResult<sw::UploadProgress> {
        self.workspace_call(files_write, methods::SESSION_FILES_UPLOAD_WRITE, &params)
            .await
    }
    async fn upload_status(&self, params: sw::UploadParams) -> BackendResult<sw::UploadProgress> {
        self.workspace_call(files_write, methods::SESSION_FILES_UPLOAD_STATUS, &params)
            .await
    }
    async fn upload_commit(
        &self,
        params: sw::UploadParams,
    ) -> BackendResult<sw::UploadCommitResult> {
        self.workspace_call(files_write, methods::SESSION_FILES_UPLOAD_COMMIT, &params)
            .await
    }
    async fn upload_abort(&self, params: sw::UploadParams) -> BackendResult<()> {
        self.workspace_unit(files_write, methods::SESSION_FILES_UPLOAD_ABORT, &params)
            .await
    }
    async fn delete(&self, params: sw::FilesDeleteParams) -> BackendResult<()> {
        self.workspace_unit(files_write, methods::SESSION_FILES_DELETE, &params)
            .await
    }
    async fn rename(&self, params: sw::FilesRenameParams) -> BackendResult<()> {
        self.workspace_unit(files_write, methods::SESSION_FILES_RENAME, &params)
            .await
    }
    async fn mkdir(&self, params: sw::FilePathParams) -> BackendResult<()> {
        self.workspace_unit(files_write, methods::SESSION_FILES_MKDIR, &params)
            .await
    }
    async fn terminal_open(
        &self,
        params: sw::TerminalOpenParams,
    ) -> BackendResult<sw::TerminalOpenResult> {
        self.workspace_call(terminal, methods::SESSION_TERMINAL_OPEN, &params)
            .await
    }
    async fn terminal_attach(
        &self,
        params: sw::TerminalAttachParams,
    ) -> BackendResult<(
        sw::TerminalAttachResult,
        Box<dyn crate::control::TerminalFeed>,
    )> {
        // Subscribed first: output may follow the answer at once.
        let output = self.terminal_output.subscribe();
        let feed = RemoteFeed {
            session: params.session.key.clone(),
            terminal: params.terminal.clone(),
            output: tokio::sync::Mutex::new(output),
        };
        let attached = self
            .workspace_call(terminal, methods::SESSION_TERMINAL_ATTACH, &params)
            .await?;
        Ok((attached, Box::new(feed)))
    }
    async fn terminal_detach(&self, params: sw::TerminalParams) -> BackendResult<()> {
        self.workspace_unit(terminal, methods::SESSION_TERMINAL_DETACH, &params)
            .await
    }
    async fn terminal_input(&self, params: sw::TerminalInputParams) -> BackendResult<()> {
        self.workspace_unit(terminal, methods::SESSION_TERMINAL_INPUT, &params)
            .await
    }
    async fn terminal_resize(&self, params: sw::TerminalResizeParams) -> BackendResult<()> {
        self.workspace_unit(terminal, methods::SESSION_TERMINAL_RESIZE, &params)
            .await
    }
    async fn terminal_signal(&self, params: sw::TerminalSignalParams) -> BackendResult<()> {
        self.workspace_unit(terminal, methods::SESSION_TERMINAL_SIGNAL, &params)
            .await
    }
    async fn terminal_close(
        &self,
        params: sw::TerminalParams,
    ) -> BackendResult<sw::TerminalCloseResult> {
        self.workspace_call(terminal, methods::SESSION_TERMINAL_CLOSE, &params)
            .await
    }
    async fn terminal_list(&self, params: SessionParams) -> BackendResult<sw::TerminalListResult> {
        self.workspace_call(terminal, methods::SESSION_TERMINAL_LIST, &params)
            .await
    }
}
