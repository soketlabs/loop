//! The server side of `session/files/*` and `session/terminal/*`: what
//! `initialize` offers per connection, outside callers, the order of
//! terminal input, and the per-connection output stream (only after an
//! attach, ended by a detach, never on another connection).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use loop_protocol::rpc::{methods, PeerInfo, SessionInfo, SessionParams};
use loop_protocol::session_workspace::{self as sw, base64};
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Caller, Capabilities,
    ContentBlock, Principal, PrincipalKind, SessionRef, SessionSpec, Tenant, ToolLock, TurnHandle,
    UpdateEnvelope,
};
use loop_wire::server::{serve_with_caller, ServeOptions};
use loop_wire::transport::memory;
use loop_wire::{
    Attachment, Authority, Connection, Incoming, RuntimeAdmin, RuntimeControl, SessionWorkspace,
    TerminalEvent, TerminalFeed,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;

struct Fake {
    updates: broadcast::Sender<UpdateEnvelope>,
    lock: Option<ToolLock>,
    /// Terminal inputs in the order the service ran them.
    inputs: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentBackend for Fake {
    fn instance(&self) -> &BackendInstanceId {
        static ID: std::sync::OnceLock<BackendInstanceId> = std::sync::OnceLock::new();
        ID.get_or_init(|| "fake".into())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }
    async fn new_session(&self, _: SessionSpec) -> BackendResult<SessionRef> {
        Err(BackendError::Unsupported("new".into()))
    }
    async fn load_session(&self, _: &SessionRef, _: SessionSpec) -> BackendResult<SessionRef> {
        Err(BackendError::Unsupported("load".into()))
    }
    async fn prompt(&self, _: &SessionRef, _: Vec<ContentBlock>) -> BackendResult<TurnHandle> {
        Err(BackendError::Unsupported("prompt".into()))
    }
    fn cancel(&self, _: &SessionRef) {}
    fn is_busy(&self, _: &SessionRef) -> bool {
        false
    }
    async fn shutdown(&self) {}
}

#[async_trait]
impl RuntimeAdmin for Fake {
    fn offered(&self) -> Capabilities {
        Capabilities {
            tool_lock: self.lock,
            ..Capabilities::default()
        }
    }
}

#[async_trait]
impl RuntimeControl for Fake {
    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        Ok(vec![])
    }
    async fn close(&self, _: &SessionRef, _: bool) -> BackendResult<()> {
        Ok(())
    }
    async fn attach(&self, _: &SessionRef, _: u64) -> BackendResult<Attachment> {
        Err(BackendError::Unsupported("attach".into()))
    }
    fn admin(&self) -> Option<&dyn RuntimeAdmin> {
        Some(self)
    }
    fn session_workspace(&self) -> Option<&dyn SessionWorkspace> {
        Some(self)
    }
}

/// A feed of `count` one-byte outputs, then the exit.
struct Counting {
    next: Mutex<u64>,
    count: u64,
}

#[async_trait]
impl TerminalFeed for Counting {
    async fn next(&self) -> Option<TerminalEvent> {
        let seq = {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            *next
        };
        tokio::time::sleep(Duration::from_millis(1)).await;
        match seq {
            s if s <= self.count => Some(TerminalEvent::Output {
                seq: s,
                data: vec![b'a' + (s % 26) as u8],
            }),
            s if s == self.count + 1 => Some(TerminalEvent::Exited {
                seq: s,
                exit: sw::TerminalEnd {
                    state: "exited".into(),
                    code: Some(0),
                    signal: None,
                },
            }),
            _ => None,
        }
    }
}

fn nope<T>() -> BackendResult<T> {
    Err(BackendError::Unsupported("fake".into()))
}

#[async_trait]
impl SessionWorkspace for Fake {
    async fn info(&self, _: SessionParams) -> BackendResult<sw::SessionWorkspaceInfo> {
        nope()
    }
    async fn list(&self, _: sw::FilesListParams) -> BackendResult<sw::FilesListResult> {
        Ok(sw::FilesListResult {
            path: String::new(),
            entries: vec![],
            next_cursor: None,
            hidden: 0,
        })
    }
    async fn stat(&self, _: sw::FilePathParams) -> BackendResult<sw::FileEntry> {
        nope()
    }
    async fn read(&self, _: sw::FilesReadParams) -> BackendResult<sw::FilesReadResult> {
        nope()
    }
    async fn upload_begin(&self, _: sw::UploadBeginParams) -> BackendResult<sw::UploadBeginResult> {
        nope()
    }
    async fn upload_write(&self, _: sw::UploadWriteParams) -> BackendResult<sw::UploadProgress> {
        nope()
    }
    async fn upload_status(&self, _: sw::UploadParams) -> BackendResult<sw::UploadProgress> {
        nope()
    }
    async fn upload_commit(&self, _: sw::UploadParams) -> BackendResult<sw::UploadCommitResult> {
        nope()
    }
    async fn upload_abort(&self, _: sw::UploadParams) -> BackendResult<()> {
        nope()
    }
    async fn delete(&self, _: sw::FilesDeleteParams) -> BackendResult<()> {
        nope()
    }
    async fn rename(&self, _: sw::FilesRenameParams) -> BackendResult<()> {
        nope()
    }
    async fn mkdir(&self, _: sw::FilePathParams) -> BackendResult<()> {
        nope()
    }
    async fn terminal_open(
        &self,
        _: sw::TerminalOpenParams,
    ) -> BackendResult<sw::TerminalOpenResult> {
        Ok(sw::TerminalOpenResult {
            terminal: "term-1".into(),
            cwd: String::new(),
        })
    }
    async fn terminal_attach(
        &self,
        params: sw::TerminalAttachParams,
    ) -> BackendResult<(sw::TerminalAttachResult, Box<dyn TerminalFeed>)> {
        let count = if params.terminal == "long" {
            100_000
        } else {
            50
        };
        Ok((
            sw::TerminalAttachResult {
                last_seq: 0,
                exit: None,
            },
            Box::new(Counting {
                next: Mutex::new(params.after_seq),
                count,
            }),
        ))
    }
    async fn terminal_detach(&self, _: sw::TerminalParams) -> BackendResult<()> {
        Ok(())
    }
    async fn terminal_input(&self, params: sw::TerminalInputParams) -> BackendResult<()> {
        let text = String::from_utf8(base64::decode(&params.data).unwrap()).unwrap();
        // A later input waits less: run in parallel, they would reorder.
        let n: u64 = text.parse().unwrap();
        tokio::time::sleep(Duration::from_micros(2000 - (n % 20) * 100)).await;
        self.inputs.lock().unwrap().push(text);
        Ok(())
    }
    async fn terminal_resize(&self, _: sw::TerminalResizeParams) -> BackendResult<()> {
        Ok(())
    }
    async fn terminal_signal(&self, _: sw::TerminalSignalParams) -> BackendResult<()> {
        Ok(())
    }
    async fn terminal_close(
        &self,
        _: sw::TerminalParams,
    ) -> BackendResult<sw::TerminalCloseResult> {
        nope()
    }
    async fn terminal_list(&self, _: SessionParams) -> BackendResult<sw::TerminalListResult> {
        nope()
    }
}

fn fake(lock: Option<ToolLock>) -> Arc<Fake> {
    Arc::new(Fake {
        updates: broadcast::channel(4).0,
        lock,
        inputs: Mutex::new(Vec::new()),
    })
}

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "0".into(),
    }
}

