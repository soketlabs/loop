//! A backend behaves the same through `serve` + `RemoteBackend` as it does
//! in-process. This is the test that decides whether the wire is correct.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::native_backend::NativeBackend;
use loop_agent::{AgentTool, AgentToolResult};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, HostServices, InteractionOutcome, InteractionRequest,
    PermissionKind, PermissionOption, SessionRef, SessionUpdate, TurnOutcome, UpdateEnvelope,
};
use loop_wire::transport::memory;
use loop_wire::{serve, Connection, Incoming, RemoteBackend, RpcError};
use serde_json::json;
use tokio::sync::{broadcast, Notify};

fn info(name: &str) -> PeerInfo {
    PeerInfo {
        name: name.into(),
        version: "test".into(),
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_initialization_closes_the_transport() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut peer, transport) = memory();
        let connecting = tokio::spawn(RemoteBackend::connect(
            transport,
            info("client"),
            Arc::new(FirstOption),
        ));
        let request = peer.incoming.recv().await.unwrap();
        assert!(request.contains("initialize"));
        connecting.abort();
        assert!(connecting.await.is_err());
        assert!(
            peer.incoming.recv().await.is_none(),
            "abandoned initialization kept the socket open"
        );
    })
    .await
    .unwrap();
}

async fn native(responses: Vec<FauxResponse>, tools: Vec<AgentTool>) -> Arc<NativeBackend> {
    let script = FauxScript::new();
    script.extend(responses);
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools,
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    let backend = NativeBackend::new(harness).await;
    // User commands pass a Policy gate; this one lets every one run.
    backend.set_command_gate(Arc::new(|_| Box::pin(async { Ok(()) })));
    backend
}

/// Answers every permission prompt with its first option.
struct FirstOption;

#[async_trait]
impl HostServices for FirstOption {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        match request {
            InteractionRequest::Permission { options, .. } => InteractionOutcome::Selected {
                native_id: options[0].native_id.clone(),
                note: Some("from the client".into()),
            },
            _ => InteractionOutcome::Cancelled,
        }
    }
}

/// Which transport a test runs over.
#[derive(Debug, Clone, Copy)]
enum Wire {
    Memory,
    #[cfg(unix)]
    Unix,
    WebSocket,
}

/// `backend` served over an in-memory transport, and the client for it.
async fn over_the_wire(backend: Arc<NativeBackend>) -> (Arc<RemoteBackend>, loop_wire::Served) {
    over(backend, Wire::Memory).await
}

/// `backend` served over `wire`, and the client for it.
async fn over(backend: Arc<NativeBackend>, wire: Wire) -> (Arc<RemoteBackend>, loop_wire::Served) {
    let (server_end, client_end) = match wire {
        Wire::Memory => memory(),
        #[cfg(unix)]
        Wire::Unix => {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("loop.sock");
            let listener = loop_wire::socket::unix_listen(&path).unwrap();
            let client = tokio::net::UnixStream::connect(&path).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();
            // Keep the directory until the connection is up; the open
            // sockets outlive it.
            drop(dir);
            (
                loop_wire::socket::unix(server),
                loop_wire::socket::unix(client),
            )
        }
        Wire::WebSocket => {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let accept = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                loop_wire::socket::websocket_accept(stream, "t0ken")
                    .await
                    .unwrap()
            });
            let client = loop_wire::socket::websocket_connect(&format!("ws://{addr}/"), "t0ken")
                .await
                .unwrap();
            (accept.await.unwrap(), client)
        }
    };
    let served = serve(backend, server_end, info("server"));
    let remote = RemoteBackend::connect(client_end, info("client"), Arc::new(FirstOption))
        .await
        .expect("initialize");
    (remote, served)
}

/// The shape of a turn without ids: each update's type, and its text.
fn shape(updates: &[UpdateEnvelope]) -> Vec<String> {
    updates
        .iter()
        .map(|e| {
            let value = serde_json::to_value(&e.update).unwrap();
            let kind = value["type"].as_str().unwrap_or("?").to_owned();
            match value["content"]["text"].as_str().or(value["text"].as_str()) {
                Some(text) => format!("{kind}:{text}"),
                None => kind,
            }
        })
        .collect()
}

