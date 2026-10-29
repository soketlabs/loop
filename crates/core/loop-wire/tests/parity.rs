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
    NativeBackend::new(harness).await
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

/// `backend` served over an in-memory transport, and the client for it.
async fn over_the_wire(backend: Arc<NativeBackend>) -> (Arc<RemoteBackend>, loop_wire::Served) {
    let (server_end, client_end) = memory();
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
async fn a_turn_is_the_same_over_the_wire() {
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
    let (remote, _server) = over_the_wire(Arc::clone(&served)).await;
    assert_eq!(remote.instance(), served.instance());
    assert_eq!(remote.capabilities(), served.capabilities());
    let (remote_updates, remote_outcome) = turn(remote.as_ref(), &session, "go").await;

    assert_eq!(remote_outcome, direct_outcome);
    assert!(
        matches!(
            remote_updates.last().map(|e| &e.update),
            Some(SessionUpdate::TurnEnded { .. })
        ),
        "over the wire the outcome resolves only after turn_ended arrived: {:?}",
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
