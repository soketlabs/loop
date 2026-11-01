use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{AgentBackend, ContentBlock, SessionSpec, TurnOutcome};
use loop_runtime::{
    log::{EventKind, EventLog, MemoryEventLog},
    Runtime, RuntimeLimits,
};
use std::sync::Arc;

fn opts(cwd: &std::path::Path) -> BootstrapOpts {
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
        permission_mode: Some("ask".into()),
        agent: None,
    }
}
async fn runtime(dir: &std::path::Path, log: Arc<dyn EventLog>, limits: RuntimeLimits) -> Runtime {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    Runtime::new(globals, opts(dir), log, limits).await.unwrap()
}
#[tokio::test]
async fn two_sessions_commit_tagged_contiguous_streams_before_completion() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryEventLog::new());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    let b = rt.new_session(SessionSpec::default()).await.unwrap();
    let mut updates = rt.subscribe();
    let (a_turn, b_turn) = tokio::join!(
        rt.prompt(&a, vec![ContentBlock::text("one")]),
        rt.prompt(&b, vec![ContentBlock::text("two")])
    );
    for turn in [a_turn.unwrap(), b_turn.unwrap()] {
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
    }
    for session in [&a, &b] {
        let records = log.read_after(&session.key, 0).await.unwrap();
        assert!(!records.is_empty());
        for (i, record) in records.iter().enumerate() {
            let EventKind::Update(e) = &record.event.kind else {
                panic!()
            };
            assert_eq!(e.session, session.key);
            assert_eq!(e.instance, session.instance);
            assert_eq!(e.seq, i as u64 + 1);
            assert_eq!(e.generation, rt.generation());
        }
    }
    let mut count = 0;
    while let Ok(e) = updates.try_recv() {
        assert!(e.session == a.key || e.session == b.key);
        count += 1;
    }
    assert_eq!(
        count,
        log.read_after(&a.key, 0).await.unwrap().len()
            + log.read_after(&b.key, 0).await.unwrap().len()
    );
    rt.shutdown().await;
}

