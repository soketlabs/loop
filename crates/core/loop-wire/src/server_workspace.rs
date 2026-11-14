//! The server side of `session/workspace`, `session/files/*` and
//! `session/terminal/*`, and the per-connection terminal streams.
//!
//! Terminal output never passes the update broadcast: an attach makes one
//! task for (this connection, this terminal). The task asks the Runtime's
//! feed for the next event only when the connection has room
//! ([`Connection::wait_bulk_room`]); while it waits, the driver's ring buffer
//! keeps the output and evicts the oldest, and the next event is a
//! `dropped` count. So a slow client is never disconnected because of
//! terminal output, and other connections are not touched.

use std::collections::HashMap;
use std::sync::Arc;

use loop_protocol::rpc::methods;
use loop_protocol::session_workspace::TerminalOutputParams;
use loop_protocol::{BackendError, BackendResult, SessionKey};
use serde_json::Value;

use super::{parse, respond, to_value, ClientHost};
use crate::connection::{codes, Connection, PeerRequestId};
use crate::control::{RuntimeControl, SessionWorkspace, TerminalEvent, TerminalFeed};
use loop_protocol::rpc::RpcErrorBody;

/// Requests this module serves.
pub(super) fn serves(method: &str) -> bool {
    method == methods::SESSION_WORKSPACE
        || method.starts_with("session/files/")
        || (method.starts_with("session/terminal/") && method != methods::SESSION_TERMINAL_OUTPUT)
}

/// Requests that keep their order on one connection: what the user types
/// and the size changes between the keys. A signal does not wait behind
/// them (a stuck write must not hold up Ctrl-C or a kill); Ctrl-C typed as
/// input keeps its place.
pub(super) fn ordered(method: &str) -> bool {
    matches!(
        method,
        methods::SESSION_TERMINAL_INPUT | methods::SESSION_TERMINAL_RESIZE
    )
}

/// Most ordered requests waiting for one terminal; more are refused with
/// `Busy`.
pub(super) const ORDERED_QUEUE: usize = 256;
/// Most terminals with an ordered queue on one connection.
const ORDERED_TERMINALS: usize = 64;
/// Most bytes of params waiting in the ordered queues of one connection.
const ORDERED_BYTES: usize = 8 * 1024 * 1024;

type Queued = (PeerRequestId, String, Value, usize);

/// One ordered queue per terminal on a connection: a stuck write to one
/// terminal does not hold up another. Bounded by count and by bytes.
#[derive(Default)]
pub(super) struct Ordered {
    queues: parking_lot::Mutex<HashMap<String, tokio::sync::mpsc::Sender<Queued>>>,
    bytes: Arc<std::sync::atomic::AtomicUsize>,
}

impl Ordered {
    /// Queue an ordered request for its terminal, or refuse it with `Busy`.
    pub(super) fn push(
        &self,
        runtime: Option<Arc<dyn RuntimeControl>>,
        host: Arc<ClientHost>,
        id: PeerRequestId,
        method: String,
        params: Value,
    ) {
        use std::sync::atomic::Ordering;
        let size = params.to_string().len();
        if self.bytes.fetch_add(size, Ordering::AcqRel) + size > ORDERED_BYTES {
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            return busy(&host.conn, id);
        }
        let terminal = params
            .get("terminal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut queues = self.queues.lock();
        queues.retain(|_, sender| !sender.is_closed());
        if !queues.contains_key(&terminal) && queues.len() >= ORDERED_TERMINALS {
            drop(queues);
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            return busy(&host.conn, id);
        }
        let sender = queues
            .entry(terminal)
            .or_insert_with(|| {
                let (sender, mut queue) = tokio::sync::mpsc::channel::<Queued>(ORDERED_QUEUE);
                let bytes = Arc::clone(&self.bytes);
                let host = Arc::clone(&host);
                let runtime = runtime.clone();
                let caller = host.caller.clone();
                tokio::spawn(super::in_scope(caller, async move {
                    while let Some((id, method, params, size)) = queue.recv().await {
                        handle(runtime.as_deref(), &host, id, &method, params).await;
                        bytes.fetch_sub(size, Ordering::AcqRel);
                    }
                }));
                sender
            })
            .clone();
        drop(queues);
        if let Err(error) = sender.try_send((id, method, params, size)) {
            let (id, _, _, size) = error.into_inner();
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            busy(&host.conn, id);
        }
    }

    /// Drop every queue (the connection ended); their workers end.
    pub(super) fn stop_all(&self) {
        self.queues.lock().clear();
    }
}

/// The terminal streams of one connection, by terminal id.
#[derive(Default)]
pub(super) struct Streams(parking_lot::Mutex<HashMap<String, tokio::task::AbortHandle>>);

impl Streams {
    fn replace(&self, terminal: String, task: tokio::task::AbortHandle) {
        let mut streams = self.0.lock();
        streams.retain(|_, task| !task.is_finished());
        if let Some(old) = streams.insert(terminal, task) {
            old.abort();
        }
    }

    fn stop(&self, terminal: &str) {
        if let Some(task) = self.0.lock().remove(terminal) {
            task.abort();
        }
    }

