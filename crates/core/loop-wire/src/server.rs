//! Serve an [`AgentBackend`] over a [`Transport`].
//!
//! One forwarder task sends everything that goes to the client, so the order
//! the client sees is the order it was sent in. A `session/prompt` response
//! goes out only once the backend has resolved the turn *and* the forwarder
//! has sent that turn's `turn_ended`: backends don't promise which of the two
//! comes first. A turn is recognised as the first `turn_started` on its
//! session after the prompt was accepted; a backend runs one turn per session
//! at a time, so that is unambiguous.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loop_protocol::rpc::{
    methods, negotiate, InitializeParams, InitializeResult, LoadParams, PeerInfo, PromptParams,
    PromptResult, RpcErrorBody, SessionParams,
};
use loop_protocol::{
    AgentBackend, BackendInstanceId, HostServices, InteractionOutcome, InteractionRequest,
    NoticeLevel, SessionKey, SessionUpdate, TurnId, TurnOutcome, UpdateEnvelope,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::connection::{codes, Connection, Incoming, PeerRequestId, RpcError};
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

/// Serve `backend` to one client over `transport`.
pub fn serve(backend: Arc<dyn AgentBackend>, transport: Transport, info: PeerInfo) -> Served {
    let (conn, incoming) = Connection::new(transport);
    let host = Arc::new(ClientHost {
        conn: Arc::clone(&conn),
    });
    let done = tokio::spawn(run(backend, conn, incoming, info));
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
}

async fn run(
    backend: Arc<dyn AgentBackend>,
    conn: Arc<Connection>,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    info: PeerInfo,
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
                            let result = InitializeResult {
                                protocol_version: negotiate(params.protocol_version),
                                server_info: info.clone(),
                                instance: backend.instance().clone(),
                                capabilities: backend.capabilities(),
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
                    backend.shutdown().await;
                    conn.respond(id, Ok(json!({})));
                    conn.close();
                    break;
                }
                let backend = Arc::clone(&backend);
                let conn = Arc::clone(&conn);
                let control = control.clone();
                // Each request runs on its own, so a long `session/load`
                // doesn't hold up a `session/cancel`.
                tokio::spawn(async move {
                    handle(backend.as_ref(), &conn, &control, id, &method, params).await;
                });
            }
        }
    }
    conn.fail_pending().await;
    forwarder.abort();
}

async fn handle(
    backend: &dyn AgentBackend,
    conn: &Connection,
    control: &mpsc::UnboundedSender<Control>,
    id: PeerRequestId,
    method: &str,
    params: Value,
) {
    let result = match method {
        methods::SESSION_NEW => match parse(params) {
            Ok(spec) => backend.new_session(spec).await.map(|s| to_value(&s)),
            Err(error) => return conn.respond(id, Err(error)),
        },
        methods::SESSION_LOAD => match parse::<LoadParams>(params) {
            Ok(LoadParams { session, spec }) => backend
                .load_session(&session, spec)
                .await
                .map(|s| to_value(&s)),
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
) {
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
                    match &envelope.update {
                        SessionUpdate::TurnStarted { turn_id } => {
                            if let Some(w) = waiting
                                .iter_mut()
                                .find(|w| w.session == envelope.session && w.turn.is_none())
                            {
                                w.turn = Some(turn_id.clone());
                            }
                        }
                        SessionUpdate::TurnEnded { turn_id, .. } => {
                            for w in waiting.iter_mut().filter(|w| w.turn.as_ref() == Some(turn_id)) {
                                w.ended = true;
                            }
                        }
                        _ => {}
                    }
                    if conn.notify(methods::SESSION_UPDATE, to_value(&envelope)).is_err() {
                        return;
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
    conn: Arc<Connection>,
}

#[async_trait]
impl HostServices for ClientHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
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
