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
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    let responses = loop_ai::providers::FauxScript::new();
    responses.push(loop_ai::providers::FauxResponse::Text("replayed".into()));
    responses.push(loop_ai::providers::FauxResponse::Text("live-input".into()));
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
            assert!(text.contains("FOLDER\tTITLE\tBUSY\tLAST ACTIVE (ms)\tPARENT"));
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
        let (tx, mut terminal, _) = script();
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
        wait_text(&output, "1. Allow once").await;
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
        assert_eq!(output.lock().unwrap().as_str(), "replayed\n");
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
