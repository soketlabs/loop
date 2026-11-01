//! An [`AgentBackend`] on the other end of a [`Transport`].
//!
//! [`RemoteBackend::connect`] runs `initialize`, then forwards the server's
//! `session/update`s to its own broadcast and answers `host/interact` with
//! the [`HostServices`] it was given. Everything else is a request.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use loop_protocol::rpc::{
    methods, AttachParams, AttachResult, ClientCapabilities, CloseParams, InitializeParams,
    InitializeResult, LoadParams, PeerInfo, PromptParams, PromptResult, RpcErrorBody, SessionInfo,
    SessionParams,
};
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, ContentBlock,
    ErrorCategory, ErrorInfo, HostServices, InteractionRequest, SessionKey, SessionRef,
    SessionSpec, SessionUpdate, TurnHandle, TurnOutcome, UpdateEnvelope, PROTOCOL_VERSION,
};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::connection::{codes, Connection, Incoming, RpcError};
use crate::transport::Transport;

/// Updates a client keeps for a slow subscriber before it lags.
const UPDATE_BUFFER: usize = 4096;

/// A backend served over a connection ([`crate::serve`]).
pub struct RemoteBackend {
    conn: Arc<Connection>,
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
}

impl RemoteBackend {
    /// Connect over `transport` and agree on a version. `host` answers the
    /// server's `host/interact` requests.
    pub async fn connect(
        transport: Transport,
        client: PeerInfo,
        host: Arc<dyn HostServices>,
    ) -> BackendResult<Arc<Self>> {
        let (conn, incoming) = Connection::new(transport);
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let busy: Arc<Mutex<HashSet<SessionKey>>> = Arc::default();
        tokio::spawn(read(
            Arc::clone(&conn),
            incoming,
            updates.clone(),
            Arc::clone(&busy),
            host,
        ));
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: client,
            client_capabilities: ClientCapabilities { interact: true },
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
        Ok(Arc::new(Self {
            conn,
            instance: result.instance,
            capabilities: result.capabilities,
            server: result.server_info,
            updates,
            busy,
            local: Arc::default(),
        }))
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

/// Handle everything the server sends, in order.
async fn read(
    conn: Arc<Connection>,
    mut incoming: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    updates: broadcast::Sender<UpdateEnvelope>,
    busy: Arc<Mutex<HashSet<SessionKey>>>,
    host: Arc<dyn HostServices>,
) {
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
}
