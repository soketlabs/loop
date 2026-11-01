//! Escalations park until a Surface can answer, and decisions are durable.
use loop_protocol::rpc::InteractParams;
use loop_protocol::{HostServices, InteractionOutcome, InteractionRequest, SessionRef};
use loop_runtime::{
    log::{EventKind, EventLog, MemoryEventLog},
    EscalationHost,
};
use loop_wire::control::EscalationRecipient;
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

struct Surface(
    mpsc::UnboundedSender<(
        InteractParams,
        oneshot::Sender<InteractionOutcome>,
        oneshot::Receiver<()>,
    )>,
);
#[async_trait::async_trait]
impl EscalationRecipient for Surface {
    fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
    async fn interact(
        &self,
        params: InteractParams,
        withdrawn: oneshot::Receiver<()>,
    ) -> Result<InteractionOutcome, ()> {
        let (tx, rx) = oneshot::channel();
        self.0.send((params, tx, withdrawn)).map_err(|_| ())?;
        rx.await.map_err(|_| ())
    }
}
fn request() -> InteractionRequest {
    loop_app_core::interaction::native_permission_request(
        loop_app_core::tool_approval::ApprovalKind::Bash,
        "bash",
        "run".into(),
        "echo ok".into(),
    )
}
fn selected(id: &str) -> InteractionOutcome {
    InteractionOutcome::Selected {
        native_id: id.into(),
        note: None,
    }
}
#[tokio::test]
async fn parked_escalation_resumes_on_attach_and_logs_session_grant() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let log = Arc::new(MemoryEventLog::new());
        let session = SessionRef {
            instance: "loop".into(),
            key: "s".into(),
            native_id: "s".into(),
        };
        let host = EscalationHost::new(session.clone(), log.clone(), 1);
        let asking = tokio::spawn({
            let host = host.clone();
            async move { host.interact(request()).await }
        });
        while log.read_runtime_after(0).await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(!asking.is_finished());
        let (tx, mut rx) = mpsc::unbounded_channel();
        host.attach(1, Arc::new(Surface(tx))).await;
        let (params, answer, _) = rx.recv().await.unwrap();
        assert_eq!(params.session, Some(session));
        answer.send(selected("accept_session")).unwrap();
        assert_eq!(asking.await.unwrap(), selected("accept_session"));
        let events = log.read_runtime_after(0).await.unwrap();
        assert!(matches!(
            events[0].event.kind,
            EventKind::EscalationRaised(_)
        ));
        let EventKind::EscalationAnswered(payload) = &events[1].event.kind else {
            panic!("missing answer")
        };
        assert_eq!(payload["grant"]["scope"], "session");
    })
    .await
    .unwrap();
}

