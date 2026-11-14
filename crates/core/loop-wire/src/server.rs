//! Serve an [`AgentBackend`] over a [`Transport`].
//!
//! One forwarder task sends everything that goes to the client, so the order
//! the client sees is the order it was sent in. A `session/prompt` response
//! goes out only once the backend has resolved the turn *and* the forwarder
//! has sent that turn's `turn_ended`: backends don't promise which of the two
//! comes first. A turn is recognised as the first `turn_started` on its
//! session after the prompt was accepted; a backend runs one turn per session
//! at a time, so that is unambiguous.
//!
//! With a [`RuntimeControl`] ([`serve_with_control`]) the connection is one of
//! many on a shared Runtime: it receives updates only for Sessions it
//! created, loaded, prompted or attached, and `session/attach` replays the
//! Event Log with no gap and no duplicate (design §7).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loop_protocol::rpc::{
    methods, negotiate, AttachParams, AttachResult, ChoicesResult, CloseParams, InitializeParams,
    InitializeResult, LoadParams, ModelsResult, PeerInfo, PromptParams, PromptResult, RpcErrorBody,
    SessionParams, SetEffortParams, SetModeParams, SetModelParams,
};
use loop_protocol::{
    AgentBackend, BackendInstanceId, BackendResult, Caller, HostServices, InteractionOutcome,
    InteractionRequest, NoticeLevel, Origin, Principal, SessionKey, SessionUpdate, TurnId,
    TurnOutcome, UpdateEnvelope,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::connection::{codes, Connection, Incoming, PeerRequestId, RpcError};
use crate::control::{Attachment, RuntimeControl};
use crate::transport::Transport;

/// How long a prompt's response waits for its `turn_ended` after the
/// backend resolved the turn. A backend that rejected the prompt before the
/// turn started may never send one.
const TURN_END_GRACE: Duration = Duration::from_secs(2);

/// Numbers prompts so two on one session never share a slot.
static NEXT_TICKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A running server.
pub struct Served {
    /// Asks the client: `host/interact`. Give it to whatever needs a user
    /// (the approval bridge, external agents).
    pub host: Arc<dyn HostServices>,
    /// Ends when the client disconnects or sends `shutdown`.
    pub done: JoinHandle<()>,
}

/// Connection ownership policy.
#[derive(Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Shut down the backend when this connection receives `shutdown`.
    /// Shared Runtime connections set this to false.
    pub owns_backend: bool,
    /// What the transport proves about the client. Defaults to remote.
    pub authority: Authority,
}
impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            owns_backend: true,
            authority: Authority::Remote,
        }
    }
}

/// Authority the transport, never a request field, gives a connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Authority {
    /// The Runtime host's own user: its 0600 Unix socket in a 0700
    /// directory, or a trusted embedded adapter. Host-only methods are served.
    HostLocal,
    /// Anything else, such as an authenticated WebSocket (even through a
    /// tunnel): Session and workspace methods only.
    #[default]
    Remote,
}

/// The OS user that runs this process, as a principal in the `local`
/// issuer: `{ person, local, uid:<euid> }` (ADR-0009), or
/// [`Principal::local_os_user`] where there is no uid.
pub fn local_principal() -> Principal {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments or preconditions.
        Principal::local_uid(unsafe { libc::geteuid() })
    }
    // No numeric user id: a distinct id, never `uid:0` (root).
    #[cfg(not(unix))]
    {
        Principal::local_os_user()
    }
}

/// The caller a connection gets when its acceptor names none: the host's
/// user on a host-local connection, and none on any other. A Remote
/// connection never gets the local identity by default; the acceptor that
/// checked the bearer token names it with [`serve_with_caller`].
fn default_caller(authority: Authority) -> Option<Caller> {
    match authority {
        Authority::HostLocal => Some(Caller::host(local_principal())),
        Authority::Remote => None,
    }
}

/// Why `method` is refused for this connection's caller when the caller is
/// a user from outside the host ([`Caller::is_host_user`] is false), such as
/// an Edge Gateway's end user. `None` serves it.
///
/// The rule (H1): an outside user is served only by a Runtime with a tool
/// lock, and never gets `session/shell`. Without the lock a Session can run
/// shell commands as the host's OS user, which reach the Runtime's 0600
/// Unix socket (host-local authority), its credentials and its token files.
/// The lock turns the shell, file writes and every non-chat tool off and
/// confines reads to the Session's folder. The check fails closed: no
/// Runtime, no admin service, or no lock offered all refuse.
fn outside_user_refusal(
    host: &ClientHost,
    runtime: Option<&dyn RuntimeControl>,
    method: &str,
) -> Option<RpcErrorBody> {
    if host.caller.as_ref().is_none_or(Caller::is_host_user) {
        return None;
    }
    let forbidden = |message: &str| {
        let mut body =
            RpcErrorBody::from(&loop_protocol::BackendError::Forbidden(method.to_owned()));
        body.message = format!("{method} is refused: {message}");
        body
    };
    if admin_offered(runtime).tool_lock.is_none() {
        return Some(forbidden(
            "an Edge Gateway user is served only by a Runtime with a tool lock",
        ));
    }
    if method == methods::SESSION_SHELL {
        return Some(forbidden(
            "an Edge Gateway user cannot run shell commands on the Runtime host",
        ));
    }
    None
}

/// A remote connection with no caller: it has no identity to check, so the
/// server serves it nothing but `initialize`.
fn anonymous(authority: Authority, host: &ClientHost) -> bool {
    authority != Authority::HostLocal && host.caller.is_none()
}

/// Whether `method` with `params` needs host-local authority: the host-only
/// methods, and a `workspace/trust` change.
fn host_only(method: &str, params: &Value) -> bool {
    methods::HOST_ONLY.contains(&method)
        || (method == methods::WORKSPACE_TRUST
            && params.get("trusted").is_some_and(|t| !t.is_null()))
}

/// Serve `backend` to one client over `transport` with backend ownership.
/// Existing in-process callers retain their shutdown behavior.
pub fn serve(backend: Arc<dyn AgentBackend>, transport: Transport, info: PeerInfo) -> Served {
    serve_with_options(backend, transport, info, ServeOptions::default())
}

/// Serve one connection with an explicit backend ownership policy.
pub fn serve_with_options(
    backend: Arc<dyn AgentBackend>,
    transport: Transport,
    info: PeerInfo,
    options: ServeOptions,
) -> Served {
    let caller = default_caller(options.authority);
    start(backend, None, transport, info, options, caller)
}

/// Serve one connection of a shared Runtime: multi-Session methods, per-
/// connection update filtering and attach. `backend` and `control` are
/// normally the same Runtime.
pub fn serve_with_control(
    backend: Arc<dyn AgentBackend>,
    control: Arc<dyn RuntimeControl>,
    transport: Transport,
    info: PeerInfo,
    options: ServeOptions,
) -> Served {
    let caller = default_caller(options.authority);
    start(backend, Some(control), transport, info, options, caller)
}

/// [`serve_with_control`] (or, without `control`, [`serve_with_options`])
/// for a connection whose acceptor authenticated `caller`: the loopback
/// WebSocket with the bearer token, or an Edge Gateway connection that
/// asserted a principal at its handshake. The caller is fixed for the
/// connection's life; no request can change it. It does not change
/// `options.authority`: only a host-local transport serves host-only methods.
pub fn serve_with_caller(
    backend: Arc<dyn AgentBackend>,
    control: Option<Arc<dyn RuntimeControl>>,
    transport: Transport,
    info: PeerInfo,
    options: ServeOptions,
    caller: Caller,
) -> Served {
    start(backend, control, transport, info, options, Some(caller))
}