#[tokio::test]
async fn close_releases_lease_reload_continues_sequences_and_listing_retains_history() {
    use loop_agent::harness::session::sqlite::SqliteSessionStore;
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryEventLog::new());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt
        .new_session(SessionSpec {
            name: Some("named".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let store = SqliteSessionStore::open(dir.path().join("agent/sessions.db")).unwrap();
    assert_eq!(
        store
            .lease(&a.native_id)
            .await
            .unwrap()
            .unwrap()
            .identity
            .owner,
        "runtime"
    );
    rt.prompt(&a, vec![ContentBlock::text("hello")])
        .await
        .unwrap()
        .outcome()
        .await;
    let before = log.latest_seq(&a.key).await.unwrap();
    rt.close(&a.key, false).await.unwrap();
    assert!(store.lease(&a.native_id).await.unwrap().is_none());
    let list = rt.list().await.unwrap();
    assert!(!list[0].live);
    assert_eq!(list[0].name.as_deref(), Some("named"));
    rt.load_session(&a, SessionSpec::default()).await.unwrap();
    assert_eq!(log.latest_seq(&a.key).await.unwrap(), before);
    assert!(!rt.transcript(&a).await.unwrap().is_empty());
    assert_eq!(log.latest_seq(&a.key).await.unwrap(), before);
    rt.prompt(&a, vec![ContentBlock::text("again")])
        .await
        .unwrap()
        .outcome()
        .await;
    let after = log.read_after(&a.key, before).await.unwrap();
    assert_eq!(after[0].session_seq, Some(before + 1));
    rt.shutdown().await;
}

#[tokio::test]
async fn session_capacity_is_separate_from_queued_turn_admission() {
    use loop_protocol::{BackendError, SessionRef};
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryEventLog::new());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let mut sessions = Vec::new();
    for _ in 0..16 {
        sessions.push(rt.new_session(SessionSpec::default()).await.unwrap());
    }
    assert_eq!(
        rt.new_session(SessionSpec::default()).await,
        Err(BackendError::Busy)
    );
    let wrong = SessionRef {
        native_id: "wrong".into(),
        ..sessions[0].clone()
    };
    assert!(rt
        .prompt(&wrong, vec![ContentBlock::text("no")])
        .await
        .is_err());
    rt.close(&sessions[0].key, false).await.unwrap();
    rt.new_session(SessionSpec::default()).await.unwrap();
    rt.shutdown().await;
}

struct Reject;
#[async_trait::async_trait]
impl loop_protocol::HostServices for Reject {
    async fn interact(
        &self,
        _: loop_protocol::InteractionRequest,
    ) -> loop_protocol::InteractionOutcome {
        loop_protocol::InteractionOutcome::Cancelled
    }
}
#[tokio::test]
async fn connection_shutdown_keeps_other_sessions_available() {
    use loop_protocol::rpc::PeerInfo;
    use loop_wire::{
        server::{serve_with_options, ServeOptions},
        transport::memory,
        RemoteBackend,
    };
    let dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(
        runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            RuntimeLimits::default(),
        )
        .await,
    );
    let info = || PeerInfo {
        name: "test".into(),
        version: "test".into(),
    };
    let (server, client) = memory();
    let served = serve_with_options(
        rt.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
        },
    );
    let one = RemoteBackend::connect(client, info(), Arc::new(Reject))
        .await
        .unwrap();
    let (server, client) = memory();
    let other = serve_with_options(
        rt.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
        },
    );
    let two = RemoteBackend::connect(client, info(), Arc::new(Reject))
        .await
        .unwrap();
    let session = two.new_session(SessionSpec::default()).await.unwrap();
    one.shutdown().await;
    served.done.await.unwrap();
    assert!(matches!(
        two.prompt(&session, vec![ContentBlock::text("still running")])
            .await
            .unwrap()
            .outcome()
            .await,
        TurnOutcome::Completed(_)
    ));
    two.shutdown().await;
    other.done.await.unwrap();
    rt.shutdown().await;
}

#[tokio::test]
async fn active_turn_limit_waits_and_parent_can_release_and_reacquire() {
    use loop_ai::providers::{FauxResponse, FauxScript};
    use loop_protocol::{BackendError, SessionUpdate};
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
        id: "park".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":"echo parked"}),
        thought_signature: None,
    }]));
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(script));
    let log = Arc::new(MemoryEventLog::new());
    let rt = Runtime::new(
        GlobalResources::with_models(dir.path().join("agent"), models).unwrap(),
        opts(dir.path()),
        log.clone(),
        RuntimeLimits {
            max_sessions: 16,
            max_active_turns: 1,
        },
    )
    .await
    .unwrap();
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    let b = rt.new_session(SessionSpec::default()).await.unwrap();
    let mut updates = rt.subscribe();
    let first = rt
        .prompt(&a, vec![ContentBlock::text("park")])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                updates.recv().await.unwrap().update,
                SessionUpdate::ToolCall(_)
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(rt.close(&a.key, false).await, Err(BackendError::Busy));
    assert!(matches!(
        rt.prompt(&a, vec![]).await,
        Err(BackendError::Busy)
    ));
    let mut second = Box::pin(
        rt.prompt(&b, vec![ContentBlock::text("queued")])
            .await
            .unwrap()
            .outcome(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), second.as_mut())
            .await
            .is_err()
    );
    assert_eq!(log.latest_seq(&b.key).await.unwrap(), 0);
    rt.release_turn_permit(&a).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap(),
        TurnOutcome::Completed(_)
    ));
    rt.reacquire_turn_permit(&a).await.unwrap();
    let queued = rt
        .prompt(&b, vec![ContentBlock::text("cancel while queued")])
        .await
        .unwrap();
    rt.cancel(&b);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), queued.outcome())
            .await
            .unwrap(),
        TurnOutcome::Cancelled
    );
    rt.close(&a.key, true).await.unwrap();
    // The native harness may finish its final model call while cancellation
    // settles an approval. Closure still waits for the actual outcome.
    let _ = first.outcome().await;
    assert!(log
        .read_runtime_after(0)
        .await
        .unwrap()
        .iter()
        .any(|e| matches!(e.event.kind, EventKind::Cancelled(_))
            && e.event.session_key.as_ref() == Some(&a.key)));
    rt.shutdown().await;
}