/// Run one turn; return the updates already received when its outcome
/// resolved, and the outcome.
async fn turn(
    backend: &dyn AgentBackend,
    session: &SessionRef,
    prompt: &str,
) -> (Vec<UpdateEnvelope>, TurnOutcome) {
    let mut updates = backend.subscribe();
    let handle = backend
        .prompt(session, vec![ContentBlock::text(prompt)])
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), handle.outcome())
        .await
        .expect("the turn ends");
    let mut seen = Vec::new();
    loop {
        match updates.try_recv() {
            Ok(envelope) if envelope.session == session.key => seen.push(envelope),
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(error) => panic!("{error}"),
        }
    }
    (seen, outcome)
}

/// Run one turn in-process, reading updates until `turn_ended`: there the
/// outcome may resolve a moment before `turn_ended` is published.
async fn turn_to_end(
    backend: &dyn AgentBackend,
    session: &SessionRef,
    prompt: &str,
) -> (Vec<UpdateEnvelope>, TurnOutcome) {
    let mut updates = backend.subscribe();
    let handle = backend
        .prompt(session, vec![ContentBlock::text(prompt)])
        .await
        .unwrap();
    let mut seen = Vec::new();
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(10), updates.recv())
            .await
            .expect("the turn ends")
            .unwrap();
        if envelope.session != session.key {
            continue;
        }
        let ended = matches!(envelope.update, SessionUpdate::TurnEnded { .. });
        seen.push(envelope);
        if ended {
            break;
        }
    }
    (seen, handle.outcome().await)
}

#[tokio::test]
async fn a_turn_is_the_same_over_memory() {
    a_turn_is_the_same_over(Wire::Memory).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_turn_is_the_same_over_a_unix_socket() {
    a_turn_is_the_same_over(Wire::Unix).await;
}

#[tokio::test]
async fn a_turn_is_the_same_over_a_websocket() {
    a_turn_is_the_same_over(Wire::WebSocket).await;
}

#[tokio::test]
async fn the_server_asks_the_client_over_a_websocket() {
    let backend = native(vec![], vec![]).await;
    let (_remote, served) = over(backend, Wire::WebSocket).await;
    let request = InteractionRequest::Question {
        id: "q1".into(),
        prompt: "?".into(),
        choices: None,
    };
    assert_eq!(
        served.host.interact(request).await,
        InteractionOutcome::Cancelled
    );
}

async fn a_turn_is_the_same_over(wire: Wire) {
    let script = || {
        vec![
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "c1".into(),
                name: "missing_tool".into(),
                arguments: json!({}),
                thought_signature: None,
            }]),
            FauxResponse::Text("final answer".into()),
        ]
    };
    let direct = native(script(), vec![]).await;
    let direct_session = direct.current_session().await;
    let (direct_updates, direct_outcome) =
        turn_to_end(direct.as_ref(), &direct_session, "go").await;

    let served = native(script(), vec![]).await;
    let session = served.current_session().await;
    let (remote, _server) = over(Arc::clone(&served), wire).await;
    assert_eq!(remote.instance(), served.instance());
    assert_eq!(remote.capabilities(), served.capabilities());
    let (remote_updates, remote_outcome) = turn(remote.as_ref(), &session, "go").await;

    assert_eq!(remote_outcome, direct_outcome);
    assert!(
        matches!(
            remote_updates.last().map(|e| &e.update),
            Some(SessionUpdate::TurnEnded { .. })
        ),
        "over {wire:?} the outcome resolves only after turn_ended arrived: {:?}",
        shape(&remote_updates)
    );
    assert_eq!(shape(&remote_updates), shape(&direct_updates));
    assert!(!remote.is_busy(&session));

    assert_eq!(
        remote.transcript(&session).await.map(|t| t.len()),
        served.transcript(&session).await.map(|t| t.len())
    );
}

/// A tool that waits until the test lets it finish, or the turn is cancelled.
fn gate_tool(release: Arc<Notify>, saw_cancel: Arc<AtomicBool>) -> AgentTool {
    AgentTool {
        name: "wait".into(),
        label: "wait".into(),
        description: "waits".into(),
        parameters: json!({"type": "object"}),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_, _, cancel, _| {
            let release = Arc::clone(&release);
            let saw_cancel = Arc::clone(&saw_cancel);
            Box::pin(async move {
                let cancelled = async {
                    match cancel {
                        Some(cancel) => cancel.cancelled().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = release.notified() => Ok(AgentToolResult::text("done")),
                    () = cancelled => {
                        saw_cancel.store(true, Ordering::SeqCst);
                        Err("cancelled".into())
                    }
                }
            })
        }),
    }
}

