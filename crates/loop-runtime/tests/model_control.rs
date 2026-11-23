//! Session model and effort control on a shared Runtime and over the wire:
//! capability parity, `config_changed` to every watcher, attach snapshots,
//! restart, explicit off, busy and concurrent setters.

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    AgentBackend, BackendError, ConfigChanged, ContentBlock, HostServices, InteractionOutcome,
    InteractionRequest, ModelControl, SessionKey, SessionSpec, SessionUpdate, UpdateEnvelope,
};
use loop_runtime::log::{EventKind, EventLog, MemoryEventLog, SqliteEventLog};
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_control, ServeOptions, Served};
use loop_wire::transport::memory;
use loop_wire::RemoteBackend;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

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

fn reasoning(template: &loop_ai::Model, id: &str) -> loop_ai::Model {
    let mut model = template.clone();
    model.id = id.into();
    model.name = id.into();
    model.reasoning = true;
    model
}

/// A Runtime whose faux models accept reasoning, with `medium` as the saved
/// default effort. `script` answers prompts.
async fn runtime(
    dir: &std::path::Path,
    log: Arc<dyn EventLog>,
    script: loop_ai::providers::FauxScript,
) -> Arc<Runtime> {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"defaultThinkingLevel":"medium"}"#,
    )
    .unwrap();
    let provider = loop_ai::providers::faux_provider(script);
    let template = provider.get_model("faux-model").unwrap();
    provider.set_models(vec![
        reasoning(&template, "faux-model"),
        reasoning(&template, "second"),
    ]);
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(provider);
    let globals = GlobalResources::with_models(agent, models).unwrap();
    {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(globals, opts(dir), log, store, RuntimeLimits::default())
    }
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

/// The next `config_changed` of `key`, with its sequence.
async fn config_changed(
    rx: &mut Receiver<UpdateEnvelope>,
    key: &SessionKey,
) -> (u64, ConfigChanged) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for config_changed")
            .unwrap();
        if &envelope.session != key {
            continue;
        }
        if let SessionUpdate::ConfigChanged(changed) = envelope.update {
            return (envelope.seq, changed);
        }
    }
}

