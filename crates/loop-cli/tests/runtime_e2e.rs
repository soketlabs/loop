//! Runtime end to end: the real `loop runtime start` and `loop session` binaries against
//! a fake OpenAI-compatible model, with isolated agent and Session directories.

use loop_protocol::{
    AgentBackend, ContentBlock, HostServices, InteractionOutcome, InteractionRequest, Origin,
    SessionRef, SessionSpec, SessionUpdate, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::log::{EventKind, EventLog, SqliteEventLog};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
};

const TEST_LIMIT: Duration = Duration::from_secs(120);
const WAIT: Duration = Duration::from_secs(30);
const PROVIDER_ENV: &[&str] = &[
    "LOOP_RUNTIME_TOKEN",
    "SOKET_API_KEY",
    "TENSORSTUDIO_API_KEY",
    "LOOP_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// Holds a `GATED` model reply until the test opens it.
#[derive(Default)]
struct Gate {
    /// (a gated request arrived, the gate is open)
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}
impl Gate {
    /// Called on the fake server's connection thread.
    fn pass(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let (_state, _) = self
            .changed
            .wait_timeout_while(state, TEST_LIMIT, |s| !s.1)
            .unwrap();
    }
    fn open(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
    async fn arrived(&self) {
        tokio::time::timeout(WAIT, async {
            while !self.state.lock().unwrap().0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("gated model request never arrived");
    }
}

async fn bounded<F: std::future::Future>(test: F) -> F::Output {
    tokio::time::timeout(TEST_LIMIT, test)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn websocket_clients_list_attach_and_cannot_call_host_methods() {
    bounded(async {
        use loop_protocol::rpc::methods;
        use loop_wire::{Connection, Incoming, RpcError};
        let fx = Fixture::new();
        let serve = fx.serve_with(&["--listen", "127.0.0.1:0"]).await;
        let local = fx.client().await;
        let status = serde_json::to_value(local.status().await.unwrap()).unwrap();
        let url = status["listen_url"].as_str().expect("WebSocket listen URL");
        let token_file = fx.agent.join("run/runtime.token");
        let token = std::fs::read_to_string(&token_file).unwrap();
        let address = url.strip_prefix("ws://").unwrap().trim_end_matches('/');
        for auth in [String::new(), "Authorization: Bearer wrong\r\n".to_owned()] {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream.write_all(format!("GET / HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{auth}\r\n").as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        }
        let remote = loop_wire::RemoteBackend::connect_websocket(url, token.trim(),
            loop_protocol::rpc::PeerInfo { name: "ws-library".into(), version: "0".into() }, Arc::new(Refuse)).await.unwrap();
        assert!(!remote.capabilities().host_defaults && !remote.capabilities().host_usage);
        let session = local.new_session(SessionSpec { cwd: Some(fx.work.clone()), ..Default::default() }).await.unwrap();
        let id = session.key.to_string();
        assert_eq!(remote.list().await.unwrap()[0].r#ref.key, session.key);
        remote.shutdown().await;
        let listing = fx.session(&["list", "--json", "--url", url, "--token-file", token_file.to_str().unwrap()]).finish().await;
        assert_eq!(sessions(&listing)[0].r#ref.key, session.key);
        let mut cmd = fx.command(&["session", "attach", &id, "--url", url]);
        cmd.env("LOOP_RUNTIME_TOKEN", token.trim());
        let mut surface = Proc::spawn(cmd);
        surface.wait_text("Attached").await;
        surface.line("hellows").await;
        surface.wait_text("reply hellows").await;
        let output = surface.finish().await;
        assert!(!output.contains(token.trim()));
        let replay = fx.session(&["attach", &id, "--url", url, "--token-file", token_file.to_str().unwrap()]).finish().await;
        assert!(replay.contains("reply hellows"), "{replay}");

        // Bypass client capability checks to prove enforcement at dispatch.
        let transport = loop_wire::socket::websocket_connect(url, token.trim()).await.unwrap();
        let (conn, mut incoming) = Connection::new(transport);
        let resolver = conn.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Response { id, result } = message {
                    resolver.resolve(id, result).await;
                }
            }
            resolver.fail_pending().await;
        });
        conn.request(methods::INITIALIZE, json!({"protocol_version":1,"client_info":{"name":"ws-test","version":"0"}})).await.unwrap();
        for method in methods::HOST_ONLY {
            assert!(matches!(conn.request(method, json!({})).await,
                Err(RpcError::Remote { code, .. }) if code == loop_protocol::rpc::error_codes::FORBIDDEN), "{method}");
        }
        for trusted in [true, false] {
            assert!(matches!(conn.request(methods::WORKSPACE_TRUST, json!({"workspace_id":"unknown","trusted":trusted})).await,
                Err(RpcError::Remote { code, .. }) if code == loop_protocol::rpc::error_codes::FORBIDDEN));
        }
        assert!(local.status().await.is_ok(), "remote stop must not shut down the Runtime");
        conn.close();
        serve.stop().await;
    }).await;
}

#[tokio::test]
async fn an_edge_gateway_names_the_user_and_each_token_opens_only_its_listener() {
    bounded(async {
        use loop_protocol::rpc::{methods, InitializeResult};
        use loop_protocol::{Caller, Principal, PrincipalKind, Tenant};
        use loop_wire::{Connection, Incoming};
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let fx = Fixture::new();
        let private = fx.home.join("edge");
        std::fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
        let token_file = private.join("edge.token");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_file)
            .unwrap();
        std::fs::write(&token_file, "edge-secret-token\n").unwrap();
        let serve = fx
            .serve_with(&[
                "--listen",
                "127.0.0.1:0",
                "--edge-listen",
                "127.0.0.1:0",
                "--edge-token-file",
                token_file.to_str().unwrap(),
            ])
            .await;
        serve.wait_text("Edge Gateway listener on ws://").await;
        let output = serve.output();
        let edge_url = output
            .split("Edge Gateway listener on ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap()
            .trim()
            .to_owned();
        let local = fx.client().await;
        let status = serde_json::to_value(local.status().await.unwrap()).unwrap();
        let user_url = status["listen_url"].as_str().unwrap().to_owned();
        let runtime_token = std::fs::read_to_string(fx.agent.join("run/runtime.token")).unwrap();

        // Each token opens only its own listener.
        let refused = loop_wire::socket::websocket_connect(&edge_url, runtime_token.trim()).await;
        assert_eq!(refused.err().map(|e| e.kind()), Some(std::io::ErrorKind::PermissionDenied));
        let refused = loop_wire::socket::websocket_connect(&user_url, "edge-secret-token").await;
        assert_eq!(refused.err().map(|e| e.kind()), Some(std::io::ErrorKind::PermissionDenied));

        // The gateway names a user; initialize reports that user.
        let principal =
            Principal::new(PrincipalKind::Person, "https://idp.example", "user-1").unwrap();
        let caller = Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap();
        let transport =
            loop_wire::socket::websocket_connect_edge(&edge_url, "edge-secret-token", &caller)
                .await
                .unwrap();
        let (conn, mut incoming) = Connection::new(transport);
        let resolver = conn.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Response { id, result } = message {
                    resolver.resolve(id, result).await;
                }
            }
            resolver.fail_pending().await;
        });
        let initialized = conn
            .request(
                methods::INITIALIZE,
                json!({"protocol_version": 1, "client_info": {"name": "gateway", "version": "0"},
                       "tenant": "local", "principal": {"kind": "person", "issuer": "local", "id": "uid:0"}}),
            )
            .await
            .unwrap();
        let result: InitializeResult = serde_json::from_value(initialized).unwrap();
        assert_eq!(result.principal.as_ref(), Some(caller.principal()));
        assert_eq!(result.tenant.as_ref(), Some(caller.tenant()));
        assert_eq!(result.via.unwrap().as_str(), "edge");
        conn.close();

        // The ordinary WebSocket is the local user via the token.
        let transport = loop_wire::socket::websocket_connect(&user_url, runtime_token.trim())
            .await
            .unwrap();
        let (conn, mut incoming) = Connection::new(transport);
        let resolver = conn.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Response { id, result } = message {
                    resolver.resolve(id, result).await;
                }
            }
            resolver.fail_pending().await;
        });
        let initialized = conn
            .request(
                methods::INITIALIZE,
                json!({"protocol_version": 1, "client_info": {"name": "ws", "version": "0"}}),
            )
            .await
            .unwrap();
        let result: InitializeResult = serde_json::from_value(initialized).unwrap();
        assert_eq!(result.principal, Some(loop_wire::local_principal()));
        assert_eq!(result.tenant, Some(Tenant::local()));
        assert_eq!(result.via.unwrap().as_str(), "token");
        conn.close();
        serve.stop().await;
    })
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_cannot_read_the_running_websocket_listeners_token() {
    bounded(async {
        use loop_agent::harness::sandbox::seatbelt::SeatbeltSandboxFactory;
        use loop_agent::harness::types::ShellExecOptions;
        use loop_agent::harness::{SandboxConfig, SandboxFactory};
        let fx = Fixture::new();
        let serve = fx.serve_with(&["--listen", "127.0.0.1:0"]).await;
        let token_file = fx.agent.join("run/runtime.token");
        let token = std::fs::read_to_string(&token_file).unwrap();
        let sandbox = SeatbeltSandboxFactory
            .create(SandboxConfig {
                workdir: fx.work.clone(),
                options: json!({"homeDir":fx.home,"agentDir":fx.agent,"allowWrite":[fx.agent]}),
                ..Default::default()
            })
            .await
            .unwrap();
        sandbox.start().await.unwrap();
        let command = format!(
            "cat '{}'",
            token_file.display().to_string().replace('\'', "'\\''")
        );
        let result = sandbox
            .env()
            .exec(
                &command,
                ShellExecOptions {
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_ne!(result.exit_code, 0, "Seatbelt allowed reading the token");
        assert!(!result.stdout.contains(token.trim()));
        sandbox.stop().await.unwrap();
        serve.stop().await;
    })
    .await;
}

// ---- fake model -------------------------------------------------------------

fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for delta_finish in chunks {
        body += &format!(
            "data: {}\n\n",
            json!({"id":"r","object":"chat.completion.chunk","choices":[delta_finish]})
        );
    }
    body + "data: [DONE]\n\n"
}
fn text_reply(text: &str) -> FakeResponse {
    FakeResponse::new(
        200,
        "text/event-stream",
        sse(&[
            json!({"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}),
            json!({"index":0,"delta":{},"finish_reason":"stop"}),
        ]),
    )
}
/// One assistant message calling `subagent` once per task, in parallel.
fn subagent_reply(tasks: &[&str]) -> FakeResponse {
    let calls: Vec<_> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            json!({"index":i,"id":format!("sub-{i}"),"type":"function",
                "function":{"name":"subagent","arguments":json!({"task":task}).to_string()}})
        })
        .collect();
    FakeResponse::new(
        200,
        "text/event-stream",
        sse(&[
            json!({"index":0,"delta":{"role":"assistant","tool_calls":calls},"finish_reason":null}),
            json!({"index":0,"delta":{},"finish_reason":"tool_calls"}),
        ]),
    )
}

/// Both children of `SPAWN2` reply only once the other has asked, so the test
/// fails unless they run at the same time.
static PAIR: [std::sync::atomic::AtomicBool; 2] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];
fn rendezvous(me: usize) -> bool {
    use std::sync::atomic::Ordering;
    PAIR[me].store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !PAIR[1 - me].load(Ordering::Acquire) {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

fn last_word(text: &str) -> String {
    text.split_whitespace()
        .last()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

fn approval_reply() -> FakeResponse {
    let call = json!({"index":0,"id":"call-1","type":"function",
        "function":{"name":"bash","arguments":"{\"command\":\"echo approved\"}"}});
    FakeResponse::new(
        200,
        "text/event-stream",
        sse(&[
            json!({"index":0,"delta":{"role":"assistant","tool_calls":[call]},"finish_reason":null}),
            json!({"index":0,"delta":{},"finish_reason":"tool_calls"}),
        ]),
    )
}
fn content_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}
/// Replies depend only on the request, so parallel Sessions cannot swap them.
/// `NEEDS-APPROVAL` asks for an approved `bash` call (held at `gate` when the
/// prompt also says `GATED`); anything else is echoed as `reply <last word>`.
fn model(request: &RecordedRequest, gate: &Gate) -> FakeResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/models") => FakeResponse::json(r#"{"data":[{"id":"fake-model"}]}"#),
        ("POST", "/v1/chat/completions") => {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let last = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .cloned()
                .unwrap_or_default();
            if last["role"] == "tool" {
                return text_reply(&format!("tool done: {}", last_word(&content_text(&last))));
            }
            let text = content_text(&last);
            // Subagent commands. A child's prompt is its task.
            if text.contains("SPAWN2") {
                return subagent_reply(&["PARA say kidA", "PARB say kidB"]);
            }
            if let Some(word) = text.strip_prefix("SPAWN1 ") {
                return subagent_reply(&[&format!("say {word}")]);
            }
            if text.contains("SPAWN-APPROVAL") {
                return subagent_reply(&["NEEDS-APPROVAL"]);
            }
            if text.contains("DEEP") {
                return subagent_reply(&["DEEP"]);
            }
            for (me, tag) in ["PARA", "PARB"].into_iter().enumerate() {
                if text.contains(tag) && !rendezvous(me) {
                    return text_reply("reply serial");
                }
            }
            if text.contains("STREAM-BASH") {
                let call = json!({"index":0,"id":"call-stream","type":"function",
                    "function":{"name":"bash","arguments":json!({
                        "command": "for i in $(seq 1 400); do echo line $i; done"
                    }).to_string()}});
                return FakeResponse::new(
                    200,
                    "text/event-stream",
                    sse(&[
                        json!({"index":0,"delta":{"role":"assistant","tool_calls":[call]},"finish_reason":null}),
                        json!({"index":0,"delta":{},"finish_reason":"tool_calls"}),
                    ]),
                );
            }
            if text.contains("NEEDS-APPROVAL") {
                if text.contains("GATED") {
                    gate.pass();
                }
                return approval_reply();
            }
            text_reply(&format!("reply {}", last_word(&text)))
        }
        _ => FakeResponse::new(404, "text/plain", "not found"),
    }
}

