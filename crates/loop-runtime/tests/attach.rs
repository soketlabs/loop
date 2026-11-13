//! Multi-Session serving over loop-wire: attach, filtering, close and detach.

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    AgentBackend, ContentBlock, HostServices, InteractionOutcome, InteractionRequest, Origin,
    SessionKey, SessionRef, SessionSpec, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::log::{EventLog, MemoryEventLog, SqliteEventLog};
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_control, serve_with_options, ServeOptions, Served};
use loop_wire::transport::memory;
use loop_wire::{Attachment, RemoteBackend, RuntimeControl};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tokio::sync::Notify;

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

async fn runtime(dir: &std::path::Path, log: Arc<dyn EventLog>) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    Arc::new(
        {
            let store = globals.open_runtime_session_store().unwrap();
            Runtime::new(globals, opts(dir), log, store, RuntimeLimits::default())
        }
        .await
        .unwrap(),
    )
}

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "test".into(),
    }
}

struct Reject;
#[async_trait::async_trait]
impl HostServices for Reject {
    async fn interact(&self, _: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Cancelled
    }
}

async fn connect(rt: &Arc<Runtime>) -> (Arc<RemoteBackend>, Served) {
    let (server, client) = memory();
    let served = serve_with_control(
        rt.clone(),
        rt.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
            authority: loop_wire::Authority::HostLocal,
        },
    );
    let remote = RemoteBackend::connect(client, info(), Arc::new(Reject))
        .await
        .unwrap();
    (remote, served)
}

async fn say(backend: &RemoteBackend, session: &SessionRef, text: &str) {
    let turn = backend
        .prompt(session, vec![ContentBlock::text(text)])
        .await
        .unwrap();
    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
}

/// Read `session`'s updates until `seq` reaches `last`.
async fn collect(
    rx: &mut Receiver<UpdateEnvelope>,
    key: &SessionKey,
    last: u64,
) -> Vec<UpdateEnvelope> {
    let mut got = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for updates")
            .unwrap();
        if &e.session == key {
            let done = e.seq >= last;
            got.push(e);
            if done {
                return got;
            }
        }
    }
}

fn assert_contiguous(updates: &[UpdateEnvelope], after: u64) {
    for (i, e) in updates.iter().enumerate() {
        assert_eq!(e.seq, after + i as u64 + 1, "gap or duplicate at {i}");
    }
}

async fn attaching_mid_turn_is_contiguous_without_gaps_or_duplicates_body() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryEventLog::new());
    let rt = runtime(dir.path(), log.clone()).await;
    let (owner, _s1) = connect(&rt).await;
    let session = owner.new_session(SessionSpec::default()).await.unwrap();
    let producer = {
        let owner = owner.clone();
        let session = session.clone();
        tokio::spawn(async move {
            for i in 0..25 {
                say(&owner, &session, &format!("turn {i}")).await;
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
        })
    };
    // Attach from several cursors while turns are running.
    let mut watchers = Vec::new();
    for delay_ms in [0u64, 5, 15, 30] {
        let (watcher, served) = connect(&rt).await;
        let rx = watcher.subscribe();
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let attached = watcher.attach(&session, 0).await.unwrap();
        assert_eq!(attached.generation, rt.generation());
        watchers.push((watcher, served, rx, attached));
    }
    producer.await.unwrap();
    let last = log.latest_seq(&session.key).await.unwrap();
    let mut mixed = 0;
    for (_w, _s, mut rx, attached) in watchers {
        let got = collect(&mut rx, &session.key, last).await;
        assert_contiguous(&got, 0);
        assert!(attached.latest_seq <= last);
        let replayed = got
            .iter()
            .take_while(|e| e.origin == Origin::Replay)
            .count();
        assert!(got[replayed..].iter().all(|e| e.origin == Origin::Live));
        if replayed > 0 && replayed < got.len() {
            mixed += 1;
        }
        assert!(got[..replayed]
            .iter()
            .all(|e| e.generation == rt.generation()));
    }
    assert!(mixed > 0, "no watcher attached mid-turn");
    rt.shutdown().await;
}

