//! E10: isolation between two outside users on one Runtime (task I3).
//!
//! One tool-locked Runtime; Alice and Bob are Edge Gateway users of one
//! tenant, each on a connection of their own. Alice works in her Session.
//! Bob drives hostile sequences against it over the wire: every Session
//! method with her Session's reference, a replay of her copy request ids,
//! her workspace id, her folder (and the folders above and inside it), a
//! cancel while her turn runs. Then the probe looks for leaks:
//!
//! - an answer that is not a refusal;
//! - a refusal that differs from the refusal for a Session that does not
//!   exist (code, message and data, with the key masked);
//! - anything Bob received (updates, requests, answers) that names her
//!   Session, her text or her folder;
//! - her turn cancelled by Bob; her Session in his lists or counts.
//!
//! Run: `cargo test -p loop-runtime --test e10_isolation -- --ignored --nocapture`.
//! The result line starts with `E10|RESULT|`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::{methods, PeerInfo};
use loop_protocol::{Caller, Principal, PrincipalKind, Tenant};
use loop_runtime::access::{Rule, METHODS};
use loop_runtime::log::MemoryEventLog;
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_caller, ServeOptions};
use loop_wire::transport::memory;
use loop_wire::{Authority, Connection, Incoming, RpcError, RuntimeControl};
use serde_json::{json, Value};

const SECRET: &str = "alice-secret-7f3a";

fn say(line: String) {
    println!("E10|{line}");
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
        // Web tools on and `ask`: a fetch raises an Escalation (approval).
        no_web: false,
        permission_mode: Some("ask".into()),
        agent: None,
    }
}

async fn runtime(dir: &Path) -> Arc<Runtime> {
    use loop_ai::providers::faux::{FauxResponder, FauxResponse};
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    let responder: FauxResponder = Arc::new(move |context: &loop_ai::Context| {
        let text = format!("{:?}", context.messages);
        let answered = context
            .messages
            .iter()
            .any(|m| matches!(m, loop_ai::Message::ToolResult(_)));
        Box::pin(async move {
            // `FETCH`: one web fetch, which asks for approval first.
            if text.contains("FETCH") && !answered {
                return FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
                    id: "call-fetch".into(),
                    name: "web_fetch".into(),
                    arguments: json!({"url": "https://example.invalid/e10"}),
                    thought_signature: None,
                }]);
            }
            if text.contains("SLOW") {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            // The answer repeats the secret, so it is in her updates.
            FauxResponse::Text(format!("noted {SECRET}"))
        })
    });
    models.set_provider(loop_ai::providers::faux_provider_fn(responder));
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The host's settings ask before every web fetch: an Escalation.
    std::fs::create_dir_all(dir.join("agent")).unwrap();
    std::fs::write(
        dir.join("agent/settings.json"),
        json!({"toolPermissions": {"web_fetch": "ask"}}).to_string(),
    )
    .unwrap();
    let globals = GlobalResources::with_models(dir.join("agent"), models)
        .unwrap()
        .with_tool_lock(Some(
            loop_app_core::permissions::tool_lock::RuntimeToolLock::new(
                loop_protocol::ToolLock::Chat,
                &ws,
            )
            .unwrap(),
        ));
    let store = globals.open_runtime_session_store().unwrap();
    Runtime::new(
        globals,
        opts(&ws),
        Arc::new(MemoryEventLog::new()),
        store,
        RuntimeLimits {
            max_sessions: 32,
            max_active_turns: 8,
        },
    )
    .await
    .unwrap()
    .into_shared()
}