#[tokio::test]
async fn capabilities_and_services_agree_on_native_runtime_and_remote() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            Default::default(),
        )
        .await;
        let (remote, _served) = connect(&rt).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();

        let caps = rt.capabilities();
        let remote_caps = remote.capabilities();
        for (name, runtime, remote) in [
            ("set_model", caps.set_model, remote_caps.set_model),
            ("efforts", caps.efforts, remote_caps.efforts),
            ("modes", caps.modes, remote_caps.modes),
            (
                "attach_snapshot",
                caps.attach_snapshot,
                remote_caps.attach_snapshot,
            ),
        ] {
            assert_eq!(runtime, remote, "{name} differs between Runtime and Remote");
        }
        assert!(caps.set_model && caps.efforts && caps.attach_snapshot);
        assert!(!caps.modes, "native Sessions offer no backend modes yet");
        assert!(rt.model_control().is_some());
        assert!(remote.model_control().is_some());

        // The native backend underneath advertises the same model services.
        let native_caps = native().await.capabilities();
        assert_eq!(
            (
                native_caps.set_model,
                native_caps.efforts,
                native_caps.modes
            ),
            (caps.set_model, caps.efforts, caps.modes)
        );

        // Same answers through the Runtime and over the wire.
        let control = remote.model_control().unwrap();
        assert_eq!(
            control.efforts(&session).await.unwrap(),
            rt.efforts(&session).await.unwrap()
        );
        assert_eq!(
            control.models(&session).await.unwrap(),
            rt.models(&session).await.unwrap()
        );
        let models = remote.session_models(&session).await.unwrap();
        assert!(models.models.iter().all(|m| m.backend.as_str() == "loop"));
        assert_eq!(models.current.as_deref(), Some("faux/faux-model"));
        // Unsupported services answer Unsupported, not "unknown method".
        assert!(matches!(
            control.modes(&session).await,
            Err(BackendError::Unsupported(_))
        ));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn two_watchers_see_each_committed_change_at_the_setters_sequence() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let (owner, _s1) = connect(&rt).await;
        let (watcher, _s2) = connect(&rt).await;
        let session = owner.new_session(SessionSpec::default()).await.unwrap();
        let mut a = owner.subscribe();
        let mut b = watcher.subscribe();
        let attached = watcher.attach(&session, 0).await.unwrap();
        let snapshot = attached.snapshot.expect("the Runtime sends a snapshot");
        assert_eq!(snapshot.config.effort.as_deref(), Some("medium"));
        assert_eq!(snapshot.at_seq, attached.latest_seq);

        let control = owner.model_control().unwrap();
        let result = control
            .set_effort(&session, "high", Some(snapshot.revision))
            .await
            .unwrap();
        assert!(!result.pending);
        for rx in [&mut a, &mut b] {
            let (seq, changed) = config_changed(rx, &session.key).await;
            assert_eq!(seq, result.seq);
            assert_eq!(changed.revision, result.revision);
            assert_eq!(changed.config.effort.as_deref(), Some("high"));
        }
        let result = control
            .set_model(&session, "faux/second", Some(result.revision))
            .await
            .unwrap();
        for rx in [&mut a, &mut b] {
            let (seq, changed) = config_changed(rx, &session.key).await;
            assert_eq!(seq, result.seq);
            assert_eq!(changed.config.model.as_deref(), Some("faux/second"));
        }
        // Committed before the reply: the Event Log already has it.
        let logged = log
            .read_after(&loop_store::Tenant::local(), &session.key, 0)
            .await
            .unwrap();
        assert!(logged.iter().any(|r| matches!(
            &r.event.kind,
            EventKind::Update(e) if e.seq == result.seq
                && matches!(e.update, SessionUpdate::ConfigChanged(_))
        )));
        // A stale revision conflicts over the wire with the current one.
        assert_eq!(
            control
                .set_effort(&session, "low", Some(0))
                .await
                .unwrap_err(),
            BackendError::Conflict {
                revision: result.revision
            }
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_change_survives_detach_and_reattach() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            Default::default(),
        )
        .await;
        let (owner, _s1) = connect(&rt).await;
        let (watcher, _s2) = connect(&rt).await;
        let session = owner.new_session(SessionSpec::default()).await.unwrap();
        let first = watcher.attach(&session, 0).await.unwrap();
        watcher.detach(&session).await.unwrap();

        // Changed while the watcher was away.
        let control = owner.model_control().unwrap();
        control.set_effort(&session, "off", None).await.unwrap();
        let last = control
            .set_model(&session, "faux/second", None)
            .await
            .unwrap();

        let mut rx = watcher.subscribe();
        let again = watcher.attach(&session, first.latest_seq).await.unwrap();
        let snapshot = again.snapshot.unwrap();
        assert_eq!(again.latest_seq, last.seq);
        assert_eq!(snapshot.at_seq, last.seq);
        assert_eq!(snapshot.revision, last.revision);
        assert_eq!(snapshot.config.effort.as_deref(), Some("off"));
        assert_eq!(snapshot.config.model.as_deref(), Some("faux/second"));
        assert!(!snapshot.pending);
        // Replay brings the missed updates, ending with the latest change.
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(changed.config.effort.as_deref(), Some("off"));
        assert!(seq > first.latest_seq);
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, last.seq);
        assert_eq!(changed.config.model.as_deref(), Some("faux/second"));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn model_and_explicit_off_survive_a_runtime_restart() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let session = {
            let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
            let rt = runtime(dir.path(), log, Default::default()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            assert_eq!(
                rt.efforts(&session).await.unwrap().current.as_deref(),
                Some("medium")
            );
            rt.set_model(&session, "faux/second", None).await.unwrap();
            rt.set_effort(&session, "off", None).await.unwrap();
            rt.shutdown().await;
            session
        };
        let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
        let rt = runtime(dir.path(), log, Default::default()).await;
        // Not loaded yet: an attach snapshot reads the Session's tree.
        let (watcher, _s) = connect(&rt).await;
        let attached = watcher.attach(&session, 0).await.unwrap();
        let recorded = attached.snapshot.unwrap();
        assert_eq!(recorded.config.effort.as_deref(), Some("off"));
        assert_eq!(recorded.config.model.as_deref(), Some("faux/second"));
        assert_eq!(recorded.revision, 2);
        watcher.detach(&session).await.unwrap();

        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let efforts = rt.efforts(&session).await.unwrap();
        assert_eq!(
            efforts.current.as_deref(),
            Some("off"),
            "an explicit off stays off"
        );
        assert_eq!(efforts.revision, 2, "revisions continue across restarts");
        assert_eq!(
            rt.models(&session).await.unwrap().current.as_deref(),
            Some("faux/second"),
            "the Session's model wins over the Runtime's startup default"
        );
        // A Session that never chose inherits the saved default.
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert_eq!(
            rt.efforts(&fresh).await.unwrap().current.as_deref(),
            Some("medium")
        );
        assert_eq!(
            rt.models(&fresh).await.unwrap().current.as_deref(),
            Some("faux/faux-model")
        );
        assert_eq!(
            rt.set_effort(&session, "low", Some(1)).await.unwrap_err(),
            BackendError::Conflict { revision: 2 }
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn during_a_turn_the_model_and_the_effort_wait_for_the_next_turn() {
    tokio::time::timeout(Duration::from_secs(30), async {
        use loop_ai::providers::{FauxResponse, FauxScript};
        let script = FauxScript::new();
        script.push(FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
            id: "call".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo waiting"}),
            thought_signature: None,
        }]));
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone(), script).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let mut rx = rt.subscribe();
        // Nobody can answer, so the turn waits on its Escalation.
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run it")])
            .await
            .unwrap();
        loop {
            let raised = log
                .read_runtime_after(
                    loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
                    0,
                )
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationRaised(_)));
            if raised {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let model = rt.set_model(&session, "faux/second", None).await.unwrap();
        assert!(model.pending);
        assert!(model.applies_after_turn.is_some());
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, model.seq);
        assert!(changed.pending);
        assert_eq!(changed.config.model.as_deref(), Some("faux/second"));
        assert_ne!(changed.effective.model.as_deref(), Some("faux/second"));
        let result = rt.set_effort(&session, "high", None).await.unwrap();
        assert!(result.pending);
        assert!(result.applies_after_turn.is_some());
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, result.seq);
        assert!(changed.pending);
        assert_eq!(changed.config.effort.as_deref(), Some("high"));
        assert_eq!(changed.effective.effort.as_deref(), Some("medium"));

        rt.cancel(&session);
        turn.outcome().await;
        let (_, applied) = config_changed(&mut rx, &session.key).await;
        assert!(!applied.pending);
        assert_eq!(applied.revision, result.revision);
        assert_eq!(applied.effective.effort.as_deref(), Some("high"));
        assert_eq!(applied.effective.model.as_deref(), Some("faux/second"));
        // Now idle: a model choice is no longer pending.
        let idle = rt.set_model(&session, "faux/second", None).await.unwrap();
        assert!(!idle.pending);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_setters_over_the_wire_commit_in_revision_order() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            Default::default(),
        )
        .await;
        let (first, _s1) = connect(&rt).await;
        let (second, _s2) = connect(&rt).await;
        let session = first.new_session(SessionSpec::default()).await.unwrap();
        second.attach(&session, 0).await.unwrap();
        let levels = [
            "off", "low", "medium", "high", "max", "minimal", "xhigh", "low",
        ];
        let tasks: Vec<_> = levels
            .into_iter()
            .enumerate()
            .map(|(i, level)| {
                let client = if i % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let session = session.clone();
                tokio::spawn(async move {
                    let result = client
                        .model_control()
                        .unwrap()
                        .set_effort(&session, level, None)
                        .await
                        .unwrap();
                    (result, level)
                })
            })
            .collect();
        let mut done = Vec::new();
        for task in tasks {
            done.push(task.await.unwrap());
        }
        done.sort_by_key(|(result, _)| result.revision);
        let revisions: Vec<u64> = done.iter().map(|(r, _)| r.revision).collect();
        assert_eq!(revisions, (1..=levels.len() as u64).collect::<Vec<_>>());
        // Later revisions were committed later.
        assert!(done.windows(2).all(|w| w[0].0.seq < w[1].0.seq));
        let (last, level) = done.last().unwrap();
        let efforts = rt.efforts(&session).await.unwrap();
        assert_eq!(efforts.current.as_deref(), Some(*level));
        assert_eq!(efforts.revision, last.revision);

        // Racing on the same expected revision: one wins, one conflicts.
        let racers: Vec<_> = [(first.clone(), "off"), (second.clone(), "high")]
            .into_iter()
            .map(|(client, level)| {
                let session = session.clone();
                let expected = last.revision;
                tokio::spawn(async move {
                    client
                        .model_control()
                        .unwrap()
                        .set_effort(&session, level, Some(expected))
                        .await
                })
            })
            .collect();
        let mut won = 0;
        for racer in racers {
            match racer.await.unwrap() {
                Ok(_) => won += 1,
                Err(error) => assert_eq!(
                    error,
                    BackendError::Conflict {
                        revision: last.revision + 1
                    }
                ),
            }
        }
        assert_eq!(won, 1);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A standalone native backend, as the terminal app uses it.