async fn runtime_with_approval(
    dir: &std::path::Path,
    log: Arc<dyn EventLog>,
) -> Arc<loop_runtime::Runtime> {
    use loop_ai::providers::{FauxResponse, FauxScript};
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
        id: "call".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":"echo approved"}),
        thought_signature: None,
    }]));
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(script));
    let opts = loop_app_core::runtime::BootstrapOpts {
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
        loop_runtime::Runtime::new(
            loop_app_core::runtime::GlobalResources::with_models(dir.join("agent"), models)
                .unwrap(),
            opts,
            log,
            Default::default(),
        )
        .await
        .unwrap(),
    )
}
async fn peer(rt: &Arc<loop_runtime::Runtime>, interactive: bool) -> loop_wire::Transport {
    let (server, mut client) = loop_wire::transport::memory();
    loop_wire::serve_with_control(
        rt.clone(),
        rt.clone(),
        server,
        loop_protocol::rpc::PeerInfo {
            name: "test".into(),
            version: "1".into(),
        },
        loop_wire::server::ServeOptions {
            owns_backend: false,
        },
    );
    client.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocol_version":1, "client_info":{"name":"test", "version":"1"}, "client_capabilities":{"interact":interactive}}}).to_string()).unwrap();
    message(&mut client).await;
    client
}
async fn message(client: &mut loop_wire::Transport) -> serde_json::Value {
    serde_json::from_str(&client.incoming.recv().await.unwrap()).unwrap()
}
async fn attach(client: &mut loop_wire::Transport, session: &SessionRef) {
    client.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":2, "method":"session/attach", "params":{"session":session}}).to_string()).unwrap();
    loop {
        let m = message(client).await;
        if m["id"] == 2 {
            assert!(m.get("result").is_some(), "{m}");
            break;
        }
    }
}
async fn interaction(client: &mut loop_wire::Transport) -> serde_json::Value {
    loop {
        let m = message(client).await;
        if m["method"] == "host/interact" {
            return m;
        }
    }
}
#[tokio::test]
async fn runtime_gate_parks_then_wire_attach_and_answer_resumes_turn() {
    tokio::time::timeout(Duration::from_secs(15), async {
        use loop_protocol::{AgentBackend, SessionSpec, ContentBlock, TurnOutcome};
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime_with_approval(dir.path(), log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt.prompt(&session, vec![ContentBlock::text("run command")]).await.unwrap();
        loop { if log.read_runtime_after(0).await.unwrap().iter().any(|e| matches!(e.event.kind, EventKind::EscalationRaised(_))) { break; } tokio::task::yield_now().await; }
        let mut client = peer(&rt, true).await;
        attach(&mut client, &session).await;
        let asked = interaction(&mut client).await;
        assert_eq!(asked["params"]["session"], serde_json::to_value(&session).unwrap());
        client.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":asked["id"], "result": selected("accept")}).to_string()).unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        rt.shutdown().await;
    }).await.unwrap();
}

#[tokio::test]
async fn two_answers_have_one_winner_and_the_loser_is_withdrawn_even_after_answering() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let log = Arc::new(MemoryEventLog::new());
        let session = SessionRef {
            instance: "loop".into(),
            key: "s".into(),
            native_id: "s".into(),
        };
        let host = EscalationHost::new(session, log.clone(), 1);
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        host.attach(1, Arc::new(Surface(tx1))).await;
        host.attach(2, Arc::new(Surface(tx2))).await;
        let asking = tokio::spawn({
            let host = host.clone();
            async move { host.interact(request()).await }
        });
        let (_, a, mut w1) = rx1.recv().await.unwrap();
        let (_, b, mut w2) = rx2.recv().await.unwrap();
        a.send(selected("accept")).unwrap();
        b.send(selected("reject")).unwrap();
        let result = asking.await.unwrap();
        if result == selected("accept") {
            assert!(w1.try_recv().is_err());
            assert_eq!(w2.try_recv(), Ok(()));
        } else {
            assert_eq!(result, selected("reject"));
            assert_eq!(w1.try_recv(), Ok(()));
            assert!(w2.try_recv().is_err());
        }
        let events = log.read_runtime_after(0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1].event.kind,
            EventKind::EscalationAnswered(_)
        ));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn detached_recipient_cannot_answer_after_reattaching_with_the_same_connection() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let log = Arc::new(MemoryEventLog::new());
        let host = EscalationHost::new(
            SessionRef {
                instance: "loop".into(),
                key: "s".into(),
                native_id: "s".into(),
            },
            log.clone(),
            1,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let surface = Arc::new(Surface(tx));
        host.attach(1, surface.clone()).await;
        let asking = tokio::spawn({
            let host = host.clone();
            async move { host.interact(request()).await }
        });
        let (_, old, _) = rx.recv().await.unwrap();
        host.detach(1).await;
        host.attach(1, surface).await;
        let (_, new, _) = rx.recv().await.unwrap();
        old.send(selected("accept_session")).unwrap();
        // Wait until the stale answer has been handled.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(!asking.is_finished(), "a detached request granted access");
        new.send(selected("reject")).unwrap();
        assert_eq!(asking.await.unwrap(), selected("reject"));
    })
    .await
    .unwrap();
}

