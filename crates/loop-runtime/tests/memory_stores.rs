//! The Runtime over injected in-memory stores, with no SQLite at all.
//!
//! The Session store is `InMemoryRuntimeStore` (leases, control markers,
//! destinations) and the Event Log is `MemoryEventLog`. The basic flows run
//! as they do over SQLite: open, prompt, attach, close, unload and reload,
//! rename, and recovery after a crash. At the end no database file exists
//! anywhere under the test folder. That proves the Runtime opens no store
//! itself.
#![cfg(unix)]

use loop_agent::harness::session::{
    InMemoryRuntimeStore, LeaseEnvironment, LeaseIdentity, PidLiveness, RuntimeSessionStore,
    SessionStore, SystemLeaseEnvironment, LEASE_STALE_AFTER_MS,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, ContentBlock, SessionKey, SessionRef, SessionSpec, SessionUpdate, StopReason,
    TurnId, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::{
    log::{Event, EventKind, EventLog, MemoryEventLog},
    Recovery, Runtime, RuntimeLimits,
};
use loop_wire::RuntimeControl;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(60);
/// A pid that the environment below reports as dead.
const DEAD_PID: u32 = 2_000_000_001;

/// The system clock and pid probe, except that the clock can be moved back
/// and `DEAD_PID` is always dead.
struct Environment {
    offset_ms: AtomicI64,
}
impl LeaseEnvironment for Environment {
    fn now_ms(&self) -> i64 {
        SystemLeaseEnvironment.now_ms() + self.offset_ms.load(Ordering::SeqCst)
    }
    fn pid_liveness(&self, pid: u32) -> PidLiveness {
        if pid == DEAD_PID {
            PidLiveness::Dead
        } else {
            SystemLeaseEnvironment.pid_liveness(pid)
        }
    }
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
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: None,
    }
}

async fn runtime(
    dir: &Path,
    log: Arc<MemoryEventLog>,
    store: Arc<dyn RuntimeSessionStore>,
    limits: RuntimeLimits,
) -> Runtime {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals =
        GlobalResources::with_models_and_store(dir.join("agent"), models, store.clone()).unwrap();
    Runtime::new(globals, opts(dir), log, store, limits)
        .await
        .unwrap()
}

/// Every file under `dir` whose name looks like a database.
fn database_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if name.ends_with(".db")
                    || name.contains(".db-")
                    || name.ends_with(".sqlite")
                    || name.contains(".sqlite-")
                {
                    found.push(path);
                }
            }
        }
    }
    found
}

fn reference(key: &SessionKey) -> SessionRef {
    SessionRef {
        instance: "loop".into(),
        key: key.clone(),
        native_id: key.0.clone(),
    }
}

async fn completed(rt: &Runtime, session: &SessionRef, text: &str) {
    let outcome = rt
        .prompt(session, vec![ContentBlock::text(text)])
        .await
        .unwrap()
        .outcome()
        .await;
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
}

#[tokio::test]
async fn basic_flows_run_over_memory_stores_without_sqlite() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let env = Arc::new(Environment {
            offset_ms: AtomicI64::new(0),
        });
        let memory = Arc::new(InMemoryRuntimeStore::new(env, None));
        let store: Arc<dyn RuntimeSessionStore> = memory.clone();
        let log = Arc::new(MemoryEventLog::new());
        let limits = RuntimeLimits {
            max_sessions: 1,
            max_active_turns: 1,
        };
        let rt = runtime(dir.path(), log.clone(), store.clone(), limits).await;

        // Open: the Runtime leases the new Session in the injected store.
        let a = rt
            .new_session(SessionSpec {
                name: Some("first".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let lease = store.lease(&a.native_id).await.unwrap().unwrap();
        assert_eq!(lease.identity.owner, "runtime");
        assert_eq!(lease.identity.generation, rt.generation() as i64);

        // Prompt with faux: the turn commits to the log and the tree.
        completed(&rt, &a, "hello").await;
        let seq = log.latest_seq(&a.key).await.unwrap();
        assert!(seq > 0);
        let entries = store
            .load(&a.native_id)
            .await
            .unwrap()
            .read_entries(None)
            .await
            .unwrap();
        assert!(entries.len() >= 2, "{entries:?}");

        // Attach replays the committed updates.
        let attached = RuntimeControl::attach(&rt, &a, 0).await.unwrap();
        assert_eq!(attached.latest_seq, seq);
        assert_eq!(attached.replay.count() as u64, seq);

        // Rename goes through the control marker of the injected store.
        rt.rename(loop_protocol::rpc::RenameParams {
            session: a.clone(),
            name: "renamed".into(),
            expected_revision: None,
        })
        .await
        .unwrap();
        assert_eq!(
            store.control_state(&a.native_id).await.unwrap().pending,
            None
        );

        // Close releases the lease; the listing keeps the Session.
        rt.close(&a.key, false).await.unwrap();
        assert_eq!(store.lease(&a.native_id).await.unwrap(), None);
        let list = rt.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].live);
        assert_eq!(list[0].name.as_deref(), Some("renamed"));

        // Reload continues the same sequence (the rename committed one more).
        let seq = log.latest_seq(&a.key).await.unwrap();
        rt.load_session(&a, SessionSpec::default()).await.unwrap();
        assert_eq!(log.latest_seq(&a.key).await.unwrap(), seq);
        completed(&rt, &a, "again").await;
        let after = log.read_after(&a.key, seq).await.unwrap();
        assert_eq!(after[0].session_seq, Some(seq + 1));

        // Unload: with room for one Session, opening B unloads idle A.
        let b = rt.new_session(SessionSpec::default()).await.unwrap();
        assert_eq!(store.lease(&a.native_id).await.unwrap(), None);
        assert!(store.lease(&b.native_id).await.unwrap().is_some());
        // A prompt for the unloaded Session loads it again (and unloads B).
        completed(&rt, &a, "back").await;
        assert!(store.lease(&a.native_id).await.unwrap().is_some());
        assert_eq!(store.lease(&b.native_id).await.unwrap(), None);

        rt.shutdown().await;
        assert_eq!(store.lease(&a.native_id).await.unwrap(), None);
        assert_eq!(database_files(dir.path()), Vec::<std::path::PathBuf>::new());
    })
    .await
    .unwrap();
}

