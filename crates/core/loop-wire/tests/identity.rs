//! Connection identity (ADR-0009): each connection gets a caller when it is
//! accepted, `initialize` reports it, the backend sees it while serving that
//! connection's requests, and no request can set or change it. An Edge
//! Gateway asserts an end user only on its own listener, with its own token,
//! at the handshake.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loop_protocol::rpc::{methods, InitializeResult, PeerInfo, SessionInfo};
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Caller, Capabilities,
    ContentBlock, LocalShell, Principal, PrincipalKind, SessionRef, SessionSpec, ShellResult,
    Tenant, ToolLock, TurnHandle, UpdateEnvelope,
};
use loop_wire::server::{serve_with_caller, serve_with_control, ServeOptions};
use loop_wire::socket::{
    edge_headers, websocket_accept, websocket_accept_edge, websocket_connect_edge,
};
use loop_wire::transport::memory;
use loop_wire::{
    Attachment, Authority, Connection, Incoming, RpcError, RuntimeAdmin, RuntimeControl, Transport,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};

fn info() -> PeerInfo {
    PeerInfo {
        name: "server".into(),
        version: "test".into(),
    }
}

/// A backend that answers each method with the caller it sees in scope, so
/// a test can tell which connection's identity reached the backend. A
/// `session/cancel` notification has no answer, so it is recorded.
struct Echo {
    instance: BackendInstanceId,
    updates: broadcast::Sender<UpdateEnvelope>,
    cancels: std::sync::Mutex<Vec<String>>,
    /// The tool lock its admin service offers; `None` is an unlocked
    /// Runtime.
    lock: Option<ToolLock>,
    /// Shell commands that reached the backend.
    shells: std::sync::Mutex<Vec<String>>,
}

fn seen() -> String {
    match Caller::current() {
        Some(caller) => format!("{}|{}", caller.principal().id(), caller.tenant()),
        None => "none".into(),
    }
}

#[async_trait]
impl AgentBackend for Echo {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            local_shell: true,
            ..Capabilities::default()
        }
    }
    fn local_shell(&self) -> Option<&dyn LocalShell> {
        Some(self)
    }
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }
    async fn new_session(&self, _spec: SessionSpec) -> BackendResult<SessionRef> {
        // Let other connections' requests run in between.
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        Ok(SessionRef {
            instance: self.instance.clone(),
            key: "s".into(),
            native_id: seen(),
        })
    }
    async fn load_session(&self, _: &SessionRef, _: SessionSpec) -> BackendResult<SessionRef> {
        Err(BackendError::Unsupported(seen()))
    }
    async fn prompt(&self, _: &SessionRef, _: Vec<ContentBlock>) -> BackendResult<TurnHandle> {
        Err(BackendError::Unsupported(seen()))
    }
    fn cancel(&self, _: &SessionRef) {
        self.cancels.lock().unwrap().push(seen());
    }
    fn is_busy(&self, _: &SessionRef) -> bool {
        false
    }
    async fn shutdown(&self) {}
}

#[async_trait]
impl LocalShell for Echo {
    async fn run(
        &self,
        _: &SessionRef,
        command: &str,
        _: Option<u64>,
    ) -> BackendResult<ShellResult> {
        self.shells.lock().unwrap().push(command.to_owned());
        Err(BackendError::Unsupported(seen()))
    }
}

#[async_trait]
impl RuntimeAdmin for Echo {
    fn offered(&self) -> Capabilities {
        Capabilities {
            tool_lock: self.lock,
            ..Capabilities::default()
        }
    }
}

#[async_trait]
impl RuntimeControl for Echo {
    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        Err(BackendError::Unsupported(seen()))
    }
    async fn close(&self, _: &SessionRef, _: bool) -> BackendResult<()> {
        Ok(())
    }
    async fn attach(&self, _: &SessionRef, _: u64) -> BackendResult<Attachment> {
        Err(BackendError::Unsupported(seen()))
    }
    fn admin(&self) -> Option<&dyn RuntimeAdmin> {
        Some(self)
    }
}

/// A tool-locked Runtime, which serves Edge Gateway users.
fn echo() -> Arc<Echo> {
    echo_with(Some(ToolLock::Chat))
}

fn echo_with(lock: Option<ToolLock>) -> Arc<Echo> {
    Arc::new(Echo {
        instance: "echo".into(),
        updates: broadcast::channel(4).0,
        cancels: std::sync::Mutex::new(Vec::new()),
        lock,
        shells: std::sync::Mutex::new(Vec::new()),
    })
}