#[tokio::test]
async fn a_busy_session_refuses_a_second_prompt_and_cancel_reaches_the_server() {
    let release = Arc::new(Notify::new());
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let backend = native(
        vec![
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "w".into(),
                name: "wait".into(),
                arguments: json!({}),
                thought_signature: None,
            }]),
            FauxResponse::Text("after".into()),
        ],
        vec![gate_tool(Arc::clone(&release), Arc::clone(&saw_cancel))],
    )
    .await;
    let session = backend.current_session().await;
    let (remote, _server) = over_the_wire(Arc::clone(&backend)).await;
    let mut updates = remote.subscribe();
    let first = remote
        .prompt(&session, vec![ContentBlock::text("go")])
        .await
        .unwrap();
    // Wait until the tool is running on the server.
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(10), updates.recv())
            .await
            .expect("the tool starts")
            .unwrap();
        if matches!(envelope.update, SessionUpdate::ToolCall(_)) {
            break;
        }
    }
    assert!(remote.is_busy(&session));
    assert!(matches!(
        remote
            .prompt(&session, vec![ContentBlock::text("again")])
            .await,
        Err(BackendError::Busy)
    ));
    remote.cancel(&session);
    let outcome = tokio::time::timeout(Duration::from_secs(10), first.outcome())
        .await
        .unwrap();
    assert!(
        saw_cancel.load(Ordering::SeqCst),
        "the cancel reached the tool on the server"
    );
    // The native backend finishes the turn after a cancelled tool call
    // (in-process too); the wire only has to report what it reports.
    assert!(
        matches!(outcome, TurnOutcome::Completed(_) | TurnOutcome::Cancelled),
        "{outcome:?}"
    );
    assert!(!remote.is_busy(&session));
}

#[tokio::test]
async fn the_server_asks_the_client_through_host_interact() {
    let backend = native(vec![], vec![]).await;
    let (_remote, served) = over_the_wire(backend).await;
    let request = InteractionRequest::Permission {
        id: "i1".into(),
        tool_call: None,
        options: vec![
            PermissionOption {
                native_id: "once".into(),
                label: "Allow once".into(),
                kind_hint: PermissionKind::AllowOnce,
            },
            PermissionOption {
                native_id: "no".into(),
                label: "Reject".into(),
                kind_hint: PermissionKind::RejectOnce,
            },
        ],
    };
    assert_eq!(
        served.host.interact(request).await,
        InteractionOutcome::Selected {
            native_id: "once".into(),
            note: Some("from the client".into())
        }
    );
}

#[tokio::test]
async fn nothing_but_initialize_is_answered_before_initialize() {
    let backend = native(vec![], vec![]).await;
    let (server_end, client_end) = memory();
    let _served = serve(backend, server_end, info("server"));
    let (conn, mut incoming) = Connection::new(client_end);
    let early = tokio::spawn({
        let conn = Arc::clone(&conn);
        async move { conn.request("session/new", json!({})).await }
    });
    if let Some(Incoming::Response { id, result }) = incoming.recv().await {
        conn.resolve(id, result).await;
    }
    assert!(matches!(
        early.await.unwrap(),
        Err(RpcError::Remote { code: -32600, .. })
    ));
}

#[tokio::test]
async fn losing_the_server_mid_turn_is_uncertain() {
    let (mut server_end, client_end) = memory();
    // A server that answers initialize, waits for the prompt, then
    // disappears without answering it.
    let fake = tokio::spawn(async move {
        let request: serde_json::Value =
            serde_json::from_str(&server_end.incoming.recv().await.unwrap()).unwrap();
        let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": {
            "protocol_version": 1,
            "server_info": {"name": "fake", "version": "0"},
            "instance": "fake",
            "capabilities": {}
        }});
        server_end.outgoing.send(reply.to_string()).unwrap();
        let _prompt = server_end.incoming.recv().await;
    });
    let remote = RemoteBackend::connect(client_end, info("client"), Arc::new(FirstOption))
        .await
        .unwrap();
    let session = SessionRef {
        instance: "fake".into(),
        key: "s".into(),
        native_id: "s".into(),
    };
    let handle = remote
        .prompt(&session, vec![ContentBlock::text("go")])
        .await
        .unwrap();
    fake.await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap(),
        TurnOutcome::Uncertain
    );
}