fn start(
    backend: Arc<dyn AgentBackend>,
    runtime: Option<Arc<dyn RuntimeControl>>,
    transport: Transport,
    info: PeerInfo,
    options: ServeOptions,
    caller: Option<Caller>,
) -> Served {
    let (conn, incoming) = Connection::new(transport);
    let interact = Arc::new(AtomicBool::new(true));
    let host = Arc::new(ClientHost {
        conn: Arc::clone(&conn),
        interact: Arc::clone(&interact),
        branch_reset: AtomicBool::new(false),
        id: NEXT_TICKET.fetch_add(1, Ordering::Relaxed),
        caller,
    });
    let served = run(
        backend,
        runtime,
        conn,
        incoming,
        info,
        options,
        host.clone(),
    );
    let done = tokio::spawn(in_scope(host.caller.clone(), served));
    Served { host, done }
}

/// Run `future` with `caller` as [`Caller::current`], when there is one.
async fn in_scope<F: std::future::Future>(caller: Option<Caller>, future: F) -> F::Output {
    match caller {
        Some(caller) => caller.scope(future).await,
        None => future.await,
    }
}

/// What the request loop tells the forwarder.
enum Control {
    /// Start forwarding updates; the client finished `initialize`.
    Start,
    /// A prompt is about to start; answer `id` once its turn ends.
    Prompted {
        ticket: u64,
        session: SessionKey,
        id: PeerRequestId,
        outcome: oneshot::Receiver<TurnOutcome>,
    },
    /// The backend refused the prompt; the request loop answers it.
    Rejected { ticket: u64 },
    /// A `session/new` or `session/load` is running: hold updates of
    /// Sessions not yet known until it says which one it made.
    RegisterBegin,
    /// That Session is now this connection's.
    Owned { session: loop_protocol::SessionRef },
    /// The `session/new` or `session/load` finished.
    RegisterEnd,
    /// An attach started: buffer live updates of `key` until it is answered.
    Attaching { key: SessionKey, op: u64 },
    /// The attach finished; answer `id`, replay, then go live.
    Attached {
        id: PeerRequestId,
        key: SessionKey,
        op: u64,
        after_seq: u64,
        result: Box<BackendResult<Attachment>>,
        session: loop_protocol::SessionRef,
    },
    /// Stop forwarding `key`, then answer `id`.
    Detach {
        id: PeerRequestId,
        session: loop_protocol::SessionRef,
    },
}

async fn run(
    backend: Arc<dyn AgentBackend>,
    runtime: Option<Arc<dyn RuntimeControl>>,
    conn: Arc<Connection>,
    mut incoming: mpsc::Receiver<Incoming>,
    info: PeerInfo,
    options: ServeOptions,
    host: Arc<ClientHost>,
) {
    // Subscribe now so nothing between `initialize` and the first update is
    // missed; forwarding starts once the client is initialized.
    let updates = backend.subscribe();
    let (control, control_rx) = mpsc::unbounded_channel();
    // Not in the caller's scope: it only forwards updates and registers
    // this connection for Escalations, and reads no identity.
    let forwarder = tokio::spawn(forward(
        Arc::clone(&conn),
        backend.instance().clone(),
        updates,
        control_rx,
        runtime.clone(),
        host.clone(),
    ));
    let mut initialized = false;
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Response { id, result } => conn.resolve(id, result).await,
            Incoming::Notification { method, params } => {
                if method == methods::SESSION_CANCEL
                    && !anonymous(options.authority, &host)
                    && outside_user_refusal(&host, runtime.as_deref(), &method).is_none()
                {
                    if let Ok(SessionParams { session }) = parse::<SessionParams>(params) {
                        backend.cancel(&session);
                    }
                }
            }
            Incoming::Request { id, method, params } => {
                if method == methods::INITIALIZE {
                    match parse::<InitializeParams>(params) {
                        Ok(params) => {
                            host.interact
                                .store(params.client_capabilities.interact, Ordering::Release);
                            host.branch_reset
                                .store(params.client_capabilities.branch_reset, Ordering::Release);
                            let mut capabilities = backend.capabilities();
                            // Advertise only what this connection can serve.
                            capabilities.multi_session = runtime.is_some();
                            capabilities.attach = runtime.is_some();
                            capabilities.attach_snapshot &= runtime.is_some();
                            if backend.model_control().is_none() {
                                capabilities.set_model = false;
                                capabilities.efforts = false;
                                capabilities.modes = false;
                            }
                            // From the connection, never from `params`.
                            let caller = host.caller.clone();
                            let offered = admin_offered(runtime.as_deref());
                            let host = options.authority == Authority::HostLocal;
                            capabilities.workspace_settings = offered.workspace_settings;
                            capabilities.workspace_mcp = offered.workspace_mcp;
                            capabilities.workspace_web_search = offered.workspace_web_search;
                            capabilities.workspace_skills = offered.workspace_skills;
                            capabilities.workspace_templates = offered.workspace_templates;
                            capabilities.workspace_reload = offered.workspace_reload;
                            capabilities.workspace_trust = offered.workspace_trust;
                            capabilities.session_sandbox = offered.session_sandbox;
                            capabilities.permission_mode = offered.permission_mode;
                            capabilities.runtime_sessions = offered.runtime_sessions;
                            capabilities.host_providers = offered.host_providers && host;
                            capabilities.host_tracing = offered.host_tracing && host;
                            capabilities.host_mcp_auth = offered.host_mcp_auth && host;
                            capabilities.host_usage = offered.host_usage && host;
                            capabilities.host_defaults = offered.host_defaults && host;
                            capabilities.runtime_stop_empty = offered.runtime_stop_empty && host;
                            // A tool lock holds on every connection: the
                            // client can hide what it turns off. The backend
                            // still refuses those calls itself.
                            capabilities.tool_lock = offered.tool_lock;
                            if capabilities.tool_lock.is_some() {
                                capabilities.local_shell = false;
                                capabilities.workflow = false;
                                // Host-only services are refused under a lock
                                // ([`LOCKED_ADMIN`]).
                                capabilities.host_providers = false;
                                capabilities.host_tracing = false;
                                capabilities.host_mcp_auth = false;
                                capabilities.host_usage = false;
                                capabilities.host_defaults = false;
                            }
                            let caller = caller.as_ref();
                            let result = InitializeResult {
                                protocol_version: negotiate(params.protocol_version),
                                server_info: info.clone(),
                                instance: backend.instance().clone(),
                                capabilities,
                                principal: caller.map(|c| c.principal().clone()),
                                tenant: caller.map(|c| c.tenant().clone()),
                                via: caller.and_then(|c| c.via().cloned()),
                            };
                            conn.respond(id, Ok(to_value(&result)));
                            if !initialized {
                                initialized = true;
                                let _ = control.send(Control::Start);
                            }
                        }
                        Err(error) => conn.respond(id, Err(error)),
                    }
                    continue;
                }
                if !initialized {
                    conn.respond(
                        id,
                        Err((codes::INVALID_REQUEST, "call initialize first".into())),
                    );
                    continue;
                }
                // An outside user (an Edge Gateway assertion) is served only
                // by a tool-locked Runtime, and never gets `session/shell`.
                if let Some(refused) = outside_user_refusal(&host, runtime.as_deref(), &method) {
                    respond(&conn, id, Err(refused));
                    continue;
                }
                // A remote connection that no acceptor named a caller for has
                // no identity to check access with (I3), so it is served
                // nothing but `initialize`. Without a caller in scope the
                // Runtime would treat a request as its own in-process work.
                if anonymous(options.authority, &host) {
                    let mut body =
                        RpcErrorBody::from(&loop_protocol::BackendError::Forbidden(method.clone()));
                    body.message = format!(
                        "{method} is refused: this connection has no identity, so it cannot \
                         use Sessions or run shell commands"
                    );
                    respond(&conn, id, Err(body));
                    continue;
                }
                // Refused before parsing or dispatch, whatever the params.
                if options.authority != Authority::HostLocal && host_only(&method, &params) {
                    let body =
                        RpcErrorBody::from(&loop_protocol::BackendError::Forbidden(method.clone()));
                    respond(&conn, id, Err(body));
                    continue;
                }
                if method == methods::SHUTDOWN {
                    if options.owns_backend {
                        backend.shutdown().await;
                    }
                    conn.respond(id, Ok(json!({})));
                    conn.close();
                    break;
                }
                let backend = Arc::clone(&backend);
                let runtime = runtime.clone();
                let control = control.clone();
                let host = host.clone();
                // Each request runs on its own, so a long `session/load`
                // doesn't hold up a `session/cancel`. The connection's
                // caller is in scope for the backend and the Runtime.
                let caller = host.caller.clone();
                tokio::spawn(in_scope(caller, async move {
                    handle(
                        backend.as_ref(),
                        runtime.as_deref(),
                        &host,
                        &control,
                        id,
                        &method,
                        params,
                    )
                    .await;
                }));
            }
        }
    }
    forwarder.abort();
    let _ = forwarder.await;
    if let Some(runtime) = &runtime {
        runtime.disconnected(host.id).await;
    }
    conn.fail_pending().await;
}

