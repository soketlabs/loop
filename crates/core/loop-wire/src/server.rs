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
    methods, negotiate, AttachParams, AttachResult, CloseParams, InitializeParams,
    InitializeResult, LoadParams, PeerInfo, PromptParams, PromptResult, RpcErrorBody,
    SessionParams,
};
use loop_protocol::{
    AgentBackend, BackendInstanceId, BackendResult, HostServices, InteractionOutcome,
    InteractionRequest, NoticeLevel, Origin, SessionKey, SessionUpdate, TurnId, TurnOutcome,
    UpdateEnvelope,
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
}
impl Default for ServeOptions {
    fn default() -> Self {
        Self { owns_backend: true }
    }
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
    start(backend, None, transport, info, options)
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
    start(backend, Some(control), transport, info, options)
}

fn start(
    backend: Arc<dyn AgentBackend>,
    runtime: Option<Arc<dyn RuntimeControl>>,
    transport: Transport,
    info: PeerInfo,
    options: ServeOptions,
) -> Served {
    let (conn, incoming) = Connection::new(transport);
    let interact = Arc::new(AtomicBool::new(true));
    let host = Arc::new(ClientHost {
        conn: Arc::clone(&conn),
        interact: Arc::clone(&interact),
        id: NEXT_TICKET.fetch_add(1, Ordering::Relaxed),
    });
    let done = tokio::spawn(run(
        backend,
        runtime,
        conn,
        incoming,
        info,
        options,
        host.clone(),
    ));
    Served { host, done }
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
        result: BackendResult<Attachment>,
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
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    info: PeerInfo,
    options: ServeOptions,
    host: Arc<ClientHost>,
) {
    // Subscribe now so nothing between `initialize` and the first update is
    // missed; forwarding starts once the client is initialized.
    let updates = backend.subscribe();
    let (control, control_rx) = mpsc::unbounded_channel();
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
                if method == methods::SESSION_CANCEL {
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
                            let mut capabilities = backend.capabilities();
                            // Advertise only what this connection can serve.
                            capabilities.multi_session = runtime.is_some();
                            capabilities.attach = runtime.is_some();
                            let result = InitializeResult {
                                protocol_version: negotiate(params.protocol_version),
                                server_info: info.clone(),
                                instance: backend.instance().clone(),
                                capabilities,
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
                // doesn't hold up a `session/cancel`.
                tokio::spawn(async move {
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
                });
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
        methods::SESSION_LIST if runtime.is_some() => {
            runtime.unwrap().list().await.map(|l| to_value(&l))
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
                let result = runtime.unwrap().attach(&session, after_seq).await;
                let _ = control.send(Control::Attached {
                    id,
                    key: session.key.clone(),
                    session,
                    op,
                    after_seq,
                    result,
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
        _ => {
            return conn.respond(
                id,
                Err((codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
            )
        }
    };
    respond(conn, id, result.map_err(|e| RpcErrorBody::from(&e)));
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
                Some(Control::Start) => started = true,
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
                        lagged(&conn, &instance, feeds.dropped);
                        return;
                    }
                    register_recipient(runtime.as_deref(), &host, &session, true).await;
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
                    if feeds.finish_attach(&conn, id, key, op, after_seq, result).is_err() {
                        return;
                    }
                    if attached { register_recipient(runtime.as_deref(), &host, &session, true).await; }
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
                    // Never let the client silently miss part of a turn.
                    lagged(&conn, &instance, missed);
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
        }
    }

    /// Send an update to the client, noting a `turn_ended`.
    fn send(&mut self, conn: &Connection, envelope: &UpdateEnvelope) -> Result<(), RpcError> {
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
                        latest_seq: attachment.latest_seq,
                        generation: attachment.generation,
                    })),
                );
                // A cursor ahead of the log would hide new updates.
                let mut last = after_seq.min(attachment.latest_seq);
                for mut envelope in attachment.replay {
                    if envelope.seq <= last {
                        continue;
                    }
                    last = envelope.seq;
                    envelope.origin = Origin::Replay;
                    envelope.generation = attachment.generation;
                    self.send(conn, &envelope)?;
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
fn lagged(conn: &Connection, instance: &BackendInstanceId, missed: u64) {
    let notice = UpdateEnvelope::live(
        instance.clone(),
        0,
        SessionKey(String::new()),
        None,
        0,
        SessionUpdate::Notice {
            level: NoticeLevel::Error,
            text: format!(
                "this client fell behind and missed {missed} updates; reconnect to continue"
            ),
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
}

async fn register_recipient(
    runtime: Option<&dyn RuntimeControl>,
    host: &Arc<ClientHost>,
    session: &loop_protocol::SessionRef,
    attached: bool,
) {
    if let Some(runtime) = runtime {
        let recipient = if attached && host.interact.load(Ordering::Acquire) {
            Some(host.clone() as Arc<dyn crate::control::EscalationRecipient>)
        } else {
            None
        };
        if let Err(error) = runtime.recipient(session, host.id, recipient).await {
            tracing::debug!(%error, "could not register Escalation recipient");
        }
    }
}

#[async_trait]
impl crate::control::EscalationRecipient for ClientHost {
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
    async fn a_superseded_attach_cannot_complete_a_newer_one() {
        let (conn, _peer) = memory();
        let (conn, _incoming) = Connection::new(conn);
        let mut feeds = Feeds::default();
        let key = SessionKey("s".into());
        feeds.start_attach(key.clone(), 1);
        feeds.start_attach(key.clone(), 2);
        let empty = || {
            Ok(Attachment {
                latest_seq: 0,
                generation: 1,
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