async fn reattach_from_a_saved_cursor_after_the_runtime_restarts_body() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(SqliteEventLog::open(dir.path()).await.unwrap());
    let rt = runtime(dir.path(), log.clone()).await;
    let (client, _s) = connect(&rt).await;
    let session = client.new_session(SessionSpec::default()).await.unwrap();
    say(&client, &session, "before").await;
    let saved = 3;
    let old_generation = rt.generation();
    let before = log.latest_seq(&session.key).await.unwrap();
    assert!(before > saved);
    drop(client);
    rt.shutdown().await;
    drop(rt);

    let log = Arc::new(SqliteEventLog::open(dir.path()).await.unwrap());
    let rt = runtime(dir.path(), log.clone()).await;
    assert!(rt.generation() > old_generation);
    let (client, _s) = connect(&rt).await;
    let mut rx = client.subscribe();
    let attached = client.attach(&session, saved).await.unwrap();
    assert_eq!(attached.latest_seq, before);
    assert_eq!(attached.generation, rt.generation());
    let replay = collect(&mut rx, &session.key, before).await;
    assert_contiguous(&replay, saved);
    assert!(replay
        .iter()
        .all(|e| e.origin == Origin::Replay && e.generation == rt.generation()));
    // Resume work: live updates continue the same sequence.
    client
        .load_session(&session, SessionSpec::default())
        .await
        .unwrap();
    say(&client, &session, "after").await;
    let last = log.latest_seq(&session.key).await.unwrap();
    let live = collect(&mut rx, &session.key, last).await;
    assert_contiguous(&live, before);
    assert!(live.iter().all(|e| e.origin == Origin::Live));
    rt.shutdown().await;
}

async fn each_client_sees_only_its_own_sessions_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let (one, _s1) = connect(&rt).await;
    let (two, _s2) = connect(&rt).await;
    let (mut rx1, mut rx2) = (one.subscribe(), two.subscribe());
    let a = one.new_session(SessionSpec::default()).await.unwrap();
    let b = two.new_session(SessionSpec::default()).await.unwrap();
    tokio::join!(say(&one, &a, "a"), say(&two, &b, "b"));
    // Read each client's stream up to its own turn's end; anything of the
    // other Session would be among it or still queued behind it.
    async fn seen(
        rx: &mut Receiver<UpdateEnvelope>,
    ) -> (std::collections::HashSet<SessionKey>, usize) {
        let mut keys = std::collections::HashSet::new();
        let mut count = 0;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = rx.recv().await.unwrap();
                let ended = matches!(e.update, loop_protocol::SessionUpdate::TurnEnded { .. });
                keys.insert(e.session);
                count += 1;
                if ended {
                    break;
                }
            }
        })
        .await
        .expect("no turn end delivered");
        while let Ok(e) = rx.try_recv() {
            keys.insert(e.session);
            count += 1;
        }
        (keys, count)
    }
    let (k1, n1) = seen(&mut rx1).await;
    let (k2, n2) = seen(&mut rx2).await;
    assert!(n1 > 0 && n2 > 0);
    assert_eq!(k1, [a.key.clone()].into());
    assert_eq!(k2, [b.key.clone()].into());
    // Listing shows both, without subscribing.
    let list = one.list().await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().any(|s| s.r#ref == b));
    rt.shutdown().await;
}