fn options(authority: Authority) -> ServeOptions {
    ServeOptions {
        owns_backend: false,
        authority,
    }
}

fn edge_caller(id: &str, tenant: &str) -> Caller {
    let principal =
        Principal::new(PrincipalKind::Person, "https://accounts.google.com", id).unwrap();
    Caller::asserted(principal, Tenant::new(tenant).unwrap()).unwrap()
}

/// A client connection that answers nothing and resolves responses.
fn client(transport: Transport) -> Arc<Connection> {
    let (conn, mut incoming) = Connection::new(transport);
    let resolver = Arc::clone(&conn);
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Response { id, result } = message {
                resolver.resolve(id, result).await;
            }
        }
    });
    conn
}

async fn initialize(conn: &Connection, extra: Value) -> (Value, InitializeResult) {
    let mut params =
        json!({"protocol_version": 1, "client_info": {"name": "client", "version": "0"}});
    if let (Value::Object(params), Value::Object(extra)) = (&mut params, extra) {
        params.extend(extra);
    }
    let value = conn.request(methods::INITIALIZE, params).await.unwrap();
    let parsed = serde_json::from_value(value.clone()).unwrap();
    (value, parsed)
}

async fn new_session_sees(conn: &Connection) -> String {
    let made = conn.request(methods::SESSION_NEW, json!({})).await.unwrap();
    made["native_id"].as_str().unwrap().to_owned()
}

/// Identity fields a client might try to send in a request.
fn forged() -> Value {
    json!({
        "principal": {"kind": "person", "issuer": "https://evil", "id": "mallory"},
        "tenant": "victim",
        "via": "edge",
        "caller": {"principal": {"kind": "person", "issuer": "https://evil", "id": "mallory"},
                   "tenant": "victim"}
    })
}

#[tokio::test]
async fn a_host_local_connection_is_the_local_user_in_the_local_tenant() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let backend = echo();
        let (server, client_end) = memory();
        let _ = serve_with_control(
            backend.clone(),
            backend,
            server,
            info(),
            options(Authority::HostLocal),
        );
        let conn = client(client_end);
        let (value, result) = initialize(&conn, forged()).await;
        let local = loop_wire::local_principal();
        assert_eq!(local.kind(), &PrincipalKind::Person);
        assert_eq!(local.issuer(), "local");
        assert!(local.id().starts_with("uid:"));
        assert_eq!(result.principal.as_ref(), Some(&local));
        assert_eq!(result.tenant, Some(Tenant::local()));
        assert_eq!(result.via, None);
        assert!(value.get("via").is_none());
        // Forged request fields are ignored, at initialize and after it.
        let id = local.id().to_owned();
        assert_eq!(new_session_sees(&conn).await, format!("{id}|local"));
        let (_, again) = initialize(&conn, forged()).await;
        assert_eq!(again.principal, Some(local));
        assert_eq!(again.tenant, Some(Tenant::local()));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_remote_connection_without_a_named_caller_has_no_identity() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let backend = echo();
        let (server, client_end) = memory();
        let _ = serve_with_control(
            backend.clone(),
            backend.clone(),
            server,
            info(),
            options(Authority::Remote),
        );
        let conn = client(client_end);
        let (value, result) = initialize(&conn, forged()).await;
        assert_eq!(result.principal, None);
        assert_eq!(result.tenant, None);
        assert!(value.get("principal").is_none() && value.get("tenant").is_none());
        // No fallback identity reaches the backend either: with no identity
        // to check access with, nothing but `initialize` is served (I3).
        let refused = conn
            .request(methods::SESSION_NEW, json!({}))
            .await
            .unwrap_err();
        assert!(
            format!("{refused:?}").contains("no identity"),
            "{refused:?}"
        );
        let refused = conn
            .request(methods::SESSION_LIST, json!({}))
            .await
            .unwrap_err();
        assert!(
            format!("{refused:?}").contains("no identity"),
            "{refused:?}"
        );
        conn.notify(methods::SESSION_CANCEL, json!({"session": a_session()}))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(backend.cancels.lock().unwrap().is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn each_connection_s_requests_see_only_its_own_caller() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let backend = echo();
        let mut conns = Vec::new();
        for (id, tenant) in [("alice", "t-a"), ("bob", "t-b")] {
            let (server, client_end) = memory();
            let _ = serve_with_caller(
                backend.clone(),
                Some(backend.clone()),
                server,
                info(),
                options(Authority::Remote),
                edge_caller(id, tenant),
            );
            let conn = client(client_end);
            let (_, result) = initialize(&conn, forged()).await;
            assert_eq!(result.principal.as_ref().unwrap().id(), id);
            assert_eq!(result.tenant.as_ref().unwrap().as_str(), tenant);
            assert_eq!(result.via.as_ref().unwrap().as_str(), "edge");
            conns.push((conn, format!("{id}|{tenant}")));
        }
        // Interleaved requests from both connections.
        let mut calls = Vec::new();
        for _ in 0..20 {
            for (conn, expected) in &conns {
                let conn = conn.clone();
                let expected = expected.clone();
                calls.push(tokio::spawn(async move {
                    assert_eq!(new_session_sees(&conn).await, expected);
                    // Every backend and RuntimeControl entry is in scope.
                    let session = json!({"instance": "echo", "key": "s", "native_id": "n"});
                    for (method, params) in [
                        (methods::SESSION_LIST, json!({})),
                        (
                            methods::SESSION_LOAD,
                            json!({"session": session, "spec": {}}),
                        ),
                        (
                            methods::SESSION_PROMPT,
                            json!({"session": session, "content": []}),
                        ),
                        (
                            methods::SESSION_ATTACH,
                            json!({"session": session, "after_seq": 0}),
                        ),
                    ] {
                        let answer = conn.request(method, params).await;
                        let text = format!("{answer:?}");
                        assert!(text.contains(&expected), "{method}: {text}");
                    }
                    conn.notify(methods::SESSION_CANCEL, json!({"session": session}))
                        .unwrap();
                }));
            }
        }
        for call in calls {
            call.await.unwrap();
        }
        // Each cancel ran with its own connection's caller.
        loop {
            let cancels = backend.cancels.lock().unwrap().clone();
            if cancels.len() == 40 {
                for (_, expected) in &conns {
                    assert_eq!(cancels.iter().filter(|c| *c == expected).count(), 20);
                }
                break;
            }
            drop(cancels);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_token_connection_is_the_local_user_via_token() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let backend = echo();
        let (server, client_end) = memory();
        let _ = serve_with_caller(
            backend.clone(),
            Some(backend),
            server,
            info(),
            options(Authority::Remote),
            Caller::local_token(loop_wire::local_principal()),
        );
        let conn = client(client_end);
        let (_, result) = initialize(&conn, forged()).await;
        assert_eq!(result.principal, Some(loop_wire::local_principal()));
        assert_eq!(result.tenant, Some(Tenant::local()));
        assert_eq!(result.via.unwrap().as_str(), "token");
        // A token connection is still remote: host-only methods are refused.
        let refused = conn.request(methods::RUNTIME_STOP, json!({})).await;
        assert!(refused.is_err());
    })
    .await
    .unwrap();
}