#[tokio::test]
async fn a_client_refuses_a_server_version_it_does_not_speak() {
    let (mut server_end, client_end) = memory();
    tokio::spawn(async move {
        let request: serde_json::Value =
            serde_json::from_str(&server_end.incoming.recv().await.unwrap()).unwrap();
        let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": {
            "protocol_version": 99,
            "server_info": {"name": "future", "version": "9"},
            "instance": "x",
            "capabilities": {}
        }});
        server_end.outgoing.send(reply.to_string()).unwrap();
        let _ = server_end.incoming.recv().await;
    });
    let error = RemoteBackend::connect(client_end, info("client"), Arc::new(FirstOption))
        .await
        .err()
        .expect("refused");
    assert!(error.to_string().contains("99"), "{error}");
}

#[tokio::test]
async fn a_refused_second_prompt_does_not_lose_the_first_turns_answer() {
    let release = Arc::new(Notify::new());
    let backend = native(
        vec![
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "w".into(),
                name: "wait".into(),
                arguments: json!({}),
                thought_signature: None,
            }]),
            FauxResponse::Text("after".into()),
        ],
        vec![gate_tool(
            Arc::clone(&release),
            Arc::new(AtomicBool::new(false)),
        )],
    )
    .await;
    let session = backend.current_session().await;
    let (server_end, client_end) = memory();
    let _served = serve(backend, server_end, info("server"));
    // A raw client: no local busy check, so the server has to refuse.
    let (conn, mut incoming) = Connection::new(client_end);
    let (tool_running, mut tool_running_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn({
        let conn = Arc::clone(&conn);
        async move {
            while let Some(message) = incoming.recv().await {
                match message {
                    Incoming::Response { id, result } => conn.resolve(id, result).await,
                    Incoming::Notification { params, .. } => {
                        if params["update"]["type"] == "tool_call" {
                            let _ = tool_running.send(());
                        }
                    }
                    Incoming::Request { .. } => {}
                }
            }
            conn.fail_pending().await;
        }
    });
    conn.request(
        "initialize",
        json!({"protocol_version": 1, "client_info": {"name": "raw", "version": "0"}}),
    )
    .await
    .unwrap();
    let prompt = json!({"session": session, "content": [{"type": "text", "text": "go"}]});
    let first = tokio::spawn({
        let conn = Arc::clone(&conn);
        let prompt = prompt.clone();
        async move { conn.request("session/prompt", prompt).await }
    });
    tool_running_rx.recv().await.unwrap();
    let second = conn.request("session/prompt", prompt).await;
    assert!(
        matches!(second, Err(RpcError::Remote { code: -32001, .. })),
        "{second:?}"
    );
    release.notify_waiters();
    let first = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .expect("the first turn is still answered")
        .unwrap()
        .unwrap();
    assert_eq!(first["stop"], "end_turn");
}