#[tokio::test]
async fn restart_preserves_session_cursor_and_advances_generation() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(
        loop_runtime::log::SqliteEventLog::open(dir.path())
            .await
            .unwrap(),
    );
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    rt.prompt(&a, vec![ContentBlock::text("before")])
        .await
        .unwrap()
        .outcome()
        .await;
    let old = log.latest_seq(&a.key).await.unwrap();
    let generation = rt.generation();
    rt.shutdown().await;
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    assert_eq!(rt.generation(), generation + 1);
    rt.load_session(&a, SessionSpec::default()).await.unwrap();
    rt.prompt(&a, vec![ContentBlock::text("after")])
        .await
        .unwrap()
        .outcome()
        .await;
    let records = log.read_after(&a.key, old).await.unwrap();
    assert_eq!(records[0].session_seq, Some(old + 1));
    for record in records {
        assert_eq!(record.event.generation, generation + 1);
    }
    rt.shutdown().await;
    let records = log.read_runtime_after(0).await.unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|e| matches!(e.event.kind, EventKind::RuntimeStarted(_)))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|e| matches!(e.event.kind, EventKind::RuntimeStopped(_)))
            .count(),
        2
    );
}

#[derive(Default)]
struct FailingLog {
    inner: MemoryEventLog,
    fail_updates: std::sync::atomic::AtomicBool,
    hold_open: std::sync::atomic::AtomicBool,
    open_started: tokio::sync::Notify,
    open_release: tokio::sync::Notify,
    hold_updates: std::sync::atomic::AtomicBool,
    update_started: tokio::sync::Notify,
    update_release: tokio::sync::Notify,
    runtime_reads: std::sync::atomic::AtomicUsize,
    finished: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl EventLog for FailingLog {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        if self.hold_open.load(std::sync::atomic::Ordering::Acquire)
            && events
                .iter()
                .any(|e| matches!(e.kind, EventKind::SessionOpened(_)))
        {
            self.open_started.notify_one();
            self.open_release.notified().await;
        }
        if self.hold_updates.load(std::sync::atomic::Ordering::Acquire)
            && events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Update(_)))
        {
            self.update_started.notify_one();
            self.update_release.notified().await;
        }
        if self.fail_updates.load(std::sync::atomic::Ordering::Acquire)
            && events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Update(_)))
        {
            return Err(loop_runtime::log::EventLogError::Storage(
                "injected write failure".into(),
            ));
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        s: &loop_protocol::SessionKey,
        n: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_after(s, n).await
    }
    async fn read_runtime_after(
        &self,
        n: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.runtime_reads
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.inner.read_runtime_after(n).await
    }
    async fn latest_seq(&self, s: &loop_protocol::SessionKey) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(s).await
    }
    async fn allocate_generation(&self, o: &str, p: u32, t: i64) -> loop_runtime::log::Result<u64> {
        self.inner.allocate_generation(o, p, t).await
    }
    async fn finish_generation(&self, g: u64, t: i64) -> loop_runtime::log::Result<()> {
        self.finished
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.inner.finish_generation(g, t).await
    }
}
#[tokio::test]
async fn failed_log_write_fails_turn_without_publishing_uncommitted_updates() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(FailingLog::default());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    let mut updates = rt.subscribe();
    log.fail_updates
        .store(true, std::sync::atomic::Ordering::Release);
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        rt.prompt(&a, vec![ContentBlock::text("fail")])
            .await
            .unwrap()
            .outcome(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, TurnOutcome::Failed(_)));
    assert!(updates.try_recv().is_err());
    assert_eq!(log.latest_seq(&a.key).await.unwrap(), 0);
    assert!(rt
        .prompt(&a, vec![ContentBlock::text("again")])
        .await
        .is_err());
    assert!(rt.close(&a.key, true).await.is_err());
    let store = loop_agent::harness::session::sqlite::SqliteSessionStore::open(
        dir.path().join("agent/sessions.db"),
    )
    .unwrap();
    assert!(store.lease(&a.native_id).await.unwrap().is_none());
    rt.shutdown().await;
}