async fn wait_raised(log: &MemoryEventLog) {
    loop {
        if log
            .read_runtime_after(0)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.event.kind, EventKind::EscalationRaised(_)))
        {
            return;
        }
        tokio::task::yield_now().await;
    }
}
async fn notification(client: &mut loop_wire::Transport, method: &str) -> serde_json::Value {
    loop {
        let m = message(client).await;
        if m["method"] == method {
            return m;
        }
    }
}
#[tokio::test]
async fn wire_race_withdraws_only_original_interactive_recipients_and_ignores_late_answers() {
    tokio::time::timeout(Duration::from_secs(15), async {
        use loop_protocol::{AgentBackend, SessionSpec, ContentBlock};
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime_with_approval(dir.path(), log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let mut a = peer(&rt, true).await;
        let mut b = peer(&rt, true).await;
        let mut observer = peer(&rt, false).await;
        attach(&mut a, &session).await;
        attach(&mut b, &session).await;
        attach(&mut observer, &session).await;
        let turn = rt.prompt(&session, vec![ContentBlock::text("run")]).await.unwrap();
        let one = interaction(&mut a).await;
        let two = interaction(&mut b).await;
        assert_eq!(one["params"]["id"], two["params"]["id"]);
        assert_eq!(one["params"]["session"], two["params"]["session"]);
        for (peer, asked) in [(&a, &one), (&b, &two)] {
            peer.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":asked["id"], "result":selected("accept_session")}).to_string()).unwrap();
        }
        turn.outcome().await;
        // Both answers may have already completed their JSON-RPC requests.
        let withdrawn = tokio::select! {
            m = notification(&mut a, "host/interact_withdrawn") => m,
            m = notification(&mut b, "host/interact_withdrawn") => m,
        };
        assert_eq!(withdrawn["params"]["id"], one["params"]["id"]);
        assert_eq!(withdrawn["params"]["session"], one["params"]["session"]);
        // An answer to an already finished request is harmless.
        a.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":one["id"], "result":selected("reject")}).to_string()).unwrap();
        let events = log.read_runtime_after(0).await.unwrap();
        assert_eq!(events.iter().filter(|e| matches!(e.event.kind, EventKind::EscalationAnswered(_))).count(), 1);
        while let Ok(m) = observer.incoming.try_recv() {
            let m: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_ne!(m["method"], "host/interact");
            assert_ne!(m["method"], "host/interact_withdrawn");
        }
        rt.shutdown().await;
    }).await.unwrap();
}
#[tokio::test]
async fn disconnect_parks_for_a_later_surface_and_cancel_withdraws_in_log_order() {
    tokio::time::timeout(Duration::from_secs(15), async {
        use loop_protocol::{AgentBackend, ContentBlock, SessionSpec, TurnOutcome};
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime_with_approval(dir.path(), log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let mut original = peer(&rt, true).await;
        attach(&mut original, &session).await;
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run")])
            .await
            .unwrap();
        let first = interaction(&mut original).await;
        drop(original);
        let mut later = peer(&rt, true).await;
        attach(&mut later, &session).await;
        let resent = interaction(&mut later).await;
        assert_eq!(first["params"], resent["params"]);
        rt.cancel(&session);
        let withdrawn = notification(&mut later, "host/interact_withdrawn").await;
        assert_eq!(withdrawn["params"]["id"], resent["params"]["id"]);
        let outcome = turn.outcome().await;
        assert!(matches!(outcome, TurnOutcome::Cancelled), "{outcome:?}");
        let kinds: Vec<_> = log
            .read_runtime_after(0)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.event.kind {
                EventKind::EscalationRaised(_) => Some("raised"),
                EventKind::Cancelled(_) => Some("cancelled"),
                EventKind::EscalationAnswered(_) => Some("answered"),
                EventKind::EscalationWithdrawn(_) => Some("withdrawn"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["raised", "cancelled", "withdrawn"]);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn closing_session_withdraws_parked_request_and_rejects_new_requests() {
    tokio::time::timeout(Duration::from_secs(15), async {
        use loop_protocol::{AgentBackend, ContentBlock, SessionSpec};
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime_with_approval(dir.path(), log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run")])
            .await
            .unwrap();
        wait_raised(&log).await;
        rt.close(&session.key, true).await.unwrap();
        turn.outcome().await;
        let records = log.read_runtime_after(0).await.unwrap();
        let withdrawn = records
            .iter()
            .position(|e| matches!(e.event.kind, EventKind::EscalationWithdrawn(_)))
            .unwrap();
        let closed = records
            .iter()
            .position(|e| matches!(e.event.kind, EventKind::SessionClosed(_)))
            .unwrap();
        assert!(withdrawn < closed);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_the_waiting_tool_withdraws_its_escalation() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let log = Arc::new(MemoryEventLog::new());
        let host = EscalationHost::new(
            SessionRef {
                instance: "loop".into(),
                key: "s".into(),
                native_id: "s".into(),
            },
            log.clone(),
            1,
        );
        let asking = tokio::spawn(async move { host.interact(request()).await });
        wait_raised(&log).await;
        asking.abort();
        assert!(asking.await.unwrap_err().is_cancelled());
        loop {
            if log
                .read_runtime_after(0)
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationWithdrawn(_)))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_option_cannot_win_and_cancellation_races_have_one_decision() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..12 {
            let log = Arc::new(MemoryEventLog::new());
            let host = EscalationHost::new(
                SessionRef {
                    instance: "loop".into(),
                    key: "s".into(),
                    native_id: "s".into(),
                },
                log.clone(),
                1,
            );
            let (tx1, mut rx1) = mpsc::unbounded_channel();
            let (tx2, mut rx2) = mpsc::unbounded_channel();
            host.attach(1, Arc::new(Surface(tx1))).await;
            host.attach(2, Arc::new(Surface(tx2))).await;
            let asking = tokio::spawn({
                let host = host.clone();
                async move { host.interact(request()).await }
            });
            let (_, invalid, _) = rx1.recv().await.unwrap();
            let (_, valid, _) = rx2.recv().await.unwrap();
            invalid.send(selected("made-up-grant")).unwrap();
            let ((), withdrawn) = tokio::join!(
                async {
                    let _ = valid.send(selected("accept_session"));
                },
                host.withdraw("turn cancelled", false)
            );
            withdrawn.unwrap();
            let result = asking.await.unwrap();
            assert!(
                result == selected("accept_session") || result == InteractionOutcome::Cancelled
            );
            let events = log.read_runtime_after(0).await.unwrap();
            assert_eq!(events.len(), 2, "there must be one terminal decision");
            assert!(matches!(
                events[0].event.kind,
                EventKind::EscalationRaised(_)
            ));
            match &events[1].event.kind {
                EventKind::EscalationAnswered(p) => {
                    assert_eq!(p["answer"]["native_id"], "accept_session")
                }
                EventKind::EscalationWithdrawn(_) => {
                    assert_eq!(result, InteractionOutcome::Cancelled)
                }
                other => panic!("unexpected decision: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn disconnecting_one_recipient_keeps_the_other_answer_valid() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let log = Arc::new(MemoryEventLog::new());
        let host = EscalationHost::new(
            SessionRef {
                instance: "loop".into(),
                key: "s".into(),
                native_id: "s".into(),
            },
            log.clone(),
            1,
        );
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        host.attach(1, Arc::new(Surface(tx1))).await;
        host.attach(2, Arc::new(Surface(tx2))).await;
        let asking = tokio::spawn({
            let host = host.clone();
            async move { host.interact(request()).await }
        });
        let (_, gone, _) = rx1.recv().await.unwrap();
        let (_, answer, _) = rx2.recv().await.unwrap();
        drop(gone);
        host.detach(1).await;
        answer.send(selected("accept")).unwrap();
        assert_eq!(asking.await.unwrap(), selected("accept"));
        assert_eq!(log.read_runtime_after(0).await.unwrap().len(), 2);
    })
    .await
    .unwrap();
}

/// Simulates a committed write whose acknowledgement has not reached the caller.
struct DelayedDecision(MemoryEventLog, bool);
#[async_trait::async_trait]
impl EventLog for DelayedDecision {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        let raised = events.iter().any(|e| {
            if self.1 {
                matches!(e.kind, EventKind::EscalationAnswered(_))
            } else {
                matches!(e.kind, EventKind::EscalationRaised(_))
            }
        });
        let records = self.0.append_batch(events).await?;
        if raised {
            std::future::pending::<()>().await;
        }
        Ok(records)
    }
    async fn read_after(
        &self,
        key: &loop_protocol::SessionKey,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.0.read_after(key, seq).await
    }
    async fn read_runtime_after(
        &self,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.0.read_runtime_after(seq).await
    }
    async fn latest_seq(&self, key: &loop_protocol::SessionKey) -> loop_runtime::log::Result<u64> {
        self.0.latest_seq(key).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        started: i64,
    ) -> loop_runtime::log::Result<u64> {
        self.0.allocate_generation(owner, pid, started).await
    }
    async fn finish_generation(
        &self,
        generation: u64,
        stopped: i64,
    ) -> loop_runtime::log::Result<()> {
        self.0.finish_generation(generation, stopped).await
    }
}
#[tokio::test]
async fn cancellation_during_raised_commit_still_logs_withdrawal() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let log = Arc::new(DelayedDecision(MemoryEventLog::new(), false));
        let host = EscalationHost::new(
            SessionRef {
                instance: "loop".into(),
                key: "s".into(),
                native_id: "s".into(),
            },
            log.clone(),
            1,
        );
        let asking = tokio::spawn(async move { host.interact(request()).await });
        wait_raised(&log.0).await;
        asking.abort();
        assert!(asking.await.unwrap_err().is_cancelled());
        loop {
            if log
                .read_runtime_after(0)
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationWithdrawn(_)))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn attaching_to_an_unloaded_session_routes_escalations_when_it_is_loaded() {
    tokio::time::timeout(Duration::from_secs(2), async {
        use loop_protocol::{AgentBackend, ContentBlock, SessionSpec};
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime_with_approval(dir.path(), log).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.close(&session.key, false).await.unwrap();
        let mut client = peer(&rt, true).await;
        attach(&mut client, &session).await;
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run")])
            .await
            .unwrap();
        let asked = interaction(&mut client).await;
        client
            .outgoing
            .send(
                serde_json::json!({"jsonrpc":"2.0", "id":asked["id"], "result":selected("accept")})
                    .to_string(),
            )
            .unwrap();
        turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn detach_queues_withdrawal_before_ack_and_reissuing_the_same_escalation() {
    tokio::time::timeout(Duration::from_secs(10), async {
        use loop_protocol::{AgentBackend, ContentBlock, SessionSpec};
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime_with_approval(dir.path(), Arc::new(MemoryEventLog::new())).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let mut client = peer(&rt, true).await;
        attach(&mut client, &session).await;
        let turn = rt.prompt(&session, vec![ContentBlock::text("run")]).await.unwrap();
        let old = interaction(&mut client).await;
        client.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":3, "method":"session/detach", "params":{"session":session}}).to_string()).unwrap();
        loop {
            let m = message(&mut client).await;
            assert_ne!(m["id"], 3, "detach was acknowledged before withdrawal was queued");
            if m["method"] == "host/interact_withdrawn" {
                assert_eq!(m["params"]["id"], old["params"]["id"]);
                break;
            }
        }
        loop { if message(&mut client).await["id"] == 3 { break; } }
        attach(&mut client, &session).await;
        let replacement = interaction(&mut client).await;
        assert_ne!(replacement["id"], old["id"]);
        assert_eq!(replacement["params"], old["params"]);
        client.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":replacement["id"], "result":selected("accept")}).to_string()).unwrap();
        turn.outcome().await;
        tokio::task::yield_now().await;
        while let Ok(m) = client.incoming.try_recv() {
            let m: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_ne!(m["method"], "host/interact_withdrawn", "stale withdrawal cleared replacement");
        }
        rt.shutdown().await;
    }).await.unwrap();
}

async fn close_with_delayed_decision(answer: bool, shutdown: bool) {
    use loop_protocol::{AgentBackend, ContentBlock, SessionSpec};
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(DelayedDecision(MemoryEventLog::new(), answer));
    let rt = Arc::new(
        Arc::try_unwrap(runtime_with_approval(dir.path(), log.clone()).await)
            .ok()
            .unwrap()
            .with_close_grace(Duration::from_millis(10)),
    );
    let session = rt.new_session(SessionSpec::default()).await.unwrap();
    let mut client = peer(&rt, true).await;
    attach(&mut client, &session).await;
    let turn = rt
        .prompt(&session, vec![ContentBlock::text("run")])
        .await
        .unwrap();
    if answer {
        let asked = interaction(&mut client).await;
        client.outgoing.send(serde_json::json!({"jsonrpc":"2.0", "id":asked["id"], "result":selected("accept_session")}).to_string()).unwrap();
        loop {
            if log
                .read_runtime_after(0)
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationAnswered(_)))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    } else {
        wait_raised(&log.0).await;
    }
    tokio::time::timeout(Duration::from_millis(250), async {
        if shutdown {
            rt.shutdown().await;
        } else {
            rt.close(&session.key, true).await.unwrap();
        }
    })
    .await
    .expect("Escalation acknowledgement bypassed close grace");
    assert!(
        !rt.list()
            .await
            .unwrap()
            .iter()
            .find(|s| s.r#ref == session)
            .unwrap()
            .live
    );
    let store = loop_agent::harness::session::sqlite::SqliteSessionStore::open(
        dir.path().join("agent/sessions.db"),
    )
    .unwrap();
    assert!(store.lease(&session.native_id).await.unwrap().is_none());
    let records = log.read_runtime_after(0).await.unwrap();
    let withdrawn = records
        .iter()
        .position(|e| matches!(e.event.kind, EventKind::EscalationWithdrawn(_)))
        .expect("missing durable withdrawal");
    let closed = records
        .iter()
        .position(|e| matches!(e.event.kind, EventKind::SessionClosed(_)))
        .unwrap();
    assert!(withdrawn < closed);
    turn.outcome().await;
    rt.shutdown().await;
}
#[tokio::test]
async fn forced_close_bounds_stalled_raised_and_answered_acknowledgements() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for answer in [false, true] {
            close_with_delayed_decision(answer, false).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn shutdown_bounds_stalled_raised_and_answered_acknowledgements() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for answer in [false, true] {
            close_with_delayed_decision(answer, true).await;
        }
    })
    .await
    .unwrap();
}