fn edge(id: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

struct Client {
    conn: Arc<Connection>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    /// Requests from the server (`host/interact`), to answer.
    asked:
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(loop_wire::PeerRequestId, Value)>>,
}

async fn connect(rt: &Arc<Runtime>, caller: Caller) -> Client {
    let (server, client_end) = memory();
    let control: Arc<dyn RuntimeControl> = rt.clone();
    let _ = serve_with_caller(
        rt.clone(),
        Some(control),
        server,
        PeerInfo {
            name: "e10".into(),
            version: "0".into(),
        },
        ServeOptions {
            owns_backend: false,
            authority: Authority::Remote,
        },
        caller,
    );
    let (conn, mut incoming) = Connection::new(client_end);
    let resolver = Arc::clone(&conn);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let (ask, asked) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Response { id, result } => resolver.resolve(id, result).await,
                Incoming::Notification { method, params } => {
                    sink.lock().unwrap().push(format!("{method} {params}"))
                }
                Incoming::Request { id, method, params } => {
                    sink.lock().unwrap().push(format!("{method} {params}"));
                    let _ = ask.send((id, params));
                }
            }
        }
    });
    conn.request(
        methods::INITIALIZE,
        json!({"protocol_version": 1, "client_info": {"name": "e10", "version": "0"},
               "client_capabilities": {"interact": true, "branch_reset": true}}),
    )
    .await
    .unwrap();
    Client {
        conn,
        seen,
        asked: tokio::sync::Mutex::new(asked),
    }
}