#[tokio::test]
async fn failed_factory_releases_lease_and_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    let globals = GlobalResources::with_models(dir.path().join("agent"), models).unwrap();
    let rt = Runtime::new(
        globals,
        opts(dir.path()),
        Arc::new(MemoryEventLog::new()),
        RuntimeLimits {
            max_sessions: 1,
            max_active_turns: 1,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            rt.new_session(SessionSpec::default()).await,
            Err(loop_protocol::BackendError::Failed(_))
        ));
    }
    let store = loop_agent::harness::session::sqlite::SqliteSessionStore::open(
        dir.path().join("agent/sessions.db"),
    )
    .unwrap();
    // A failed open must not leave an empty Session row behind.
    assert!(rt.list().await.unwrap().is_empty());
    let _ = store;
    rt.shutdown().await;
}

#[tokio::test]
async fn live_leases_renew_and_ownership_loss_stops_admission() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(FailingLog::default());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    let store = loop_agent::harness::session::sqlite::SqliteSessionStore::open(
        dir.path().join("agent/sessions.db"),
    )
    .unwrap();
    let first = store.lease(&a.native_id).await.unwrap().unwrap();
    log.hold_updates
        .store(true, std::sync::atomic::Ordering::Release);
    let turn = rt
        .prompt(&a, vec![ContentBlock::text("slow log")])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), log.update_started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if store
                .lease(&a.native_id)
                .await
                .unwrap()
                .unwrap()
                .heartbeat_ms
                > first.heartbeat_ms + 1_000
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    log.hold_updates
        .store(false, std::sync::atomic::Ordering::Release);
    log.update_release.notify_one();
    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
    store
        .release_lease(&a.native_id, &first.identity)
        .await
        .unwrap();
    // The next renewal discovers ownership loss. A failed Session cannot run
    // further work; the store independently fences every write as well.
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    assert!(rt
        .prompt(&a, vec![ContentBlock::text("must not run")])
        .await
        .is_err());
    let _ = rt.close(&a.key, true).await;
    rt.shutdown().await;
}

#[tokio::test]
async fn interrupted_session_open_releases_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(FailingLog::default());
    log.hold_open
        .store(true, std::sync::atomic::Ordering::Release);
    let rt = Arc::new(runtime(dir.path(), log.clone(), RuntimeLimits::default()).await);
    let opening = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.new_session(SessionSpec::default()).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        log.open_started.notified(),
    )
    .await
    .unwrap();
    opening.abort();
    let _ = opening.await;
    let list = rt.list().await.unwrap();
    assert_eq!(list.len(), 1);
    let id = &list[0].r#ref.native_id;
    let store = loop_agent::harness::session::sqlite::SqliteSessionStore::open(
        dir.path().join("agent/sessions.db"),
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while store.lease(id).await.unwrap().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("interrupted admission leaked its Session lease");
    rt.shutdown().await;
}

fn store(dir: &std::path::Path) -> loop_agent::harness::session::sqlite::SqliteSessionStore {
    loop_agent::harness::session::sqlite::SqliteSessionStore::open(dir.join("agent/sessions.db"))
        .unwrap()
}

