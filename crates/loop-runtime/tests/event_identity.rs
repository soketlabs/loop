//! Step B3 and I2: every record the Runtime appends names who acted
//! (principal, `via`, `actor`) in the Session's tenant; Runtime records name
//! the Runtime; reads are scoped by tenant; the Session's creator is its
//! owner of record.

use std::sync::Arc;
use std::time::Duration;

use loop_ai::providers::{FauxResponder, FauxResponse};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, Caller, ContentBlock, ModelControl, Principal, PrincipalKind, SessionKey,
    SessionRef, SessionSpec, SessionUpdate, Tenant, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::log::{
    Event, EventKind, EventLog, Identity, LoggedEvent, MemoryEventLog, Scope, SystemScope,
    SESSION_CREATED_KEY,
};
use loop_runtime::{Runtime, RuntimeLimits};

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
        permission_mode: Some("full-access".into()),
        agent: None,
    }
}

fn host() -> Caller {
    Caller::host(Principal::local_uid(501))
}

/// The host's user on its WebSocket with the Runtime token: another
/// identity (`via: token`) with the same full access to `local` Sessions.
fn token() -> Caller {
    Caller::local_token(Principal::local_uid(501))
}

/// Who an in-process call (no connection) acts as: the Runtime.
fn runtime_caller() -> Caller {
    Caller::host(Principal::runtime())
}

/// A turn of an in-process call (internal authority, no caller).
async fn prompt_in_process(rt: &Runtime, session: &SessionRef, text: &str) {
    let turn = rt
        .prompt(session, vec![ContentBlock::text(text)])
        .await
        .unwrap();
    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
}

/// A folder of its own for an edge user's Session: a Session of another
/// owner may not share, contain or sit in it (the folder rule, I3).
fn own_folder(dir: &std::path::Path, name: &str) -> SessionSpec {
    let cwd = dir.join(name);
    std::fs::create_dir_all(&cwd).unwrap();
    SessionSpec {
        cwd: Some(cwd),
        ..SessionSpec::default()
    }
}

fn acme() -> Tenant {
    Tenant::new("acme").unwrap()
}

fn edge(id: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, acme()).unwrap()
}

/// The model's answers: text, or one `subagent` call for a prompt that says
/// `SPAWN`. A prompt that says `HOLD` waits until `release` is notified.
struct Script {
    release: tokio::sync::Notify,
    held: tokio::sync::Notify,
}

async fn runtime(dir: &std::path::Path, log: Arc<dyn EventLog>) -> (Arc<Runtime>, Arc<Script>) {
    runtime_with(dir, log, RuntimeLimits::default()).await
}

async fn runtime_with(
    dir: &std::path::Path,
    log: Arc<dyn EventLog>,
    limits: RuntimeLimits,
) -> (Arc<Runtime>, Arc<Script>) {
    let script = Arc::new(Script {
        release: tokio::sync::Notify::new(),
        held: tokio::sync::Notify::new(),
    });
    let seen = script.clone();
    let responder: FauxResponder = Arc::new(move |context: &loop_ai::Context| {
        let text = format!("{:?}", context.messages);
        let answered = context
            .messages
            .iter()
            .any(|m| matches!(m, loop_ai::Message::Assistant(_)));
        let seen = seen.clone();
        Box::pin(async move {
            if text.contains("HOLD") && !answered {
                seen.held.notify_one();
                seen.release.notified().await;
            }
            if text.contains("SPAWN") && !answered {
                FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
                    id: "call-spawn".into(),
                    name: "subagent".into(),
                    arguments: serde_json::json!({"task": "child work"}),
                    thought_signature: None,
                }])
            } else {
                FauxResponse::Text("done".into())
            }
        })
    });
    let provider = loop_ai::providers::faux_provider_fn(responder);
    let template = provider.get_model("faux-model").unwrap();
    let mut second = template.clone();
    second.id = "second".into();
    second.name = "second".into();
    provider.set_models(vec![template, second]);
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(provider);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(globals, opts(&ws), log, store, limits)
        .await
        .unwrap()
        .into_shared();
    (rt, script)
}