fn forbidden(answer: Result<Value, RpcError>) -> String {
    match answer {
        Err(RpcError::Remote { code, message, .. })
            if code == loop_protocol::rpc::error_codes::FORBIDDEN =>
        {
            message
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

fn a_session() -> Value {
    json!({"instance": "echo", "key": "s", "native_id": "n"})
}

#[tokio::test]
async fn an_edge_user_is_refused_everything_by_a_runtime_without_a_tool_lock() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let backend = echo_with(None);
        let (server, client_end) = memory();
        let _ = serve_with_caller(
            backend.clone(),
            Some(backend.clone()),
            server,
            info(),
            options(Authority::Remote),
            edge_caller("alice", "acme"),
        );
        let conn = client(client_end);
        // initialize answers, so the gateway sees who it is and that no lock
        // is offered ...
        let (_, result) = initialize(&conn, json!({})).await;
        assert_eq!(result.capabilities.tool_lock, None);
        // ... and every other request is refused before the backend sees it.
        for (method, params) in [
            (methods::SESSION_NEW, json!({})),
            (methods::SESSION_LIST, json!({})),
            (
                methods::SESSION_LOAD,
                json!({"session": a_session(), "spec": {}}),
            ),
            (
                methods::SESSION_ATTACH,
                json!({"session": a_session(), "after_seq": 0}),
            ),
            (
                methods::SESSION_PROMPT,
                json!({"session": a_session(), "content": []}),
            ),
            (
                methods::SESSION_SHELL,
                json!({"session": a_session(), "command": "id"}),
            ),
            ("a/future_method", json!({})),
        ] {
            let message = forbidden(conn.request(method, params).await);
            assert!(message.contains("tool lock"), "{method}: {message}");
        }
        conn.notify(methods::SESSION_CANCEL, json!({"session": a_session()}))
            .unwrap();
        // The same backend with no admin service also refuses (fail closed).
        let (server, client_end) = memory();
        let _ = serve_with_caller(
            backend.clone() as Arc<dyn AgentBackend>,
            None,
            server,
            info(),
            options(Authority::Remote),
            edge_caller("alice", "acme"),
        );
        let conn = client(client_end);
        initialize(&conn, json!({})).await;
        forbidden(conn.request(methods::SESSION_NEW, json!({})).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(backend.cancels.lock().unwrap().is_empty());
        assert!(backend.shells.lock().unwrap().is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_edge_user_never_runs_a_shell_command_but_the_host_user_does() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let backend = echo();
        let shell = json!({"session": a_session(), "command": "id"});
        let (server, client_end) = memory();
        let _ = serve_with_caller(
            backend.clone(),
            Some(backend.clone()),
            server,
            info(),
            options(Authority::Remote),
            edge_caller("alice", "acme"),
        );
        let edge = client(client_end);
        initialize(&edge, json!({})).await;
        // Under a lock an edge user is served ...
        assert_eq!(new_session_sees(&edge).await, "alice|acme");
        // ... except the shell, refused before the backend.
        let message = forbidden(edge.request(methods::SESSION_SHELL, shell.clone()).await);
        assert!(message.contains("shell"), "{message}");
        assert!(backend.shells.lock().unwrap().is_empty());
        // The host's user is not an outside user: the shell reaches the
        // backend (which answers for itself).
        let host = echo_with(None);
        let (server, client_end) = memory();
        let _ = serve_with_control(
            host.clone(),
            host.clone(),
            server,
            info(),
            options(Authority::HostLocal),
        );
        let local = client(client_end);
        initialize(&local, json!({})).await;
        let _ = local.request(methods::SESSION_SHELL, shell).await;
        assert_eq!(host.shells.lock().unwrap().as_slice(), ["id"]);
    })
    .await
    .unwrap();
}

// ---- the edge handshake over a real WebSocket ----

const USER_TOKEN: &str = "user-token";
const EDGE_TOKEN: &str = "edge-token";

fn good_headers() -> Vec<(&'static str, String)> {
    vec![
        (edge_headers::PRINCIPAL_KIND, "person".into()),
        (
            edge_headers::PRINCIPAL_ISSUER,
            "https://accounts.google.com".into(),
        ),
        (edge_headers::PRINCIPAL_ID, "110169484474386276334".into()),
        (edge_headers::TENANT, "acme".into()),
    ]
}

enum Listener {
    User,
    Edge,
}

/// Connect to a fresh listener of `kind` with `token` and `headers`. Returns
/// the refusal status, or the client transport and the server's result.
async fn handshake(
    kind: Listener,
    token: &str,
    headers: &[(&'static str, String)],
) -> Result<(Transport, Option<Caller>), StatusCode> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        match kind {
            Listener::User => websocket_accept(stream, USER_TOKEN)
                .await
                .map(|transport| (transport, None)),
            Listener::Edge => websocket_accept_edge(stream, EDGE_TOKEN)
                .await
                .map(|(transport, caller)| (transport, Some(caller))),
        }
    });
    let mut request = format!("ws://{addr}/").into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    for (name, value) in headers {
        request
            .headers_mut()
            .append(*name, HeaderValue::from_bytes(value.as_bytes()).unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => {
            let (transport, caller) = server.await.unwrap().unwrap();
            // Keep the client end open while the test uses the server end.
            tokio::spawn(async move {
                let _ws = ws;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
            Ok((transport, caller))
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert!(server.await.unwrap().is_err());
            Err(response.status())
        }
        Err(other) => panic!("unexpected handshake error: {other}"),
    }
}

#[tokio::test]
async fn an_edge_connection_s_asserted_identity_is_what_initialize_reports() {
    tokio::time::timeout(Duration::from_secs(20), async {
        // The Runtime side: the edge listener, served as `runtime_cmd` does.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = echo();
        let server_backend = backend.clone();
        let accepted = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (transport, caller) = websocket_accept_edge(stream, EDGE_TOKEN).await.unwrap();
            let _ = serve_with_caller(
                server_backend.clone(),
                Some(server_backend),
                transport,
                info(),
                options(Authority::Remote),
                caller.clone(),
            );
            caller
        });
        // The gateway side, over the real WebSocket.
        let caller = edge_caller("110169484474386276334", "acme");
        let transport = websocket_connect_edge(&format!("ws://{addr}/"), EDGE_TOKEN, &caller)
            .await
            .unwrap();
        assert_eq!(accepted.await.unwrap(), caller);
        let conn = client(transport);
        let (_, result) = initialize(&conn, forged()).await;
        assert_eq!(result.principal.as_ref(), Some(caller.principal()));
        assert_eq!(result.tenant.as_ref(), Some(caller.tenant()));
        assert_eq!(result.via.as_ref().unwrap().as_str(), "edge");
        // A second initialize with forged fields changes nothing.
        let (_, again) = initialize(&conn, forged()).await;
        assert_eq!(again.principal.as_ref(), Some(caller.principal()));
        assert_eq!(again.tenant.as_ref(), Some(caller.tenant()));
        assert_eq!(new_session_sees(&conn).await, "110169484474386276334|acme");
        // An edge connection has remote authority.
        assert!(conn
            .request(methods::RUNTIME_STOP, json!({}))
            .await
            .is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_gateway_client_refuses_a_reserved_identity() {
    let principal = Principal::local_uid(0);
    let local = Caller::host(principal);
    let error = websocket_connect_edge("ws://127.0.0.1:9/", EDGE_TOKEN, &local)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn each_token_works_only_on_its_own_listener() {
    tokio::time::timeout(Duration::from_secs(20), async {
        // The user token never opens an edge connection, with or without an
        // assertion.
        assert_eq!(
            handshake(Listener::Edge, USER_TOKEN, &good_headers())
                .await
                .err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            handshake(Listener::Edge, USER_TOKEN, &[]).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        // The edge token never opens a user connection.
        assert_eq!(
            handshake(Listener::User, EDGE_TOKEN, &[]).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        // An ordinary client that sends an assertion is refused.
        for header in good_headers() {
            assert_eq!(
                handshake(Listener::User, USER_TOKEN, &[header]).await.err(),
                Some(StatusCode::BAD_REQUEST)
            );
        }
        // Without one, it connects as before.
        let (_, caller) = handshake(Listener::User, USER_TOKEN, &[]).await.unwrap();
        assert_eq!(caller, None);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_malformed_oversized_or_reserved_assertion_is_refused() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let with = |name: &'static str, value: &str| {
            let mut headers = good_headers();
            headers.retain(|(n, _)| *n != name);
            headers.push((name, value.to_owned()));
            headers
        };
        let without = |name: &'static str| {
            let mut headers = good_headers();
            headers.retain(|(n, _)| *n != name);
            headers
        };
        let long = "x".repeat(256);
        let mut cases = vec![
            with(edge_headers::PRINCIPAL_KIND, "robot"),
            with(edge_headers::PRINCIPAL_KIND, "unknown"),
            with(edge_headers::PRINCIPAL_ISSUER, "local"),
            with(edge_headers::PRINCIPAL_ISSUER, ""),
            with(edge_headers::PRINCIPAL_ID, "a b"),
            with(edge_headers::PRINCIPAL_ID, &long),
            with(edge_headers::TENANT, "local"),
            with(edge_headers::TENANT, "system"),
            with(edge_headers::TENANT, "Acme"),
            with(edge_headers::TENANT, "a/b"),
            with(edge_headers::TENANT, &"t".repeat(65)),
            with(edge_headers::TENANT, "a\tb"),
            // Far over a part's limit. There is no separate cap on all four
            // together: valid values are at most 581 bytes in all.
            with(edge_headers::PRINCIPAL_ISSUER, &"i".repeat(900)),
        ];
        for name in edge_headers::ALL {
            cases.push(without(name));
        }
        // Each header once: a second value is refused, not chosen from.
        let mut twice = good_headers();
        twice.push((edge_headers::TENANT, "other".into()));
        cases.push(twice);
        for headers in cases {
            assert_eq!(
                handshake(Listener::Edge, EDGE_TOKEN, &headers).await.err(),
                Some(StatusCode::BAD_REQUEST),
                "{headers:?}"
            );
        }
    })
    .await
    .unwrap();
}

/// A remote connection that no acceptor named a caller for has no identity
/// to check, so it never runs a shell command; the host's own connection
/// still does.
#[tokio::test]
async fn a_remote_connection_with_no_caller_never_runs_a_shell_command() {
    let backend = echo();
    let (server, client_end) = memory();
    let _ = loop_wire::server::serve_with_options(
        backend.clone(),
        server,
        info(),
        options(Authority::Remote),
    );
    let conn = client(client_end);
    initialize(&conn, json!({})).await;
    let refused = forbidden(
        conn.request(
            methods::SESSION_SHELL,
            json!({"session": a_session(), "command": "true"}),
        )
        .await,
    );
    assert!(refused.contains("no identity"), "{refused}");
    assert!(backend.shells.lock().unwrap().is_empty());
}
