//! Crash recovery on Runtime start (design §12).
use loop_agent::harness::session::{
    sqlite::{LeaseIdentity, SqliteSessionStore},
    SessionStore,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, ContentBlock, SessionKey, SessionRef, SessionSpec, SessionUpdate, StopReason,
    TurnId, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::{
    log::{Event, EventKind, EventLog, SqliteEventLog},
    Recovery, Runtime, RuntimeLimits,
};
use loop_wire::RuntimeControl;
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};

const LIMIT: Duration = Duration::from_secs(60);

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

async fn runtime(dir: &Path, log: Arc<dyn EventLog>) -> Runtime {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    Runtime::new(globals, opts(dir), log, RuntimeLimits::default())
        .await
        .unwrap()
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
fn started(generation: u64, session: &SessionKey, turn: &str) -> Event {
    update(
        generation,
        session,
        turn,
        SessionUpdate::TurnStarted {
            turn_id: TurnId(turn.into()),
        },
    )
}
fn ended(generation: u64, session: &SessionKey, turn: &str) -> Event {
    update(
        generation,
        session,
        turn,
        SessionUpdate::TurnEnded {
            turn_id: TurnId(turn.into()),
            stop: StopReason::EndTurn,
            error: None,
        },
    )
}
fn audit(generation: u64, session: &SessionKey, kind: EventKind) -> Event {
    Event {
        generation,
        session_key: Some(session.clone()),
        turn: None,
        ts_ms: 1,
        kind,
    }
}
fn reference(key: &SessionKey) -> SessionRef {
    SessionRef {
        instance: "loop".into(),
        key: key.clone(),
        native_id: key.0.clone(),
    }
}

struct Seeded {
    a: SessionKey,
    b: SessionKey,
    store: SqliteSessionStore,
    log: Arc<SqliteEventLog>,
}

/// Generation 1 crashed with Session A mid-turn and an approval parked in A.
/// Turn and approval ids are process-local counters that repeat.
async fn seed(dir: &Path) -> Seeded {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let store = SqliteSessionStore::open(agent.join("sessions.db")).unwrap();
    let cwd = Some(dir.to_string_lossy().into_owned());
    let a: SessionKey = store
        .create(cwd.clone(), None)
        .await
        .unwrap()
        .metadata()
        .id
        .clone()
        .into();
    let b: SessionKey = store
        .create(cwd, None)
        .await
        .unwrap()
        .metadata()
        .id
        .clone()
        .into();
    let log = Arc::new(SqliteEventLog::open(&agent).await.unwrap());
    let g = log.allocate_generation("runtime", 1, 0).await.unwrap();
    assert_eq!(g, 1);
    let approval = |n: u32| json!({"id": format!("approval-{n}")});
    log.append_batch(vec![
        started(g, &a, "turn-1"),
        ended(g, &a, "turn-1"),
        started(g, &b, "turn-1"),
        ended(g, &b, "turn-1"),
        audit(g, &a, EventKind::EscalationRaised(approval(1))),
        audit(g, &a, EventKind::EscalationAnswered(approval(1))),
        audit(g, &b, EventKind::EscalationRaised(approval(1))),
        audit(g, &b, EventKind::EscalationWithdrawn(approval(1))),
        // The id repeats; only this second request is still open.
        started(g, &a, "turn-2"),
        audit(g, &a, EventKind::EscalationRaised(approval(1))),
    ])
    .await
    .unwrap();
    // Session A's lease belongs to the dead Runtime. Its pid (1) is alive and
    // its heartbeat fresh, so only the runtime.lock proof may reclaim it.
    let dead_runtime = LeaseIdentity {
        owner: "runtime".into(),
        pid: 1,
        generation: 1,
    };
    store.acquire_lease(&a.0, &dead_runtime).await.unwrap();
    let standalone = LeaseIdentity {
        owner: "standalone".into(),
        pid: 1,
        generation: 0,
    };
    store.acquire_lease(&b.0, &standalone).await.unwrap();
    Seeded { a, b, store, log }
}

fn withdrawals(records: &[loop_runtime::log::LoggedEvent]) -> Vec<serde_json::Value> {
    records
        .iter()
        .filter_map(|r| match &r.event.kind {
            EventKind::EscalationWithdrawn(p) if p["reason"] == "runtime_restarted" => {
                Some(p.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn recovery_ends_open_turns_withdraws_parked_escalations_and_reclaims_leases() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let Seeded { a, b, store, log } = seed(dir.path()).await;
        let a_before = log.latest_seq(&a).await.unwrap();
        let b_before = log.latest_seq(&b).await.unwrap();
        let rt = runtime(dir.path(), log.clone()).await;
        assert_eq!(rt.generation(), 2);

        let report = rt.recover(true).await.unwrap();
        assert_eq!(
            report,
            Recovery {
                turns_ended: 1,
                escalations_withdrawn: 1,
                leases_reclaimed: 1,
            }
        );

        // Exactly one uncertain end, as the next contiguous Update of A.
        let attached = rt.attach(&reference(&a), a_before).await.unwrap();
        assert_eq!(attached.latest_seq, a_before + 1);
        let replay: Vec<_> = attached.replay.collect();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, a_before + 1);
        assert_eq!(replay[0].generation, 2);
        assert_eq!(replay[0].turn, Some(TurnId("turn-2".into())));
        assert!(matches!(
            &replay[0].update,
            SessionUpdate::TurnEnded { turn_id, stop: StopReason::RuntimeRestarted, error: None }
                if turn_id.0 == "turn-2"
        ));
        assert_eq!(log.latest_seq(&b).await.unwrap(), b_before);

        let all = log.read_runtime_after(0).await.unwrap();
        let withdrawn = withdrawals(&all);
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["id"], "approval-1");
        assert_eq!(withdrawn[0]["generation"], 1);
        let record = all
            .iter()
            .find(|r| matches!(&r.event.kind, EventKind::EscalationWithdrawn(p) if p["reason"] == "runtime_restarted"))
            .unwrap();
        assert_eq!(record.event.session_key.as_ref(), Some(&a));
        assert_eq!(record.event.generation, 2);

        // The dead Runtime's lease is gone; another owner's lease is untouched.
        assert!(store.lease(&a.0).await.unwrap().is_none());
        assert_eq!(
            store.lease(&b.0).await.unwrap().unwrap().identity.owner,
            "standalone"
        );

        // Idempotent within a generation.
        let count = all.len();
        assert_eq!(rt.recover(true).await.unwrap(), Recovery::default());
        assert_eq!(log.read_runtime_after(0).await.unwrap().len(), count);

        // Session A is usable again.
        let session = rt
            .load_session(&reference(&a), SessionSpec::default())
            .await
            .unwrap();
        let outcome = rt
            .prompt(&session, vec![ContentBlock::text("again")])
            .await
            .unwrap()
            .outcome()
            .await;
        assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
        rt.shutdown().await;

        // A later generation finds nothing more to recover.
        let rt = runtime(dir.path(), log.clone()).await;
        assert_eq!(rt.generation(), 3);
        assert_eq!(rt.recover(true).await.unwrap(), Recovery::default());
        let after = log.read_runtime_after(0).await.unwrap();
        assert_eq!(withdrawals(&after).len(), 1);
        let restarted = log
            .read_after(&a, 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| {
                matches!(
                    &r.event.kind,
                    EventKind::Update(e) if matches!(e.update, SessionUpdate::TurnEnded { stop: StopReason::RuntimeRestarted, .. })
                )
            })
            .count();
        assert_eq!(restarted, 1);
        rt.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn without_the_runtime_lock_a_live_runtime_lease_is_never_taken_over() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let Seeded { a, store, log, .. } = seed(dir.path()).await;
        let rt = runtime(dir.path(), log.clone()).await;
        let report = rt.recover(false).await.unwrap();
        assert_eq!(report.leases_reclaimed, 0);
        // Log recovery does not depend on lease ownership.
        assert_eq!(report.turns_ended, 1);
        assert_eq!(
            store
                .lease(&a.0)
                .await
                .unwrap()
                .unwrap()
                .identity
                .generation,
            1
        );
        rt.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn opening_takes_over_only_a_stale_lease_of_a_dead_process() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let Seeded { a, b, store, log } = seed(dir.path()).await;
        let rt = runtime(dir.path(), log.clone()).await;
        // A crashed standalone writer: dead pid and a heartbeat long past 30 s.
        store
            .release_lease(
                &b.0,
                &LeaseIdentity {
                    owner: "standalone".into(),
                    pid: 1,
                    generation: 0,
                },
            )
            .await
            .unwrap();
        let crashed = LeaseIdentity {
            owner: "standalone".into(),
            pid: i32::MAX as u32,
            generation: 0,
        };
        store.acquire_lease(&b.0, &crashed).await.unwrap();
        rusqlite::Connection::open(dir.path().join("agent/sessions.db"))
            .unwrap()
            .execute("UPDATE session_leases SET heartbeat_ms=0", [])
            .unwrap();
        rt.load_session(&reference(&b), SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(
            store.lease(&b.0).await.unwrap().unwrap().identity.owner,
            "runtime"
        );
        // A live owner (pid 1), even with an old heartbeat, is never taken over.
        let error = rt
            .load_session(&reference(&a), SessionSpec::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("in use"), "{error}");
        assert_eq!(
            store
                .lease(&a.0)
                .await
                .unwrap()
                .unwrap()
                .identity
                .generation,
            1
        );
        rt.shutdown().await;
    })
    .await
    .expect("test timed out");
}