    /// Stop every stream (the connection ended).
    pub(super) fn stop_all(&self) {
        for (_, task) in self.0.lock().drain() {
            task.abort();
        }
    }
}

/// Serve one request of this module and answer it.
pub(super) async fn handle(
    runtime: Option<&dyn RuntimeControl>,
    host: &Arc<ClientHost>,
    id: PeerRequestId,
    method: &str,
    params: Value,
) {
    let conn = &host.conn;
    let Some(service) = runtime.and_then(|runtime| runtime.session_workspace()) else {
        return conn.respond(
            id,
            Err((codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        );
    };
    if method == methods::SESSION_TERMINAL_ATTACH {
        return attach(service, host, id, params).await;
    }
    let result = match call(service, host, method, params).await {
        Ok(result) => result,
        Err(error) => return conn.respond(id, Err(error)),
    };
    respond(conn, id, result.map_err(|e| RpcErrorBody::from(&e)));
}

/// `session/terminal/attach`: the answer is queued first, then the stream
/// starts, so a client sees no output of the terminal before the answer.
async fn attach(
    service: &dyn SessionWorkspace,
    host: &Arc<ClientHost>,
    id: PeerRequestId,
    params: Value,
) {
    let params: loop_protocol::session_workspace::TerminalAttachParams = match parse(params) {
        Ok(params) => params,
        Err(error) => return host.conn.respond(id, Err(error)),
    };
    let session = params.session.key.clone();
    let terminal = params.terminal.clone();
    match service.terminal_attach(params).await {
        Ok((attached, feed)) => {
            respond(&host.conn, id, Ok(to_value(&attached)));
            start_stream(host, session, terminal, feed);
        }
        Err(error) => respond(&host.conn, id, Err(RpcErrorBody::from(&error))),
    }
}

/// The call; the outer error is a malformed request.
async fn call(
    service: &dyn SessionWorkspace,
    host: &Arc<ClientHost>,
    method: &str,
    params: Value,
) -> Result<BackendResult<Value>, (i64, String)> {
    let empty = |result: BackendResult<()>| result.map(|()| serde_json::json!({}));
    Ok(match method {
        methods::SESSION_WORKSPACE => service.info(parse(params)?).await.map(|r| to_value(&r)),
        methods::SESSION_FILES_LIST => service.list(parse(params)?).await.map(|r| to_value(&r)),
        methods::SESSION_FILES_STAT => service.stat(parse(params)?).await.map(|r| to_value(&r)),
        methods::SESSION_FILES_READ => service.read(parse(params)?).await.map(|r| to_value(&r)),
        methods::SESSION_FILES_UPLOAD_BEGIN => service
            .upload_begin(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::SESSION_FILES_UPLOAD_WRITE => service
            .upload_write(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::SESSION_FILES_UPLOAD_STATUS => service
            .upload_status(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::SESSION_FILES_UPLOAD_COMMIT => service
            .upload_commit(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::SESSION_FILES_UPLOAD_ABORT => empty(service.upload_abort(parse(params)?).await),
        methods::SESSION_FILES_DELETE => empty(service.delete(parse(params)?).await),
        methods::SESSION_FILES_RENAME => empty(service.rename(parse(params)?).await),
        methods::SESSION_FILES_MKDIR => empty(service.mkdir(parse(params)?).await),
        methods::SESSION_TERMINAL_OPEN => service
            .terminal_open(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::SESSION_TERMINAL_DETACH => {
            let params: loop_protocol::session_workspace::TerminalParams = parse(params)?;
            let terminal = params.terminal.clone();
            let result = service.terminal_detach(params).await;
            if result.is_ok() {
                host.streams.stop(&terminal);
            }
            empty(result)
        }
        methods::SESSION_TERMINAL_INPUT => empty(service.terminal_input(parse(params)?).await),
        methods::SESSION_TERMINAL_RESIZE => empty(service.terminal_resize(parse(params)?).await),
        methods::SESSION_TERMINAL_SIGNAL => empty(service.terminal_signal(parse(params)?).await),
        methods::SESSION_TERMINAL_CLOSE => service
            .terminal_close(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        methods::SESSION_TERMINAL_LIST => service
            .terminal_list(parse(params)?)
            .await
            .map(|r| to_value(&r)),
        _ => Err(BackendError::Unsupported(method.to_owned())),
    })
}

/// Start (or replace) this connection's stream of `terminal`.
fn start_stream(
    host: &Arc<ClientHost>,
    session: SessionKey,
    terminal: String,
    feed: Box<dyn TerminalFeed>,
) {
    let conn = Arc::clone(&host.conn);
    let task = tokio::spawn(stream(conn, session, terminal.clone(), feed));
    host.streams.replace(terminal, task.abort_handle());
}

/// Send `feed`'s events as `session/terminal/output` while the connection
/// has room. Ends at the exit, when the feed ends, or when the connection
/// closes.
async fn stream(
    conn: Arc<Connection>,
    session: SessionKey,
    terminal: String,
    feed: Box<dyn TerminalFeed>,
) {
    loop {
        if !conn.wait_bulk_room().await {
            return;
        }
        let Some(event) = feed.next().await else {
            return;
        };
        let mut params = TerminalOutputParams {
            session: session.clone(),
            terminal: terminal.clone(),
            seq: 0,
            data: None,
            dropped: None,
            exit: None,
        };
        let last = match event {
            TerminalEvent::Output { seq, data } => {
                params.seq = seq;
                params.data = Some(loop_protocol::session_workspace::base64::encode(&data));
                false
            }
            TerminalEvent::Dropped { seq, bytes } => {
                params.seq = seq;
                params.dropped = Some(bytes);
                false
            }
            TerminalEvent::Exited { seq, exit } => {
                params.seq = seq;
                params.exit = Some(exit);
                true
            }
        };
        if conn
            .notify(methods::SESSION_TERMINAL_OUTPUT, to_value(&params))
            .is_err()
            || last
        {
            return;
        }
    }
}

/// Refuse an ordered request that does not fit the queue.
pub(super) fn busy(conn: &Connection, id: PeerRequestId) {
    respond(conn, id, Err(RpcErrorBody::from(&BackendError::Busy)));
}
