#![cfg(unix)]
use loop_cli::attach_cmd::{connect, drive, AttachArgs, Input, Terminal};
use loop_protocol::{rpc::PeerInfo, AgentBackend, ContentBlock, SessionSpec};
use loop_runtime::{log::MemoryEventLog, Runtime, RuntimeLimits};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

struct Script {
    input: mpsc::UnboundedReceiver<Input>,
    output: Arc<Mutex<String>>,
}
#[async_trait::async_trait]
impl Terminal for Script {
    async fn read(&mut self) -> anyhow::Result<Input> {
        Ok(self.input.recv().await.unwrap_or(Input::Eof))
    }
    fn write(&mut self, text: &str) -> anyhow::Result<()> {
        self.output.lock().unwrap().push_str(text);
        Ok(())
    }
}
fn script() -> (mpsc::UnboundedSender<Input>, Script, Arc<Mutex<String>>) {
    let (tx, input) = mpsc::unbounded_channel();
    let output = Arc::new(Mutex::new(String::new()));
    (
        tx,
        Script {
            input,
            output: output.clone(),
        },
        output,
    )
}
async fn wait_text(output: &Mutex<String>, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if output.lock().unwrap().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {needle:?}; output: {:?}", output.lock().unwrap()));
}
async fn runtime(dir: &std::path::Path) -> Arc<Runtime> {
    runtime_with_log(dir, Arc::new(MemoryEventLog::new())).await
}
async fn runtime_with_log(
    dir: &std::path::Path,
    log: Arc<dyn loop_runtime::log::EventLog>,
) -> Arc<Runtime> {
    let responses = loop_ai::providers::FauxScript::new();
    responses.push(loop_ai::providers::FauxResponse::Text("replayed".into()));
    responses.push(loop_ai::providers::FauxResponse::Text("live-input".into()));
    runtime_with_script(dir, log, responses).await
}
async fn runtime_with_script(
    dir: &std::path::Path,
    log: Arc<dyn loop_runtime::log::EventLog>,
    responses: loop_ai::providers::FauxScript,
) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(responses));
    let globals =
        loop_app_core::runtime::GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let opts = loop_app_core::BootstrapOpts {
        cwd: dir.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: None,
    };
    Arc::new(
        Runtime::new(globals, opts, log, RuntimeLimits::default())
            .await
            .unwrap(),
    )
}
fn serve(
    rt: Arc<Runtime>,
    transport: loop_wire::transport::Transport,
) -> loop_wire::server::Served {
    loop_wire::server::serve_with_control(
        rt.clone(),
        rt,
        transport,
        PeerInfo {
            name: "test".into(),
            version: "1".into(),
        },
        loop_wire::server::ServeOptions {
            owns_backend: false,
        },
    )
}