/// Choosing model and effort is the same in-process and over each wire.
#[tokio::test]
async fn model_control_is_the_same_over_the_wire() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for wire in [
            Wire::Memory,
            #[cfg(unix)]
            Wire::Unix,
            Wire::WebSocket,
        ] {
            let local = native(vec![], vec![]).await;
            let (remote, _served) = over(native(vec![], vec![]).await, wire).await;
            let local_session = local.current_session().await;
            let remote_session = remote
                .new_session(loop_protocol::SessionSpec::default())
                .await
                .unwrap();
            let caps = (local.capabilities(), remote.capabilities());
            assert_eq!(
                (caps.0.set_model, caps.0.efforts, caps.0.modes),
                (caps.1.set_model, caps.1.efforts, caps.1.modes),
                "{wire:?}"
            );
            let (here, there) = (
                local.model_control().unwrap(),
                remote.model_control().unwrap(),
            );
            assert_eq!(
                here.models(&local_session).await.unwrap(),
                there.models(&remote_session).await.unwrap(),
                "{wire:?}"
            );
            assert_eq!(
                here.efforts(&local_session).await.unwrap(),
                there.efforts(&remote_session).await.unwrap(),
                "{wire:?}"
            );
            let mut rx = remote.subscribe();
            let result = there
                .set_model(&remote_session, "faux/faux-model", Some(0))
                .await
                .unwrap();
            assert_eq!(result.revision, 1);
            let changed = loop {
                let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if let SessionUpdate::ConfigChanged(changed) = envelope.update {
                    assert_eq!(envelope.seq, result.seq);
                    break changed;
                }
            };
            assert_eq!(changed.config.model.as_deref(), Some("faux/faux-model"));
            // Errors keep their kind across the wire.
            assert_eq!(
                there
                    .set_effort(&remote_session, "off", Some(0))
                    .await
                    .unwrap_err(),
                BackendError::Conflict { revision: 1 },
                "{wire:?}"
            );
            let error = there
                .set_effort(&remote_session, "max", None)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("choose one of"), "{error}");
            assert!(matches!(
                there.modes(&remote_session).await,
                Err(BackendError::Unsupported(_))
            ));
        }
    })
    .await
    .expect("test timed out");
}

/// A backend with selectable modes, recording what it was asked.
struct Moded {
    inner: Arc<NativeBackend>,
    mode: parking_lot::Mutex<String>,
}

#[async_trait]
impl AgentBackend for Moded {
    fn instance(&self) -> &loop_protocol::BackendInstanceId {
        self.inner.instance()
    }
    fn capabilities(&self) -> loop_protocol::Capabilities {
        loop_protocol::Capabilities {
            modes: true,
            ..Default::default()
        }
    }
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.inner.subscribe()
    }
    async fn new_session(
        &self,
        spec: loop_protocol::SessionSpec,
    ) -> loop_protocol::BackendResult<SessionRef> {
        self.inner.new_session(spec).await
    }
    async fn load_session(
        &self,
        session: &SessionRef,
        spec: loop_protocol::SessionSpec,
    ) -> loop_protocol::BackendResult<SessionRef> {
        self.inner.load_session(session, spec).await
    }
    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> loop_protocol::BackendResult<loop_protocol::TurnHandle> {
        self.inner.prompt(session, input).await
    }
    fn cancel(&self, session: &SessionRef) {
        self.inner.cancel(session)
    }
    fn is_busy(&self, session: &SessionRef) -> bool {
        self.inner.is_busy(session)
    }
    async fn shutdown(&self) {}
    fn model_control(&self) -> Option<&dyn loop_protocol::ModelControl> {
        Some(self)
    }
}

#[async_trait]
impl loop_protocol::ModelControl for Moded {
    async fn models(
        &self,
        _: &SessionRef,
    ) -> loop_protocol::BackendResult<loop_protocol::ModelChoices> {
        unreachable!("the server refuses methods without their capability")
    }
    async fn set_model(
        &self,
        _: &SessionRef,
        _: &str,
        _: Option<u64>,
    ) -> loop_protocol::BackendResult<loop_protocol::MutationResult> {
        unreachable!("the server refuses methods without their capability")
    }
    async fn modes(
        &self,
        _: &SessionRef,
    ) -> loop_protocol::BackendResult<loop_protocol::ModelChoices> {
        Ok(loop_protocol::ModelChoices {
            current: Some(self.mode.lock().clone()),
            models: ["ask", "plan"]
                .into_iter()
                .map(|id| loop_protocol::ModelChoice {
                    id: id.into(),
                    name: id.to_uppercase(),
                    description: None,
                })
                .collect(),
            revision: 7,
        })
    }
    async fn set_mode(
        &self,
        _: &SessionRef,
        mode: &str,
        expected_revision: Option<u64>,
    ) -> loop_protocol::BackendResult<loop_protocol::MutationResult> {
        if expected_revision.is_some_and(|r| r != 7) {
            return Err(BackendError::Conflict { revision: 7 });
        }
        *self.mode.lock() = mode.into();
        Ok(loop_protocol::MutationResult {
            revision: 8,
            seq: 3,
            pending: true,
            applies_after_turn: Some("turn-1".into()),
        })
    }
}