async fn prompt(rt: &Runtime, caller: &Caller, session: &SessionRef, text: &str) {
    let turn = caller
        .clone()
        .scope(rt.prompt(session, vec![ContentBlock::text(text)]))
        .await
        .unwrap();
    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
}

fn all() -> Scope<'static> {
    static SCOPE: std::sync::OnceLock<SystemScope> = std::sync::OnceLock::new();
    Scope::System(SCOPE.get_or_init(SystemScope::for_runtime_startup))
}

/// Every record of `key`, in all tenants.
async fn records_of(log: &dyn EventLog, key: &SessionKey) -> Vec<LoggedEvent> {
    log.read_runtime_after(all(), 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.event.session_key.as_ref() == Some(key))
        .collect()
}

fn acting(caller: &Caller, tenant: Tenant) -> Identity {
    Identity::acting(caller, tenant)
}

fn update_of(record: &LoggedEvent) -> Option<&UpdateEnvelope> {
    match &record.event.kind {
        EventKind::Update(envelope) => Some(envelope),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_a_single_user_runtime_every_record_names_the_host_user_or_the_runtime() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let (rt, _) = runtime(dir.path(), log.clone()).await;
        let me = host();
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        prompt(&rt, &me, &session, "hello").await;
        me.clone()
            .scope(rt.set_model(&session, "faux/second", None))
            .await
            .unwrap();
        me.clone()
            .scope(rt.close(&session.key, false))
            .await
            .unwrap();
        rt.shutdown().await;

        let all_records = log.read_runtime_after(all(), 0).await.unwrap();
        let mine = acting(&me, Tenant::local());
        for record in &all_records {
            match &record.event.session_key {
                None => assert_eq!(
                    record.event.identity,
                    Identity::runtime_in(Tenant::local()),
                    "{:?}",
                    record.event.kind
                ),
                Some(_) => assert_eq!(record.event.identity, mine, "{:?}", record.event.kind),
            }
        }
        // Each kind is there, with the host user's identity.
        let session_records = records_of(log.as_ref(), &session.key).await;
        let has = |f: &dyn Fn(&EventKind) -> bool| session_records.iter().any(|r| f(&r.event.kind));
        assert!(has(
            &|k| matches!(k, EventKind::SessionOpened(p) if p[SESSION_CREATED_KEY] == true)
        ));
        assert!(has(&|k| matches!(k, EventKind::PromptSent(_))));
        assert!(has(&|k| matches!(k, EventKind::SessionClosed(_))));
        assert!(session_records.iter().any(|r| matches!(
            update_of(r).map(|e| &e.update),
            Some(SessionUpdate::TurnStarted { .. })
        )));
        assert!(session_records.iter().any(|r| matches!(
            update_of(r).map(|e| &e.update),
            Some(SessionUpdate::ConfigChanged(_))
        )));
        // The owner of record is the creator.
        assert_eq!(
            log.session_owner(&Tenant::local(), &session.key)
                .await
                .unwrap(),
            Some(mine)
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_keeps_its_tenant_whoever_acts_and_reads_are_scoped() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let (rt, _) = runtime(dir.path(), log.clone()).await;
        let (me, alice) = (host(), edge("alice"));

        // Alice (tenant acme) creates a Session; then in-process work (the
        // only other identity that may act in it, I3) uses it.
        let theirs = alice
            .clone()
            .scope(rt.new_session(own_folder(dir.path(), "alice")))
            .await
            .unwrap();
        assert_eq!(rt.session_tenant(&theirs.key), acme());
        prompt(&rt, &alice, &theirs, "first").await;
        prompt_in_process(&rt, &theirs, "second").await;
        let records = records_of(log.as_ref(), &theirs.key).await;
        assert!(records.iter().all(|r| r.event.tenant() == &acme()));
        let prompts: Vec<_> = records
            .iter()
            .filter(|r| matches!(r.event.kind, EventKind::PromptSent(_)))
            .map(|r| r.event.identity.clone())
            .collect();
        assert_eq!(
            prompts,
            vec![acting(&alice, acme()), acting(&runtime_caller(), acme())],
            "the principal is who acted; the tenant is the Session's"
        );
        // One contiguous sequence, in acme only.
        let updates = log.read_after(&acme(), &theirs.key, 0).await.unwrap();
        assert_eq!(
            updates
                .iter()
                .map(|r| r.session_seq.unwrap())
                .collect::<Vec<_>>(),
            (1..=updates.len() as u64).collect::<Vec<_>>()
        );
        assert!(log
            .read_after(&Tenant::local(), &theirs.key, 0)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            log.session_summary(&Tenant::local(), &theirs.key)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            rt.session_owner(&theirs.key).await.unwrap(),
            Some(acting(&alice, acme()))
        );

        // Alice may not use a Session the host user created (I3); the host
        // user on its token connection may: it stays local.
        let mine = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        let refused = alice
            .clone()
            .scope(rt.prompt(&mine, vec![ContentBlock::text("hi")]))
            .await;
        assert!(matches!(
            refused,
            Err(loop_protocol::BackendError::SessionNotFound(_))
        ));
        let alice = token();
        prompt(&rt, &alice, &mine, "hi").await;
        let records = records_of(log.as_ref(), &mine.key).await;
        assert!(records.iter().all(|r| r.event.tenant() == &Tenant::local()));
        let alice_prompt = records
            .iter()
            .find(|r| matches!(r.event.kind, EventKind::PromptSent(_)))
            .unwrap();
        assert_eq!(alice_prompt.event.identity, acting(&alice, Tenant::local()));
        assert_eq!(
            alice_prompt.event.identity.via,
            Some(loop_protocol::Via::token())
        );
        let turn_updates: Vec<_> = records
            .iter()
            .filter(|r| update_of(r).is_some_and(|e| e.turn.is_some()))
            .collect();
        assert!(!turn_updates.is_empty());
        assert!(turn_updates
            .iter()
            .all(|r| r.event.identity == acting(&alice, Tenant::local())));
        assert_eq!(
            rt.session_owner(&mine.key).await.unwrap(),
            Some(acting(&me, Tenant::local())),
            "using a Session does not make its user its owner"
        );
        // An attach reads the Session's tenant.
        let attached = loop_wire::RuntimeControl::attach(rt.as_ref(), &theirs, 0)
            .await
            .unwrap();
        assert!(attached.replay.count() > 0);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_config_change_during_another_caller_s_turn_names_its_own_caller() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let (rt, script) = runtime(dir.path(), log.clone()).await;
        let (me, alice) = (host(), token());
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        let turn = me
            .clone()
            .scope(rt.prompt(&session, vec![ContentBlock::text("HOLD")]))
            .await
            .unwrap();
        script.held.notified().await;
        // Alice changes the model while the host user's turn runs.
        alice
            .clone()
            .scope(rt.set_model(&session, "faux/second", None))
            .await
            .unwrap();
        script.release.notify_one();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        // Then the two prompt back to back.
        prompt(&rt, &alice, &session, "a").await;
        prompt(&rt, &me, &session, "b").await;
        let records = records_of(log.as_ref(), &session.key).await;
        let mut changes = 0;
        let mut turns: Vec<(loop_protocol::TurnId, Identity)> = Vec::new();
        for record in &records {
            let Some(envelope) = update_of(record) else {
                continue;
            };
            match (&envelope.update, &envelope.turn) {
                (SessionUpdate::ConfigChanged(changed), None) if changed.revision > 0 => {
                    changes += 1;
                    assert_eq!(record.event.identity, acting(&alice, Tenant::local()));
                }
                (_, Some(turn)) => match turns.iter().find(|(t, _)| t == turn) {
                    Some((_, identity)) => assert_eq!(
                        &record.event.identity, identity,
                        "one turn, one caller: {:?}",
                        envelope.update
                    ),
                    None => turns.push((turn.clone(), record.event.identity.clone())),
                },
                _ => {}
            }
        }
        assert!(changes >= 1);
        let callers: Vec<_> = turns.into_iter().map(|(_, identity)| identity).collect();
        assert_eq!(
            callers,
            vec![
                acting(&me, Tenant::local()),
                acting(&alice, Tenant::local()),
                acting(&me, Tenant::local()),
            ]
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subagent_acts_for_its_parent_turn_s_caller_with_the_parent_as_actor() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let (rt, _) = runtime(dir.path(), log.clone()).await;
        let alice = edge("alice");
        let parent = alice
            .clone()
            .scope(rt.new_session(own_folder(dir.path(), "alice")))
            .await
            .unwrap();
        prompt(&rt, &alice, &parent, "SPAWN").await;
        let all_records = log.read_runtime_after(all(), 0).await.unwrap();
        let child = all_records
            .iter()
            .find_map(|r| match &r.event.kind {
                EventKind::SessionOpened(p) if p["parent"] == parent.key.0.as_str() => {
                    r.event.session_key.clone()
                }
                _ => None,
            })
            .expect("a child opened");
        let expected = acting(&alice, acme()).with_actor(Some(parent.key.0.clone()));
        let child_records = records_of(log.as_ref(), &child).await;
        assert!(!child_records.is_empty());
        for record in &child_records {
            assert_eq!(record.event.identity, expected, "{:?}", record.event.kind);
        }
        assert!(child_records
            .iter()
            .any(|r| matches!(r.event.kind, EventKind::PromptSent(_))));
        // The parent's own records of the call name Alice, with no actor.
        for record in records_of(log.as_ref(), &parent.key).await {
            if matches!(
                record.event.kind,
                EventKind::SubagentStarted(_) | EventKind::SubagentEnded(_)
            ) {
                assert_eq!(record.event.identity, acting(&alice, acme()));
            }
        }
        assert_eq!(
            log.subagent_sessions(Scope::Tenant(&acme())).await.unwrap(),
            std::collections::HashSet::from([(acme(), child.clone())])
        );
        assert!(log
            .subagent_sessions(Scope::Tenant(&Tenant::local()))
            .await
            .unwrap()
            .is_empty());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_restart_a_session_keeps_its_tenant_and_recovery_names_the_runtime() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let alice = edge("alice");
        let (session, generation) = {
            let (rt, _) = runtime(dir.path(), log.clone()).await;
            let session = alice
                .clone()
                .scope(rt.new_session(own_folder(dir.path(), "alice")))
                .await
                .unwrap();
            let generation = rt.generation();
            rt.shutdown().await;
            (session, generation)
        };
        // A turn the earlier Runtime left open (a crash in the turn).
        let started = Event::update(
            generation,
            1,
            UpdateEnvelope::live(
                "native".into(),
                generation,
                session.key.clone(),
                Some("turn-9".into()),
                0,
                SessionUpdate::TurnStarted {
                    turn_id: "turn-9".into(),
                },
            ),
            acting(&alice, acme()),
        );
        log.append_batch(vec![started]).await.unwrap();

        let (rt, _) = runtime(dir.path(), log.clone()).await;
        assert_eq!(rt.recover(None).await.unwrap().turns_ended, 1);
        let ended = records_of(log.as_ref(), &session.key)
            .await
            .into_iter()
            .find(|r| {
                matches!(
                    update_of(r).map(|e| &e.update),
                    Some(SessionUpdate::TurnEnded {
                        stop: loop_protocol::StopReason::RuntimeRestarted,
                        ..
                    })
                )
            })
            .expect("recovery ended the turn");
        assert_eq!(ended.event.identity, Identity::runtime_in(acme()));
        // The new Runtime knows the Session's tenant from the log.
        assert_eq!(rt.session_tenant(&session.key), acme());
        // The host's user may not load another tenant's Session (I3); an
        // in-process load (the Runtime) may, and is not its owner.
        let refused = host()
            .scope(rt.load_session(&session, SessionSpec::default()))
            .await;
        assert!(matches!(
            refused,
            Err(loop_protocol::BackendError::SessionNotFound(_))
        ));
        let me = runtime_caller();
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        prompt_in_process(&rt, &session, "again").await;
        let records = records_of(log.as_ref(), &session.key).await;
        assert!(records.iter().all(|r| r.event.tenant() == &acme()));
        let reopened = records
            .iter()
            .rfind(|r| matches!(r.event.kind, EventKind::SessionOpened(_)))
            .unwrap();
        assert_eq!(reopened.event.identity, acting(&me, acme()));
        match &reopened.event.kind {
            EventKind::SessionOpened(payload) => {
                assert!(payload.get(SESSION_CREATED_KEY).is_none())
            }
            _ => unreachable!(),
        }
        assert_eq!(
            rt.session_owner(&session.key).await.unwrap(),
            Some(acting(&alice, acme())),
            "a reload does not change the owner of record"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_a_clone_and_an_unload_name_who_acted() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let limits = RuntimeLimits {
            max_sessions: 2,
            max_active_turns: 2,
        };
        let (rt, _) = runtime_with(dir.path(), log.clone(), limits).await;
        let (me, alice) = (host(), edge("alice"));
        // A second host-user connection, over the token WebSocket.
        let token = Caller::local_token(Principal::local_uid(501));
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        prompt(&rt, &me, &session, "hello").await;

        // `session/shell`: its decision record and any update are its caller's.
        token
            .clone()
            .scope(loop_protocol::LocalShell::run(
                rt.as_ref(),
                &session,
                "echo hi",
                None,
            ))
            .await
            .unwrap();
        let decisions: Vec<_> = records_of(log.as_ref(), &session.key)
            .await
            .into_iter()
            .filter(|r| matches!(r.event.kind, EventKind::PolicyDecision(_)))
            .collect();
        let shell = decisions.last().expect("a decision of the command");
        assert_eq!(shell.event.identity, acting(&token, Tenant::local()));

        // Alice may not clone the host user's Session (I3: it needs read
        // access to the source). She clones her own: the copy is hers, in
        // acme.
        let refused = alice
            .clone()
            .scope(rt.clone_session(loop_protocol::rpc::CloneParams {
                session: session.clone(),
                name: None,
                request_id: "copy-0".into(),
            }))
            .await;
        assert!(matches!(
            refused,
            Err(loop_protocol::BackendError::SessionNotFound(_))
        ));
        let source = alice
            .clone()
            .scope(rt.new_session(own_folder(dir.path(), "alice")))
            .await
            .unwrap();
        let copy = alice
            .clone()
            .scope(rt.clone_session(loop_protocol::rpc::CloneParams {
                session: source,
                name: None,
                request_id: "copy-1".into(),
            }))
            .await
            .unwrap()
            .session;
        assert_eq!(rt.session_tenant(&copy.key), acme());
        assert_eq!(
            rt.session_owner(&copy.key).await.unwrap(),
            Some(acting(&alice, acme()))
        );
        assert!(records_of(log.as_ref(), &copy.key)
            .await
            .iter()
            .all(|r| r.event.identity == acting(&alice, acme())));

        // A third Session makes room by unloading the oldest idle one: the
        // Runtime's own record, in that Session's tenant.
        me.clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        let unloaded = log
            .read_runtime_after(all(), 0)
            .await
            .unwrap()
            .into_iter()
            .find(|r| {
                matches!(&r.event.kind, EventKind::SessionClosed(p) if p["reason"] == "unloaded")
            })
            .expect("an unload");
        let tenant = rt.session_tenant(unloaded.event.session_key.as_ref().unwrap());
        assert_eq!(unloaded.event.identity, Identity::runtime_in(tenant));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// An Event Log whose appends wait while it is closed.
struct GatedLog {
    inner: MemoryEventLog,
    open: tokio::sync::watch::Sender<bool>,
    waiting: std::sync::atomic::AtomicUsize,
}
impl GatedLog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryEventLog::new(),
            open: tokio::sync::watch::channel(true).0,
            waiting: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    fn set_open(&self, open: bool) {
        self.open.send_replace(open);
    }
    fn waiting(&self) -> usize {
        self.waiting.load(std::sync::atomic::Ordering::Acquire)
    }
}
#[async_trait::async_trait]
impl EventLog for GatedLog {
    async fn append_batch(
        &self,
        events: Vec<Event>,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        let mut open = self.open.subscribe();
        if !*open.borrow() {
            self.waiting
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let _ = open.wait_for(|open| *open).await;
            self.waiting
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.read_after(tenant, session, after_seq).await
    }
    async fn read_runtime_after(
        &self,
        scope: Scope<'_>,
        global_seq: u64,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.read_runtime_after(scope, global_seq).await
    }
    async fn latest_seq(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(tenant, session).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        started_ms: i64,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.allocate_generation(owner, pid, started_ms).await
    }
    async fn finish_generation(
        &self,
        generation: u64,
        stopped_ms: i64,
    ) -> loop_runtime::log::Result<()> {
        self.inner.finish_generation(generation, stopped_ms).await
    }
}

/// The interleaving of the review: A's config change waits in the pump's
/// queue behind an earlier update while B's prompt is admitted. The change
/// is still A's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_config_change_keeps_its_caller_when_another_caller_prompts() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = GatedLog::new();
        let (rt, _) = runtime(dir.path(), log.clone()).await;
        let (me, alice) = (host(), token());
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        log.set_open(false);
        // Alice's first change: the pump blocks on its commit.
        let first = tokio::spawn({
            let (rt, session, alice) = (rt.clone(), session.clone(), alice.clone());
            async move {
                alice
                    .scope(rt.set_model(&session, "faux/second", None))
                    .await
            }
        });
        while log.waiting() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Alice's second change waits in the pump's queue.
        let second = tokio::spawn({
            let (rt, session, alice) = (rt.clone(), session.clone(), alice.clone());
            async move {
                alice
                    .scope(rt.set_model(&session, "faux/second", None))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The host user's prompt is admitted meanwhile.
        let turn = me
            .clone()
            .scope(rt.prompt(&session, vec![ContentBlock::text("go")]))
            .await
            .unwrap();
        log.set_open(true);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        let changes: Vec<_> = records_of(&log.inner, &session.key)
            .await
            .into_iter()
            .filter(|r| {
                matches!(
                    update_of(r).map(|e| &e.update),
                    Some(SessionUpdate::ConfigChanged(c)) if c.revision > 0
                )
            })
            .collect();
        assert!(changes.len() >= 2, "{changes:?}");
        for change in &changes {
            assert_eq!(
                change.event.identity,
                acting(&alice, Tenant::local()),
                "a config change is its setter's"
            );
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_rename_navigate_compact_and_an_edge_decision_name_their_callers() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let (rt, script) = runtime(dir.path(), log.clone()).await;
        let (me, alice) = (host(), token());
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        // The host user's turn, cancelled by Alice.
        let turn = me
            .clone()
            .scope(rt.prompt(&session, vec![ContentBlock::text("HOLD")]))
            .await
            .unwrap();
        script.held.notified().await;
        alice.clone().scope(async { rt.cancel(&session) }).await;
        script.release.notify_one();
        let _ = turn.outcome().await;
        let alice_local = acting(&alice, Tenant::local());
        let records = records_of(log.as_ref(), &session.key).await;
        let cancelled = records
            .iter()
            .find(|r| matches!(r.event.kind, EventKind::Cancelled(_)))
            .expect("a cancel row");
        assert_eq!(cancelled.event.identity, alice_local);
        prompt(&rt, &me, &session, "after").await;

        // Rename, navigate and compact by Alice.
        let renamed: loop_protocol::MutationResult = alice
            .clone()
            .scope(rt.rename(loop_protocol::rpc::RenameParams {
                session: session.clone(),
                name: "named".into(),
                expected_revision: None,
            }))
            .await
            .unwrap();
        prompt(&rt, &me, &session, "more").await;
        alice
            .clone()
            .scope(rt.compact(&session, None))
            .await
            .unwrap();
        let before = log.read_runtime_after(all(), 0).await.unwrap().len();
        alice
            .clone()
            .scope(rt.navigate(loop_protocol::rpc::NavigateParams {
                session: session.clone(),
                target_entry_id: None,
                summarize: false,
                expected_revision: None,
            }))
            .await
            .unwrap();
        let navigated = log.read_runtime_after(all(), 0).await.unwrap()[before..].to_vec();
        let records = records_of(log.as_ref(), &session.key).await;
        let resets: Vec<_> = records
            .iter()
            .filter(|r| {
                matches!(
                    update_of(r).map(|e| &e.update),
                    Some(SessionUpdate::BranchReset { .. })
                )
            })
            .collect();
        assert_eq!(resets.len(), 2, "navigate and compact");
        assert!(resets.iter().all(|r| r.event.identity == alice_local));
        assert!(navigated.iter().any(|r| matches!(
            update_of(r).map(|e| &e.update),
            Some(SessionUpdate::BranchReset { .. })
        )));
        let rename = records
            .iter()
            .find(|r| {
                matches!(
                    update_of(r).map(|e| &e.update),
                    Some(SessionUpdate::ConfigChanged(c)) if c.revision == renamed.revision
                )
            })
            .expect("the rename's update");
        assert_eq!(rename.event.identity, alice_local);

        // A decision of an edge user's command on its own Session: the row
        // and the record name the edge user, in its tenant. (An edge user
        // may not act in the host user's Session, I3; the server never runs
        // its shell commands at all.)
        let edge_user = edge("alice");
        let theirs = edge_user
            .clone()
            .scope(rt.new_session(own_folder(dir.path(), "alice")))
            .await
            .unwrap();
        edge_user
            .clone()
            .scope(loop_protocol::LocalShell::run(
                rt.as_ref(),
                &theirs,
                "echo hi",
                None,
            ))
            .await
            .unwrap();
        let decision = records_of(log.as_ref(), &theirs.key)
            .await
            .into_iter()
            .rfind(|r| matches!(r.event.kind, EventKind::PolicyDecision(_)))
            .expect("a decision");
        assert_eq!(decision.event.identity, acting(&edge_user, acme()));
        let EventKind::PolicyDecision(payload) = &decision.event.kind else {
            unreachable!()
        };
        assert_eq!(payload["tenant"], "acme");
        assert_eq!(payload["principal"]["id"], "alice");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A downgrade wrote a row of a Session of tenant acme as `local` (an older
/// binary has no tenant). The Runtime reads the Session in its creating
/// tenant and refuses to load it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_split_across_tenants_by_an_older_binary_is_refused() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events");
        let alice = edge("alice");
        let session = {
            let log = Arc::new(loop_runtime::log::SqliteEventLog::open(&events).await.unwrap());
            let (rt, _) = runtime(dir.path(), log).await;
            let session = alice
                .clone()
                .scope(rt.new_session(own_folder(dir.path(), "alice")))
                .await
                .unwrap();
            prompt(&rt, &alice, &session, "hello").await;
            rt.shutdown().await;
            session
        };
        {
            // The append of a binary from before step B3: no identity
            // columns, its sequence over the key in all tenants.
            let conn = rusqlite::Connection::open(events.join("events.db")).unwrap();
            let (global, seq): (i64, i64) = conn
                .query_row(
                    "SELECT (SELECT MAX(global_seq) FROM events), \
                     (SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1 AND session_seq IS NOT NULL)",
                    [session.key.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let payload = serde_json::json!({"v": 1, "event": {"kind": "update", "payload":
                serde_json::to_value(UpdateEnvelope::live("native".into(), 1, session.key.clone(), None, (seq + 1) as u64,
                    SessionUpdate::Notice { level: loop_protocol::NoticeLevel::Info, text: "old".into() })).unwrap()}});
            conn.execute(
                "INSERT INTO events (global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload) VALUES (?1,1,?2,?3,'update',NULL,1,?4)",
                rusqlite::params![global + 1, session.key.as_str(), seq + 1, payload.to_string()],
            )
            .unwrap();
        }
        let log = Arc::new(loop_runtime::log::SqliteEventLog::open(&events).await.unwrap());
        let (rt, _) = runtime(dir.path(), log).await;
        assert_eq!(rt.session_tenant(&session.key), acme(), "the creating tenant");
        let refused = alice
            .clone()
            .scope(rt.load_session(&session, SessionSpec::default()))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("more than one tenant"), "{refused}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[cfg(feature = "orchestration")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workflow_s_records_are_its_caller_s() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let (rt, _) = runtime(dir.path(), log.clone()).await;
        let (me, alice) = (host(), token());
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        let before = log.read_runtime_after(all(), 0).await.unwrap().len();
        let started = alice
            .clone()
            .scope(loop_wire::RuntimeControl::workflow(
                rt.as_ref(),
                loop_protocol::session_services::WorkflowParams {
                    session: session.clone(),
                    goal: "say done".into(),
                    concurrency: None,
                    request_id: "wf-1".into(),
                },
            ))
            .await;
        if started.is_ok() {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while rt.is_busy(&session) && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let after: Vec<_> = log.read_runtime_after(all(), 0).await.unwrap()[before..]
            .iter()
            .filter(|r| r.event.session_key.as_ref() == Some(&session.key))
            .cloned()
            .collect();
        assert!(!after.is_empty(), "the workflow left records");
        for record in &after {
            assert_eq!(
                record.event.identity,
                acting(&alice, Tenant::local()),
                "{:?}",
                record.event.kind
            );
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// Only the Runtime's startup code makes a `SystemScope`. Rust cannot
/// limit the constructor to one other crate, so this scans the whole
/// workspace: outside test code, `for_runtime_startup` appears only at its
/// definition in loop-store and once in loop-runtime's `Runtime::new`, and
/// no code renames the type or the constructor.
#[test]
fn only_runtime_startup_makes_a_system_scope() {
    fn walk(dir: &std::path::Path, found: &mut Vec<(String, usize)>, aliases: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.to_string_lossy().into_owned();
            if path.is_dir() {
                let base = path.file_name().unwrap().to_string_lossy().into_owned();
                if base == "tests" || base == "target" || base == "loop-store-testkit" {
                    continue;
                }
                walk(&path, found, aliases);
                continue;
            }
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let code = text.split("#[cfg(test)]\nmod tests").next().unwrap();
            let count = code.matches("for_runtime_startup").count();
            if count > 0 {
                found.push((name.clone(), count));
            }
            if code.contains("SystemScope as ") || code.contains("for_runtime_startup as ") {
                aliases.push(name);
            }
        }
    }
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let (mut found, mut aliases) = (Vec::new(), Vec::new());
    walk(&crates, &mut found, &mut aliases);
    found.sort();
    assert!(aliases.is_empty(), "renamed: {aliases:?}");
    let places: Vec<_> = found
        .iter()
        .map(|(name, count)| {
            let short = name.split("/crates/").last().unwrap().to_owned();
            (short, *count)
        })
        .collect();
    assert_eq!(
        places,
        vec![
            ("core/loop-store/src/identity.rs".to_owned(), 1),
            ("loop-runtime/src/runtime.rs".to_owned(), 1),
        ],
        "{found:?}"
    );
}