fn session() -> Value {
    json!({"instance": "fake", "key": "s", "native_id": "s"})
}

/// A client: its connection and the notifications it got.
async fn connect(
    fake: &Arc<Fake>,
    caller: Caller,
    authority: Authority,
) -> (Arc<Connection>, Arc<Mutex<Vec<Value>>>, Value) {
    let (server, client) = memory();
    let control: Arc<dyn RuntimeControl> = fake.clone();
    let _ = serve_with_caller(
        fake.clone(),
        Some(control),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
            authority,
        },
        caller,
    );
    let (conn, mut incoming) = Connection::new(client);
    let resolver = Arc::clone(&conn);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Response { id, result } => resolver.resolve(id, result).await,
                Incoming::Notification { method, params } => {
                    assert_eq!(method, methods::SESSION_TERMINAL_OUTPUT);
                    sink.lock().unwrap().push(params);
                }
                Incoming::Request { .. } => {}
            }
        }
    });
    let init = conn
        .request(
            methods::INITIALIZE,
            json!({"protocol_version": 1, "client_info": {"name": "c", "version": "0"}}),
        )
        .await
        .unwrap();
    (conn, seen, init)
}

fn host() -> Caller {
    Caller::host(Principal::local_uid(501))
}

fn edge() -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp", "alice").unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

