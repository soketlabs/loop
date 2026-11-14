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
    let rt = Runtime::new(globals, opts(&ws), log, store, RuntimeLimits::default())
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

        // Alice (tenant acme) creates a Session; then the host user uses it.
        let theirs = alice
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        assert_eq!(rt.session_tenant(&theirs.key), acme());
        prompt(&rt, &alice, &theirs, "first").await;
        prompt(&rt, &me, &theirs, "second").await;
        let records = records_of(log.as_ref(), &theirs.key).await;
        assert!(records.iter().all(|r| r.event.tenant() == &acme()));
        let prompts: Vec<_> = records
            .iter()
            .filter(|r| matches!(r.event.kind, EventKind::PromptSent(_)))
            .map(|r| r.event.identity.clone())
            .collect();
        assert_eq!(
            prompts,
            vec![acting(&alice, acme()), acting(&me, acme())],
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

        // Alice uses a Session the host user created: it stays local.
        let mine = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
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
            Some(loop_protocol::Via::edge())
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
            "using a Session does not make Alice its owner"
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
        let (me, alice) = (host(), edge("alice"));
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
            .scope(rt.new_session(SessionSpec::default()))
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
                .scope(rt.new_session(SessionSpec::default()))
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
        let me = host();
        me.clone()
            .scope(rt.load_session(&session, SessionSpec::default()))
            .await
            .unwrap();
        prompt(&rt, &me, &session, "again").await;
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

/// Only the Runtime's startup code makes a `SystemScope`: outside tests,
/// `SystemScope::for_runtime_startup` appears once in this crate's source,
/// in `Runtime::new`.
#[test]
fn only_runtime_startup_makes_a_system_scope() {
    fn walk(dir: &std::path::Path, found: &mut Vec<(String, usize)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
                continue;
            }
            let name = path.to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let code = text.split("#[cfg(test)]\nmod tests").next().unwrap();
            let count = code.matches("SystemScope::for_runtime_startup").count();
            if count > 0 {
                found.push((name, count));
            }
        }
    }
    let mut found = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].0.ends_with("src/runtime.rs") && found[0].1 == 1,
        "{found:?}"
    );
}