async fn native() -> Arc<loop_agent::native_backend::NativeBackend> {
    use loop_agent::harness::{
        create_in_memory_session_store, create_session_repository, AgentHarness,
        AgentHarnessOptions, ExecutionEnv, HostExecutionEnv, SandboxMode,
    };
    let models = Arc::new(loop_ai::Models::new());
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        model: models.get_model("faux", "faux-model"),
        models,
        session: repo.create(None, None).await.unwrap(),
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    loop_agent::native_backend::NativeBackend::new(harness).await
}

/// Holds Update appends while `held`, to stall the Event Log.
#[derive(Default)]
struct GatedLog {
    inner: MemoryEventLog,
    held: std::sync::atomic::AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl EventLog for GatedLog {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        if events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Update(_)))
            && self.held.load(std::sync::atomic::Ordering::Acquire)
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        tenant: &loop_store::Tenant,
        key: &SessionKey,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_after(tenant, key, seq).await
    }
    async fn read_runtime_after(
        &self,
        scope: loop_store::Scope<'_>,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_runtime_after(scope, seq).await
    }
    async fn latest_seq(
        &self,
        tenant: &loop_store::Tenant,
        key: &SessionKey,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(tenant, key).await
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
    async fn sync(&self) -> loop_runtime::log::Result<()> {
        self.inner.sync().await
    }
    async fn read_session_page(
        &self,
        tenant: &loop_store::Tenant,
        session: &loop_protocol::SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<loop_runtime::log::LoggedEvent>> {
        self.inner
            .read_session_page(tenant, session, after_seq, through_seq, limit)
            .await
    }
    async fn session_summary(
        &self,
        tenant: &loop_store::Tenant,
        session: &loop_protocol::SessionKey,
    ) -> loop_runtime::log::Result<Option<loop_runtime::log::SessionSummary>> {
        self.inner.session_summary(tenant, session).await
    }
    async fn recovery_page(
        &self,
        scope: loop_store::Scope<'_>,
        from_generation: u64,
        after_global: u64,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<loop_runtime::log::LoggedEvent>> {
        self.inner
            .recovery_page(scope, from_generation, after_global, limit)
            .await
    }
}

#[tokio::test]
async fn a_setter_waiting_on_a_stalled_event_log_holds_up_nothing_else() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(GatedLog::default());
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        log.held.store(true, std::sync::atomic::Ordering::Release);
        let setter = tokio::spawn({
            let rt = rt.clone();
            let session = session.clone();
            async move { rt.set_effort(&session, "high", None).await }
        });
        log.started.notified().await;

        // Attach, listing and prompt admission go ahead meanwhile.
        let quick = Duration::from_secs(5);
        let (watcher, _served) = connect(&rt).await;
        let mut rx = watcher.subscribe();
        let attached = tokio::time::timeout(quick, watcher.attach(&session, 0))
            .await
            .expect("attach waits for no setter")
            .unwrap();
        let snapshot = attached.snapshot.unwrap();
        assert_eq!(snapshot.config.effort.as_deref(), Some("medium"));
        tokio::time::timeout(quick, rt.list())
            .await
            .unwrap()
            .unwrap();
        let turn = tokio::time::timeout(
            quick,
            rt.prompt(&session, vec![ContentBlock::text("meanwhile")]),
        )
        .await
        .expect("admission waits for no setter")
        .unwrap();
        assert!(!setter.is_finished(), "the setter waits for its commit");

        log.held.store(false, std::sync::atomic::Ordering::Release);
        log.release.notify_one();
        let result = setter.await.unwrap().unwrap();
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, result.seq);
        assert!(seq > snapshot.at_seq);
        assert_eq!(changed.revision, result.revision);
        turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// The config a Session's tree records, read by a separate reader, as after
/// a crash.
async fn recorded(
    dir: &std::path::Path,
    session: &loop_protocol::SessionRef,
) -> loop_agent::harness::session::types::SessionConfigEntries {
    use loop_agent::harness::SessionStore;
    let db = loop_app_core::config::paths::sessions_db_path(&dir.join("agent"));
    let store = loop_agent::harness::session::sqlite::SqliteSessionStore::open(db).unwrap();
    let reader = store.load(&session.native_id).await.unwrap();
    let head = reader.read_head().await.unwrap();
    let entries = reader.read_entries(None).await.unwrap();
    loop_agent::harness::session::types::config_from_entries(&entries, head.as_deref())
}

#[tokio::test]
async fn a_change_acknowledged_during_a_turn_survives_a_crash() {
    tokio::time::timeout(Duration::from_secs(30), async {
        use loop_ai::providers::{FauxResponse, FauxScript};
        let script = FauxScript::new();
        script.push(FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
            id: "call".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo waiting"}),
            thought_signature: None,
        }]));
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
        let rt = runtime(dir.path(), log.clone(), script).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run it")])
            .await
            .unwrap();
        loop {
            let raised = log
                .read_runtime_after(
                    loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
                    0,
                )
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationRaised(_)));
            if raised {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let result = rt.set_effort(&session, "high", None).await.unwrap();
        assert!(result.pending);
        // Durable as soon as it is acknowledged, while the turn still runs.
        let now = recorded(dir.path(), &session).await;
        assert_eq!(
            now.thinking_level,
            Some(loop_agent::AgentThinkingLevel::High)
        );
        assert_eq!(now.revision, result.revision);
        // The turn never finishes normally; the Session comes back with it.
        rt.cancel(&session);
        turn.outcome().await;
        rt.shutdown().await;
        let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
        let rt = runtime(dir.path(), log, Default::default()).await;
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let efforts = rt.efforts(&session).await.unwrap();
        assert_eq!(efforts.current.as_deref(), Some("high"));
        assert_eq!(efforts.revision, result.revision);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// Refuses to commit `config_changed` updates while `failing`.
struct FailingLog {
    inner: Arc<MemoryEventLog>,
    failing: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl EventLog for FailingLog {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        let config = events.iter().any(|e| {
            matches!(
                &e.kind,
                EventKind::Update(UpdateEnvelope {
                    update: SessionUpdate::ConfigChanged(_),
                    ..
                })
            )
        });
        if config && self.failing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(loop_runtime::log::EventLogError::Invalid(
                "injected failure".into(),
            ));
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        tenant: &loop_store::Tenant,
        key: &SessionKey,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_after(tenant, key, seq).await
    }
    async fn read_runtime_after(
        &self,
        scope: loop_store::Scope<'_>,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_runtime_after(scope, seq).await
    }
    async fn latest_seq(
        &self,
        tenant: &loop_store::Tenant,
        key: &SessionKey,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(tenant, key).await
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
    async fn sync(&self) -> loop_runtime::log::Result<()> {
        self.inner.sync().await
    }
    async fn read_session_page(
        &self,
        tenant: &loop_store::Tenant,
        session: &loop_protocol::SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<loop_runtime::log::LoggedEvent>> {
        self.inner
            .read_session_page(tenant, session, after_seq, through_seq, limit)
            .await
    }
    async fn session_summary(
        &self,
        tenant: &loop_store::Tenant,
        session: &loop_protocol::SessionKey,
    ) -> loop_runtime::log::Result<Option<loop_runtime::log::SessionSummary>> {
        self.inner.session_summary(tenant, session).await
    }
    async fn recovery_page(
        &self,
        scope: loop_store::Scope<'_>,
        from_generation: u64,
        after_global: u64,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<loop_runtime::log::LoggedEvent>> {
        self.inner
            .recovery_page(scope, from_generation, after_global, limit)
            .await
    }
}

/// A change recorded in the tree whose announcement never reached the
/// Event Log is announced when the Session opens again, before anyone sees it.
#[tokio::test]
async fn an_unannounced_change_is_announced_on_reopen() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(MemoryEventLog::new());
        let log = Arc::new(FailingLog {
            inner: inner.clone(),
            failing: std::sync::atomic::AtomicBool::new(false),
        });
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        log.failing
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(rt.set_effort(&session, "low", None).await.is_err());
        assert_eq!(recorded(dir.path(), &session).await.revision, 1);
        rt.shutdown().await;

        log.failing
            .store(false, std::sync::atomic::Ordering::Release);
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let (watcher, _served) = connect(&rt).await;
        let mut rx = watcher.subscribe();
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let attached = watcher.attach(&session, 0).await.unwrap();
        assert_eq!(attached.snapshot.unwrap().revision, 1);
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert!(seq <= attached.latest_seq, "announced before serving");
        assert_eq!(changed.revision, 1);
        assert_eq!(changed.config.effort.as_deref(), Some("low"));
        // Announced once: opening again adds nothing.
        rt.close(&session.key, false).await.unwrap();
        let before = inner
            .latest_seq(&loop_store::Tenant::local(), &session.key)
            .await
            .unwrap();
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(
            inner
                .latest_seq(&loop_store::Tenant::local(), &session.key)
                .await
                .unwrap(),
            before
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// Any number of setters may wait on a stalled Event Log; each still gets
/// the sequence of its own change.
#[tokio::test]
async fn many_setters_waiting_on_a_stalled_log_each_get_their_receipt() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(GatedLog::default());
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        log.held.store(true, std::sync::atomic::Ordering::Release);
        let levels = ["off", "low", "medium", "high"];
        let setters: Vec<_> = (0..100)
            .map(|i| {
                let rt = rt.clone();
                let session = session.clone();
                tokio::spawn(async move { rt.set_effort(&session, levels[i % 4], None).await })
            })
            .collect();
        // Every change is made and waits for its commit.
        while rt.efforts(&session).await.unwrap().revision < 100 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        log.held.store(false, std::sync::atomic::Ordering::Release);
        log.release.notify_one();
        let mut receipts = Vec::new();
        for setter in setters {
            let result = setter.await.unwrap().unwrap();
            receipts.push((result.revision, result.seq));
        }
        receipts.sort();
        let revisions: Vec<u64> = receipts.iter().map(|(r, _)| *r).collect();
        assert_eq!(revisions, (1..=100).collect::<Vec<_>>());
        assert!(receipts.windows(2).all(|w| w[0].1 < w[1].1));
        // Each receipt names the update that announced that revision.
        let logged = log.inner.read_after(&loop_store::Tenant::local(), &session.key, 0).await.unwrap();
        for (revision, seq) in receipts {
            assert!(logged.iter().any(|r| matches!(
                &r.event.kind,
                EventKind::Update(e) if e.seq == seq
                    && matches!(&e.update, SessionUpdate::ConfigChanged(c) if c.revision == revision)
            )));
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A Session not loaded is described exactly as loading it sets it up.
#[tokio::test]
async fn cold_and_loaded_snapshots_agree() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let (chose_model, chose_nothing) = {
            let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
            let rt = runtime(dir.path(), log, Default::default()).await;
            let chose_model = rt.new_session(SessionSpec::default()).await.unwrap();
            rt.set_model(&chose_model, "faux/second", None)
                .await
                .unwrap();
            let chose_nothing = rt.new_session(SessionSpec::default()).await.unwrap();
            rt.shutdown().await;
            (chose_model, chose_nothing)
        };
        let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
        let rt = runtime(dir.path(), log, Default::default()).await;
        for session in [&chose_model, &chose_nothing] {
            let (cold, _s1) = connect(&rt).await;
            let cold = cold.attach(session, 0).await.unwrap().snapshot.unwrap();
            rt.load_session(session, SessionSpec::default())
                .await
                .unwrap();
            let (loaded, _s2) = connect(&rt).await;
            let loaded = loaded.attach(session, 0).await.unwrap().snapshot.unwrap();
            assert_eq!(cold.config, loaded.config, "{}", session.native_id);
            assert_eq!(cold.effective, loaded.effective);
            assert_eq!(cold.revision, loaded.revision);
            assert_eq!(cold.config.effort.as_deref(), Some("medium"));
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn rename_between_config_changes_keeps_both_in_the_snapshot() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use loop_protocol::rpc::RenameParams;
        use loop_wire::RuntimeControl;
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let model = rt.set_model(&session, "faux/second", None).await.unwrap();
        let renamed = rt
            .rename(RenameParams {
                session: session.clone(),
                name: "Review".into(),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert!(renamed.seq > model.seq);
        // A rename is a Session revision like any setter's; the next setter
        // still records and announces its own change.
        let effort = rt
            .set_effort(&session, "high", Some(renamed.revision))
            .await
            .unwrap();
        assert!(effort.seq > renamed.seq && effort.revision > model.revision);
        for snapshot in [
            rt.attach_snapshot_watching(7, &session, 0)
                .await
                .unwrap()
                .snapshot,
            rt.attach_watching(8, &session, 0).await.unwrap().snapshot,
        ] {
            let snapshot = snapshot.unwrap();
            assert_eq!(snapshot.metadata.name.as_deref(), Some("Review"));
            assert_eq!(snapshot.config.model.as_deref(), Some("faux/second"));
            assert_eq!(snapshot.config.effort.as_deref(), Some("high"));
            assert_eq!(snapshot.revision, effort.revision);
            assert_eq!(
                snapshot.at_seq,
                log.latest_seq(&loop_store::Tenant::local(), &session.key)
                    .await
                    .unwrap()
            );
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn one_session_revision_orders_every_setter_across_a_restart() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use loop_protocol::rpc::{NavigateParams, RenameParams};
        use loop_wire::RuntimeControl;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let (session, last) = {
            let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
            let rt = runtime(dir.path(), log, Default::default()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let start = rt
                .attach_snapshot_watching(1, &session, 0)
                .await
                .unwrap()
                .snapshot
                .unwrap()
                .revision;
            let model = rt
                .set_model(&session, "faux/second", Some(start))
                .await
                .unwrap();
            let rename = |expected| RenameParams {
                session: session.clone(),
                name: "Review".into(),
                expected_revision: Some(expected),
            };
            assert_eq!(
                rt.rename(rename(start)).await.unwrap_err(),
                BackendError::Conflict {
                    revision: model.revision
                }
            );
            let renamed = rt.rename(rename(model.revision)).await.unwrap();
            assert_eq!(renamed.revision, model.revision + 1);
            assert_eq!(
                rt.set_effort(&session, "high", Some(model.revision))
                    .await
                    .unwrap_err(),
                BackendError::Conflict {
                    revision: renamed.revision
                }
            );
            let navigate = |expected| NavigateParams {
                session: session.clone(),
                target_entry_id: None,
                summarize: false,
                expected_revision: Some(expected),
            };
            let moved = rt.navigate(navigate(renamed.revision)).await.unwrap();
            assert_eq!(moved.revision, renamed.revision + 1);
            assert_eq!(
                rt.navigate(navigate(renamed.revision)).await.unwrap_err(),
                BackendError::Conflict {
                    revision: moved.revision
                }
            );
            let effort = rt
                .set_effort(&session, "high", Some(moved.revision))
                .await
                .unwrap();
            assert_eq!(effort.revision, moved.revision + 1);
            let snapshot = rt
                .attach_snapshot_watching(2, &session, 0)
                .await
                .unwrap()
                .snapshot
                .unwrap();
            assert_eq!(snapshot.revision, effort.revision);
            assert_eq!(snapshot.metadata.name.as_deref(), Some("Review"));
            rt.shutdown().await;
            (session, effort.revision)
        };
        // The revision is the tree's, so a cold read and a reload agree.
        let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
        let rt = runtime(dir.path(), log, Default::default()).await;
        let (watcher, _served) = connect(&rt).await;
        assert_eq!(
            watcher
                .attach(&session, 0)
                .await
                .unwrap()
                .snapshot
                .unwrap()
                .revision,
            last
        );
        watcher.detach(&session).await.unwrap();
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let renamed = rt
            .rename(RenameParams {
                session: session.clone(),
                name: "Again".into(),
                expected_revision: Some(last),
            })
            .await
            .unwrap();
        assert_eq!(renamed.revision, last + 1);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// The response-timeout override is Session config: during a turn it waits
/// for the next one, it never becomes the default for new Sessions, and a
/// reopened Session keeps it.
#[tokio::test]
async fn ttfb_waits_for_the_turn_and_never_changes_the_default() {
    tokio::time::timeout(Duration::from_secs(30), async {
        use loop_ai::providers::{FauxResponse, FauxScript};
        let script = FauxScript::new();
        script.push(FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
            id: "call".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo waiting"}),
            thought_signature: None,
        }]));
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone(), script).await;
        assert!(rt.capabilities().session_ttfb);
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let read = rt.ttfb(&session, None, None).await.unwrap();
        assert_eq!((read.override_ms, read.effective_ms), (None, 120_000));
        let mut rx = rt.subscribe();
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run it")])
            .await
            .unwrap();
        loop {
            let raised = log
                .read_runtime_after(
                    loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
                    0,
                )
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationRaised(_)));
            if raised {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let result = rt
            .ttfb(&session, Some(Some(0)), Some(read.revision))
            .await
            .unwrap();
        assert!(result.pending);
        assert_eq!(result.revision, read.revision + 1);
        assert_eq!((result.override_ms, result.effective_ms), (Some(0), 0));
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(Some(seq), result.seq);
        assert!(changed.pending);
        assert_eq!(changed.config.response_timeout_ms, Some(0));
        assert_eq!(changed.effective.response_timeout_ms, None);
        rt.cancel(&session);
        turn.outcome().await;
        let (_, applied) = config_changed(&mut rx, &session.key).await;
        assert!(!applied.pending);
        assert_eq!(applied.effective.response_timeout_ms, Some(0));

        // New Sessions keep the host default.
        let other = rt.new_session(SessionSpec::default()).await.unwrap();
        let fresh = rt.ttfb(&other, None, None).await.unwrap();
        assert_eq!((fresh.override_ms, fresh.effective_ms), (None, 120_000));

        // The override is counted from the tree, so it survives a reopen.
        rt.close(&session.key, false).await.unwrap();
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let reopened = rt.ttfb(&session, None, None).await.unwrap();
        assert_eq!(
            (
                reopened.override_ms,
                reopened.effective_ms,
                reopened.revision
            ),
            (Some(0), 0, result.revision)
        );
        let cleared = rt.ttfb(&session, Some(None), None).await.unwrap();
        assert_eq!((cleared.override_ms, cleared.effective_ms), (None, 120_000));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
