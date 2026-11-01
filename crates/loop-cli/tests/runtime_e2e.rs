#![cfg(unix)]
//! Runtime end to end: the real `loop serve` and `loop attach` binaries against
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
/// How long the model takes before asking for a tool when told `SLOW`.
const SLOW: Duration = Duration::from_secs(3);

async fn bounded<F: std::future::Future>(test: F) -> F::Output {
    tokio::time::timeout(TEST_LIMIT, test)
        .await
        .expect("test timed out")
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
/// `NEEDS-APPROVAL` asks for an approved `bash` call; anything else is echoed
/// as `reply <last word>`.
fn model(request: &RecordedRequest) -> FakeResponse {
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
                return text_reply("tool done");
            }
            let text = content_text(&last);
            if text.contains("NEEDS-APPROVAL") {
                let reply = approval_reply();
                return if text.contains("SLOW") {
                    reply.with_body_delay(SLOW)
                } else {
                    reply
                };
            }
            let word: String = text
                .split_whitespace()
                .last()
                .unwrap_or("")
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect();
            text_reply(&format!("reply {word}"))
        }
        _ => FakeResponse::new(404, "text/plain", "not found"),
    }
}

// ---- fixture ----------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    agent: PathBuf,
    work: PathBuf,
    socket: PathBuf,
    _model: FakeHttpServer,
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
        let model = FakeHttpServer::start(model);
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
            _dir: dir,
            _model: model,
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loop"));
        cmd.args(args)
            .env("LOOP_CODING_AGENT_DIR", &self.agent)
            .env("LOOP_CODING_AGENT_SESSION_DIR", self.agent.join("sessions"))
            .env_remove("LOOP_DEBUG")
            .env_remove("RUST_LOG")
            .current_dir(&self.work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
    async fn serve(&self) -> Proc {
        let mut serve = Proc::spawn(self.command(&["serve", "--permission-mode", "ask"]));
        // After `kill -9` the old socket file remains; wait for a real listener.
        let ready = async {
            loop {
                if let Some(status) = serve.child.try_wait().unwrap() {
                    panic!("loop serve exited early: {status}\n{}", serve.output());
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
    fn attach(&self, args: &[&str]) -> Proc {
        let mut all = vec!["attach"];
        all.extend_from_slice(args);
        Proc::spawn(self.command(&all))
    }
    /// The only Session's id, from `loop attach` listing.
    async fn only_session(&self) -> String {
        let listing = self.attach(&[]).finish().await;
        let rows: Vec<_> = listing.lines().skip(1).collect();
        assert_eq!(rows.len(), 1, "{listing}");
        rows[0].split('\t').next().unwrap().to_owned()
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
    /// Close stdin (detach for `loop attach`) and return the full output.
    async fn finish(mut self) -> String {
        self.stdin.take();
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("process did not exit")
            .unwrap();
        // Let the readers drain the closed pipes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(status.success(), "{status}\n{}", self.output());
        self.output()
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
            .expect("loop serve did not stop")
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
        let mut a = fx.attach(&["--new", "--cwd", work]);
        let mut b = fx.attach(&["--new", "--cwd", work]);
        a.line("say alpha").await;
        b.line("say beta").await;
        a.wait_text("reply alpha").await;
        b.wait_text("reply beta").await;
        let (a, b) = (a.finish().await, b.finish().await);
        assert!(!a.contains("reply beta"), "{a}");
        assert!(!b.contains("reply alpha"), "{b}");

        // Detaching does not close either Session.
        let listing = fx.attach(&[]).finish().await;
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
            let envelope = updates.recv().await.unwrap();
            if envelope.session == session.key {
                seen.push(envelope);
            }
        }
    })
    .await
    .expect("updates stopped early");
    seen
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
        let mut starter = fx.attach(&["--new", "--cwd", work]);
        starter.line("NEEDS-APPROVAL SLOW").await;
        starter.finish().await;
        let raised = |e: &EventKind| matches!(e, EventKind::EscalationRaised(_));
        assert_eq!(count(&fx.events().await, raised), 0, "raised too early");
        tokio::time::timeout(WAIT, async {
            while count(&fx.events().await, raised) == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Escalation never raised");

        // Both Surfaces receive the parked Escalation; one answers.
        let id = fx.only_session().await;
        let mut b = fx.attach(&[&id]);
        let c = fx.attach(&[&id]);
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
        let mut surface = fx.attach(&["--new", "--cwd", work]);
        surface.line("NEEDS-APPROVAL").await;
        surface.wait_text("Escalation:").await;
        let id = fx.only_session().await;

        serve.signal("-KILL");
        let mut surface = surface;
        surface.stdin.take();
        let status = tokio::time::timeout(WAIT, surface.child.wait())
            .await
            .expect("loop attach did not notice the lost Runtime")
            .unwrap();
        assert!(!status.success(), "{}", surface.output());
        assert!(surface.output().contains("Lost contact with Runtime"));
        drop(serve);

        let serve = fx.serve().await;
        serve
            .wait_text(
                "recovered 1 interrupted turn(s), withdrew 1 Escalation(s), reclaimed 1 lease(s)",
            )
            .await;

        let mut surface = fx.attach(&[&id]);
        surface.wait_text("uncertain — Runtime restarted").await;
        surface.line("say again").await;
        surface.wait_text("reply again").await;
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