fn folder(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join("ws").join(name);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

/// Params for `method` aimed at `session`, with the ids Bob may replay.
fn params(method: &str, session: &Value, workspace: &str, request_id: &str) -> Value {
    match method {
        methods::SESSION_LOAD => json!({"session": session, "spec": {}}),
        methods::SESSION_ATTACH => json!({"session": session, "after_seq": 0}),
        methods::SESSION_PROMPT => {
            json!({"session": session, "content": [{"type": "text", "text": "hi"}]})
        }
        methods::SESSION_SKILL => json!({"session": session, "name": "x", "active": true}),
        methods::SESSION_WORKFLOW => {
            json!({"session": session, "goal": "x", "request_id": request_id})
        }
        methods::SESSION_VIEW => json!({"session": session, "view_id": "x"}),
        methods::SESSION_NAVIGATE => json!({"session": session, "summarize": false}),
        methods::SESSION_RENAME => json!({"session": session, "name": "pwned"}),
        methods::SESSION_FORK => {
            json!({"session": session, "before_entry_id": "x", "request_id": request_id})
        }
        methods::SESSION_CLONE => json!({"session": session, "request_id": request_id}),
        methods::SESSION_SET_MODEL => json!({"session": session, "model": "faux/x"}),
        methods::SESSION_SET_EFFORT => json!({"session": session, "effort": "high"}),
        methods::SESSION_SET_MODE => json!({"session": session, "mode": "x"}),
        methods::SESSION_TTFB => json!({"session": session, "timeout_ms": 5}),
        methods::SESSION_SHELL => json!({"session": session, "command": "id"}),
        methods::SESSION_SANDBOX => json!({"session": session, "action": "status"}),
        methods::SESSION_PERMISSION_MODE => json!({"session": session, "mode": "full-access"}),
        methods::SESSION_IMPORT => {
            json!({"workspace_id": workspace, "jsonl": "", "request_id": request_id})
        }
        m if m.starts_with("workspace/") => json!({"workspace_id": workspace, "action": "list"}),
        _ => json!({"session": session}),
    }
}

/// An error as Bob sees it, with `key` masked.
fn masked(error: &RpcError, key: &str) -> String {
    format!("{error:?}").replace(key, "<KEY>")
}

/// What one probe run found.
struct Found {
    attempts: u64,
    refused: u64,
    checks: u64,
    leaks: Vec<String>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e10_isolation() {
    let started = std::time::Instant::now();
    let found = probe("bob").await;
    say(format!(
        "RESULT|attempts={}|refused={}|access_refusals={}|leaks={}|wall={}ms",
        found.attempts,
        found.refused,
        found.checks,
        found.leaks.len(),
        started.elapsed().as_millis()
    ));
    for leak in &found.leaks {
        say(format!("LEAK|{leak}"));
    }
    assert!(found.leaks.is_empty(), "{} leaks", found.leaks.len());
    assert_eq!(found.attempts, found.refused);
}

/// The probe must be able to fail: with Alice herself as the attacker,
/// every attempt reaches her Session, and the probe reports it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e10_negative_control() {
    let found = probe("alice").await;
    say(format!(
        "CONTROL|attacker is the owner: attempts={}|leaks={} (a working probe reports many)",
        found.attempts,
        found.leaks.len()
    ));
    assert!(
        found.leaks.len() as u64 > found.attempts / 2,
        "{:?}",
        found.leaks
    );
}

/// Alice works; `attacker` (Bob, or Alice herself for the control) tries
/// her Session.
async fn probe(attacker: &str) -> Found {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path()).await;
    let alice = connect(&rt, edge("alice")).await;
    let bob = connect(&rt, edge(attacker)).await;
    let alice_folder = folder(dir.path(), "alice");
    let bob_folder = folder(dir.path(), "bob");

    // Alice works.
    let hers = alice
        .conn
        .request(methods::SESSION_NEW, json!({"cwd": alice_folder}))
        .await
        .unwrap();
    let key = hers["key"].as_str().unwrap().to_owned();
    alice
        .conn
        .request(
            methods::SESSION_ATTACH,
            json!({"session": hers, "after_seq": 0}),
        )
        .await
        .unwrap();
    alice
        .conn
        .request(
            methods::SESSION_PROMPT,
            json!({"session": hers, "content": [{"type": "text", "text": SECRET}]}),
        )
        .await
        .unwrap();
    let her_clone = "alice-clone-1";
    alice
        .conn
        .request(
            methods::SESSION_CLONE,
            json!({"session": hers, "request_id": her_clone}),
        )
        .await
        .unwrap();
    let workspace = loop_runtime::workspace_id(&alice_folder);
    // Bob has a Session of his own (so his workspace reads work).
    let his = bob
        .conn
        .request(methods::SESSION_NEW, json!({"cwd": bob_folder}))
        .await
        .unwrap();

    // A Session that does not exist: the refusal Bob must not tell apart.
    let ghost_key = "00000000-0000-0000-0000-000000000000";
    let ghost = json!({"instance": hers["instance"], "key": ghost_key, "native_id": ghost_key});

    let mut attempts = 0u64;
    let mut refused = 0u64;
    let mut leaks: Vec<String> = Vec::new();
    let before = rt.access_refusals();

    // 1. Every request method of the table, with her Session (and, for
    //    workspace methods, her workspace id).
    for (method, rule) in METHODS {
        if matches!(rule, Rule::Connection | Rule::ToClient(_) | Rule::HostOnly)
            || *method == methods::SESSION_CANCEL
            || *method == methods::SESSION_NEW
        {
            continue;
        }
        attempts += 1;
        let at_her = bob
            .conn
            .request(method, params(method, &hers, &workspace, her_clone))
            .await;
        let at_ghost = bob
            .conn
            .request(
                method,
                params(method, &ghost, "ghost-workspace", "ghost-request"),
            )
            .await;
        match (&at_her, &at_ghost) {
            (Err(her), Err(none)) => {
                refused += 1;
                let (her, none) = (masked(her, &key), masked(none, ghost_key));
                let none = none.replace("ghost-workspace", &workspace);
                if her != none {
                    leaks.push(format!("{method}: refusal differs: {her} vs {none}"));
                }
            }
            (Ok(value), _) => match *rule {
                // A list or a count: checked below. Detach answers `{}` for
                // any key.
                Rule::Filter(_) | Rule::Runtime | Rule::Detach => {
                    refused += 1;
                    if value.to_string().contains(&key) {
                        leaks.push(format!("{method}: names her Session: {value}"));
                    }
                }
                _ => leaks.push(format!("{method}: answered: {value}")),
            },
            (Err(her), Ok(_)) => leaks.push(format!("{method}: only her Session refused: {her:?}")),
        }
    }

    // 2. Copies: replay her clone request id with her payload.
    attempts += 1;
    match bob
        .conn
        .request(
            methods::SESSION_CLONE,
            json!({"session": hers, "request_id": her_clone}),
        )
        .await
    {
        Err(_) => refused += 1,
        Ok(v) => leaks.push(format!("clone replay answered: {v}")),
    }

    // 3. Her folder, a folder inside it, the folder above it.
    let inside = alice_folder.join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    for cwd in [
        alice_folder.clone(),
        inside,
        alice_folder.parent().unwrap().to_path_buf(),
    ] {
        attempts += 1;
        match bob
            .conn
            .request(methods::SESSION_NEW, json!({"cwd": cwd}))
            .await
        {
            Err(_) => refused += 1,
            Ok(v) => leaks.push(format!("session/new in {} answered: {v}", cwd.display())),
        }
    }
    // A load of her Session into his folder.
    attempts += 1;
    match bob
        .conn
        .request(
            methods::SESSION_LOAD,
            json!({"session": hers, "spec": {"cwd": bob_folder}}),
        )
        .await
    {
        Err(_) => refused += 1,
        Ok(v) => leaks.push(format!("load into his folder answered: {v}")),
    }

    // 4. A cancel while her turn runs: her turn must complete.
    attempts += 1;
    let turn = {
        let conn = alice.conn.clone();
        let hers = hers.clone();
        tokio::spawn(async move {
            conn.request(
                methods::SESSION_PROMPT,
                json!({"session": hers, "content": [{"type": "text", "text": "SLOW"}]}),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    for _ in 0..5 {
        bob.conn
            .notify(methods::SESSION_CANCEL, json!({"session": hers}))
            .unwrap();
    }
    let outcome = turn.await.unwrap().unwrap();
    if outcome.to_string().contains("ancel") {
        leaks.push(format!("Bob cancelled her turn: {outcome}"));
    } else {
        refused += 1;
    }

    // 5. Request ids (F2): Bob uses Alice's clone request id for a clone of
    //    his own Session. It is his own id: no "already used".
    attempts += 1;
    match bob
        .conn
        .request(
            methods::SESSION_CLONE,
            json!({"session": his, "request_id": her_clone}),
        )
        .await
    {
        Ok(copy) if copy.to_string().contains(&key) => {
            leaks.push(format!("his clone named her Session: {copy}"))
        }
        Ok(_) => refused += 1,
        Err(error) => leaks.push(format!("her request id blocks his: {error:?}")),
    }

    // 6. The root (no `cwd`, or the lock root itself) and a load that moves
    //    his own Session into a free folder: all refused; the free folder
    //    stays free for its user.
    let root = alice_folder.parent().unwrap().to_path_buf();
    let free = folder(dir.path(), "dave");
    for (what, method, params) in [
        ("no cwd", methods::SESSION_NEW, json!({})),
        ("the root", methods::SESSION_NEW, json!({"cwd": root})),
        (
            "load into a free folder",
            methods::SESSION_LOAD,
            json!({"session": his, "spec": {"cwd": free}}),
        ),
    ] {
        attempts += 1;
        if what == "load into a free folder" {
            // Closed first, so the load would really open it again.
            let _ = bob
                .conn
                .request(methods::SESSION_CLOSE, json!({"session": his}))
                .await;
        }
        match bob.conn.request(method, params).await {
            Err(_) => refused += 1,
            Ok(v) => leaks.push(format!("{what} answered: {v}")),
        }
    }
    let dave = connect(&rt, edge("dave")).await;
    if let Err(error) = dave
        .conn
        .request(methods::SESSION_NEW, json!({"cwd": free}))
        .await
    {
        leaks.push(format!("Dave is locked out of his folder: {error:?}"));
    }

    // 7. Escalations over the wire: Alice's turn asks for approval. Bob gets
    //    nothing, cannot answer it (stray responses on his connection), and
    //    cannot withdraw it (cancel, close); then Alice answers.
    attempts += 1;
    let fetch = {
        let conn = alice.conn.clone();
        let hers = hers.clone();
        tokio::spawn(async move {
            conn.request(
                methods::SESSION_PROMPT,
                json!({"session": hers, "content": [{"type": "text", "text": "FETCH it"}]}),
            )
            .await
        })
    };
    let asked = tokio::time::timeout(Duration::from_secs(20), alice.asked.lock().await.recv())
        .await
        .ok()
        .flatten();
    match asked {
        None => leaks.push("Alice was never asked: the probe saw no Escalation".into()),
        Some((id, request)) => {
            for guess in 0..32u64 {
                let guess: loop_wire::PeerRequestId = json!(guess);
                bob.conn.respond(
                    guess,
                    Ok(json!({"outcome": "selected", "native_id": "accept"})),
                );
            }
            bob.conn
                .notify(methods::SESSION_CANCEL, json!({"session": hers}))
                .unwrap();
            let _ = bob
                .conn
                .request(
                    methods::SESSION_CLOSE,
                    json!({"session": hers, "force": true}),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let withdrawn = alice
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.starts_with(methods::HOST_INTERACT_WITHDRAWN));
            if withdrawn || fetch.is_finished() {
                leaks.push(format!(
                    "Bob ended her Escalation (withdrawn={withdrawn}): {request}"
                ));
            } else {
                refused += 1;
            }
            let reject = serde_json::to_value(loop_protocol::InteractionOutcome::Selected {
                native_id: "reject".into(),
                note: None,
            })
            .unwrap();
            alice.conn.respond(id, Ok(reject));
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(20), fetch).await;
    if bob
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.starts_with(methods::HOST_INTERACT))
    {
        leaks.push("Bob received a host/interact".into());
    }

    // 8. The refill path: Bob comes back on a new connection (as after a lag
    //    disconnect) and attaches with a cursor: only his own Session
    //    replays; hers is not found.
    attempts += 1;
    let back = connect(&rt, edge(attacker)).await;
    let _ = back
        .conn
        .request(methods::SESSION_LOAD, json!({"session": his, "spec": {}}))
        .await;
    let _ = back
        .conn
        .request(
            methods::SESSION_ATTACH,
            json!({"session": his, "after_seq": 0}),
        )
        .await;
    match back
        .conn
        .request(
            methods::SESSION_ATTACH,
            json!({"session": hers, "after_seq": 1}),
        )
        .await
    {
        Err(_) => refused += 1,
        Ok(v) => leaks.push(format!("re-attach to hers answered: {v}")),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replayed = back.seen.lock().unwrap().join("\n");
    for needle in [key.as_str(), SECRET] {
        if replayed.contains(needle) {
            leaks.push(format!("the refill replayed {needle:?}"));
        }
    }

    // 9. What Bob can see: lists, counts, everything he received.
    let listed = bob
        .conn
        .request(methods::SESSION_LIST, json!({}))
        .await
        .unwrap();
    let rows = bob
        .conn
        .request(methods::RUNTIME_SESSIONS, json!({}))
        .await
        .unwrap();
    let status = bob
        .conn
        .request(methods::RUNTIME_STATUS, json!({}))
        .await
        .unwrap();
    // His own Sessions only: the first one and his clone (section 5).
    if listed.as_array().map(Vec::len) != Some(2) || listed.to_string().contains(&key) {
        leaks.push(format!("session/list: {listed}"));
    }
    let loaded = rows["totals"]["sessions"].as_u64().unwrap_or(99);
    if loaded > 2
        || status["loaded"] != loaded
        || status["pid"] != 0
        || rows.to_string().contains(&key)
    {
        leaks.push(format!("counts: {rows} {status}"));
    }
    let seen = bob.seen.lock().unwrap().join("\n");
    for needle in [key.as_str(), SECRET, &alice_folder.display().to_string()] {
        if seen.contains(needle) {
            leaks.push(format!("Bob received {needle:?}"));
        }
    }
    // Alice did see her own work (the probe can fail).
    let hers_seen = alice.seen.lock().unwrap().join("\n");
    if !hers_seen.contains(SECRET) {
        leaks.push("Alice never saw her own updates: the probe saw nothing".into());
    }
    let _ = his;
    let checks = rt.access_refusals() - before;
    rt.shutdown().await;
    Found {
        attempts,
        refused,
        checks,
        leaks,
    }
}