#[tokio::test]
async fn modes_follow_the_capability_and_missing_ones_are_unsupported() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let backend = Arc::new(Moded {
            inner: native(vec![], vec![]).await,
            mode: parking_lot::Mutex::new("ask".into()),
        });
        let (server_end, client_end) = memory();
        let _served = serve(backend.clone(), server_end, info("server"));
        let remote = RemoteBackend::connect(client_end, info("client"), Arc::new(FirstOption))
            .await
            .unwrap();
        let session = backend.inner.current_session().await;
        let control = remote.model_control().expect("modes are offered");
        let modes = control.modes(&session).await.unwrap();
        assert_eq!(modes.current.as_deref(), Some("ask"));
        assert_eq!(modes.revision, 7);
        assert_eq!(modes.models[1].name, "PLAN");
        let result = control.set_mode(&session, "plan", Some(7)).await.unwrap();
        assert_eq!(
            result.applies_after_turn.as_ref().map(|t| t.as_str()),
            Some("turn-1")
        );
        assert!(result.pending);
        assert_eq!(*backend.mode.lock(), "plan");
        assert_eq!(
            control
                .set_mode(&session, "ask", Some(1))
                .await
                .unwrap_err(),
            BackendError::Conflict { revision: 7 }
        );
        // Not advertised: the client refuses before asking, and the server
        // refuses a client that asks anyway.
        assert!(matches!(
            control.models(&session).await,
            Err(BackendError::Unsupported(_))
        ));
        assert!(matches!(
            control.set_effort(&session, "off", None).await,
            Err(BackendError::Unsupported(_))
        ));
        let (server_end, client_end) = memory();
        let _served = serve(backend.clone(), server_end, info("server"));
        let (conn, mut incoming) = Connection::new(client_end);
        tokio::spawn({
            let conn = conn.clone();
            async move {
                while let Some(message) = incoming.recv().await {
                    if let Incoming::Response { id, result } = message {
                        conn.resolve(id, result).await;
                    }
                }
            }
        });
        conn.request(
            "initialize",
            json!({"protocol_version": 1, "client_info": {"name": "old", "version": "0"}}),
        )
        .await
        .unwrap();
        let refused = conn
            .request(
                "session/set_model",
                json!({"session": session, "model": "faux/faux-model"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(refused, RpcError::Remote { code, .. } if code == loop_protocol::rpc::error_codes::UNSUPPORTED),
            "{refused:?}"
        );
    })
    .await
    .expect("test timed out");
}

/// During a turn the model is refused and an effort change waits, the same
/// in-process and over the wire.
#[tokio::test]
async fn busy_model_rules_are_the_same_over_the_wire() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let release = Arc::new(Notify::new());
        let backend = native(
            vec![
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "w".into(),
                    name: "wait".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                FauxResponse::Text("after".into()),
            ],
            vec![gate_tool(Arc::clone(&release), Arc::default())],
        )
        .await;
        let session = backend.current_session().await;
        let (remote, _server) = over_the_wire(Arc::clone(&backend)).await;
        let mut updates = remote.subscribe();
        let turn = remote
            .prompt(&session, vec![ContentBlock::text("go")])
            .await
            .unwrap();
        loop {
            let envelope = updates.recv().await.unwrap();
            if matches!(envelope.update, SessionUpdate::ToolCall(_)) {
                break;
            }
        }
        let local = backend.model_control().unwrap();
        let wire = remote.model_control().unwrap();
        for control in [local, wire] {
            let result = control
                .set_model(&session, "faux/faux-model", None)
                .await
                .unwrap();
            assert!(result.pending, "a busy Session applies the model next turn");
            let result = control.set_effort(&session, "off", None).await.unwrap();
            assert!(result.pending);
        }
        release.notify_one();
        turn.outcome().await;
        wire.set_model(&session, "faux/faux-model", None)
            .await
            .unwrap();
    })
    .await
    .expect("test timed out");
}