async fn a_v1_client_works_and_unknown_methods_stay_unknown_without_control_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    // Served as before: no RuntimeControl, so no new methods or capabilities.
    let (server, client) = memory();
    let _served = serve_with_options(
        rt.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
            authority: loop_wire::Authority::HostLocal,
        },
    );
    let old = RemoteBackend::connect(client, info(), Arc::new(Reject))
        .await
        .unwrap();
    let session = old.new_session(SessionSpec::default()).await.unwrap();
    say(&old, &session, "hi").await;
    assert!(!old.capabilities().multi_session && !old.capabilities().attach);
    assert!(old.list().await.is_err());
    assert!(old.attach(&session, 0).await.is_err());
    assert!(old.status().await.is_err());
    assert!(old.stop().await.is_err());
    // A client of a Runtime connection that never uses the new methods too.
    let (new, _s) = connect(&rt).await;
    assert!(new.capabilities().multi_session && new.capabilities().attach);
    let session = new.new_session(SessionSpec::default()).await.unwrap();
    say(&new, &session, "hi").await;
    rt.shutdown().await;
}

async fn detach_stops_updates_and_close_unloads_the_session_body() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryEventLog::new());
    let rt = runtime(dir.path(), log.clone()).await;
    let (client, _s) = connect(&rt).await;
    let mut rx = client.subscribe();
    let session = client.new_session(SessionSpec::default()).await.unwrap();
    say(&client, &session, "one").await;
    client.detach(&session).await.unwrap();
    while rx.try_recv().is_ok() {}
    let before = log.latest_seq(&session.key).await.unwrap();
    // Detach does not cancel work: the Runtime keeps committing.
    rt.prompt(&session, vec![ContentBlock::text("two")])
        .await
        .unwrap()
        .outcome()
        .await;
    assert!(log.latest_seq(&session.key).await.unwrap() > before);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err(), "detached client still got updates");
    // Re-attach picks up exactly what it missed.
    let attached = client.attach(&session, before).await.unwrap();
    let last = log.latest_seq(&session.key).await.unwrap();
    assert_eq!(attached.latest_seq, last);
    let got = collect(&mut rx, &session.key, last).await;
    assert_contiguous(&got, before);

    client.close(&session, false).await.unwrap();
    let list = client.list().await.unwrap();
    assert!(!list.iter().find(|s| s.r#ref == session).unwrap().live);
    assert!(client.close(&session, false).await.is_err());
    let missing = SessionRef {
        native_id: "nope".into(),
        key: SessionKey("nope".into()),
        ..session.clone()
    };
    assert!(client.attach(&missing, 0).await.is_err());
    rt.shutdown().await;
}

async fn status_counts_loaded_and_busy_sessions_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let (client, _s) = connect(&rt).await;
    let status = client.status().await.unwrap();
    assert_eq!(status.pid, std::process::id());
    assert_eq!(status.generation, rt.generation());
    assert_eq!((status.loaded, status.busy), (0, 0));
    assert_eq!((status.max_sessions, status.max_active_turns), (16, 8));
    let a = client.new_session(SessionSpec::default()).await.unwrap();
    client.new_session(SessionSpec::default()).await.unwrap();
    assert_eq!(client.status().await.unwrap().loaded, 2);
    client.close(&a, false).await.unwrap();
    assert_eq!(client.status().await.unwrap().loaded, 1);
    // The Runtime alone does not own a process to stop.
    assert!(client.stop().await.is_err());
    rt.shutdown().await;
}

#[tokio::test]
async fn status_counts_loaded_and_busy_sessions() {
    tokio::time::timeout(
        Duration::from_secs(60),
        status_counts_loaded_and_busy_sessions_body(),
    )
    .await
    .expect("test timed out");
}

async fn an_observer_is_not_asked_to_answer_body() {
    use loop_wire::Connection;
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let (server, client) = memory();
    let served = serve_with_control(
        rt.clone(),
        rt.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
            authority: loop_wire::Authority::HostLocal,
        },
    );
    let (conn, mut incoming) = Connection::new(client);
    // Any request reaching the observer fails the test.
    let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = asked.clone();
    let reader = conn.clone();
    tokio::spawn(async move {
        while let Some(m) = incoming.recv().await {
            match m {
                loop_wire::Incoming::Request { .. } => {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst)
                }
                loop_wire::Incoming::Response { id, result } => reader.resolve(id, result).await,
                _ => {}
            }
        }
    });
    conn.request(
        "initialize",
        serde_json::json!({"protocol_version": 1, "client_info": {"name": "o", "version": "1"},
            "client_capabilities": {"interact": false}}),
    )
    .await
    .unwrap();
    let outcome = served
        .host
        .interact(InteractionRequest::Permission {
            id: "p".into(),
            tool_call: None,
            options: vec![],
        })
        .await;
    assert!(matches!(outcome, InteractionOutcome::Rejected { .. }));
    assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    rt.shutdown().await;
}