fn update(generation: u64, session: &SessionKey, turn: &str, update: SessionUpdate) -> Event {
    Event::update(
        generation,
        1,
        UpdateEnvelope::live(
            "loop".into(),
            generation,
            session.clone(),
            Some(TurnId(turn.into())),
            0,
            update,
        ),
    )
}

#[tokio::test]
async fn recovery_over_memory_stores_ends_turns_and_reclaims_and_sweeps_leases() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let sessions_db = dir.path().join("agent/sessions.db");
        let env = Arc::new(Environment {
            offset_ms: AtomicI64::new(0),
        });
        let memory = Arc::new(InMemoryRuntimeStore::new(
            env.clone(),
            Some(sessions_db.clone()),
        ));
        let store: Arc<dyn RuntimeSessionStore> = memory.clone();
        let log = Arc::new(MemoryEventLog::new());

        // Generation 1 crashed: A is mid-turn and leased by it, B is leased
        // by a standalone process that died long ago, C by a live one.
        let cwd = Some(dir.path().to_string_lossy().into_owned());
        let mut keys = Vec::new();
        for _ in 0..3 {
            let reader = SessionStore::create(store.as_ref(), cwd.clone(), None)
                .await
                .unwrap();
            keys.push(SessionKey::from(reader.metadata().id.clone()));
        }
        let (a, b, c) = (keys[0].clone(), keys[1].clone(), keys[2].clone());
        let g = log.allocate_generation("runtime", 1, 0).await.unwrap();
        log.append_batch(vec![update(
            g,
            &a,
            "turn-1",
            SessionUpdate::TurnStarted {
                turn_id: TurnId("turn-1".into()),
            },
        )])
        .await
        .unwrap();
        let dead_runtime = LeaseIdentity {
            owner: "runtime".into(),
            pid: 1,
            generation: g as i64,
        };
        store.acquire_lease(&a.0, &dead_runtime).await.unwrap();
        env.offset_ms
            .store(-2 * LEASE_STALE_AFTER_MS, Ordering::SeqCst);
        let gone = LeaseIdentity {
            owner: "standalone".into(),
            pid: DEAD_PID,
            generation: 1,
        };
        store.acquire_lease(&b.0, &gone).await.unwrap();
        env.offset_ms.store(0, Ordering::SeqCst);
        let live = LeaseIdentity {
            owner: "standalone".into(),
            pid: std::process::id(),
            generation: 2,
        };
        store.acquire_lease(&c.0, &live).await.unwrap();

        let rt = runtime(
            dir.path(),
            log.clone(),
            store.clone(),
            RuntimeLimits::default(),
        )
        .await;
        let held =
            loop_app_core::runtime_instance::SessionDatabaseLock::acquire(&sessions_db).unwrap();
        let report = rt.recover(Some(&held)).await.unwrap();
        assert_eq!(
            report,
            Recovery {
                turns_ended: 1,
                escalations_withdrawn: 0,
                leases_reclaimed: 1,
                leases_swept: 1,
            }
        );
        let ended = log.read_after(&a, 0).await.unwrap();
        assert!(matches!(
            &ended.last().unwrap().event.kind,
            EventKind::Update(e) if matches!(
                &e.update,
                SessionUpdate::TurnEnded { stop: StopReason::RuntimeRestarted, .. }
            )
        ));
        assert_eq!(store.lease(&a.0).await.unwrap(), None);
        assert_eq!(store.lease(&b.0).await.unwrap(), None);
        assert_eq!(store.lease(&c.0).await.unwrap().unwrap().identity, live);
        // A second pass changes nothing.
        assert_eq!(rt.recover(Some(&held)).await.unwrap(), Recovery::default());

        // A lock of another database is refused, with the same text as over SQLite.
        let other = tempfile::tempdir().unwrap();
        let wrong = loop_app_core::runtime_instance::SessionDatabaseLock::acquire(
            &other.path().join("sessions.db"),
        )
        .unwrap();
        let error = rt.recover(Some(&wrong)).await.unwrap_err().to_string();
        assert!(
            error.contains("not this Runtime's Session database"),
            "{error}"
        );

        // Session A is usable again.
        let session = rt
            .load_session(&reference(&a), SessionSpec::default())
            .await
            .unwrap();
        completed(&rt, &session, "again").await;
        rt.shutdown().await;
        assert_eq!(database_files(dir.path()), Vec::<std::path::PathBuf>::new());
    })
    .await
    .unwrap();
}
