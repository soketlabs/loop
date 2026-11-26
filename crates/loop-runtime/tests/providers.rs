//! `runtime/providers` over the wire: a host-local client saves, lists and
//! removes a provider key, and Sessions use the saved key without a Runtime
//! restart; any other connection is refused. Everything lives in temp dirs.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use loop_app_core::config::paths::{auth_path, settings_path};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::{methods, PeerInfo};
use loop_protocol::{
    AgentBackend, Caller, ContentBlock, Principal, PrincipalKind, SessionOptions, SessionSpec,
    Tenant, TurnOutcome,
};
use loop_runtime::log::MemoryEventLog;
use loop_runtime::{Runtime, RuntimeLimits};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};
use loop_wire::server::{serve_with_caller, serve_with_control, ServeOptions};
use loop_wire::transport::memory;
use loop_wire::{Authority, Connection, Incoming, RuntimeControl};
use serde_json::{json, Value};

fn sse(text: &str) -> String {
    let mut body = String::new();
    for choice in [
        json!({"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}),
        json!({"index":0,"delta":{},"finish_reason":"stop"}),
    ] {
        body += &format!(
            "data: {}\n\n",
            json!({"id":"r","object":"chat.completion.chunk","choices":[choice]})
        );
    }
    body + "data: [DONE]\n\n"
}

/// An OpenAI-compatible endpoint that lists one model and answers chats.
fn gateway() -> FakeHttpServer {
    FakeHttpServer::start(
        |r: &RecordedRequest| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/v1/models") => FakeResponse::json(r#"{"data":[{"id":"m-one"}]}"#),
            ("POST", "/v1/chat/completions") => {
                FakeResponse::new(200, "text/event-stream", sse("hello"))
            }
            _ => FakeResponse::new(404, "text/plain", "not found"),
        },
    )
}

fn opts(cwd: &Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("full-access".into()),
        agent: None,
    }
}

/// A Runtime built like the daemon's: the catalog and the Runtime share one
/// credential store under `dir/agent`.
async fn runtime(dir: &Path) -> Arc<Runtime> {
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let globals = GlobalResources::load(dir.join("agent"), &[], false)
        .await
        .unwrap();
    globals
        .models
        .set_provider(loop_ai::providers::faux_provider(Default::default()));
    let store = globals.open_runtime_session_store().unwrap();
    Runtime::new(
        globals,
        opts(&ws),
        Arc::new(MemoryEventLog::new()),
        store,
        RuntimeLimits::default(),
    )
    .await
    .unwrap()
    .into_shared()
}

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "test".into(),
    }
}

/// An initialized raw connection, host-local unless `remote` names a caller;
/// returns its advertised capabilities too.
async fn connect(rt: &Arc<Runtime>, remote: Option<Caller>) -> (Arc<Connection>, Value) {
    let (server, client_end) = memory();
    let control: Arc<dyn RuntimeControl> = rt.clone();
    match remote {
        None => {
            let _ = serve_with_control(
                rt.clone(),
                control,
                server,
                info(),
                ServeOptions {
                    owns_backend: false,
                    authority: Authority::HostLocal,
                },
            );
        }
        Some(caller) => {
            let _ = serve_with_caller(
                rt.clone(),
                Some(control),
                server,
                info(),
                ServeOptions {
                    owns_backend: false,
                    authority: Authority::Remote,
                },
                caller,
            );
        }
    }
    let (conn, mut incoming) = Connection::new(client_end);
    let resolver = Arc::clone(&conn);
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Response { id, result } = message {
                resolver.resolve(id, result).await;
            }
        }
    });
    let init = conn
        .request(
            methods::INITIALIZE,
            json!({"protocol_version": 1, "client_info": {"name": "t", "version": "0"},
                   "client_capabilities": {"interact": true}}),
        )
        .await
        .unwrap();
    (conn, init["capabilities"].clone())
}

fn connected_ids(listed: &Value) -> Vec<String> {
    listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["connected"] == json!(true))
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect()
}

fn login(base_url: &str, key: &str) -> Value {
    json!({"action": "connect", "login": {"Custom": {
        "name": "Gateway",
        "base_url": format!("{base_url}/v1"),
        "api_key": key,
        "models": null,
    }}})
}

/// The bearer token of the last chat request `server` got.
fn last_chat_auth(server: &FakeHttpServer) -> Option<String> {
    server
        .requests()
        .iter()
        .rfind(|r| r.path == "/v1/chat/completions")
        .and_then(|r| r.header("authorization").map(str::to_owned))
}