#[tokio::test]
async fn list_and_ambiguous_prefix_show_session_details_newest_first() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let first = rt
            .new_session(SessionSpec {
                name: Some("First".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = rt
            .new_session(SessionSpec {
                name: Some("Second".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        for id in [None, Some(String::new())] {
            let (server, client) = loop_wire::transport::memory();
            let served = serve(rt.clone(), server);
            let (backend, events) = connect(client).await.unwrap();
            let (_tx, mut terminal, output) = script();
            let result = drive(
                backend,
                events,
                AttachArgs {
                    id: id.clone(),
                    ..Default::default()
                },
                &mut terminal,
            )
            .await;
            assert_eq!(result.is_err(), id.is_some());
            let text = output.lock().unwrap().clone();
            let header = text.lines().next().unwrap();
            let columns: Vec<_> = header.split_whitespace().collect();
            assert_eq!(
                columns,
                ["ID", "FOLDER", "TITLE", "BUSY", "LAST", "ACTIVE", "PARENT"],
                "{text}"
            );
            let row = text.lines().find(|l| l.contains("First")).unwrap();
            assert!(row.contains(" no ") && row.contains("just now"), "{text}");
            // Columns line up: every row puts BUSY where the header does.
            let busy = header.find("BUSY").unwrap();
            assert!(
                text.lines()
                    .skip(1)
                    .all(|l| matches!(&l[busy..busy + 3], "no " | "yes")),
                "{text}"
            );
            assert!(
                text.find(&second.key.to_string()).unwrap()
                    < text.find(&first.key.to_string()).unwrap()
            );
            assert!(text.contains("First") && text.contains(&dir.path().display().to_string()));
            served.done.await.unwrap();
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn attach_renders_replay_then_live_and_typed_line_becomes_prompt() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.prompt(&session, vec![ContentBlock::text("replayed")])
            .await
            .unwrap()
            .outcome()
            .await;
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        let key = session.key.to_string();
        let task = tokio::spawn(async move {
            drive(
                backend,
                events,
                AttachArgs {
                    id: Some(key[..8].into()),
                    ..Default::default()
                },
                &mut terminal,
            )
            .await
        });
        wait_text(&output, "replayed").await;
        let header = output
            .lock()
            .unwrap()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        assert!(
            header.starts_with(&format!("Attached to Session {}", session.key))
                && header.contains(&dir.path().display().to_string())
                && header.contains("Ctrl+D"),
            "header: {header:?}"
        );
        tx.send(Input::Line("live-input".into())).unwrap();
        wait_text(&output, "live-input").await;
        while rt.is_busy(&session) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tx.send(Input::Eof).unwrap();
        task.await.unwrap().unwrap();
        served.done.await.unwrap();
        let text = output.lock().unwrap().clone();
        assert!(text.find("replayed").unwrap() < text.find("live-input").unwrap());
        assert!(rt.transcript(&session).await.unwrap().len() >= 4);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn new_session_uses_cwd_and_eof_only_detaches() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        tx.send(Input::Eof).unwrap();
        drive(
            backend,
            events,
            AttachArgs {
                new: true,
                cwd: Some(dir.path().into()),
                ..Default::default()
            },
            &mut terminal,
        )
        .await
        .unwrap();
        served.done.await.unwrap();
        let list = rt.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].live);
        let text = output.lock().unwrap().clone();
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.starts_with(&format!("New Session {}", list[0].r#ref.key))
                && header.contains(
                    &dir.path()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                )
                && header.contains("Ctrl+D")
                && header.contains("Ctrl+C twice"),
            "header: {text:?}"
        );
        assert_eq!(
            list[0].cwd.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
        rt.prompt(&list[0].r#ref, vec![ContentBlock::text("still available")])
            .await
            .unwrap()
            .outcome()
            .await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn no_runtime_explains_how_to_start_one_without_bootstrap() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_loop"))
            .args(["attach", "--socket"])
            .arg(dir.path().join("missing.sock"))
            .env("LOOP_CODING_AGENT_DIR", dir.path().join("agent"))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("loop serve"));
        assert!(!dir.path().join("agent").exists());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn numbered_approval_withdrawal_and_interrupts_use_the_wire() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap(); let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.prompt(&session, vec![ContentBlock::text("history")]).await.unwrap().outcome().await;
        let (mut server, client) = loop_wire::transport::memory();
        let inject = server.outgoing.clone();
        let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = messages.clone();
        let (forward, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(raw) = server.incoming.recv().await {
                captured.lock().unwrap().push(serde_json::from_str(&raw).unwrap());
                if forward.send(raw).is_err() { break; }
            }
        });
        let served = serve(rt.clone(), loop_wire::transport::Transport { incoming, outgoing: server.outgoing });
        let (backend, events) = connect(client).await.unwrap(); let (tx, mut terminal, output) = script();
        let key = session.key.to_string();
        let task = tokio::spawn(async move { drive(backend, events, AttachArgs { id: Some(key), ..Default::default() }, &mut terminal).await });
        wait_text(&output, "replayed").await;
        let request = |rpc_id: u64, id: &str| serde_json::json!({"jsonrpc":"2.0", "id":rpc_id, "method":"host/interact", "params":{
            "session":session, "kind":"permission", "id":id,
            "options":[{"native_id":"allow-once", "label":"Allow once", "kind_hint":"allow_once"}, {"native_id":"reject", "label":"Reject", "kind_hint":"reject_once"}]
        }}).to_string();
        inject.send(request(9001, "approve-1")).unwrap();
        wait_text(&output, "1. Allow once").await;
        tx.send(Input::Line("9".into())).unwrap();
        wait_text(&output, "Enter one").await;
        tx.send(Input::Line("1".into())).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async { loop {
            if messages.lock().unwrap().iter().any(|v| v["id"] == 9001 && v["result"]["native_id"] == "allow-once") { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        }).await.expect("approval response missing");
        output.lock().unwrap().clear();
        inject.send(request(9002, "approve-2")).unwrap();
        wait_text(&output, "3. Allow once").await;
        inject.send(serde_json::json!({"jsonrpc":"2.0", "method":"host/interact_withdrawn", "params":{"session":session,"id":"approve-2"}}).to_string()).unwrap();
        wait_text(&output, "Another client answered").await;
        // Release the injection sender so server shutdown closes the transport.
        drop(inject);
        tx.send(Input::Line("new prompt after withdrawal".into())).unwrap();
        wait_text(&output, "live-input").await;
        tx.send(Input::Interrupt).unwrap();
        wait_text(&output, "Ctrl+C again to detach").await;
        tx.send(Input::Interrupt).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap_or_else(|_| panic!("client did not detach; messages={:?}; output={:?}", messages.lock().unwrap(), output.lock().unwrap())).unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), served.done).await.expect("server did not close").unwrap();
        let messages = messages.lock().unwrap().clone();
        assert_eq!(messages.iter().filter(|v| v["method"] == "session/cancel").count(), 1);
        assert_eq!(messages.iter().filter(|v| v["method"] == "session/detach").count(), 1);
        assert!(!messages.iter().any(|v| v["method"] == "session/close"));
        assert!(messages.iter().any(|v| v["id"] == 9002 && v["result"]["outcome"] == "cancelled"));
        assert!(rt.list().await.unwrap()[0].live);
        tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await.expect("Runtime did not stop");
    }).await.unwrap();
}

#[tokio::test]
async fn immediate_eof_renders_the_complete_replay_before_detaching() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.prompt(&session, vec![ContentBlock::text("history")])
            .await
            .unwrap()
            .outcome()
            .await;
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        tx.send(Input::Eof).unwrap();
        drive(
            backend,
            events,
            AttachArgs {
                id: Some(session.key.to_string()),
                ..Default::default()
            },
            &mut terminal,
        )
        .await
        .unwrap();
        let text = output.lock().unwrap().clone();
        let (header, replay) = text.split_once('\n').unwrap();
        assert!(header.starts_with("Attached to Session"), "{text:?}");
        assert_eq!(replay, "replayed\n");
        served.done.await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[derive(Default)]
struct GatedLog {
    inner: MemoryEventLog,
    held: std::sync::atomic::AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl loop_runtime::log::EventLog for GatedLog {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        if events
            .iter()
            .any(|e| matches!(e.kind, loop_runtime::log::EventKind::Update(_)))
            && self.held.load(std::sync::atomic::Ordering::Acquire)
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        key: &loop_protocol::SessionKey,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_after(key, seq).await
    }
    async fn read_runtime_after(
        &self,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_runtime_after(seq).await
    }
    async fn latest_seq(&self, key: &loop_protocol::SessionKey) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(key).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        ms: i64,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.allocate_generation(owner, pid, ms).await
    }
    async fn finish_generation(&self, generation: u64, ms: i64) -> loop_runtime::log::Result<()> {
        self.inner.finish_generation(generation, ms).await
    }
}

#[tokio::test]
async fn eof_detaches_without_stopping_a_running_turn() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(GatedLog::default());
        let rt = runtime_with_log(dir.path(), log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        log.held.store(true, std::sync::atomic::Ordering::Release);
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("continue after detach")])
            .await
            .unwrap();
        log.started.notified().await;
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, _) = script();
        tx.send(Input::Eof).unwrap();
        drive(
            backend,
            events,
            AttachArgs {
                id: Some(session.key.to_string()),
                ..Default::default()
            },
            &mut terminal,
        )
        .await
        .unwrap();
        served.done.await.unwrap();
        assert!(rt.is_busy(&session));
        log.held.store(false, std::sync::atomic::Ordering::Release);
        log.release.notify_one();
        assert!(matches!(
            turn.outcome().await,
            loop_protocol::TurnOutcome::Completed(_)
        ));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn connection_loss_reports_error_and_leaves_the_session_running() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.prompt(&session, vec![ContentBlock::text("history")])
            .await
            .unwrap()
            .outcome()
            .await;
        let (server, mut client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (cut, cut_rx) = tokio::sync::oneshot::channel::<()>();
        let (forward, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::pin!(cut_rx);
            loop {
                tokio::select! {
                    _ = &mut cut_rx => return,
                    raw = client.incoming.recv() => match raw {
                        Some(raw) => { if forward.send(raw).is_err() { return; } }
                        None => return,
                    }
                }
            }
        });
        let (backend, events) = connect(loop_wire::transport::Transport {
            incoming,
            outgoing: client.outgoing,
        })
        .await
        .unwrap();
        let (_tx, mut terminal, output) = script();
        let id = session.key.to_string();
        let task = tokio::spawn(async move {
            drive(
                backend,
                events,
                AttachArgs {
                    id: Some(id),
                    ..Default::default()
                },
                &mut terminal,
            )
            .await
        });
        wait_text(&output, "replayed").await;
        cut.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("client hung after connection loss")
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("Lost contact"));
        assert!(rt.list().await.unwrap()[0].live);
        served.done.abort();
        let _ = served.done.await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