/// Compaction, stats and the user shell answer the same in-process and
/// over each wire, including their failures and output bounds.
#[tokio::test]
async fn compact_stats_and_shell_are_the_same_over_the_wire() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for wire in [
            Wire::Memory,
            #[cfg(unix)]
            Wire::Unix,
            Wire::WebSocket,
        ] {
            let local = native(vec![], vec![]).await;
            let (remote, _served) = over(native(vec![], vec![]).await, wire).await;
            let local_session = local.current_session().await;
            let remote_session = remote
                .new_session(loop_protocol::SessionSpec::default())
                .await
                .unwrap();
            let flags =
                |c: loop_protocol::Capabilities| (c.compaction, c.session_stats, c.local_shell);
            assert_eq!(flags(local.capabilities()), (true, true, true));
            assert_eq!(flags(remote.capabilities()), (true, true, true), "{wire:?}");

            let here = local
                .session_stats()
                .unwrap()
                .stats(&local_session)
                .await
                .unwrap();
            let there = remote
                .session_stats()
                .unwrap()
                .stats(&remote_session)
                .await
                .unwrap();
            // The window is the Session model's, known before any usage.
            assert!(here.context_window.is_some_and(|w| w > 0));
            assert_eq!(here.context_window, there.context_window, "{wire:?}");
            assert_eq!(here.total_tokens, there.total_tokens);
            assert_eq!(there.turn_state.as_deref(), Some("idle"));
            assert!(!there.report.is_empty());

            let command = "printf out; printf err >&2; exit 3";
            let here = local
                .local_shell()
                .unwrap()
                .run(&local_session, command, None)
                .await
                .unwrap();
            let there = remote
                .local_shell()
                .unwrap()
                .run(&remote_session, command, None)
                .await
                .unwrap();
            assert_eq!(here, there, "{wire:?}");
            assert_eq!(
                (
                    there.stdout.as_str(),
                    there.stderr.as_str(),
                    there.exit_code
                ),
                ("out", "err", 3)
            );

            // Output is bounded per stream and the cut is marked.
            let noisy = "head -c 1100000 /dev/zero | tr '\\0' a";
            let big = remote
                .local_shell()
                .unwrap()
                .run(&remote_session, noisy, None)
                .await
                .unwrap();
            assert!(big.truncated, "{wire:?}");
            assert_eq!(big.stdout.len(), loop_protocol::SHELL_OUTPUT_LIMIT);

            // A timeout stops the command; bad timeouts are refused first.
            let slow = remote
                .local_shell()
                .unwrap()
                .run(&remote_session, "sleep 5", Some(100))
                .await
                .unwrap_err();
            assert!(slow.to_string().contains("timed out"), "{slow}");
            for bad in [0, 600_001] {
                let here = local
                    .local_shell()
                    .unwrap()
                    .run(&local_session, "true", Some(bad))
                    .await;
                let there = remote
                    .local_shell()
                    .unwrap()
                    .run(&remote_session, "true", Some(bad))
                    .await;
                assert_eq!(here.unwrap_err(), there.unwrap_err(), "{wire:?}");
            }

            // Nothing to compact yet: the same typed refusal on both sides.
            let here = local
                .compaction()
                .unwrap()
                .compact(&local_session, None)
                .await
                .unwrap_err();
            let there = remote
                .compaction()
                .unwrap()
                .compact(&remote_session, Some("keep it short"))
                .await
                .unwrap_err();
            assert_eq!(here.to_string(), there.to_string(), "{wire:?}");
        }
    })
    .await
    .expect("test timed out");
}

/// A backend that does not offer the services: the client hides them and
/// the server refuses a client that asks anyway.
#[tokio::test]
async fn compact_stats_and_shell_need_their_capabilities() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let backend = Arc::new(Moded {
            inner: native(vec![], vec![]).await,
            mode: parking_lot::Mutex::new("ask".into()),
        });
        let (server_end, client_end) = memory();
        let _served = serve(backend.clone(), server_end, info("server"));
        let remote = RemoteBackend::connect(client_end, info("client"), Arc::new(FirstOption))
            .await
            .unwrap();
        assert!(remote.compaction().is_none());
        assert!(remote.session_stats().is_none());
        assert!(remote.local_shell().is_none());
        let session = backend.inner.current_session().await;
        let (server_end, client_end) = memory();
        let _served = serve(backend.clone(), server_end, info("server"));
        let (conn, mut incoming) = Connection::new(client_end);
        tokio::spawn({
            let conn = conn.clone();
            async move {
                while let Some(message) = incoming.recv().await {
                    if let Incoming::Response { id, result } = message {
                        conn.resolve(id, result).await;
                    }
                }
            }
        });
        conn.request(
            "initialize",
            json!({"protocol_version": 1, "client_info": {"name": "old", "version": "0"}}),
        )
        .await
        .unwrap();
        for (method, params) in [
            ("session/compact", json!({"session": session})),
            ("session/stats", json!({"session": session})),
            ("session/shell", json!({"session": session, "command": "true"})),
            ("session/ttfb", json!({"session": session})),
        ] {
            let refused = conn.request(method, params).await.unwrap_err();
            assert!(
                matches!(refused, RpcError::Remote { code, .. } if code == loop_protocol::rpc::error_codes::UNSUPPORTED),
                "{method}: {refused:?}"
            );
        }
    })
    .await
    .expect("test timed out");
}