async fn chat(rt: &Runtime, session: &loop_protocol::SessionRef) {
    let turn = rt
        .prompt(session, vec![ContentBlock::text("hi")])
        .await
        .unwrap();
    let outcome = turn.outcome().await;
    assert!(
        matches!(outcome, TurnOutcome::Completed(_)),
        "turn did not complete: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_local_client_saves_lists_and_removes_a_provider_key() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let agent = dir.path().join("agent");
        let server = gateway();
        let (conn, capabilities) = connect(&rt, None).await;
        assert_eq!(capabilities["host_providers"], json!(true));

        let saved = conn
            .request(
                methods::RUNTIME_PROVIDERS,
                login(server.base_url(), "k-first"),
            )
            .await
            .unwrap();
        assert_eq!(saved["providers"][0]["id"], json!("gateway"));
        assert_eq!(saved["providers"][0]["model_count"], json!(1));
        assert!(std::fs::read_to_string(auth_path(&agent))
            .unwrap()
            .contains("k-first"));
        let settings = std::fs::read_to_string(settings_path(&agent)).unwrap();
        assert!(settings.contains(r#""id": "gateway""#));
        assert!(!settings.contains("k-first"));

        let listed = conn
            .request(methods::RUNTIME_PROVIDERS, json!({"action": "list"}))
            .await
            .unwrap();
        assert!(connected_ids(&listed).contains(&"gateway".to_owned()));

        // A Session opened afterwards, in the same Runtime, uses the key.
        let ws = dir.path().join("ws");
        let session = rt
            .new_session(SessionSpec {
                cwd: Some(ws.clone()),
                options: Some(SessionOptions {
                    provider: Some("gateway".into()),
                    model: Some("m-one".into()),
                    ..SessionOptions::default()
                }),
                ..SessionSpec::default()
            })
            .await
            .unwrap();
        chat(&rt, &session).await;
        assert_eq!(last_chat_auth(&server).as_deref(), Some("Bearer k-first"));

        // A new key reaches the running Session on its next request.
        conn.request(
            methods::RUNTIME_PROVIDERS,
            login(server.base_url(), "k-second"),
        )
        .await
        .unwrap();
        chat(&rt, &session).await;
        assert_eq!(last_chat_auth(&server).as_deref(), Some("Bearer k-second"));

        let removed = conn
            .request(
                methods::RUNTIME_PROVIDERS,
                json!({"action": "disconnect", "provider": "gateway"}),
            )
            .await
            .unwrap();
        assert_eq!(removed["providers"][0]["connected"], json!(false));
        assert_eq!(removed["providers"][0]["name"], json!("Gateway"));
        let listed = conn
            .request(methods::RUNTIME_PROVIDERS, json!({"action": "list"}))
            .await
            .unwrap();
        assert!(!connected_ids(&listed).contains(&"gateway".to_owned()));
        assert!(!std::fs::read_to_string(auth_path(&agent))
            .unwrap()
            .contains("k-second"));
        assert!(!std::fs::read_to_string(settings_path(&agent))
            .unwrap()
            .contains("gateway"));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_caller_is_refused_runtime_providers() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let agent = dir.path().join("agent");
        let server = gateway();
        let principal =
            Principal::new(PrincipalKind::Person, "https://idp.example", "eve").unwrap();
        let callers = [
            Caller::local_token(loop_wire::local_principal()),
            Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap(),
        ];
        for caller in callers {
            let (conn, capabilities) = connect(&rt, Some(caller)).await;
            assert_ne!(capabilities["host_providers"], json!(true));
            for params in [
                json!({"action": "list"}),
                login(server.base_url(), "k-evil"),
            ] {
                let error = conn
                    .request(methods::RUNTIME_PROVIDERS, params)
                    .await
                    .unwrap_err();
                // Forbidden: a local token is not the host; an outside user
                // is refused before that by this unlocked Runtime.
                let text = format!("{error:?}");
                assert!(
                    text.contains(&format!(
                        "code: {}",
                        loop_protocol::rpc::error_codes::FORBIDDEN
                    )),
                    "{text}"
                );
            }
        }
        // Nothing was verified or saved.
        assert!(server.requests().is_empty());
        let auth = std::fs::read_to_string(auth_path(&agent)).unwrap_or_default();
        assert!(!auth.contains("k-evil"));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