async fn handle(
    backend: &dyn AgentBackend,
    runtime: Option<&dyn RuntimeControl>,
    host: &Arc<ClientHost>,
    control: &mpsc::UnboundedSender<Control>,
    id: PeerRequestId,
    method: &str,
    params: Value,
) {
    let conn = &host.conn;
    let result = match method {
        methods::SESSION_NEW => match parse(params) {
            Ok(spec) => {
                let _ = control.send(Control::RegisterBegin);
                let made = backend.new_session(spec).await;
                if let Ok(session) = &made {
                    let _ = control.send(Control::Owned {
                        session: session.clone(),
                    });
                }
                let _ = control.send(Control::RegisterEnd);
                made.map(|s| to_value(&s))
            }
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_LOAD => match parse::<LoadParams>(params) {
            Ok(LoadParams { session, spec }) => {
                let _ = control.send(Control::RegisterBegin);
                let loaded = backend.load_session(&session, spec).await;
                if let Ok(session) = &loaded {
                    let _ = control.send(Control::Owned {
                        session: session.clone(),
                    });
                }
                let _ = control.send(Control::RegisterEnd);
                loaded.map(|s| to_value(&s))
            }
            Err(error) => return conn.respond(id, Err(error)),
        },
        // Session-tree dispatch stays grouped for parallel protocol additions.
        methods::SESSION_CONFIG if runtime.is_some() => {
            if !backend.capabilities().session_config {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse(params) {
                    Ok(p) => runtime.unwrap().config(p).await.map(|r| to_value(&r)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_SKILL if runtime.is_some() => {
            if !backend.capabilities().session_skills {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse(params) {
                    Ok(p) => runtime.unwrap().skill(p).await.map(|r| to_value(&r)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_WORKFLOW if runtime.is_some() => {
            if !backend.capabilities().workflow {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse(params) {
                    Ok(p) => runtime.unwrap().workflow(p).await.map(|r| to_value(&r)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_TREE if runtime.is_some() => {
            if !backend.capabilities().session_tree {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::TreeParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .tree(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_VIEW if runtime.is_some() => {
            if !backend.capabilities().branch_snapshot {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::ViewParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .view(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_NAVIGATE if runtime.is_some() => {
            if !backend.capabilities().session_tree {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::NavigateParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .navigate(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_RENAME if runtime.is_some() => {
            if !backend.capabilities().session_rename {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::RenameParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .rename(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_FORK if runtime.is_some() => {
            if !backend.capabilities().session_fork {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::ForkParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .fork(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_CLONE if runtime.is_some() => {
            if !backend.capabilities().session_fork {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::CloneParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .clone_session(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_EXPORT if runtime.is_some() => {
            if !backend.capabilities().session_transfer {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::SessionParams>(params) {
                    Ok(params) => runtime.unwrap().export(params).await.and_then(|result| {
                        export_response_fits(&id, &result)?;
                        Ok(to_value(&result))
                    }),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_IMPORT if runtime.is_some() => {
            if !backend.capabilities().session_transfer {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                match parse::<loop_protocol::rpc::ImportParams>(params) {
                    Ok(params) => runtime
                        .unwrap()
                        .import(params)
                        .await
                        .map(|result| to_value(&result)),
                    Err(error) => return conn.respond(id, Err(error)),
                }
            }
        }
        methods::SESSION_LIST if runtime.is_some() => {
            runtime.unwrap().list().await.map(|l| to_value(&l))
        }
        methods::RUNTIME_STATUS if runtime.is_some() => match runtime.unwrap().status().await {
            Ok(status) if host.caller.as_ref().is_some_and(|c| !c.is_host_user()) => {
                outside_status(runtime.unwrap(), status)
                    .await
                    .map(|s| to_value(&s))
            }
            other => other.map(|s| to_value(&s)),
        },
        methods::RUNTIME_STOP_EMPTY if runtime.is_some() => {
            if !admin_offered(runtime).runtime_stop_empty {
                Err(loop_protocol::BackendError::Unsupported(method.into()))
            } else {
                runtime.unwrap().stop_empty().await.map(|()| json!({}))
            }
        }
        methods::RUNTIME_STOP if runtime.is_some() => {
            runtime.unwrap().stop().await.map(|()| json!({}))
        }
        methods::SESSION_CLOSE if runtime.is_some() => match parse::<CloseParams>(params) {
            Ok(CloseParams { session, force }) => runtime
                .unwrap()
                .close(&session, force)
                .await
                .map(|()| json!({})),
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_ATTACH if runtime.is_some() => match parse::<AttachParams>(params) {
            Ok(AttachParams { session, after_seq }) => {
                // The forwarder buffers this Session's live updates from here
                // until it has sent the replay, then answers the request.
                let op = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
                let _ = control.send(Control::Attaching {
                    key: session.key.clone(),
                    op,
                });
                let result = if host.branch_reset.load(Ordering::Acquire) {
                    runtime
                        .unwrap()
                        .attach_snapshot_watching(host.id, &session, after_seq)
                        .await
                } else {
                    runtime
                        .unwrap()
                        .attach_watching(host.id, &session, after_seq)
                        .await
                };
                let _ = control.send(Control::Attached {
                    id,
                    key: session.key.clone(),
                    session,
                    op,
                    after_seq,
                    result: Box::new(result),
                });
                return;
            }
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_DETACH if runtime.is_some() => match parse::<SessionParams>(params) {
            Ok(SessionParams { session }) => {
                let _ = control.send(Control::Detach { id, session });
                return;
            }
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_TRANSCRIPT => match parse::<SessionParams>(params) {
            Ok(SessionParams { session }) => {
                backend.transcript(&session).await.map(|t| to_value(&t))
            }
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_PROMPT => match parse::<PromptParams>(params) {
            Ok(PromptParams { session, content }) => {
                // Register first, so the forwarder knows to watch for this
                // turn's `turn_started` before the backend can send it.
                let (tx, outcome) = oneshot::channel();
                let ticket = NEXT_TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = control.send(Control::Prompted {
                    ticket,
                    session: session.key.clone(),
                    id: id.clone(),
                    outcome,
                });
                match backend.prompt(&session, content).await {
                    Ok(turn) => {
                        tokio::spawn(async move {
                            let _ = tx.send(turn.outcome().await);
                        });
                        return;
                    }
                    Err(error) => {
                        let _ = control.send(Control::Rejected { ticket });
                        Err(error)
                    }
                }
            }
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_MODELS
        | methods::SESSION_SET_MODEL
        | methods::SESSION_EFFORTS
        | methods::SESSION_SET_EFFORT
        | methods::SESSION_MODES
        | methods::SESSION_SET_MODE => match model_control(backend, method, params).await {
            Ok(result) => result,
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_TTFB => match ttfb(backend, params).await {
            Ok(result) => result,
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_COMPACT | methods::SESSION_STATS | methods::SESSION_SHELL => {
            match session_services(backend, method, params).await {
                Ok(result) => result,
                Err(error) => return conn.respond(id, Err(error)),
            }
        }
        methods::RUNTIME_PROVIDERS
        | methods::RUNTIME_TRACING
        | methods::RUNTIME_MCP_AUTH
        | methods::WORKSPACE_SETTINGS
        | methods::WORKSPACE_MCP
        | methods::WORKSPACE_WEB_SEARCH
        | methods::WORKSPACE_SKILLS
        | methods::WORKSPACE_TEMPLATES
        | methods::WORKSPACE_RELOAD
        | methods::WORKSPACE_TRUST
        | methods::SESSION_SANDBOX
        | methods::SESSION_PERMISSION_MODE
        | methods::RUNTIME_SESSIONS
        | methods::RUNTIME_USAGE
        | methods::RUNTIME_DEFAULTS => {
            match admin(admin_offered(runtime), runtime, method, params).await {
                Ok(result) => result,
                Err(error) => return conn.respond(id, Err(error)),
            }
        }
        _ => {
            return conn.respond(
                id,
                Err((codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
            )
        }
    };
    respond(conn, id, result.map_err(|e| RpcErrorBody::from(&e)));
}

/// `session/models`, `session/efforts`, `session/modes` and their setters.
/// A backend without the service, or without the method's capability,
/// answers Unsupported. The outer error is a malformed request.
async fn model_control(
    backend: &dyn AgentBackend,
    method: &str,
    params: Value,
) -> Result<BackendResult<Value>, (i64, String)> {
    let capabilities = backend.capabilities();
    let (offered, what) = match method {
        methods::SESSION_MODELS | methods::SESSION_SET_MODEL => {
            (capabilities.set_model, "choosing a model")
        }
        methods::SESSION_EFFORTS | methods::SESSION_SET_EFFORT => {
            (capabilities.efforts, "choosing a reasoning effort")
        }
        _ => (capabilities.modes, "choosing a mode"),
    };
    let Some(control) = backend.model_control().filter(|_| offered) else {
        return Ok(Err(loop_protocol::BackendError::Unsupported(what.into())));
    };
    Ok(match method {
        methods::SESSION_MODELS => {
            let SessionParams { session } = parse(params)?;
            control
                .models(&session)
                .await
                .map(|c| to_value(&ModelsResult::from_choices(c, &session.instance)))
        }
        methods::SESSION_EFFORTS => {
            let SessionParams { session } = parse(params)?;
            control
                .efforts(&session)
                .await
                .map(|c| to_value(&ChoicesResult::from(c)))
        }
        methods::SESSION_MODES => {
            let SessionParams { session } = parse(params)?;
            control
                .modes(&session)
                .await
                .map(|c| to_value(&ChoicesResult::from(c)))
        }
        methods::SESSION_SET_MODEL => {
            let p: SetModelParams = parse(params)?;
            control
                .set_model(&p.session, &p.model, p.expected_revision)
                .await
                .map(|r| to_value(&r))
        }
        methods::SESSION_SET_EFFORT => {
            let p: SetEffortParams = parse(params)?;
            control
                .set_effort(&p.session, &p.effort, p.expected_revision)
                .await
                .map(|r| to_value(&r))
        }
        _ => {
            let p: SetModeParams = parse(params)?;
            control
                .set_mode(&p.session, &p.mode, p.expected_revision)
                .await
                .map(|r| to_value(&r))
        }
    })
}

/// `session/ttfb`: read or change the Session's response-timeout override.
/// It never writes the default for new Sessions, so it is Session-scoped
/// and served on every authenticated transport.
async fn ttfb(
    backend: &dyn AgentBackend,
    params: Value,
) -> Result<BackendResult<Value>, (i64, String)> {
    let Some(control) = backend
        .model_control()
        .filter(|_| backend.capabilities().session_ttfb)
    else {
        return Ok(Err(loop_protocol::BackendError::Unsupported(
            "changing the response timeout".into(),
        )));
    };
    let p: loop_protocol::rpc::TtfbParams = parse(params)?;
    let change = match p.change() {
        Ok(change) => change,
        Err(error) => return Ok(Err(error)),
    };
    Ok(control
        .ttfb(&p.session, change, p.expected_revision)
        .await
        .map(|r| to_value(&r)))
}

/// `session/compact`, `session/stats` and `session/shell`. A backend
/// without the service, or without its capability, answers Unsupported.
/// The outer error is a malformed request.
async fn session_services(
    backend: &dyn AgentBackend,
    method: &str,
    params: Value,
) -> Result<BackendResult<Value>, (i64, String)> {
    let capabilities = backend.capabilities();
    let unsupported = |what: &str| Ok(Err(loop_protocol::BackendError::Unsupported(what.into())));
    Ok(match method {
        methods::SESSION_COMPACT => {
            let Some(service) = backend.compaction().filter(|_| capabilities.compaction) else {
                return unsupported("compacting the conversation");
            };
            let p: loop_protocol::rpc::CompactParams = parse(params)?;
            service
                .compact(&p.session, p.instructions.as_deref())
                .await
                .map(|r| to_value(&r))
        }
        methods::SESSION_STATS => {
            let Some(service) = backend
                .session_stats()
                .filter(|_| capabilities.session_stats)
            else {
                return unsupported("session statistics");
            };
            let SessionParams { session } = parse(params)?;
            service.stats(&session).await.map(|stats| {
                let busy = backend.is_busy(&session);
                to_value(&loop_protocol::rpc::StatsResult {
                    total_tokens: stats.total_tokens,
                    context_tokens: stats.context_tokens,
                    context_window: stats.context_window,
                    report: stats.report,
                    turn_state: stats
                        .turn_state
                        .unwrap_or_else(|| if busy { "busy" } else { "idle" }.into()),
                    usage: stats.usage,
                    taken_at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis() as u64),
                })
            })
        }
        _ => {
            let Some(service) = backend.local_shell().filter(|_| capabilities.local_shell) else {
                return unsupported("running shell commands");
            };
            let p: loop_protocol::rpc::ShellParams = parse(params)?;
            service
                .run(&p.session, &p.command, p.timeout_ms)
                .await
                .map(|r| to_value(&r))
        }
    })
}

/// The admin methods a Runtime with a tool lock serves. Each one's writes
/// are refused by the Runtime under the lock; reads stay.
pub const LOCKED_ADMIN: &[&str] = &[
    methods::WORKSPACE_SETTINGS,
    methods::WORKSPACE_MCP,
    methods::WORKSPACE_WEB_SEARCH,
    methods::WORKSPACE_SKILLS,
    methods::WORKSPACE_TEMPLATES,
    methods::WORKSPACE_RELOAD,
    methods::WORKSPACE_TRUST,
    methods::SESSION_SANDBOX,
    methods::SESSION_PERMISSION_MODE,
    methods::RUNTIME_SESSIONS,
];

/// The admin flags the Runtime's admin service offers; none without one.
fn admin_offered(runtime: Option<&dyn RuntimeControl>) -> loop_protocol::Capabilities {
    runtime
        .and_then(|r| r.admin())
        .map(|a| a.offered())
        .unwrap_or_default()
}

/// Workspace admin and host-only services. Host-local authority was
/// checked before dispatch. A Runtime without the service, or without the
/// method's capability, answers Unsupported; the outer error is a malformed
/// request.
async fn admin(
    capabilities: loop_protocol::Capabilities,
    runtime: Option<&dyn RuntimeControl>,
    method: &str,
    params: Value,
) -> Result<BackendResult<Value>, (i64, String)> {
    // Under a tool lock only the admin methods a chat client needs are
    // served; any other one, a new one included, is refused. The Runtime
    // still refuses their writes itself.
    if let Some(lock) = capabilities.tool_lock {
        if !LOCKED_ADMIN.contains(&method) {
            return Ok(Err(loop_protocol::BackendError::ToolLocked {
                what: method.into(),
                lock,
            }));
        }
    }
    let offered = match method {
        methods::RUNTIME_PROVIDERS => capabilities.host_providers,
        methods::RUNTIME_TRACING => capabilities.host_tracing,
        methods::RUNTIME_MCP_AUTH => capabilities.host_mcp_auth,
        methods::WORKSPACE_SETTINGS => capabilities.workspace_settings,
        methods::WORKSPACE_MCP => capabilities.workspace_mcp,
        methods::WORKSPACE_WEB_SEARCH => capabilities.workspace_web_search,
        methods::WORKSPACE_SKILLS => capabilities.workspace_skills,
        methods::WORKSPACE_TEMPLATES => capabilities.workspace_templates,
        methods::WORKSPACE_RELOAD => capabilities.workspace_reload,
        methods::WORKSPACE_TRUST => capabilities.workspace_trust,
        methods::SESSION_SANDBOX => capabilities.session_sandbox,
        methods::SESSION_PERMISSION_MODE => capabilities.permission_mode,
        methods::RUNTIME_SESSIONS => capabilities.runtime_sessions,
        methods::RUNTIME_USAGE => capabilities.host_usage,
        _ => capabilities.host_defaults,
    };
    let Some(admin) = runtime.and_then(|r| r.admin()).filter(|_| offered) else {
        return Ok(Err(loop_protocol::BackendError::Unsupported(method.into())));
    };
    Ok(match method {
        methods::RUNTIME_PROVIDERS => admin.providers(parse(params)?).await.map(|r| to_value(&r)),
        methods::RUNTIME_TRACING => admin.tracing(parse(params)?).await.map(|r| to_value(&r)),
        methods::RUNTIME_MCP_AUTH => admin.mcp_auth(parse(params)?).await.map(|r| to_value(&r)),
        methods::WORKSPACE_SETTINGS => admin.settings(parse(params)?).await.map(|r| to_value(&r)),
        methods::WORKSPACE_MCP => admin.mcp(parse(params)?).await.map(|r| to_value(&r)),
        methods::WORKSPACE_WEB_SEARCH => {
            admin.web_search(parse(params)?).await.map(|r| to_value(&r))
        }
        methods::WORKSPACE_SKILLS => admin.skills(parse(params)?).await.map(|r| to_value(&r)),
        methods::WORKSPACE_TEMPLATES => admin.templates(parse(params)?).await.map(|r| to_value(&r)),
        methods::WORKSPACE_RELOAD => admin.reload(parse(params)?).await.map(|r| to_value(&r)),
        methods::WORKSPACE_TRUST => admin.trust(parse(params)?).await.map(|r| to_value(&r)),
        methods::SESSION_SANDBOX => admin.sandbox(parse(params)?).await.map(|r| to_value(&r)),
        methods::SESSION_PERMISSION_MODE => admin
            .permission_mode(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::RUNTIME_SESSIONS => admin.sessions().await.map(|r| to_value(&r)),
        methods::RUNTIME_USAGE => admin.usage(parse(params)?).await.map(|r| to_value(&r)),
        _ => admin.defaults(parse(params)?).await.map(|r| to_value(&r)),
    })
}

/// A prompt waiting for its turn to end.
struct Waiting {
    ticket: u64,
    session: SessionKey,
    id: PeerRequestId,
    /// The turn, once its `turn_started` was forwarded.
    turn: Option<TurnId>,
    /// Whether the turn's `turn_ended` was forwarded.
    ended: bool,
    outcome: Option<TurnOutcome>,
    /// When to answer anyway, once the outcome is known.
    deadline: Option<tokio::time::Instant>,
}

async fn forward(
    conn: Arc<Connection>,
    instance: BackendInstanceId,
    mut updates: broadcast::Receiver<UpdateEnvelope>,
    mut control: mpsc::UnboundedReceiver<Control>,
    runtime: Option<Arc<dyn RuntimeControl>>,
    host: Arc<ClientHost>,
) {
    let filtering = runtime.is_some();
    let mut feeds = Feeds::default();
    let mut started = false;
    // In the order the prompts were sent; a session's next `turn_started`
    // belongs to its oldest prompt without a turn.
    let mut waiting: Vec<Waiting> = Vec::new();
    let (outcome_tx, mut outcomes) = mpsc::unbounded_channel::<(u64, TurnOutcome)>();
    loop {
        let next_deadline = waiting.iter().filter_map(|w| w.deadline).min();
        tokio::select! {
            // Control first: a prompt registered before its turn started
            // must be seen before that turn's updates.
            biased;
            command = control.recv() => match command {
                Some(Control::Start) => { started = true; feeds.branch_reset = host.branch_reset.load(Ordering::Acquire); },
                Some(Control::Rejected { ticket }) => waiting.retain(|w| w.ticket != ticket),
                Some(Control::RegisterBegin) => feeds.registering += 1,
                Some(Control::RegisterEnd) => {
                    feeds.registering = feeds.registering.saturating_sub(1);
                    if feeds.registering == 0 {
                        feeds.pending.clear();
                        feeds.dropped = 0;
                    }
                }
                Some(Control::Owned { session }) => {
                    let key = session.key.clone();
                    if feeds.dropped > 0 {
                        // Updates were discarded while this Session was being
                        // made; never skip silently.
                        let outside = host.caller.as_ref().is_some_and(|c| !c.is_host_user());
                        lagged(&conn, &instance, (!outside).then_some(feeds.dropped));
                        return;
                    }
                    // The Runtime checks the caller may read it; a refusal
                    // keeps the feed off.
                    if !register_recipient(runtime.as_deref(), &host, &session, true).await {
                        continue;
                    }
                    if feeds.own(key.clone()) {
                        for envelope in feeds.take_pending(&key) {
                            if feeds.deliver(&conn, envelope).is_err() {
                                return;
                            }
                        }
                    }
                }
                Some(Control::Attaching { key, op }) => feeds.start_attach(key, op),
                Some(Control::Attached { id, key, op, after_seq, result, session }) => {
                    let attached = result.is_ok() && matches!(feeds.map.get(&key), Some(Feed::Attaching { op: current, .. }) if *current == op);
                    if feeds.finish_attach(&conn, id, key, op, after_seq, *result).is_err() {
                        return;
                    }
                    if attached
                        && feeds.map.contains_key(&session.key)
                        && !register_recipient(runtime.as_deref(), &host, &session, true).await
                    {
                        // The Runtime refused this caller: no live updates.
                        feeds.map.remove(&session.key);
                    }
                }
                Some(Control::Detach { id, session }) => {
                    register_recipient(runtime.as_deref(), &host, &session, false).await;
                    feeds.map.remove(&session.key);
                    conn.respond(id, Ok(json!({})));
                }
                Some(Control::Prompted { ticket, session, id, outcome }) => {
                    let tx = outcome_tx.clone();
                    tokio::spawn(async move {
                        let outcome = outcome.await.unwrap_or(TurnOutcome::Uncertain);
                        let _ = tx.send((ticket, outcome));
                    });
                    waiting.push(Waiting {
                        ticket,
                        session,
                        id,
                        turn: None,
                        ended: false,
                        outcome: None,
                        deadline: None,
                    });
                }
                None => return,
            },
            Some((ticket, outcome)) = outcomes.recv() => {
                if let Some(w) = waiting.iter_mut().find(|w| w.ticket == ticket) {
                    w.outcome = Some(outcome);
                    w.deadline = Some(tokio::time::Instant::now() + TURN_END_GRACE);
                }
            }
            update = updates.recv(), if started => match update {
                Ok(envelope) => {
                    if let SessionUpdate::TurnStarted { turn_id } = &envelope.update {
                        if let Some(w) = waiting
                            .iter_mut()
                            .find(|w| w.session == envelope.session && w.turn.is_none())
                        {
                            w.turn = Some(turn_id.clone());
                        }
                    }
                    let ended_turn = match &envelope.update {
                        SessionUpdate::TurnEnded { turn_id, .. } => Some(turn_id.clone()),
                        _ => None,
                    };
                    let routed = if filtering {
                        feeds.route(&conn, envelope)
                    } else {
                        conn.notify(methods::SESSION_UPDATE, to_value(&envelope))
                            .map(|()| false)
                    };
                    match routed {
                        Err(_) => return,
                        // A buffered end is sent later by the attach, which
                        // records it in `feeds.ended`.
                        Ok(buffered) => {
                            if let (Some(turn), false) = (ended_turn, buffered) {
                                feeds.ended.push(turn);
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    // Never let the client silently miss part of a turn. The
                    // channel is shared: an outside user is not told how
                    // many updates (of every Session) went by.
                    let outside = host.caller.as_ref().is_some_and(|c| !c.is_host_user());
                    lagged(&conn, &instance, (!outside).then_some(missed));
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    for w in waiting.drain(..) {
                        let outcome = w.outcome.unwrap_or(TurnOutcome::Uncertain);
                        conn.respond(w.id, Ok(to_value(&PromptResult::from(outcome))));
                    }
                    return;
                }
            },
            () = sleep_until(next_deadline), if next_deadline.is_some() => {
                let now = tokio::time::Instant::now();
                for w in &mut waiting {
                    if w.deadline.is_some_and(|d| d <= now) {
                        w.ended = true;
                    }
                }
            }
        }
        for session in std::mem::take(&mut feeds.detached) {
            register_recipient(runtime.as_deref(), &host, &session, false).await;
        }
        for turn in feeds.ended.drain(..) {
            for w in waiting
                .iter_mut()
                .filter(|w| w.turn.as_ref() == Some(&turn))
            {
                w.ended = true;
            }
        }
        let (done, still): (Vec<Waiting>, Vec<Waiting>) = waiting
            .into_iter()
            .partition(|w| w.ended && w.outcome.is_some());
        waiting = still;
        for w in done {
            let outcome = w.outcome.unwrap_or(TurnOutcome::Uncertain);
            conn.respond(w.id, Ok(to_value(&PromptResult::from(outcome))));
        }
    }
}

/// What a connection receives of one Session.
enum Feed {
    /// Forwarding; `last_sent` is the newest `session_seq` the client has.
    Live { last_sent: u64 },
    /// An attach is reading the Event Log; live updates wait in `buffer`.
    /// `prior` is the cursor of an earlier feed, kept if the attach fails.
    Attaching {
        /// The attach operation that owns this entry.
        op: u64,
        buffer: Vec<UpdateEnvelope>,
        prior: Option<u64>,
    },
}

/// The Sessions this connection created, loaded or attached.
struct Feeds {
    map: HashMap<SessionKey, Feed>,
    /// Running `session/new` / `session/load` calls.
    registering: usize,
    /// Updates of unknown Sessions seen while `registering`: one of them may
    /// be the Session being made.
    pending: Vec<UpdateEnvelope>,
    /// Capacity of `pending`.
    pending_limit: usize,
    /// Updates that did not fit in `pending` during the current registration.
    dropped: u64,
    /// Turns whose `turn_ended` reached the client (or needs no sending).
    ended: Vec<TurnId>,
    branch_reset: bool,
    detached: Vec<loop_protocol::SessionRef>,
}

/// Most updates held for a Session that is still being created or loaded.
const PENDING_LIMIT: usize = 65_536;

impl Default for Feeds {
    fn default() -> Self {
        Self::with_limit(PENDING_LIMIT)
    }
}

impl Feeds {
    fn with_limit(pending_limit: usize) -> Self {
        Self {
            map: HashMap::new(),
            registering: 0,
            pending: Vec::new(),
            pending_limit,
            dropped: 0,
            ended: Vec::new(),
            branch_reset: false,
            detached: Vec::new(),
        }
    }

    /// Send an update to the client, noting a `turn_ended`.
    fn send(&mut self, conn: &Connection, envelope: &UpdateEnvelope) -> Result<(), RpcError> {
        if matches!(envelope.update, SessionUpdate::BranchReset { .. }) && !self.branch_reset {
            self.map.remove(&envelope.session);
            self.detached.push(loop_protocol::SessionRef {
                instance: envelope.instance.clone(),
                key: envelope.session.clone(),
                native_id: envelope.session.to_string(),
            });
            let mut notice = envelope.clone();
            notice.update = SessionUpdate::Notice {
                level: NoticeLevel::Warning,
                text: "this Session changed branch; attach again".into(),
            };
            conn.notify(methods::SESSION_UPDATE, to_value(&notice))?;
            return Ok(());
        }
        conn.notify(methods::SESSION_UPDATE, to_value(envelope))?;
        if let SessionUpdate::TurnEnded { turn_id, .. } = &envelope.update {
            self.ended.push(turn_id.clone());
        }
        Ok(())
    }

    /// Make `key` this connection's. False if it already was.
    fn own(&mut self, key: SessionKey) -> bool {
        if self.map.contains_key(&key) {
            return false;
        }
        self.map.insert(key, Feed::Live { last_sent: 0 });
        true
    }

    fn take_pending(&mut self, key: &SessionKey) -> Vec<UpdateEnvelope> {
        let (mine, rest) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|e| &e.session == key);
        self.pending = rest;
        mine
    }

    /// Forward, buffer or drop an update from the Runtime. True if it was
    /// buffered behind an attach rather than sent or dropped.
    fn route(&mut self, conn: &Connection, envelope: UpdateEnvelope) -> Result<bool, RpcError> {
        match self.map.get_mut(&envelope.session) {
            Some(Feed::Attaching { buffer, .. }) => {
                buffer.push(envelope);
                return Ok(true);
            }
            Some(Feed::Live { .. }) => self.deliver(conn, envelope)?,
            None if self.registering > 0 => {
                if self.pending.len() < self.pending_limit {
                    self.pending.push(envelope);
                } else {
                    self.dropped += 1;
                }
            }
            None => {}
        }
        Ok(false)
    }

    /// Send a live update of a Live feed unless the client already has it.
    fn deliver(&mut self, conn: &Connection, envelope: UpdateEnvelope) -> Result<(), RpcError> {
        if let Some(Feed::Live { last_sent }) = self.map.get_mut(&envelope.session) {
            if envelope.seq > *last_sent {
                *last_sent = envelope.seq;
                self.send(conn, &envelope)?;
            }
        }
        Ok(())
    }

    /// Begin attach operation `op`, superseding any earlier one for `key`.
    fn start_attach(&mut self, key: SessionKey, op: u64) {
        let (buffer, prior) = match self.map.remove(&key) {
            Some(Feed::Live { last_sent }) => (Vec::new(), Some(last_sent)),
            Some(Feed::Attaching { buffer, prior, .. }) => (buffer, prior),
            None => (Vec::new(), None),
        };
        self.map.insert(key, Feed::Attaching { op, buffer, prior });
    }

    /// Answer the attach, then replay, then the updates buffered meanwhile.
    fn finish_attach(
        &mut self,
        conn: &Connection,
        id: PeerRequestId,
        key: SessionKey,
        op: u64,
        after_seq: u64,
        result: BackendResult<Attachment>,
    ) -> Result<(), RpcError> {
        // Only the operation that owns the entry may complete it; a detach or a
        // newer attach supersedes this one.
        let ours = matches!(self.map.get(&key), Some(Feed::Attaching { op: o, .. }) if *o == op);
        if !ours {
            conn.respond_error(
                id,
                codes::INVALID_REQUEST,
                "the attach was superseded by a detach or a newer attach".into(),
                None,
            );
            return Ok(());
        }
        let Some(Feed::Attaching { buffer, prior, .. }) = self.map.remove(&key) else {
            return Ok(());
        };
        let (last, buffer) = match result {
            Ok(attachment) => {
                conn.respond(
                    id,
                    Ok(to_value(&AttachResult {
                        snapshot: attachment.snapshot,
                        latest_seq: attachment.latest_seq,
                        generation: attachment.generation,
                    })),
                );
                // A cursor ahead of the log would hide new updates.
                let mut last = after_seq.min(attachment.latest_seq);
                // A fresh branch transcript. Only its last update carries its
                // stamp, so a client's cursor reaches it once the whole
                // transcript has arrived.
                let count = attachment.baseline.len();
                for (index, mut envelope) in attachment.baseline.into_iter().enumerate() {
                    if index + 1 == count {
                        last = last.max(envelope.seq);
                    }
                    envelope.seq = last;
                    envelope.origin = Origin::Replay;
                    envelope.generation = attachment.generation;
                    self.send(conn, &envelope)?;
                }
                for mut envelope in attachment.replay {
                    if envelope.seq <= last {
                        continue;
                    }
                    last = envelope.seq;
                    envelope.origin = Origin::Replay;
                    envelope.generation = attachment.generation;
                    self.send(conn, &envelope)?;
                    if !self.detached.is_empty() {
                        return Ok(());
                    }
                }
                (last, buffer)
            }
            Err(error) => {
                respond(conn, id, Err(RpcErrorBody::from(&error)));
                match prior {
                    Some(last) => (last, buffer),
                    None => return Ok(()),
                }
            }
        };
        self.map.insert(key.clone(), Feed::Live { last_sent: last });
        for envelope in buffer {
            self.deliver(conn, envelope)?;
        }
        Ok(())
    }
}

/// Tell the client it missed updates, then close the connection.
fn lagged(conn: &Connection, instance: &BackendInstanceId, missed: Option<u64>) {
    let notice = UpdateEnvelope::live(
        instance.clone(),
        0,
        SessionKey(String::new()),
        None,
        0,
        SessionUpdate::Notice {
            level: NoticeLevel::Error,
            text: match missed {
                Some(missed) => format!(
                    "this client fell behind and missed {missed} updates; reconnect to continue"
                ),
                None => "this client fell behind and missed updates; reconnect to continue".into(),
            },
        },
    );
    let _ = conn.notify(methods::SESSION_UPDATE, to_value(&notice));
    conn.close();
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Asks the connected client through `host/interact`.
struct ClientHost {
    id: u64,
    conn: Arc<Connection>,
    /// Whether the client declared `interact` at `initialize`.
    interact: Arc<AtomicBool>,
    branch_reset: AtomicBool,
    /// Who this connection acts for. Fixed when the connection was accepted.
    caller: Option<Caller>,
}

/// Register this connection as a watcher (and Escalation recipient) of
/// `session`, with the connection's caller in scope: the forwarder runs
/// outside it. False when the Runtime refuses the caller (the Session is
/// "not found" for it): the connection must not keep a feed of it.
async fn register_recipient(
    runtime: Option<&dyn RuntimeControl>,
    host: &Arc<ClientHost>,
    session: &loop_protocol::SessionRef,
    attached: bool,
) -> bool {
    let Some(runtime) = runtime else {
        return true;
    };
    let recipient = if attached && host.interact.load(Ordering::Acquire) {
        Some(host.clone() as Arc<dyn crate::control::EscalationRecipient>)
    } else {
        None
    };
    let registered = in_scope(
        host.caller.clone(),
        runtime.register(session, host.id, attached, recipient),
    )
    .await;
    match registered {
        Ok(()) => true,
        Err(loop_protocol::BackendError::SessionNotFound(_)) if attached => false,
        Err(error) => {
            tracing::debug!(%error, "could not register Escalation recipient");
            true
        }
    }
}

/// `runtime/status` for a user from outside the host: nothing about the
/// host (no pid, socket, URL or where the limits came from), and counts of
/// only the Sessions it may list (`runtime/sessions` filters them).
async fn outside_status(
    runtime: &dyn RuntimeControl,
    mut status: loop_protocol::rpc::RuntimeStatus,
) -> BackendResult<loop_protocol::rpc::RuntimeStatus> {
    status.auto_started = false;
    status.pid = 0;
    status.socket = std::path::PathBuf::new();
    status.listen_url = None;
    status.max_sessions_from = None;
    status.max_active_turns_from = None;
    let totals = match runtime.admin() {
        Some(admin) => admin.sessions().await.map(|r| r.totals).unwrap_or_default(),
        None => Default::default(),
    };
    status.loaded = u64::from(totals.sessions);
    status.busy = u64::from(totals.busy);
    Ok(status)
}

#[async_trait]
impl crate::control::EscalationRecipient for ClientHost {
    fn caller(&self) -> Option<Caller> {
        self.caller.clone()
    }

    fn withdraw(&self, params: loop_protocol::rpc::InteractWithdrawnParams) {
        let _ = self
            .conn
            .notify(methods::HOST_INTERACT_WITHDRAWN, to_value(&params));
    }

    async fn interact(
        &self,
        params: loop_protocol::rpc::InteractParams,
        withdrawn: oneshot::Receiver<()>,
    ) -> Result<InteractionOutcome, ()> {
        if !self.interact.load(Ordering::Acquire) {
            return Err(());
        }
        let value = self
            .conn
            .interaction(params, withdrawn)
            .await
            .map_err(|_| ())?;
        serde_json::from_value(value).map_err(|_| ())
    }
}

#[async_trait]
impl HostServices for ClientHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        if !self.interact.load(Ordering::Acquire) {
            return InteractionOutcome::Rejected {
                reason: "the client is an observer and cannot answer".into(),
            };
        }
        let result = self
            .conn
            .request(methods::HOST_INTERACT, to_value(&request))
            .await;
        match result.and_then(|value| {
            serde_json::from_value(value).map_err(|e| RpcError::Invalid(e.to_string()))
        }) {
            Ok(outcome) => outcome,
            Err(error) => InteractionOutcome::Rejected {
                reason: format!("the client could not answer: {error}"),
            },
        }
    }
}

/// Include framing and the caller's actual request id in the export bound.
fn export_response_fits(id: &PeerRequestId, result: &impl serde::Serialize) -> BackendResult<()> {
    #[derive(serde::Serialize)]
    struct Response<'a, T> {
        jsonrpc: &'static str,
        id: &'a PeerRequestId,
        result: &'a T,
    }
    struct Bounded(usize);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("encoded Session export exceeds 16 MiB"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(
        Bounded(loop_protocol::rpc::SESSION_RESPONSE_LIMIT),
        &Response {
            jsonrpc: "2.0",
            id,
            result,
        },
    )
    .map_err(|error| {
        loop_protocol::BackendError::failed(
            loop_protocol::ErrorCategory::Protocol,
            error.to_string(),
        )
    })
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, (i64, String)> {
    serde_json::from_value(params).map_err(|e| (codes::INVALID_PARAMS, e.to_string()))
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Answer `id` with a result or a backend error, keeping the error's data.
fn respond(conn: &Connection, id: PeerRequestId, result: Result<Value, RpcErrorBody>) {
    match result {
        Ok(value) => conn.respond(id, Ok(value)),
        Err(body) => conn.respond_error(id, body.code, body.message, body.data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::memory;

    fn update(session: &str, seq: u64) -> UpdateEnvelope {
        UpdateEnvelope::live(
            "i".into(),
            1,
            SessionKey(session.into()),
            None,
            seq,
            SessionUpdate::Notice {
                level: NoticeLevel::Info,
                text: String::new(),
            },
        )
    }

    #[test]
    fn export_response_bound_includes_json_escaping_and_request_id() {
        let id = json!("x".repeat(loop_protocol::rpc::SESSION_RESPONSE_LIMIT));
        assert!(export_response_fits(&id, &json!({})).is_err());
        assert!(export_response_fits(&json!(1), &json!({"jsonl":"small"})).is_ok());
    }

    #[tokio::test]
    async fn reset_replay_detaches_legacy_before_buffered_replacement_updates() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for branch_reset in [false, true] {
                let (transport, _peer) = memory();
                let (conn, _incoming) = Connection::new(transport);
                let mut feeds = Feeds {
                    branch_reset,
                    ..Default::default()
                };
                feeds.own("other".into());
                let key = SessionKey("s".into());
                feeds.start_attach(key.clone(), 1);
                feeds.route(&conn, update("s", 3)).unwrap();
                let mut reset = update("s", 2);
                reset.update = SessionUpdate::BranchReset {
                    revision: 1,
                    branch_revision: 1,
                    view_id: "view".into(),
                    reason: "navigate".into(),
                };
                feeds
                    .finish_attach(
                        &conn,
                        json!(1),
                        key.clone(),
                        1,
                        0,
                        Ok(Attachment {
                            snapshot: None,
                            latest_seq: 2,
                            generation: 1,
                            baseline: Vec::new(),
                            replay: Box::new(vec![update("s", 1), reset].into_iter()),
                        }),
                    )
                    .unwrap();
                assert!(feeds.map.contains_key(&SessionKey("other".into())));
                if branch_reset {
                    assert!(matches!(
                        feeds.map.get(&key),
                        Some(Feed::Live { last_sent: 3 })
                    ));
                    assert!(feeds.detached.is_empty());
                } else {
                    assert!(!feeds.map.contains_key(&key));
                    assert_eq!(feeds.detached.len(), 1);
                    assert_eq!(feeds.detached[0].key, key);
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn registration_overflow_is_counted_not_silently_skipped() {
        let (conn, _peer) = memory();
        let (conn, _incoming) = Connection::new(conn);
        let mut feeds = Feeds::with_limit(4);
        feeds.registering = 1;
        for seq in 1..=10 {
            feeds.route(&conn, update("other", seq)).unwrap();
        }
        assert_eq!(feeds.pending.len(), 4);
        assert_eq!(feeds.dropped, 6, "overflow must be recorded");
    }

    #[tokio::test]
    async fn a_baseline_advances_the_cursor_only_with_its_last_update() {
        let (transport, mut peer) = memory();
        let (conn, _incoming) = Connection::new(transport);
        let mut feeds = Feeds::default();
        let key = SessionKey("s".into());
        feeds.start_attach(key.clone(), 1);
        feeds
            .finish_attach(
                &conn,
                json!(1),
                key.clone(),
                1,
                5,
                Ok(Attachment {
                    snapshot: None,
                    latest_seq: 9,
                    generation: 1,
                    baseline: vec![update("s", 9), update("s", 9), update("s", 9)],
                    // Superseded by the baseline: never sent.
                    replay: Box::new(vec![update("s", 9)].into_iter()),
                }),
            )
            .unwrap();
        assert!(matches!(
            feeds.map.get(&key),
            Some(Feed::Live { last_sent: 9 })
        ));
        let mut seqs = Vec::new();
        while seqs.len() < 3 {
            let message = tokio::time::timeout(Duration::from_secs(10), peer.incoming.recv())
                .await
                .unwrap()
                .unwrap();
            let message: Value = serde_json::from_str(&message).unwrap();
            if message["method"] == methods::SESSION_UPDATE {
                seqs.push(message["params"]["seq"].as_u64().unwrap());
            }
        }
        assert_eq!(seqs, [5, 5, 9]);
    }

    #[tokio::test]
    async fn a_superseded_attach_cannot_complete_a_newer_one() {
        let (conn, _peer) = memory();
        let (conn, _incoming) = Connection::new(conn);
        let mut feeds = Feeds::default();
        let key = SessionKey("s".into());
        feeds.start_attach(key.clone(), 1);
        feeds.start_attach(key.clone(), 2);
        let empty = || {
            Ok(Attachment {
                snapshot: None,
                latest_seq: 0,
                generation: 1,
                baseline: Vec::new(),
                replay: Box::new(std::iter::empty()),
            })
        };
        // The first completion is stale: it must not touch the newer entry.
        feeds
            .finish_attach(&conn, json!(1), key.clone(), 1, 0, empty())
            .unwrap();
        assert!(matches!(
            feeds.map.get(&key),
            Some(Feed::Attaching { op: 2, .. })
        ));
        feeds
            .finish_attach(&conn, json!(2), key.clone(), 2, 0, empty())
            .unwrap();
        assert!(matches!(feeds.map.get(&key), Some(Feed::Live { .. })));
        // Detach, then a stale completion: no feed is resurrected or removed.
        feeds.map.remove(&key);
        feeds.start_attach(key.clone(), 3);
        feeds.map.remove(&key);
        feeds.start_attach(key.clone(), 4);
        feeds
            .finish_attach(&conn, json!(3), key.clone(), 3, 0, empty())
            .unwrap();
        assert!(matches!(
            feeds.map.get(&key),
            Some(Feed::Attaching { op: 4, .. })
        ));
    }
}