#[tokio::test]
async fn shutdown_releases_leases_logs_stop_finishes_generation_once() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(FailingLog::default());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    let b = rt.new_session(SessionSpec::default()).await.unwrap();
    rt.shutdown().await;
    rt.shutdown().await;
    let store = store(dir.path());
    for s in [&a, &b] {
        assert!(store.lease(&s.native_id).await.unwrap().is_none());
    }
    let records = log.read_runtime_after(0).await.unwrap();
    let count = |f: fn(&EventKind) -> bool| records.iter().filter(|e| f(&e.event.kind)).count();
    assert_eq!(count(|k| matches!(k, EventKind::RuntimeStopped(_))), 1);
    assert_eq!(count(|k| matches!(k, EventKind::SessionClosed(_))), 2);
    assert_eq!(log.finished.load(std::sync::atomic::Ordering::Acquire), 1);
    assert!(rt.new_session(SessionSpec::default()).await.is_err());
}

// A pump blocked in a log write never settles a turn: cancellation cannot reach it.
async fn stuck_turn(
    dir: &std::path::Path,
) -> (
    Arc<FailingLog>,
    Runtime,
    loop_protocol::SessionRef,
    loop_protocol::TurnHandle,
) {
    let log = Arc::new(FailingLog::default());
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let rt = Runtime::new(globals, opts(dir), log.clone(), RuntimeLimits::default())
        .await
        .unwrap()
        .with_close_grace(std::time::Duration::from_millis(300));
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    log.hold_updates
        .store(true, std::sync::atomic::Ordering::Release);
    let turn = rt
        .prompt(&a, vec![ContentBlock::text("stuck")])
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        log.update_started.notified(),
    )
    .await
    .unwrap();
    (log, rt, a, turn)
}

#[tokio::test]
async fn forced_close_gives_up_on_a_turn_that_ignores_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let (log, rt, a, _turn) = stuck_turn(dir.path()).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), rt.close(&a.key, true))
        .await
        .expect("forced close must be bounded")
        .ok();
    assert!(store(dir.path())
        .lease(&a.native_id)
        .await
        .unwrap()
        .is_none());
    assert!(!rt.is_busy(&a));
    assert!(rt.tail(&a).is_err());
    let records = log.read_runtime_after(0).await.unwrap();
    assert!(records
        .iter()
        .any(|e| matches!(e.event.kind, EventKind::SessionClosed(_))));
    rt.shutdown().await;
}

#[tokio::test]
async fn shutdown_is_bounded_and_does_not_block_admission_while_settling() {
    let dir = tempfile::tempdir().unwrap();
    let (_log, rt, a, _turn) = stuck_turn(dir.path()).await;
    let rt = Arc::new(rt);
    let closing = {
        let rt = rt.clone();
        let key = a.key.clone();
        tokio::spawn(async move { rt.close(&key, true).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // The forced close is still settling: admission and listing stay responsive.
    tokio::time::timeout(std::time::Duration::from_millis(200), rt.list())
        .await
        .expect("list must not wait for close")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), rt.shutdown())
        .await
        .expect("shutdown must be bounded");
    let _ = closing.await;
    assert!(store(dir.path())
        .lease(&a.native_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn list_reads_activity_from_memory_not_the_event_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(FailingLog::default());
    let rt = runtime(dir.path(), log.clone(), RuntimeLimits::default()).await;
    let a = rt.new_session(SessionSpec::default()).await.unwrap();
    let before = rt.list().await.unwrap()[0].last_active_ms;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    rt.prompt(&a, vec![ContentBlock::text("hi")])
        .await
        .unwrap()
        .outcome()
        .await;
    let reads = log.runtime_reads.load(std::sync::atomic::Ordering::Acquire);
    for _ in 0..3 {
        let listed = rt.list().await.unwrap();
        assert!(listed[0].last_active_ms > before);
    }
    assert_eq!(
        log.runtime_reads.load(std::sync::atomic::Ordering::Acquire),
        reads
    );
    rt.shutdown().await;
}