#[tokio::test]
async fn initialize_offers_files_and_terminals_by_connection() {
    let open = fake(None);
    let (_, _, init) = connect(&open, host(), Authority::HostLocal).await;
    let caps = &init["capabilities"];
    assert_eq!(
        (
            &caps["session_files"],
            &caps["session_files_write"],
            &caps["session_terminal"]
        ),
        (&json!(true), &json!(true), &json!(true))
    );
    let locked = fake(Some(ToolLock::Chat));
    for (caller, authority) in [(host(), Authority::HostLocal), (edge(), Authority::Remote)] {
        let (conn, _, init) = connect(&locked, caller, authority).await;
        let caps = &init["capabilities"];
        assert_eq!(caps["session_files"], json!(true));
        assert!(caps.get("session_files_write").is_none(), "{caps}");
        assert!(caps.get("session_terminal").is_none(), "{caps}");
        // Files are served to both; an outside user never gets a terminal.
        conn.request(methods::SESSION_FILES_LIST, json!({"session": session()}))
            .await
            .unwrap();
        let open = conn
            .request(
                methods::SESSION_TERMINAL_OPEN,
                json!({"session": session(), "cols": 80, "rows": 24}),
            )
            .await;
        match authority {
            Authority::Remote => {
                let Err(loop_wire::RpcError::Remote { code, .. }) = open else {
                    panic!("{open:?}");
                };
                assert_eq!(code, loop_protocol::rpc::error_codes::FORBIDDEN);
            }
            // The fake does not lock; the Runtime refuses it itself.
            Authority::HostLocal => assert!(open.is_ok()),
        }
    }
}

#[tokio::test]
async fn terminal_input_keeps_its_order_on_one_connection() {
    let f = fake(None);
    // A raw client: the requests are on the wire in order, at once.
    let (server, mut client) = memory();
    let control: Arc<dyn RuntimeControl> = f.clone();
    let _ = serve_with_caller(
        f.clone(),
        Some(control),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
            authority: Authority::HostLocal,
        },
        host(),
    );
    let request = |id: u64, method: &str, params: Value| {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
    };
    client
        .outgoing
        .send(request(
            0,
            methods::INITIALIZE,
            json!({"protocol_version": 1, "client_info": {"name": "c", "version": "0"}}),
        ))
        .unwrap();
    client.incoming.recv().await.unwrap();
    for n in 1..=200u64 {
        client
            .outgoing
            .send(request(
                n,
                methods::SESSION_TERMINAL_INPUT,
                json!({"session": session(), "terminal": "term-1",
                       "data": base64::encode(n.to_string().as_bytes())}),
            ))
            .unwrap();
    }
    for _ in 1..=200 {
        let answer: Value = serde_json::from_str(&client.incoming.recv().await.unwrap()).unwrap();
        assert!(answer.get("result").is_some(), "{answer}");
    }
    let order: Vec<u64> = f
        .inputs
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.parse().unwrap())
        .collect();
    assert_eq!(order, (1..=200).collect::<Vec<_>>());
}

#[tokio::test]
async fn output_streams_only_to_the_attached_connection_until_detach() {
    let f = fake(None);
    let (a, seen_a, _) = connect(&f, host(), Authority::HostLocal).await;
    let (_b, seen_b, _) = connect(&f, host(), Authority::HostLocal).await;
    a.request(
        methods::SESSION_TERMINAL_ATTACH,
        json!({"session": session(), "terminal": "term-1", "after_seq": 10}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while seen_a
            .lock()
            .unwrap()
            .last()
            .is_none_or(|v| v.get("exit").is_none())
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let got = seen_a.lock().unwrap().clone();
    // After seq 10: 11..=50, then the exit (51).
    let seqs: Vec<u64> = got.iter().map(|v| v["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (11..=51).collect::<Vec<_>>());
    assert_eq!(got[0]["session"], "s");
    assert_eq!(got[0]["terminal"], "term-1");
    assert_eq!(
        base64::decode(got[0]["data"].as_str().unwrap()).unwrap(),
        b"l"
    );
    assert_eq!(got[40]["exit"]["code"], 0);
    assert!(seen_b.lock().unwrap().is_empty());
    // A long stream stops at detach.
    a.request(
        methods::SESSION_TERMINAL_ATTACH,
        json!({"session": session(), "terminal": "long", "after_seq": 0}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    a.request(
        methods::SESSION_TERMINAL_DETACH,
        json!({"session": session(), "terminal": "long"}),
    )
    .await
    .unwrap();
    let after_detach = seen_a.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(seen_a.lock().unwrap().len() <= after_detach + 1);
}

#[tokio::test]
async fn the_memory_transport_counts_its_backlog() {
    let (a, mut b) = memory();
    let probe = a.outgoing.probe();
    assert_eq!(probe.backlog().unwrap().messages, 0);
    a.outgoing.send("12345".into()).unwrap();
    a.outgoing.send("678".into()).unwrap();
    let backlog = probe.backlog().unwrap();
    assert_eq!((backlog.messages, backlog.bytes), (2, 8));
    b.incoming.recv().await.unwrap();
    let backlog = probe.backlog().unwrap();
    assert_eq!((backlog.messages, backlog.bytes), (1, 3));
}