#[tokio::test]
async fn attaching_mid_turn_is_contiguous_without_gaps_or_duplicates() {
    tokio::time::timeout(
        Duration::from_secs(60),
        attaching_mid_turn_is_contiguous_without_gaps_or_duplicates_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn reattach_from_a_saved_cursor_after_the_runtime_restarts() {
    tokio::time::timeout(
        Duration::from_secs(60),
        reattach_from_a_saved_cursor_after_the_runtime_restarts_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn each_client_sees_only_its_own_sessions() {
    tokio::time::timeout(
        Duration::from_secs(60),
        each_client_sees_only_its_own_sessions_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_v1_client_works_and_unknown_methods_stay_unknown_without_control() {
    tokio::time::timeout(
        Duration::from_secs(60),
        a_v1_client_works_and_unknown_methods_stay_unknown_without_control_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn detach_stops_updates_and_close_unloads_the_session() {
    tokio::time::timeout(
        Duration::from_secs(60),
        detach_stops_updates_and_close_unloads_the_session_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn an_observer_is_not_asked_to_answer() {
    tokio::time::timeout(
        Duration::from_secs(60),
        an_observer_is_not_asked_to_answer_body(),
    )
    .await
    .expect("test timed out");
}

async fn rejected_prompt_does_not_subscribe_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let (owner, _s1) = connect(&rt).await;
    let (other, _s2) = connect(&rt).await;
    let mut rx = other.subscribe();
    let session = owner.new_session(SessionSpec::default()).await.unwrap();
    let wrong = SessionRef {
        native_id: "wrong".into(),
        ..session.clone()
    };
    let rejected = other
        .prompt(&wrong, vec![ContentBlock::text("no")])
        .await
        .unwrap()
        .outcome()
        .await;
    assert!(!matches!(rejected, TurnOutcome::Completed(_)));
    say(&owner, &session, "mine").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        rx.try_recv().is_err(),
        "a rejected prompt subscribed the connection"
    );
}

/// Holds `attach` until released.
struct Gate {
    rt: Arc<Runtime>,
    open: Notify,
}
#[async_trait::async_trait]
impl RuntimeControl for Gate {
    async fn list(&self) -> loop_protocol::BackendResult<Vec<loop_protocol::rpc::SessionInfo>> {
        RuntimeControl::list(&*self.rt).await
    }
    async fn close(&self, s: &SessionRef, f: bool) -> loop_protocol::BackendResult<()> {
        RuntimeControl::close(&*self.rt, s, f).await
    }
    async fn attach(&self, s: &SessionRef, a: u64) -> loop_protocol::BackendResult<Attachment> {
        let snapshot = RuntimeControl::attach(&*self.rt, s, a).await;
        self.open.notified().await;
        snapshot
    }
}

async fn prompt_completes_only_after_its_end_update_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let gate = Arc::new(Gate {
        rt: rt.clone(),
        open: Notify::new(),
    });
    let (server, client) = memory();
    let _served = serve_with_control(
        rt.clone(),
        gate.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
            authority: loop_wire::Authority::HostLocal,
        },
    );
    let remote = RemoteBackend::connect(client, info(), Arc::new(Reject))
        .await
        .unwrap();
    let session = remote.new_session(SessionSpec::default()).await.unwrap();
    let mut rx = remote.subscribe();
    let attaching = {
        let (remote, session) = (remote.clone(), session.clone());
        tokio::spawn(async move { remote.attach(&session, 0).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let turn = remote
        .prompt(&session, vec![ContentBlock::text("x")])
        .await
        .unwrap();
    // The turn finishes in the Runtime while the snapshot is still held.
    while rt.is_busy(&session) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.open.notify_one();
    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
    attaching.await.unwrap().unwrap();
    let mut ended = false;
    while let Ok(e) = rx.try_recv() {
        ended |= matches!(e.update, loop_protocol::SessionUpdate::TurnEnded { .. });
    }
    assert!(
        ended,
        "the prompt completed before its turn_ended was delivered"
    );
}

async fn overlapping_attaches_both_get_a_typed_answer_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let (owner, _s) = connect(&rt).await;
    let session = owner.new_session(SessionSpec::default()).await.unwrap();
    say(&owner, &session, "a").await;
    let mut rx = owner.subscribe();
    let (a, b) = tokio::join!(owner.attach(&session, 0), owner.attach(&session, 0));
    // One may be superseded, but none may answer with a non-attach result.
    assert!(a.is_ok() || b.is_ok());
    say(&owner, &session, "b").await;
    let last = rt.event_log().latest_seq(&session.key).await.unwrap();
    let got = collect(&mut rx, &session.key, last).await;
    assert!(got.last().unwrap().seq == last);
}

async fn detaching_mid_turn_clears_observed_busy_body() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
    let (owner, _s1) = connect(&rt).await;
    let (watcher, _s2) = connect(&rt).await;
    let session = owner.new_session(SessionSpec::default()).await.unwrap();
    let mut rx = watcher.subscribe();
    watcher.attach(&session, 0).await.unwrap();
    let producer = {
        let (owner, session) = (owner.clone(), session.clone());
        tokio::spawn(async move {
            for i in 0..10 {
                say(&owner, &session, &format!("{i}")).await;
            }
        })
    };
    loop {
        let e = rx.recv().await.unwrap();
        if matches!(e.update, loop_protocol::SessionUpdate::TurnStarted { .. }) {
            break;
        }
    }
    watcher.detach(&session).await.unwrap();
    producer.await.unwrap();
    assert!(!watcher.is_busy(&session));
    watcher
        .load_session(&session, SessionSpec::default())
        .await
        .unwrap();
    say(&watcher, &session, "mine").await;
}

#[tokio::test]
async fn rejected_prompt_does_not_subscribe() {
    tokio::time::timeout(
        Duration::from_secs(60),
        rejected_prompt_does_not_subscribe_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn prompt_completes_only_after_its_end_update() {
    tokio::time::timeout(
        Duration::from_secs(60),
        prompt_completes_only_after_its_end_update_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn overlapping_attaches_both_get_a_typed_answer() {
    tokio::time::timeout(
        Duration::from_secs(60),
        overlapping_attaches_both_get_a_typed_answer_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn detaching_mid_turn_clears_observed_busy() {
    tokio::time::timeout(
        Duration::from_secs(60),
        detaching_mid_turn_clears_observed_busy_body(),
    )
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn tree_wire_reset_detaches_only_legacy_session_and_rename_is_metadata() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use loop_protocol::rpc::*;
        use loop_protocol::SessionUpdate;
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone()).await;
        let (legacy, _legacy_server) = connect(&rt).await;
        let source = legacy.new_session(SessionSpec::default()).await.unwrap();
        let other = legacy.new_session(SessionSpec::default()).await.unwrap();
        say(&legacy, &source, "source prompt").await;
        let (server, client) = memory();
        let _capable_server = serve_with_control(rt.clone(), rt.clone(), server, info(), ServeOptions { owns_backend: false, authority: loop_wire::Authority::HostLocal });
        let capable = RemoteBackend::connect_with_capabilities(client,info(),Arc::new(Reject),ClientCapabilities { interact:true,branch_reset:true }).await.unwrap();
        let mut capable_rx = capable.subscribe();
        let attached = capable.attach(&source,log.latest_seq(&source.key).await.unwrap()).await.unwrap();
        let snapshot = attached.snapshot.unwrap();
        let original_view = snapshot.view_id.clone().unwrap();
        let mut legacy_rx = legacy.subscribe();
        let renamed = capable.rename(RenameParams { session:source.clone(),name:"  renamed  ".into(),expected_revision:Some(snapshot.revision) }).await.unwrap();
        for receiver in [&mut capable_rx,&mut legacy_rx] {
            let events = collect(receiver,&source.key,renamed.seq).await;
            assert!(matches!(events.last().unwrap().update,SessionUpdate::ConfigChanged(ref changed) if changed.metadata.name.as_deref() == Some("renamed")));
            assert!(!events.iter().any(|event| matches!(event.update,SessionUpdate::BranchReset { .. })));
        }
        let tree = capable.tree(TreeParams { session:source.clone(),cursor:None,limit:Some(1) }).await.unwrap();
        let point = tree.fork_points[0].entry_id.as_str();
        let fork = capable.fork(ForkParams { session:source.clone(),before_entry_id:point.into(),name:None,request_id:"wire-fork".into() }).await.unwrap();
        assert_ne!(fork.session,source);
        assert_eq!(fork.draft.as_deref(),Some("source prompt"));
        assert_eq!(rt.watcher_count(&source.key),2);
        let reset = capable.navigate(NavigateParams { session:source.clone(),target_entry_id:None,summarize:false,expected_revision:Some(renamed.revision) }).await.unwrap();
        let events = collect(&mut capable_rx,&source.key,reset.seq).await;
        let view_id = match &events.last().unwrap().update {
            SessionUpdate::BranchReset { view_id,.. } => view_id.clone(),
            other => panic!("expected reset, got {other:?}"),
        };
        let events = collect(&mut legacy_rx,&source.key,reset.seq).await;
        assert!(matches!(&events.last().unwrap().update,SessionUpdate::Notice { text,.. } if text == "this Session changed branch; attach again"));
        assert!(!events.iter().any(|event| matches!(event.update,SessionUpdate::BranchReset { .. })));
        while rt.watcher_count(&source.key) != 1 { tokio::task::yield_now().await; }
        assert_eq!(rt.watcher_count(&other.key),1);
        let reset_view = capable.view(ViewParams { session:source.clone(),view_id,cursor:None,limit:None }).await.unwrap();
        assert!(reset_view.updates.is_empty());
        let old_view = capable.view(ViewParams { session:source.clone(),view_id:original_view,cursor:None,limit:None }).await.unwrap();
        assert!(!old_view.updates.is_empty(),"old snapshot must stay immutable");
        legacy.attach(&source,reset.seq).await.unwrap();
        say(&capable,&source,"after reset").await;
        let end = log.latest_seq(&source.key).await.unwrap();
        assert!(!collect(&mut legacy_rx,&source.key,end).await.is_empty());
        let payload = capable.export(SessionParams { session:source.clone() }).await.unwrap();
        let imported = capable.import(ImportParams { workspace_id:fork.workspace_id,jsonl:payload.jsonl,request_id:"wire-import".into() }).await.unwrap();
        assert_ne!(imported.session,source);
        rt.shutdown().await;
    }).await.unwrap();
}

/// Exact-text message chunks among `events`.
fn texts(events: &[UpdateEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.update {
            loop_protocol::SessionUpdate::MessageChunk { content, .. } => {
                content.as_text().map(str::to_owned)
            }
            _ => None,
        })
        .collect()
}
fn detach_notices(events: &[UpdateEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| matches!(&event.update, loop_protocol::SessionUpdate::Notice { text, .. } if text == "this Session changed branch; attach again"))
        .count()
}

#[tokio::test]
async fn a_legacy_reattach_recovers_the_current_branch() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use loop_protocol::rpc::*;
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone()).await;
        let (legacy, _legacy_server) = connect(&rt).await;
        let source = legacy.new_session(SessionSpec::default()).await.unwrap();
        say(&legacy, &source, "first branch").await;
        say(&legacy, &source, "second branch").await;
        let (server, client) = memory();
        let _capable_server = serve_with_control(
            rt.clone(),
            rt.clone(),
            server,
            info(),
            ServeOptions {
                owns_backend: false,
                authority: loop_wire::Authority::HostLocal,
            },
        );
        let capable = RemoteBackend::connect_with_capabilities(
            client,
            info(),
            Arc::new(Reject),
            ClientCapabilities {
                interact: true,
                branch_reset: true,
            },
        )
        .await
        .unwrap();
        capable
            .attach(&source, log.latest_seq(&source.key).await.unwrap())
            .await
            .unwrap();
        let tree = capable
            .tree(TreeParams {
                session: source.clone(),
                cursor: None,
                limit: None,
            })
            .await
            .unwrap();
        let second = tree
            .entries
            .iter()
            .find(|entry| entry.text.as_deref() == Some("second branch"))
            .unwrap();
        let mut legacy_rx = legacy.subscribe();
        let reset = capable
            .navigate(NavigateParams {
                session: source.clone(),
                target_entry_id: second.parent_id.clone(),
                summarize: false,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(
            detach_notices(&collect(&mut legacy_rx, &source.key, reset.seq).await),
            1
        );

        // Reattaching at the cursor the detach left starts from the current branch.
        legacy.attach(&source, reset.seq).await.unwrap();
        say(&capable, &source, "third branch").await;
        let events = collect(
            &mut legacy_rx,
            &source.key,
            log.latest_seq(&source.key).await.unwrap(),
        )
        .await;
        let seen = texts(&events);
        assert_eq!(
            seen.iter().filter(|text| *text == "first branch").count(),
            1,
            "{seen:?}"
        );
        assert!(!seen.iter().any(|text| text == "second branch"), "{seen:?}");
        assert!(seen.iter().any(|text| text == "third branch"), "{seen:?}");
        assert_eq!(detach_notices(&events), 0);

        // A fresh legacy attach from zero is not detached by the historical reset.
        let (fresh, _fresh_server) = connect(&rt).await;
        let mut fresh_rx = fresh.subscribe();
        fresh.attach(&source, 0).await.unwrap();
        say(&capable, &source, "fourth branch").await;
        let events = collect(
            &mut fresh_rx,
            &source.key,
            log.latest_seq(&source.key).await.unwrap(),
        )
        .await;
        let seen = texts(&events);
        assert_eq!(detach_notices(&events), 0);
        assert_eq!(
            seen.iter().filter(|text| *text == "first branch").count(),
            1,
            "{seen:?}"
        );
        assert!(!seen.iter().any(|text| text == "second branch"), "{seen:?}");
        assert!(seen.iter().any(|text| text == "fourth branch"), "{seen:?}");
        assert!(rt.watcher_count(&source.key) >= 3);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// Compaction resets the branch: only a reset-capable watcher receives
/// BranchReset, a legacy one is detached, and the revision it carries is
/// the one a reopened Session counts from its tree.
#[tokio::test]
async fn compaction_resets_capable_watchers_and_detaches_legacy_ones() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use loop_protocol::rpc::ClientCapabilities;
        use loop_protocol::{BackendError, ModelControl, SessionUpdate};
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone()).await;
        let (legacy, _legacy_server) = connect(&rt).await;
        let source = legacy.new_session(SessionSpec::default()).await.unwrap();
        // Enough earlier context to leave something behind the kept tail.
        say(&legacy, &source, &"early context ".repeat(8000)).await;
        say(&legacy, &source, "recent").await;
        let (server, client) = memory();
        let _capable_server = serve_with_control(
            rt.clone(),
            rt.clone(),
            server,
            info(),
            ServeOptions {
                owns_backend: false,
                authority: loop_wire::Authority::HostLocal,
            },
        );
        let capable = RemoteBackend::connect_with_capabilities(
            client,
            info(),
            Arc::new(Reject),
            ClientCapabilities {
                interact: true,
                branch_reset: true,
            },
        )
        .await
        .unwrap();
        capable
            .attach(&source, log.latest_seq(&source.key).await.unwrap())
            .await
            .unwrap();
        let mut legacy_rx = legacy.subscribe();
        let mut capable_rx = capable.subscribe();
        let report = capable
            .compaction()
            .expect("the Runtime offers compaction")
            .compact(&source, Some("keep it short"))
            .await
            .unwrap();
        assert!(report.tokens_before.is_some_and(|t| t > 0));
        assert!(report.branch_revision >= 1);
        let events = collect(&mut capable_rx, &source.key, report.seq).await;
        let revision = events
            .iter()
            .find_map(|e| match &e.update {
                SessionUpdate::BranchReset {
                    revision,
                    branch_revision,
                    reason,
                    ..
                } if e.seq == report.seq => {
                    assert_eq!(reason, "compact");
                    assert_eq!(*branch_revision, report.branch_revision);
                    Some(*revision)
                }
                _ => None,
            })
            .expect("the capable watcher receives BranchReset");
        let legacy_events = collect(&mut legacy_rx, &source.key, report.seq).await;
        assert_eq!(detach_notices(&legacy_events), 1);
        assert!(!legacy_events
            .iter()
            .any(|e| matches!(e.update, SessionUpdate::BranchReset { .. })));

        let stats = capable
            .session_stats()
            .unwrap()
            .stats(&source)
            .await
            .unwrap();
        assert_eq!(stats.turn_state.as_deref(), Some("idle"));
        assert!(stats.context_window.is_some_and(|w| w > 0));

        // A user command holds the Session busy: compaction and prompts
        // are refused meanwhile, and a cancel stops the command.
        let shell = tokio::spawn({
            let capable = capable.clone();
            let source = source.clone();
            async move {
                capable
                    .local_shell()
                    .unwrap()
                    .run(&source, "sleep 30", None)
                    .await
            }
        });
        while !rt.is_busy(&source) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            capable
                .compaction()
                .unwrap()
                .compact(&source, None)
                .await
                .unwrap_err(),
            BackendError::Busy
        );
        assert!(matches!(
            rt.prompt(&source, vec![ContentBlock::text("no")]).await,
            Err(BackendError::Busy)
        ));
        capable.cancel(&source);
        let stopped = shell.await.unwrap().unwrap_err();
        assert!(stopped.to_string().contains("cancelled"), "{stopped}");
        assert!(!rt.is_busy(&source));
        let out = capable
            .local_shell()
            .unwrap()
            .run(&source, "printf ok", None)
            .await
            .unwrap();
        assert_eq!(out.stdout, "ok");
        // In-process Runtime and Remote answer alike.
        let command = "printf out; printf err >&2; exit 4";
        assert_eq!(
            rt.local_shell()
                .unwrap()
                .run(&source, command, None)
                .await
                .unwrap(),
            capable
                .local_shell()
                .unwrap()
                .run(&source, command, None)
                .await
                .unwrap()
        );
        let here = rt.session_stats().unwrap().stats(&source).await.unwrap();
        let there = capable
            .session_stats()
            .unwrap()
            .stats(&source)
            .await
            .unwrap();
        assert_eq!(
            (here.total_tokens, here.context_window, here.turn_state),
            (there.total_tokens, there.context_window, there.turn_state)
        );

        // Reopened from its tree, the Session counts the same revision.
        capable.detach(&source).await.unwrap();
        legacy.detach(&source).await.unwrap();
        rt.close(&source.key, false).await.unwrap();
        rt.load_session(&source, SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(rt.models(&source).await.unwrap().revision, revision);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