struct PausedInput {
    script: Script,
    enabled: tokio::sync::watch::Receiver<bool>,
}
#[async_trait::async_trait]
impl Terminal for PausedInput {
    async fn read(&mut self) -> anyhow::Result<Input> {
        while !*self.enabled.borrow_and_update() {
            self.enabled.changed().await?;
        }
        self.script.read().await
    }
    fn write(&mut self, text: &str) -> anyhow::Result<()> {
        self.script.write(text)
    }
}

#[tokio::test]
async fn buffered_withdrawn_answer_never_answers_the_next_request_or_becomes_a_prompt() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for queued in [false, true] {
            let dir = tempfile::tempdir().unwrap(); let rt = runtime(dir.path()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (mut server, client) = loop_wire::transport::memory();
            let inject = server.outgoing.clone();
            let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new())); let captured = messages.clone();
            let (forward, incoming) = mpsc::unbounded_channel();
            tokio::spawn(async move { while let Some(raw) = server.incoming.recv().await {
                captured.lock().unwrap().push(serde_json::from_str(&raw).unwrap());
                if forward.send(raw).is_err() { break; }
            }});
            let served = serve(rt.clone(), loop_wire::transport::Transport { incoming, outgoing: server.outgoing });
            let (backend, events) = connect(client).await.unwrap(); let (tx, script, output) = script();
            let (enable, enabled) = tokio::sync::watch::channel(false); let mut terminal = PausedInput { script, enabled };
            let key = session.key.to_string();
            let task = tokio::spawn(async move { drive(backend, events, AttachArgs { id: Some(key), ..Default::default() }, &mut terminal).await });
            let approval = |rpc_id: u64, id: &str| serde_json::json!({"jsonrpc":"2.0", "id":rpc_id, "method":"host/interact", "params":{
                "session": session, "kind":"permission", "id":id,
                "options":[{"native_id":"allow", "label":id, "kind_hint":"allow_once"}]
            }}).to_string();
            // Wait until attachment before injecting an Escalation.
            loop {
                if messages.lock().unwrap().iter().any(|m| m["method"] == "session/attach") { break; }
                tokio::task::yield_now().await;
            }
            inject.send(approval(9101, "A")).unwrap(); wait_text(&output, "1. A").await;
            if queued { inject.send(approval(9102, "B")).unwrap(); }
            tx.send(Input::Line("1".into())).unwrap(); // Entered for A, still buffered.
            inject.send(serde_json::json!({"jsonrpc":"2.0", "method":"host/interact_withdrawn", "params":{"session":session,"id":"A"}}).to_string()).unwrap();
            wait_text(&output, "Another client answered").await;
            if queued { wait_text(&output, ". B").await; }
            drop(inject); enable.send(true).unwrap();
            tx.send(Input::Interrupt).unwrap(); wait_text(&output, "Ctrl+C again").await;
            tx.send(Input::Interrupt).unwrap(); task.await.unwrap().unwrap(); served.done.await.unwrap();
            let messages = messages.lock().unwrap().clone();
            assert!(!messages.iter().any(|m| m["id"] == 9102 && m["result"]["outcome"] == "selected"), "stale answer approved B: {messages:?}");
            assert!(!messages.iter().any(|m| m["method"] == "session/prompt"), "stale answer became a prompt: {messages:?}");
            rt.shutdown().await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn runtime_loss_after_detach_request_closes_locally_without_a_shutdown_rpc() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for drop_connection in [true, false] {
        let dir = tempfile::tempdir().unwrap(); let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let (mut server, mut client) = loop_wire::transport::memory();
        let (cut, cut_rx) = tokio::sync::oneshot::channel();
        let (to_client, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move { tokio::pin!(cut_rx); loop { tokio::select! {
            _ = &mut cut_rx => return,
            raw = client.incoming.recv() => match raw { Some(raw) => { if to_client.send(raw).is_err() { return; } }, None => return }
        }}});
        let (forward, server_incoming) = mpsc::unbounded_channel();
        let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new())); let captured = messages.clone();
        let closed = Arc::new(tokio::sync::Notify::new()); let closed_peer = closed.clone();
        tokio::spawn(async move {
            let mut cut = Some(cut); let mut forward = Some(forward);
            while let Some(raw) = server.incoming.recv().await {
                let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
                let detach = value["method"] == "session/detach"; captured.lock().unwrap().push(value);
                if detach {
                    if drop_connection { let _ = cut.take().unwrap().send(()); forward.take(); }
                    // Otherwise keep the transport alive but never acknowledge.
                }
                else if let Some(forward) = &forward { let _ = forward.send(raw); }
            }
            closed_peer.notify_one();
        });
        let served = serve(rt.clone(), loop_wire::transport::Transport { incoming: server_incoming, outgoing: server.outgoing });
        let (backend, events) = connect(loop_wire::transport::Transport { incoming, outgoing: client.outgoing }).await.unwrap();
        let (tx, mut terminal, _) = script(); tx.send(Input::Eof).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), drive(backend, events, AttachArgs { id: Some(session.key.to_string()), ..Default::default() }, &mut terminal)).await.expect("cleanup hung after Runtime loss");
        assert!(result.is_err());
        tokio::time::timeout(Duration::from_secs(1), closed.notified()).await.expect("local transport stayed open");
        assert!(!messages.lock().unwrap().iter().any(|m| m["method"] == "shutdown"));
        served.done.await.unwrap(); assert!(rt.list().await.unwrap()[0].live); rt.shutdown().await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn interrupts_cancel_and_detach_before_gated_replay_finishes() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.prompt(&session, vec![ContentBlock::text("history")])
            .await
            .unwrap()
            .outcome()
            .await;
        let (mut server, client) = loop_wire::transport::memory();
        let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = messages.clone();
        let (forward, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(raw) = server.incoming.recv().await {
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&raw).unwrap());
                if forward.send(raw).is_err() {
                    break;
                }
            }
        });
        let (outgoing, mut server_output) = mpsc::unbounded_channel::<String>();
        let replay_held = Arc::new(tokio::sync::Notify::new());
        let held = replay_held.clone();
        tokio::spawn(async move {
            while let Some(raw) = server_output.recv().await {
                let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
                if value["method"] == "session/update" {
                    held.notify_one();
                    continue;
                }
                if server.outgoing.send(raw).is_err() {
                    break;
                }
            }
        });
        let served = serve(
            rt.clone(),
            loop_wire::transport::Transport { incoming, outgoing },
        );
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        let key = session.key.to_string();
        let task = tokio::spawn(async move {
            drive(
                backend,
                events,
                AttachArgs {
                    id: Some(key),
                    ..Default::default()
                },
                &mut terminal,
            )
            .await
        });
        replay_held.notified().await;
        tx.send(Input::Interrupt).unwrap();
        wait_text(&output, "Ctrl+C again").await;
        tx.send(Input::Interrupt).unwrap();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("interrupt could not detach during replay")
            .unwrap()
            .unwrap();
        served.done.await.unwrap();
        let messages = messages.lock().unwrap().clone();
        assert!(messages.iter().any(|m| m["method"] == "session/cancel"));
        assert!(messages.iter().any(|m| m["method"] == "session/detach"));
        assert!(!output.lock().unwrap().contains("replayed"));
        assert!(rt.list().await.unwrap()[0].live);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn attaching_to_a_session_held_by_another_loop_says_what_to_do() {
    tokio::time::timeout(Duration::from_secs(30), async {
        use loop_agent::harness::session::sqlite::{LeaseIdentity, SqliteSessionStore};
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.close(&session.key, false).await.unwrap();
        // A terminal `loop` (alive: pid 1) now holds the Session.
        SqliteSessionStore::open(dir.path().join("agent/sessions.db"))
            .unwrap()
            .acquire_lease(
                &session.native_id,
                &LeaseIdentity {
                    owner: "standalone".into(),
                    pid: 1,
                    generation: 0,
                },
            )
            .await
            .unwrap();
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (_tx, mut terminal, _output) = script();
        let error = drive(
            backend,
            events,
            AttachArgs {
                id: Some(session.key.to_string()),
                ..Default::default()
            },
            &mut terminal,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("in use"), "{error}");
        assert!(!error.contains("loop attach"), "circular hint: {error}");
        assert!(error.contains("Close it there"), "{error}");
        served.done.await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn attach_shows_tool_calls_and_starts_each_message_on_its_own_line() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let responses = loop_ai::providers::FauxScript::new();
        responses.push(loop_ai::providers::FauxResponse::Text("LIVE-OK".into()));
        responses.push(loop_ai::providers::FauxResponse::ToolCalls(vec![
            loop_ai::ToolCall {
                id: "call-ls".into(),
                name: "ls".into(),
                arguments: serde_json::json!({"path": "."}),
                thought_signature: None,
            },
        ]));
        responses.push(loop_ai::providers::FauxResponse::Text(
            "The folder is empty.".into(),
        ));
        let rt = runtime_with_script(dir.path(), Arc::new(MemoryEventLog::new()), responses).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        for prompt in ["first", "second"] {
            rt.prompt(&session, vec![ContentBlock::text(prompt)])
                .await
                .unwrap()
                .outcome()
                .await;
        }
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        tx.send(Input::Eof).unwrap();
        drive(
            backend,
            events,
            AttachArgs {
                id: Some(session.key.to_string()),
                ..Default::default()
            },
            &mut terminal,
        )
        .await
        .unwrap();
        let text = output.lock().unwrap().clone();
        assert!(
            text.ends_with("LIVE-OK\n• ls .\n  └ done\nThe folder is empty.\n"),
            "{text:?}"
        );
        served.done.await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_subagents_escalation_is_labelled_and_withdrawn_by_its_own_session() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let child = rt
            .new_session(SessionSpec {
                name: Some("check the folder".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let (mut server, client) = loop_wire::transport::memory();
        let inject = server.outgoing.clone();
        let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = messages.clone();
        let (forward, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(raw) = server.incoming.recv().await {
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&raw).unwrap());
                if forward.send(raw).is_err() {
                    break;
                }
            }
        });
        let served = serve(
            rt.clone(),
            loop_wire::transport::Transport {
                incoming,
                outgoing: server.outgoing,
            },
        );
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        let key = session.key.to_string();
        let task = tokio::spawn(async move {
            drive(
                backend,
                events,
                AttachArgs {
                    id: Some(key),
                    ..Default::default()
                },
                &mut terminal,
            )
            .await
        });
        wait_text(&output, "Attached to Session").await;
        let request = |rpc_id: u64, from: &loop_protocol::SessionRef, title: &str| {
            serde_json::json!({"jsonrpc":"2.0", "id":rpc_id, "method":"host/interact", "params":{
                "session":from, "kind":"permission", "id":"approval-1",
                "tool_call":{"id":"t", "title":title, "kind":"execute", "status":"pending"},
                "options":[{"native_id":"accept", "label":"Allow once", "kind_hint":"allow_once"}]
            }})
            .to_string()
        };
        inject.send(request(9001, &child, "ls child")).unwrap();
        let short = &child.key.as_str()[..8];
        wait_text(&output, &format!("Subagent {short}: ls child")).await;
        wait_text(&output, &format!("Subagent {short} (check the folder)")).await;
        // Same native id, different Session: a separate request.
        inject.send(request(9002, &session, "ls parent")).unwrap();
        inject
            .send(
                serde_json::json!({"jsonrpc":"2.0", "method":"host/interact_withdrawn",
                "params":{"session":child, "id":"approval-1"}})
                .to_string(),
            )
            .unwrap();
        wait_text(&output, "Escalation: ls parent").await;
        tx.send(Input::Line("2".into())).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if messages
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|v| v["id"] == 9002 && v["result"]["native_id"] == "accept")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("parent approval not answered: {:?}", output.lock().unwrap()));
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["id"] == 9001 && v["result"]["outcome"] == "cancelled"));
        drop(inject);
        tx.send(Input::Eof).unwrap();
        task.await.unwrap().unwrap();
        served.done.await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_slow_session_list_never_delays_a_subagents_escalation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let child = rt
            .new_session(SessionSpec {
                name: Some("slow name".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let (mut server, client) = loop_wire::transport::memory();
        let inject = server.outgoing.clone();
        let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = messages.clone();
        let (forward, incoming) = mpsc::unbounded_channel();
        let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stalling = stall.clone();
        tokio::spawn(async move {
            while let Some(raw) = server.incoming.recv().await {
                let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
                captured.lock().unwrap().push(value.clone());
                // Once attached, the Runtime never answers a listing.
                if stalling.load(std::sync::atomic::Ordering::Acquire)
                    && value["method"] == "session/list"
                {
                    continue;
                }
                if forward.send(raw).is_err() {
                    break;
                }
            }
        });
        let served = serve(
            rt.clone(),
            loop_wire::transport::Transport {
                incoming,
                outgoing: server.outgoing,
            },
        );
        let (backend, events) = connect(client).await.unwrap();
        let (tx, mut terminal, output) = script();
        let key = session.key.to_string();
        let task = tokio::spawn(async move {
            drive(
                backend,
                events,
                AttachArgs {
                    id: Some(key),
                    ..Default::default()
                },
                &mut terminal,
            )
            .await
        });
        wait_text(&output, "Attached to Session").await;
        stall.store(true, std::sync::atomic::Ordering::Release);
        inject
            .send(serde_json::json!({"jsonrpc":"2.0", "id":9001, "method":"host/interact", "params":{
                "session":child, "kind":"permission", "id":"approval-1",
                "tool_call":{"id":"t", "title":"ls child", "kind":"execute", "status":"pending"},
                "options":[{"native_id":"accept", "label":"Allow once", "kind_hint":"allow_once"}]
            }}).to_string())
            .unwrap();
        let short = &child.key.as_str()[..8];
        let started = std::time::Instant::now();
        wait_text(&output, &format!("Subagent {short}")).await;
        tx.send(Input::Line("1".into())).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if messages.lock().unwrap().iter().any(|v| v["id"] == 9001 && v["result"]["native_id"] == "accept") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "answered after {:?}",
            started.elapsed()
        );
        drop(inject);
        tx.send(Input::Eof).unwrap();
        task.await.unwrap().unwrap();
        served.done.await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn json_listing_prints_every_session_as_session_info() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt
            .new_session(SessionSpec {
                name: Some("two  spaces".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let (server, client) = loop_wire::transport::memory();
        let served = serve(rt.clone(), server);
        let (backend, events) = connect(client).await.unwrap();
        let (_tx, mut terminal, output) = script();
        drive(
            backend,
            events,
            AttachArgs {
                json: true,
                ..Default::default()
            },
            &mut terminal,
        )
        .await
        .unwrap();
        let text = output.lock().unwrap().clone();
        let list: Vec<loop_protocol::rpc::SessionInfo> =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].r#ref, session);
        assert_eq!(list[0].name.as_deref(), Some("two  spaces"));
        served.done.await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