// ---- fixture ----------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    gate: Arc<Gate>,
    agent: PathBuf,
    work: PathBuf,
    socket: PathBuf,
    model: FakeHttpServer,
}
impl Fixture {
    fn new() -> Self {
        // Under /tmp so the default socket path fits the Unix limit.
        let dir = tempfile::Builder::new()
            .prefix("lrt")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = dir.path().join("agent");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let gate = Arc::new(Gate::default());
        let route_gate = gate.clone();
        let model = FakeHttpServer::start(move |r| model(r, &route_gate));
        std::fs::write(
            agent.join("models.json"),
            json!({"providers":[{"id":"fake","baseUrl":format!("{}/v1", model.base_url()),
                "models":["fake-model"]}]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({"defaultProvider":"fake","defaultModel":"fake-model"}).to_string(),
        )
        .unwrap();
        Self {
            socket: agent.join("run/runtime.sock"),
            agent,
            work,
            home,
            gate,
            _dir: dir,
            model,
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loop"));
        cmd.args(args)
            .env("LOOP_CODING_AGENT_DIR", &self.agent)
            .env("LOOP_CODING_AGENT_SESSION_DIR", self.agent.join("sessions"))
            .env("HOME", &self.home)
            .env_remove("LOOP_DEBUG")
            .env_remove("RUST_LOG")
            .current_dir(&self.work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in PROVIDER_ENV {
            cmd.env_remove(key);
        }
        cmd
    }
    async fn serve(&self) -> Proc {
        self.serve_with(&[]).await
    }
    async fn serve_with(&self, extra: &[&str]) -> Proc {
        let mut args = vec!["runtime", "start", "--permission-mode", "ask"];
        args.extend_from_slice(extra);
        let mut serve = Proc::spawn(self.command(&args));
        // After `kill -9` the old socket file remains; wait for a real listener.
        let ready = async {
            loop {
                if let Some(status) = serve.child.try_wait().unwrap() {
                    panic!(
                        "loop runtime start exited early: {status}\n{}",
                        serve.output()
                    );
                }
                if tokio::net::UnixStream::connect(&self.socket).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(WAIT, ready)
            .await
            .unwrap_or_else(|_| panic!("Runtime never listened\n{}", serve.output()));
        serve
    }
    fn session(&self, args: &[&str]) -> Proc {
        let mut all = vec!["session"];
        all.extend_from_slice(args);
        Proc::spawn(self.command(&all))
    }
    /// The only Session's id, from `loop session list --json`.
    async fn only_session(&self) -> String {
        let listing = self.session(&["list", "--json"]).finish().await;
        let sessions = sessions(&listing);
        assert_eq!(sessions.len(), 1, "{listing}");
        sessions[0].r#ref.key.to_string()
    }
    /// The id of the only Session without a parent.
    async fn only_parent(&self) -> String {
        let listing = self.session(&["list", "--json"]).finish().await;
        let roots: Vec<_> = sessions(&listing)
            .into_iter()
            .filter(|s| s.parent.is_none())
            .collect();
        assert_eq!(roots.len(), 1, "{listing}");
        roots[0].r#ref.key.to_string()
    }
    async fn client(&self) -> Arc<loop_wire::RemoteBackend> {
        let stream = tokio::net::UnixStream::connect(&self.socket).await.unwrap();
        loop_wire::RemoteBackend::connect(
            loop_wire::socket::unix(stream),
            loop_protocol::rpc::PeerInfo {
                name: "e2e".into(),
                version: "0".into(),
            },
            Arc::new(Refuse) as Arc<dyn HostServices>,
        )
        .await
        .unwrap()
    }
    /// A read-only view of the Runtime's Event Log.
    async fn events(&self) -> Vec<loop_runtime::log::LoggedEvent> {
        SqliteEventLog::open(&self.agent)
            .await
            .unwrap()
            .read_runtime_after(0)
            .await
            .unwrap()
    }
}

struct Refuse;
#[async_trait::async_trait]
impl HostServices for Refuse {
    async fn interact(&self, _: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Rejected {
            reason: "test client does not answer".into(),
        }
    }
}

/// A child process whose stdout and stderr are captured together.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    out: Arc<Mutex<String>>,
}
impl Proc {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd.spawn().unwrap();
        let out = Arc::new(Mutex::new(String::new()));
        for mut stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let out = out.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    out.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }
        Self {
            stdin: child.stdin.take(),
            child,
            out,
        }
    }
    fn output(&self) -> String {
        self.out.lock().unwrap().clone()
    }
    async fn wait_text(&self, needle: &str) {
        let found = tokio::time::timeout(WAIT, async {
            while !self.output().contains(needle) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(found.is_ok(), "never saw {needle:?} in:\n{}", self.output());
    }
    async fn line(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("{text}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }
    /// Close stdin (detach for `loop session attach`) and return the full output.
    async fn finish(self) -> String {
        let (status, output) = self.exit().await;
        assert!(status.success(), "{status}\n{output}");
        output
    }
    /// Close stdin and return the exit status and the full output.
    async fn exit(mut self) -> (std::process::ExitStatus, String) {
        self.stdin.take();
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("process did not exit")
            .unwrap();
        // Let the readers drain the closed pipes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        (status, self.output())
    }
    fn signal(&self, name: &str) {
        assert!(std::process::Command::new("kill")
            .args([name, &self.child.id().unwrap().to_string()])
            .status()
            .unwrap()
            .success());
    }
    async fn stop(mut self) {
        self.signal("-TERM");
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("loop runtime start did not stop")
            .unwrap();
        assert!(status.success(), "{status}\n{}", self.output());
    }
}

fn count(events: &[loop_runtime::log::LoggedEvent], pick: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|e| pick(&e.event.kind)).count()
}
fn restarted_end(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Update(e) if matches!(e.update,
        SessionUpdate::TurnEnded { stop: loop_protocol::StopReason::RuntimeRestarted, .. }))
}

// ---- tests ------------------------------------------------------------------

#[tokio::test]
async fn two_clients_drive_two_sessions_independently() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut a = fx.session(&["new", "--cwd", work]);
        let mut b = fx.session(&["new", "--cwd", work]);
        a.line("say alpha").await;
        b.line("say beta").await;
        a.wait_text("reply alpha").await;
        b.wait_text("reply beta").await;
        let (a, b) = (a.finish().await, b.finish().await);
        assert!(!a.contains("reply beta"), "{a}");
        assert!(!b.contains("reply alpha"), "{b}");

        // Detaching does not close either Session.
        let listing = fx.session(&["list"]).finish().await;
        assert_eq!(listing.lines().count(), 3, "{listing}");
        serve.stop().await;
    })
    .await
}

async fn session_updates(
    updates: &mut tokio::sync::broadcast::Receiver<UpdateEnvelope>,
    session: &SessionRef,
    through: u64,
) -> Vec<UpdateEnvelope> {
    let mut seen: Vec<UpdateEnvelope> = Vec::new();
    tokio::time::timeout(WAIT, async {
        while seen.last().is_none_or(|e| e.seq < through) {
            let envelope = match updates.recv().await {
                Ok(envelope) => envelope,
                Err(error) => panic!("update stream failed after {} updates: {error}", seen.len()),
            };
            if envelope.session == session.key {
                seen.push(envelope);
            }
        }
    })
    .await
    .expect("updates stopped early");
    seen
}

/// A wire client never sees progress for a bash call after its completed
/// update, so a Surface cannot show the finished call as running again.
#[tokio::test]
async fn no_tool_update_reaches_a_client_after_the_call_completed() {
    bounded(async {
        let fx = Fixture::new();
        // Bash runs without an approval in this test.
        std::fs::write(
            fx.agent.join("settings.json"),
            json!({"defaultProvider":"fake","defaultModel":"fake-model",
                "toolPermissions":{"bash":"allow"}})
            .to_string(),
        )
        .unwrap();
        let serve = fx.serve().await;
        let client = fx.client().await;
        let mut updates = client.subscribe();
        let session = client
            .new_session(SessionSpec {
                cwd: Some(fx.work.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let turn = client
            .prompt(&session, vec![ContentBlock::text("STREAM-BASH")])
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        let latest = client.attach(&session, 0).await.unwrap().latest_seq;
        let seen = session_updates(&mut updates, &session, latest).await;
        let mut completed = false;
        for envelope in &seen {
            if let SessionUpdate::ToolCallUpdate(update) = &envelope.update {
                if update.id.as_str() != "call-stream" {
                    continue;
                }
                assert!(!completed, "an update after the completed one: {update:?}");
                completed = matches!(
                    update.status,
                    Some(loop_protocol::ToolCallStatus::Completed)
                );
            }
        }
        assert!(completed, "the bash call never completed: {seen:?}");
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn reattach_from_a_saved_cursor_replays_exactly_what_was_missed() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;

        // The first client runs a turn, keeps its cursor and detaches.
        let first = fx.client().await;
        let mut updates = first.subscribe();
        let session = first
            .new_session(SessionSpec {
                cwd: Some(fx.work.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let turn = first
            .prompt(&session, vec![ContentBlock::text("say first")])
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        let cursor = first.attach(&session, 0).await.unwrap().latest_seq;
        assert!(cursor > 0);
        let seen = session_updates(&mut updates, &session, cursor).await;
        let seqs: Vec<_> = seen.iter().map(|e| e.seq).collect();
        assert!(seqs.windows(2).all(|w| w[1] > w[0]), "{seqs:?}");
        first.detach(&session).await.unwrap();

        // Another client runs a turn while the first is away.
        let second = fx.client().await;
        assert_eq!(
            second.attach(&session, cursor).await.unwrap().latest_seq,
            cursor
        );
        let turn = second
            .prompt(&session, vec![ContentBlock::text("say second")])
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        second.detach(&session).await.unwrap();

        // Reattaching from the cursor replays every missed update once, in order.
        let mut updates = first.subscribe();
        let latest = first.attach(&session, cursor).await.unwrap().latest_seq;
        assert!(latest > cursor);
        let replay = session_updates(&mut updates, &session, latest).await;
        let seqs: Vec<_> = replay.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (cursor + 1..=latest).collect::<Vec<_>>());
        assert!(replay.iter().all(|e| e.origin == Origin::Replay));
        let text: String = replay
            .iter()
            .filter_map(|e| loop_cli::print_mode::agent_text_chunk(&e.update))
            .collect();
        assert!(text.contains("reply second"), "{text}");
        // Nothing is delivered twice.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(extra) = updates.try_recv() {
            assert_ne!(extra.session, session.key, "duplicate {extra:?}");
        }
        drop((first, second));
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn approval_parked_without_clients_is_answered_by_one_of_two_surfaces() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();

        // Start a turn, then detach before the model asks for approval.
        let mut starter = fx.session(&["new", "--cwd", work]);
        starter.line("NEEDS-APPROVAL GATED").await;
        fx.gate.arrived().await;
        starter.finish().await;
        let raised = |e: &EventKind| matches!(e, EventKind::EscalationRaised(_));
        assert_eq!(count(&fx.events().await, raised), 0, "raised too early");
        fx.gate.open();
        tokio::time::timeout(WAIT, async {
            while count(&fx.events().await, raised) == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Escalation never raised");

        // Both Surfaces receive the parked Escalation; one answers.
        let id = fx.only_session().await;
        let mut b = fx.session(&["attach", &id]);
        let c = fx.session(&["attach", &id]);
        b.wait_text("Escalation:").await;
        c.wait_text("Escalation:").await;
        b.line("1").await;
        b.wait_text("tool done").await;
        c.wait_text("Escalation withdrawn").await;
        c.wait_text("tool done").await;
        let (b, _) = (b.finish().await, c.finish().await);
        assert!(!b.contains("Escalation withdrawn"), "{b}");

        let events = fx.events().await;
        let answers: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event.kind {
                EventKind::EscalationAnswered(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert_eq!(answers[0]["answer"]["native_id"], "accept", "{answers:?}");
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn kill_9_mid_turn_then_restart_ends_the_turn_uncertain_and_the_session_works() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();

        // Mid-turn: the tool call waits for approval.
        let mut surface = fx.session(&["new", "--cwd", work]);
        surface.line("NEEDS-APPROVAL").await;
        surface.wait_text("Escalation:").await;
        let id = fx.only_session().await;

        serve.signal("-KILL");
        let mut surface = surface;
        surface.stdin.take();
        let status = tokio::time::timeout(WAIT, surface.child.wait())
            .await
            .expect("loop session attach did not notice the lost Runtime")
            .unwrap();
        assert!(!status.success(), "{}", surface.output());
        // The reader tasks can still hold the last bytes after the exit.
        surface.wait_text("Lost contact with Runtime").await;
        drop(serve);

        let serve = fx.serve().await;
        serve
            .wait_text(
                "recovered 1 interrupted turn(s), withdrew 1 Escalation(s), reclaimed 1 lease(s)",
            )
            .await;

        let mut surface = fx.session(&["attach", &id]);
        surface.wait_text("uncertain — Runtime restarted").await;
        surface.line("say again").await;
        surface.wait_text("reply again").await;
        // The interrupted tool call must not reach the model unanswered;
        // OpenAI-compatible APIs reject such a history.
        let last = fx
            .model
            .requests()
            .into_iter()
            .rfind(|r| r.path == "/v1/chat/completions")
            .unwrap();
        let body: Value = serde_json::from_slice(&last.body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let calls: Vec<_> = messages
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
            .map(|c| c["id"].clone())
            .collect();
        let answered: Vec<_> = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].clone())
            .collect();
        assert!(
            calls.iter().all(|c| answered.contains(c)),
            "unanswered tool call in {messages:#?}"
        );
        if calls.contains(&json!("call-1")) {
            // The interrupted call is reported as such, never as an empty success.
            let result = messages
                .iter()
                .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call-1")
                .unwrap();
            assert!(
                content_text(result).contains("may or may not have run"),
                "{result:#}"
            );
        } else {
            // Otherwise the Session tree holds none of the interrupted turn
            // (design §12: the tree may lack it until it projects the log).
            assert!(
                !messages
                    .iter()
                    .any(|m| content_text(m).contains("NEEDS-APPROVAL")),
                "half an interrupted turn reached the model: {messages:#?}"
            );
        }
        let output = surface.finish().await;
        assert!(!output.contains("Escalation:"), "stale approval replayed: {output}");

        let events = fx.events().await;
        assert_eq!(count(&events, restarted_end), 1);
        assert_eq!(
            count(&events, |e| matches!(e, EventKind::EscalationWithdrawn(p) if p["reason"] == "runtime_restarted")),
            1
        );
        serve.stop().await;

        // A clean restart recovers nothing more.
        let serve = fx.serve().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!serve.output().contains("recovered"), "{}", serve.output());
        assert_eq!(count(&fx.events().await, restarted_end), 1);
        serve.stop().await;
    })
    .await
}

// ---- Subagents -------------------------------------------------------------

fn payloads(
    events: &[loop_runtime::log::LoggedEvent],
    pick: impl Fn(&EventKind) -> Option<&Value>,
) -> Vec<(Option<String>, Value)> {
    events
        .iter()
        .filter_map(|e| {
            pick(&e.event.kind)
                .map(|p| (e.event.session_key.as_ref().map(|k| k.0.clone()), p.clone()))
        })
        .collect()
}
fn started_children(events: &[loop_runtime::log::LoggedEvent]) -> Vec<(Option<String>, Value)> {
    payloads(events, |k| match k {
        EventKind::SubagentStarted(p) => Some(p),
        _ => None,
    })
}
fn ended_children(events: &[loop_runtime::log::LoggedEvent]) -> Vec<(Option<String>, Value)> {
    payloads(events, |k| match k {
        EventKind::SubagentEnded(p) => Some(p),
        _ => None,
    })
}
/// Some model request carried every one of these tool results together.
fn tool_results_seen_together(model: &FakeHttpServer, needles: &[&str]) -> bool {
    model.requests().iter().any(|r| {
        let body: Value = serde_json::from_slice(&r.body).unwrap_or_default();
        let results: Vec<String> = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "tool")
            .map(content_text)
            .collect();
        needles
            .iter()
            .all(|n| results.iter().any(|r| r.contains(n)))
    })
}
async fn wait_events(
    fx: &Fixture,
    what: &str,
    done: impl Fn(&[loop_runtime::log::LoggedEvent]) -> bool,
) {
    tokio::time::timeout(WAIT, async {
        while !done(&fx.events().await) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {what}"));
}
/// Sessions from `loop session list --json`.
fn sessions(listing: &str) -> Vec<loop_protocol::rpc::SessionInfo> {
    serde_json::from_str(listing).unwrap_or_else(|e| panic!("{e}: {listing}"))
}
/// Rows of the human listing: lines between the header and the first blank.
fn listed(listing: &str) -> usize {
    listing
        .lines()
        .skip(1)
        .take_while(|l| !l.is_empty())
        .count()
}

#[tokio::test]
async fn a_parent_runs_two_subagents_in_parallel_and_gets_both_answers() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN2").await;
        parent.wait_text("tool done").await;
        parent.finish().await;

        let events = fx.events().await;
        assert_eq!(started_children(&events).len(), 2);
        let ended = ended_children(&events);
        assert_eq!(ended.len(), 2, "{ended:?}");
        assert!(
            ended.iter().all(|(_, p)| p["status"] == "completed"),
            "{ended:?}"
        );
        // Each child waited for the other, so they ran at the same time.
        assert!(
            tool_results_seen_together(&fx.model, &["reply kidA", "reply kidB"]),
            "the parent never received both answers"
        );
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn finished_children_are_hidden_unless_all_and_stay_attachable() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN1 kid").await;
        parent.wait_text("tool done: kid").await;
        parent.finish().await;

        // A finished child is closed: hidden unless asked for.
        let listing = fx.session(&["list"]).finish().await;
        assert_eq!(listed(&listing), 1, "{listing}");
        assert!(
            listing.contains("1 more; `loop session list --all`"),
            "{listing}"
        );
        let listing = fx.session(&["list", "--all"]).finish().await;
        assert_eq!(listed(&listing), 2, "{listing}");
        let listing = fx.session(&["list", "--all", "--json"]).finish().await;
        let all = sessions(&listing);
        assert_eq!(all.len(), 2, "{listing}");
        let root = all.iter().find(|s| s.parent.is_none()).expect("root");
        let child = all.iter().find(|s| s.parent.is_some()).expect("child");
        assert_eq!(child.parent.as_ref(), Some(&root.r#ref), "{listing}");
        assert!(!child.live, "{listing}");
        // The child's log stays; attaching replays its answer.
        let output = fx
            .session(&["attach", child.r#ref.key.as_str()])
            .finish()
            .await;
        assert!(
            output.starts_with("Replaying finished Subagent Session"),
            "{output}"
        );
        assert!(output.contains("reply kid"), "{output}");
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn cancelling_the_parent_cascades_and_withdraws_a_parked_child_escalation() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN-APPROVAL").await;
        let raised = |e: &EventKind| matches!(e, EventKind::EscalationRaised(_));
        wait_events(&fx, "the child's Escalation", |e| count(e, raised) == 1).await;
        let child = started_children(&fx.events().await)[0].1["child"]
            .as_str()
            .unwrap()
            .to_owned();
        // The approval belongs to the child Session; the parent's Surface sees it.
        let events = fx.events().await;
        let escalation = events.iter().find(|e| raised(&e.event.kind)).unwrap();
        assert_eq!(escalation.event.session_key.as_ref().unwrap().0, child);

        parent.signal("-INT");
        parent.wait_text("Turn cancelled").await;
        wait_events(&fx, "the cascade", |e| {
            count(e, |k| matches!(k, EventKind::EscalationWithdrawn(_))) == 1
                && ended_children(e).len() == 1
        })
        .await;
        let events = fx.events().await;
        let withdrawn = events
            .iter()
            .find(|e| matches!(e.event.kind, EventKind::EscalationWithdrawn(_)))
            .unwrap();
        assert_eq!(withdrawn.event.session_key.as_ref().unwrap().0, child);
        assert_eq!(ended_children(&events)[0].1["status"], "cancelled");
        assert_eq!(
            count(&events, |e| matches!(e, EventKind::EscalationAnswered(_))),
            0
        );
        parent.finish().await;

        // The cancelled child was closed.
        let client = fx.client().await;
        let sessions = client.list().await.unwrap();
        let child = sessions.iter().find(|s| s.r#ref.key.0 == child).unwrap();
        assert!(!child.live && child.parent.is_some(), "{child:?}");
        drop(client);
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn a_surface_watching_the_parent_answers_its_childs_escalation() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN-APPROVAL").await;
        parent.wait_text("Escalation from Subagent ").await;
        // The task arrives once the child's name is looked up.
        parent.wait_text("(NEEDS-APPROVAL)").await;
        parent.wait_text("1. ").await;
        parent.line("1").await;
        parent.wait_text("tool done: approved").await;
        let events = fx.events().await;
        let ended = ended_children(&events);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0].1["status"], "completed", "{ended:?}");
        assert_eq!(
            count(&events, |e| matches!(e, EventKind::EscalationAnswered(_))),
            1
        );
        parent.finish().await;
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn kill_9_while_a_child_waits_ends_parent_and_child_uncertain_without_retrying() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN-APPROVAL").await;
        let raised = |e: &EventKind| matches!(e, EventKind::EscalationRaised(_));
        wait_events(&fx, "the child's Escalation", |e| count(e, raised) == 1).await;
        let listing = fx.session(&["list", "--json"]).finish().await;
        let root = sessions(&listing)
            .into_iter()
            .find(|s| s.parent.is_none())
            .unwrap()
            .r#ref
            .key
            .to_string();
        let chat_calls = || {
            fx.model
                .requests()
                .iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count()
        };
        let model_calls = chat_calls();

        serve.signal("-KILL");
        parent.stdin.take();
        let _ = tokio::time::timeout(WAIT, parent.child.wait()).await;
        drop(serve);
        let serve = fx.serve().await;
        serve
            .wait_text(
                "recovered 2 interrupted turn(s), withdrew 1 Escalation(s), reclaimed 2 lease(s)",
            )
            .await;

        let events = fx.events().await;
        let ended: std::collections::HashSet<_> = events
            .iter()
            .filter(|e| restarted_end(&e.event.kind))
            .map(|e| e.event.session_key.clone())
            .collect();
        assert_eq!(ended.len(), 2, "parent and child each end once");
        let surface = fx.session(&["attach", &root]);
        surface.wait_text("uncertain — Runtime restarted").await;
        surface.finish().await;
        // Nothing was retried: no prompt reached the model again.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(chat_calls(), model_calls);
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn a_finished_child_frees_its_session_slot() {
    bounded(async {
        let fx = Fixture::new();
        // Room for the parent and one child only.
        let serve = fx.serve_with(&["--max-sessions", "2"]).await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN1 one").await;
        parent.wait_text("tool done: one").await;
        let first = parent.finish().await;
        let root = fx.only_parent().await;
        let client = fx.client().await;
        tokio::time::timeout(WAIT, async {
            while client.list().await.unwrap().iter().any(|s| s.busy) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("parent turn never ended");
        // A second child fits only if the first one's slot was freed.
        let mut parent = fx.session(&["attach", &root]);
        parent.line("SPAWN1 two").await;
        parent.wait_text("tool done: two").await;
        let output = first + parent.finish().await.as_str();
        assert!(!output.contains("limit"), "{output}");

        let ended = ended_children(&fx.events().await);
        assert_eq!(ended.len(), 2, "{ended:?}");
        assert!(ended.iter().all(|(_, p)| p["status"] == "completed"
            && p["closed"].is_null()
            && p["closed_ok"] == true));
        let client = fx.client().await;
        let live = client
            .list()
            .await
            .unwrap()
            .iter()
            .filter(|s| s.live)
            .count();
        assert_eq!(live, 1, "only the parent stays loaded");
        drop(client);
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn the_subagent_depth_limit_is_enforced_through_the_binary() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        // Every level asks for another child; depth 2 is refused.
        parent.line("DEEP").await;
        parent.wait_text("tool done").await;
        parent.finish().await;

        let started = started_children(&fx.events().await);
        let depths: Vec<_> = started.iter().map(|(_, p)| p["depth"].clone()).collect();
        assert_eq!(depths, [json!(1), json!(2)], "{started:?}");
        // Each level's first model call: the depth-2 child is not offered
        // `subagent`, so its call fails and it answers without a child.
        let offers: Vec<bool> = fx
            .model
            .requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .filter_map(|r| {
                let body: Value = serde_json::from_slice(&r.body).ok()?;
                (body["messages"].as_array()?.len() == 2).then(|| {
                    body["tools"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|t| t["function"]["name"] == "subagent")
                })
            })
            .collect();
        assert_eq!(offers, [true, true, false], "{offers:?}");
        assert!(tool_results_seen_together(
            &fx.model,
            &["subagent not found"]
        ));
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn closing_a_parent_with_a_running_subagent_needs_force_and_cascades() {
    bounded(async {
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let work = fx.work.to_str().unwrap();
        let mut parent = fx.session(&["new", "--cwd", work]);
        parent.line("SPAWN-APPROVAL").await;
        let raised = |e: &EventKind| matches!(e, EventKind::EscalationRaised(_));
        wait_events(&fx, "the child's Escalation", |e| count(e, raised) == 1).await;
        let parent_id = fx.only_parent().await;

        let (status, refused) = fx.session(&["close", &parent_id]).exit().await;
        assert!(!status.success(), "{refused}");
        assert!(
            refused.contains("busy") && refused.contains("1 running Subagent"),
            "{refused}"
        );
        assert!(refused.contains("--force"), "{refused}");
        let client = fx.client().await;
        assert!(client.list().await.unwrap().iter().all(|s| s.live));

        let closed = fx.session(&["close", &parent_id, "--force"]).finish().await;
        assert!(closed.contains("Closed Session"), "{closed}");
        wait_events(&fx, "the cascade", |e| ended_children(e).len() == 1).await;
        let sessions = client.list().await.unwrap();
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        assert!(sessions.iter().all(|s| !s.live), "{sessions:?}");
        assert_eq!(client.status().await.unwrap().loaded, 0);
        drop(client);
        parent.finish().await;
        serve.stop().await;
    })
    .await
}

#[tokio::test]
async fn the_daemon_serves_workspace_admin_and_host_only_methods_on_its_socket() {
    bounded(async {
        use loop_protocol::admin::*;
        use loop_wire::RuntimeAdmin;
        let fx = Fixture::new();
        let serve = fx.serve().await;
        let client = fx.client().await;
        let offered = client.capabilities();
        assert!(offered.workspace_reload && offered.workspace_skills && offered.workspace_trust);
        assert!(offered.permission_mode && offered.session_sandbox);
        // The 0600 Unix socket is host-local: host-only methods are offered.
        assert!(offered.host_defaults);
        // The usage history is the `usage` feature's: without it the daemon does
        // not offer it, even host-locally.
        assert_eq!(offered.host_usage, cfg!(feature = "usage"));

        let session = client
            .new_session(loop_protocol::SessionSpec {
                cwd: Some(fx.work.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let workspace_id = client
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.r#ref.key == session.key)
            .and_then(|s| s.summary.workspace_id)
            .expect("the listing names the workspace");
        let reloaded = client
            .reload(ReloadParams {
                workspace_id: workspace_id.clone(),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(reloaded.revision, 1);
        assert_eq!(reloaded.pending_sessions, 0);
        assert!(offered.runtime_sessions);
        let panel = client.sessions().await.unwrap();
        assert_eq!(panel.totals.sessions, 1);
        assert_eq!(panel.sessions[0].workspace_id, workspace_id);

        let saved = client
            .defaults(DefaultsParams {
                patch: Some(DefaultsPatch {
                    response_header_timeout_ms: Some(45_000),
                    ..DefaultsPatch::default()
                }),
                ..DefaultsParams::default()
            })
            .await
            .unwrap();
        assert_eq!(saved.response_header_timeout_ms, Some(45_000));
        let settings = std::fs::read_to_string(fx.agent.join("settings.json")).unwrap();
        assert!(settings.contains("45000"), "{settings}");
        #[cfg(feature = "usage")]
        {
            let usage = client.usage(UsageParams::default()).await.unwrap();
            assert!(usage.next_cursor.is_none());
        }
        serve.stop().await;
    })
    .await
}