/// The response-timeout override reads and changes alike in-process and over
/// each wire, including an authenticated WebSocket, and never becomes the
/// default for new Sessions.
#[tokio::test]
async fn ttfb_is_the_same_over_the_wire() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use loop_protocol::rpc::TtfbResult;
        for wire in [
            Wire::Memory,
            #[cfg(unix)]
            Wire::Unix,
            Wire::WebSocket,
        ] {
            let local = native(vec![], vec![]).await;
            let (remote, _served) = over(native(vec![], vec![]).await, wire).await;
            assert!(local.capabilities().session_ttfb);
            assert!(remote.capabilities().session_ttfb, "{wire:?}");
            let local_session = local.current_session().await;
            let remote_session = remote
                .new_session(loop_protocol::SessionSpec::default())
                .await
                .unwrap();
            let (here, there) = (
                local.model_control().unwrap(),
                remote.model_control().unwrap(),
            );
            let read = there.ttfb(&remote_session, None, None).await.unwrap();
            assert_eq!(here.ttfb(&local_session, None, None).await.unwrap(), read);
            assert_eq!(read.override_ms, None);
            assert_eq!(read.seq, None);
            let default = read.effective_ms;
            assert!(default > 0);

            // Explicit zero is an override that waits indefinitely.
            let mut rx = remote.subscribe();
            let zero = there
                .ttfb(&remote_session, Some(Some(0)), Some(read.revision))
                .await
                .unwrap();
            assert_eq!(
                (
                    zero.override_ms,
                    zero.effective_ms,
                    zero.revision,
                    zero.pending
                ),
                (Some(0), 0, read.revision + 1, false)
            );
            let changed = loop {
                let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if let SessionUpdate::ConfigChanged(changed) = envelope.update {
                    assert_eq!(Some(envelope.seq), zero.seq);
                    break changed;
                }
            };
            assert_eq!(changed.config.response_timeout_ms, Some(0));
            assert_eq!(changed.revision, zero.revision);
            assert_eq!(
                here.ttfb(&local_session, Some(Some(0)), None)
                    .await
                    .unwrap()
                    .override_ms,
                Some(0)
            );
            // A stale revision conflicts, over the wire too.
            assert_eq!(
                there
                    .ttfb(&remote_session, Some(Some(5_000)), Some(read.revision))
                    .await
                    .unwrap_err(),
                BackendError::Conflict {
                    revision: zero.revision
                },
                "{wire:?}"
            );
            let set = there
                .ttfb(&remote_session, Some(Some(45_000)), None)
                .await
                .unwrap();
            assert_eq!((set.override_ms, set.effective_ms), (Some(45_000), 45_000));

            // Clearing inherits the default again.
            let cleared = there.ttfb(&remote_session, Some(None), None).await.unwrap();
            assert_eq!((cleared.override_ms, cleared.effective_ms), (None, default));
            // Set again, then a new Session still starts from the default.
            there
                .ttfb(&remote_session, Some(Some(1_000)), None)
                .await
                .unwrap();
            let fresh = remote
                .new_session(loop_protocol::SessionSpec::default())
                .await
                .unwrap();
            assert_eq!(
                there.ttfb(&fresh, None, None).await.unwrap(),
                TtfbResult {
                    override_ms: None,
                    effective_ms: default,
                    revision: 0,
                    seq: None,
                    pending: false,
                },
                "{wire:?}"
            );
        }
    })
    .await
    .expect("test timed out");
}
