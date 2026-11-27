//! Lease behavior at the SQLite store boundary, with deterministic time and pid checks.
#![cfg(feature = "sqlite")]

use loop_agent::harness::session::sqlite::{
    LeaseEnvironment, LeaseIdentity, PidLiveness, SqliteSessionStore,
};
use loop_agent::harness::session::PendingSessionWrite;
use loop_agent::harness::SessionStore;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;

struct Environment {
    now: AtomicI64,
    liveness: AtomicU8,
}
impl LeaseEnvironment for Environment {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn pid_liveness(&self, _: u32) -> PidLiveness {
        match self.liveness.load(Ordering::SeqCst) {
            0 => PidLiveness::Dead,
            1 => PidLiveness::Alive,
            _ => PidLiveness::Unknown,
        }
    }
}
fn identity(owner: &str, pid: u32, generation: i64) -> LeaseIdentity {
    LeaseIdentity {
        owner: owner.into(),
        pid,
        generation,
    }
}

#[tokio::test]
async fn acquire_conflicts_and_renew_release_are_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(Environment {
        now: AtomicI64::new(1000),
        liveness: AtomicU8::new(1),
    });
    let store =
        SqliteSessionStore::with_environment(dir.path().join("sessions.db"), env.clone()).unwrap();
    let session = store.create(None, None).await.unwrap();
    let id = &session.metadata().id;
    let owner = identity("runtime", 42, 1);
    store.acquire_lease(id, &owner).await.unwrap();
    let error = store.check_resume(id).await.unwrap_err().to_string();
    assert!(error.contains(&format!(
        "Session {id} is in use by Runtime 42. Use `loop session attach {id}` instead."
    )));
    assert!(store
        .acquire_lease(id, &identity("standalone", 7, 1))
        .await
        .is_err());
    for impostor in [
        identity("runtime", 43, 1),
        identity("runtime", 42, 2),
        identity("standalone", 42, 1),
    ] {
        assert!(store.renew_lease(id, &impostor).await.is_err());
        assert!(store.release_lease(id, &impostor).await.is_err());
    }
    env.now.store(6000, Ordering::SeqCst);
    store.renew_lease(id, &owner).await.unwrap();
    assert_eq!(store.lease(id).await.unwrap().unwrap().heartbeat_ms, 6000);
    store.release_lease(id, &owner).await.unwrap();
    assert!(store.lease(id).await.unwrap().is_none());
    store.check_resume(id).await.unwrap();
}

#[tokio::test]
async fn takeover_requires_stale_heartbeat_and_proven_dead_pid() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(Environment {
        now: AtomicI64::new(1000),
        liveness: AtomicU8::new(0),
    });
    let store =
        SqliteSessionStore::with_environment(dir.path().join("sessions.db"), env.clone()).unwrap();
    let session = store.create(None, None).await.unwrap();
    let id = &session.metadata().id;
    let old = identity("runtime", 42, 1);
    let new = identity("standalone", 7, 2);
    store.acquire_lease(id, &old).await.unwrap();
    // Dead but fresh, including just before the exact 30 s threshold.
    for now in [1000, 30_999] {
        env.now.store(now, Ordering::SeqCst);
        assert!(store.takeover_lease(id, &new, false).await.is_err());
    }
    env.now.store(31_000, Ordering::SeqCst);
    for liveness in [1, 2] {
        env.liveness.store(liveness, Ordering::SeqCst);
        assert!(store.takeover_lease(id, &new, false).await.is_err());
    }
    env.liveness.store(0, Ordering::SeqCst);
    store.takeover_lease(id, &new, false).await.unwrap();
    assert_eq!(store.lease(id).await.unwrap().unwrap().identity, new);
    assert!(store.renew_lease(id, &old).await.is_err());
    assert!(store.release_lease(id, &old).await.is_err());
    let pending = || PendingSessionWrite::Label {
        label: "fenced".into(),
    };
    assert!(store
        .for_lease(old)
        .append_entry(id, pending())
        .await
        .is_err());
    assert!(store.append_entry(id, pending()).await.is_err());
    assert!(store.delete(id).await.is_err());
    store
        .for_lease(new.clone())
        .append_entry(id, pending())
        .await
        .unwrap();
    store.release_lease(id, &new).await.unwrap();
    assert!(store
        .for_lease(new)
        .append_entry(id, pending())
        .await
        .is_err());
}

#[tokio::test]
async fn runtime_reclaim_requires_lock_and_only_reclaims_runtime_leases() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(Environment {
        now: AtomicI64::new(1000),
        liveness: AtomicU8::new(2),
    });
    let store = SqliteSessionStore::with_environment(dir.path().join("sessions.db"), env).unwrap();
    let session = store.create(None, None).await.unwrap();
    let id = &session.metadata().id;
    let old = identity("runtime", 42, 1);
    let new = identity("runtime", 43, 2);
    store.acquire_lease(id, &old).await.unwrap();
    assert!(store.takeover_lease(id, &new, false).await.is_err());
    assert!(store
        .takeover_lease(id, &identity("standalone", 7, 2), true)
        .await
        .is_err());
    // The associated Lock is proof that the previous Runtime is dead, so no wait.
    store.takeover_lease(id, &new, true).await.unwrap();
    assert!(store.for_lease(old).delete(id).await.is_err());
    store.for_lease(new.clone()).delete(id).await.unwrap();
    assert!(store.lease(id).await.unwrap().is_none()); // ON DELETE CASCADE
    let second = store.create(None, None).await.unwrap();
    let id = &second.metadata().id;
    store
        .acquire_lease(id, &identity("standalone", 7, 1))
        .await
        .unwrap();
    assert!(store.takeover_lease(id, &new, true).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_acquisitions_have_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
    let session = store.create(None, None).await.unwrap();
    let mut tasks = Vec::new();
    for generation in 1..=16 {
        let store = store.clone();
        let id = session.metadata().id.clone();
        tasks.push(tokio::spawn(async move {
            store
                .acquire_lease(&id, &identity("runtime", std::process::id(), generation))
                .await
                .is_ok()
        }));
    }
    let mut winners = 0;
    for task in tasks {
        if task.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);
}

#[tokio::test(start_paused = true)]
async fn standalone_resume_takes_over_only_stale_dead_owners_and_renews() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(Environment {
        now: AtomicI64::new(1000),
        liveness: AtomicU8::new(0),
    });
    let store =
        SqliteSessionStore::with_environment(dir.path().join("resume.db"), env.clone()).unwrap();
    let reader = store.create(None, None).await.unwrap();
    let id = &reader.metadata().id;
    store
        .acquire_lease(id, &identity("runtime", 42, 1))
        .await
        .unwrap();
    assert!(store.standalone_resume(id).await.is_err());
    env.now.store(31_000, Ordering::SeqCst);
    let resumed = store.standalone_resume(id).await.unwrap();
    let lease = store.lease(id).await.unwrap().unwrap();
    assert_eq!(lease.identity.owner, "standalone");
    resumed
        .append_entry(
            id,
            PendingSessionWrite::Label {
                label: "resumed".into(),
            },
        )
        .await
        .unwrap();
    env.now.store(36_000, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    // The renewal's storage operation runs on the blocking pool, which takes
    // real time however the test clock is set.
    eventually("the renewal", || async {
        store.lease(id).await.unwrap().unwrap().heartbeat_ms == 36_000
    })
    .await;
    drop(resumed);
    eventually("the release", || async {
        store.lease(id).await.unwrap().is_none()
    })
    .await;
}

/// Wait in real time (not the paused test clock) until `check` holds,
/// for at most 30 s.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !check().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(2)))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn standalone_store_can_create_fork_and_switch_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("standalone.db")).unwrap();
    let source = store.create(None, None).await.unwrap();
    let resumed = store
        .standalone_resume(&source.metadata().id)
        .await
        .unwrap();
    let created = resumed.create(None, None).await.unwrap();
    resumed
        .append_entry(
            &created.metadata().id,
            PendingSessionWrite::Label {
                label: "new".into(),
            },
        )
        .await
        .unwrap();
    let forked = resumed
        .fork(
            &created.metadata().id,
            loop_agent::harness::SessionForkSelection::All,
            None,
            None,
        )
        .await
        .unwrap();
    resumed
        .append_entry(
            &forked.metadata().id,
            PendingSessionWrite::Label {
                label: "fork".into(),
            },
        )
        .await
        .unwrap();
    let other = store.create(None, None).await.unwrap();
    resumed.open_writer(&other.metadata().id).await.unwrap();
    resumed
        .append_entry(
            &other.metadata().id,
            PendingSessionWrite::Label {
                label: "switched".into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn runtime_lock_reclaims_recycled_pid_but_ordinary_takeover_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(Environment {
        now: AtomicI64::new(1000),
        liveness: AtomicU8::new(1),
    });
    let store =
        SqliteSessionStore::with_environment(dir.path().join("recycled.db"), env.clone()).unwrap();
    let old = identity("runtime", std::process::id(), 1);
    let replacement = identity("runtime", std::process::id(), 2);
    // A live pid can belong to a different process incarnation. The associated
    // runtime.lock proves that the recorded Runtime owner is dead.
    for now in [1000, 31_000] {
        env.now.store(1000, Ordering::SeqCst);
        let reader = store.create(None, None).await.unwrap();
        let id = &reader.metadata().id;
        store.acquire_lease(id, &old).await.unwrap();
        env.now.store(now, Ordering::SeqCst);
        assert!(store.takeover_lease(id, &replacement, false).await.is_err());
        store.takeover_lease(id, &replacement, true).await.unwrap();
        assert_eq!(
            store.lease(id).await.unwrap().unwrap().identity,
            replacement
        );
        assert!(store.renew_lease(id, &old).await.is_err());
        assert!(store.release_lease(id, &old).await.is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn failed_standalone_renewal_cancels_work_and_fences_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("lost.db")).unwrap();
    let reader = store.create(None, None).await.unwrap();
    let id = &reader.metadata().id;
    let resumed = store.standalone_resume(id).await.unwrap();
    let cancelled = resumed.write_cancellation().unwrap().child_token();
    let identity = store.lease(id).await.unwrap().unwrap().identity;
    store.release_lease(id, &identity).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    eventually("the failed renewal to cancel work", || async {
        cancelled.is_cancelled()
    })
    .await;
    // A transient renewal failure must fence this handle even if ownership
    // subsequently appears to match again.
    store.acquire_lease(id, &identity).await.unwrap();
    assert!(resumed
        .append_entry(
            id,
            PendingSessionWrite::Label {
                label: "lost".into()
            }
        )
        .await
        .is_err());
}

#[tokio::test]
async fn picker_and_search_preserve_leases_and_show_another_writers_history() {
    use loop_agent::harness::session::{create_scanning_session_search, list_session_summaries};
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("listing.db")).unwrap();
    let candidate = store.create(None, None).await.unwrap();
    let owned = store.create(None, None).await.unwrap();
    let empty = store.create(None, None).await.unwrap();
    for reader in [&candidate, &owned] {
        store
            .append_entry(
                &reader.metadata().id,
                PendingSessionWrite::Message {
                    message: loop_agent::AgentMessage::user_text("picker history"),
                },
            )
            .await
            .unwrap();
    }
    let writer_a = store.standalone_writer();
    let _writer_b = store.standalone_resume(&owned.metadata().id).await.unwrap();
    let ids = [
        &candidate.metadata().id,
        &owned.metadata().id,
        &empty.metadata().id,
    ];
    let mut before = Vec::new();
    for id in ids {
        before.push(store.lease(id).await.unwrap());
    }
    let summaries = list_session_summaries(&writer_a, None).await.unwrap();
    assert!(
        summaries.iter().any(|s| s.id == owned.metadata().id),
        "leased history disappeared from the picker"
    );
    assert!(summaries.iter().any(|s| s.id == candidate.metadata().id));
    assert!(!summaries.iter().any(|s| s.id == empty.metadata().id));
    let search = create_scanning_session_search(Arc::new(writer_a.clone()));
    let hits = search.search("picker history", None, 10).await.unwrap();
    assert_eq!(hits.len(), 2);
    // Reader loading must also be usable directly on another writer's Session.
    writer_a.load(&owned.metadata().id).await.unwrap();
    for (id, expected) in ids.into_iter().zip(before) {
        assert_eq!(
            store.lease(id).await.unwrap(),
            expected,
            "reading changed a lease"
        );
    }
    // Merely inspecting a Session leaves it available to a different writer.
    let _new_writer = store
        .standalone_resume(&candidate.metadata().id)
        .await
        .unwrap();
}

#[tokio::test]
async fn standalone_bootstrap_lease_integration_is_unix_only() {
    use loop_agent::harness::session::sqlite::create_sqlite_session_store_for_resume;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("platform.db");
    let first = create_sqlite_session_store_for_resume(&path, None)
        .await
        .unwrap();
    let session = first.create(None, None).await.unwrap();
    let id = &session.metadata().id;
    let probe = SqliteSessionStore::open(&path).unwrap();
    let second = create_sqlite_session_store_for_resume(&path, Some(id)).await;
    if cfg!(unix) {
        assert!(probe.lease(id).await.unwrap().is_some());
        assert!(second.is_err());
    } else {
        // No persistent owner row can strand a Session after a Windows crash.
        assert!(probe.lease(id).await.unwrap().is_none());
        let second = second.unwrap();
        second
            .append_entry(
                id,
                PendingSessionWrite::Label {
                    label: "no platform lease".into(),
                },
            )
            .await
            .unwrap();
        assert!(probe.lease(id).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn repository_open_and_harness_switch_admit_writers_but_readers_do_not() {
    use loop_agent::harness::{
        create_session_repository, AgentHarness, AgentHarnessOptions, AgentHarnessResources,
        HostExecutionEnv, SandboxMode,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("admission.db")).unwrap();
    let writer = Arc::new(store.standalone_writer());
    let repo = create_session_repository(writer.clone(), None);
    let initial = repo.create(None, None).await.unwrap();
    let initial_id = initial.metadata().id.clone();
    let owned = store.create(None, None).await.unwrap();
    let _other_writer = store.standalone_resume(&owned.metadata().id).await.unwrap();
    writer.load(&owned.metadata().id).await.unwrap();
    assert!(repo.open(&owned.metadata().id).await.is_err());
    let candidate = store.create(None, None).await.unwrap();
    repo.open(&candidate.metadata().id).await.unwrap();
    assert!(store
        .lease(&candidate.metadata().id)
        .await
        .unwrap()
        .is_some());
    let harness = AgentHarness::new(AgentHarnessOptions {
        models: Arc::new(loop_ai::Models::create(Default::default())),
        model: None,
        session: initial,
        host_env: Arc::new(HostExecutionEnv::new(dir.path())),
        tools: vec![],
        system_prompt: String::new(),
        sandbox: SandboxMode::Disabled,
        resources: AgentHarnessResources::default(),
    });
    assert!(harness.switch_session(&owned.metadata().id).await.is_err());
    assert_eq!(harness.session_id().await, initial_id);
    let switch_target = store.create(None, None).await.unwrap();
    writer.load(&switch_target.metadata().id).await.unwrap();
    assert!(store
        .lease(&switch_target.metadata().id)
        .await
        .unwrap()
        .is_none());
    harness
        .switch_session(&switch_target.metadata().id)
        .await
        .unwrap();
    assert_eq!(
        store
            .lease(&switch_target.metadata().id)
            .await
            .unwrap()
            .unwrap()
            .identity,
        store.lease(&initial_id).await.unwrap().unwrap().identity
    );
}

fn label(text: &str) -> PendingSessionWrite {
    PendingSessionWrite::Label { label: text.into() }
}

#[tokio::test]
async fn release_leases_is_terminal_idempotent_and_spares_other_owners() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let base = SqliteSessionStore::open(&path).unwrap();
        let writer = base.standalone_writer();
        let a = writer
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let b = writer
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        // Another writer's Session keeps its lease.
        let other = base.standalone_writer();
        let c = other
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        writer.append_entry(&a, label("before")).await.unwrap();

        writer.release_leases().await.unwrap();
        assert!(base.lease(&a).await.unwrap().is_none());
        assert!(base.lease(&b).await.unwrap().is_none());
        assert!(base.lease(&c).await.unwrap().is_some());
        // Terminal: nothing more is written or claimed through it.
        assert!(writer.append_entry(&a, label("after")).await.is_err());
        assert!(writer.create(None, None).await.is_err());
        assert!(writer.open_writer(&a).await.is_err());
        assert!(base.lease(&a).await.unwrap().is_none());
        // Idempotent.
        writer.release_leases().await.unwrap();
        other.append_entry(&c, label("still mine")).await.unwrap();
        // A store without leases has nothing to release.
        base.release_leases().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn release_during_an_active_write_is_fenced_and_reports_errors() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let base = SqliteSessionStore::open(&path).unwrap();
        let writer = Arc::new(base.standalone_writer());
        let id = writer
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        // Writes race the release; each either lands before it or is refused.
        let mut writes = tokio::task::JoinSet::new();
        for n in 0..20 {
            let (writer, id) = (writer.clone(), id.clone());
            writes.spawn(async move { writer.append_entry(&id, label(&format!("w{n}"))).await });
        }
        // Release closes the writer at once; its delete may lose to a write
        // holding the lock, and then says so, and a repeat finishes it.
        if let Err(error) = writer.release_leases().await {
            assert!(error.to_string().contains("locked"), "{error}");
        }
        while let Some(done) = writes.join_next().await {
            let _ = done.unwrap();
        }
        writer.release_leases().await.unwrap();
        assert!(base.lease(&id).await.unwrap().is_none());
        assert!(writer.append_entry(&id, label("late")).await.is_err());

        // A database that stays locked makes the release fail, and says so.
        let locked = base.standalone_writer();
        let id = locked
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let holder = rusqlite::Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        // It fails fast: exit must not hang on a locked database; the
        // startup sweep removes what is left.
        let started = std::time::Instant::now();
        let error = locked.release_leases().await.unwrap_err();
        assert!(
            // A 200 ms busy timeout; the bound only needs to stay below the
            // 5 s one other writes use, with room for a loaded machine.
            started.elapsed() < std::time::Duration::from_secs(3),
            "release took {:?}",
            started.elapsed()
        );
        assert!(error.to_string().contains("locked"), "{error}");
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(
            base.lease(&id).await.unwrap().is_some(),
            "nothing was released"
        );
        locked.release_leases().await.unwrap();
        assert!(base.lease(&id).await.unwrap().is_none());
    })
    .await
    .unwrap();
}

#[test]
fn dropping_the_last_store_without_a_runtime_releases_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (writer, id) = rt.block_on(async {
        let writer = SqliteSessionStore::open(&path).unwrap().standalone_writer();
        let id = writer
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        (writer, id)
    });
    drop(rt);
    // No Tokio runtime here: the fallback deletes synchronously.
    drop(writer);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_leases WHERE session_id=?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

/// A store on a fresh database with a fake clock and pid probe.
fn swept_store(
    dir: &std::path::Path,
) -> (SqliteSessionStore, Arc<Environment>, std::path::PathBuf) {
    let path = dir.join("sessions.db");
    let env = Arc::new(Environment {
        now: AtomicI64::new(1_000_000),
        liveness: AtomicU8::new(0),
    });
    let store = SqliteSessionStore::with_environment(&path, env.clone()).unwrap();
    (store, env, path)
}

async fn leased(store: &SqliteSessionStore, owner: &LeaseIdentity) -> String {
    let id = store
        .create(None, None)
        .await
        .unwrap()
        .metadata()
        .id
        .clone();
    store.acquire_lease(&id, owner).await.unwrap();
    id
}

const STALE: i64 = 31_000;

#[tokio::test]
async fn the_sweep_removes_stale_rows_of_dead_owners() {
    let dir = tempfile::tempdir().unwrap();
    let (store, env, _) = swept_store(dir.path());
    // Four rows left by `loop` processes that exited.
    let mut left = Vec::new();
    for pid in [77560, 48014, 59908, 60022] {
        left.push(leased(&store, &identity("standalone", pid, 1)).await);
    }
    // Fresh rows are never swept, whatever the pid.
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 0);
    env.now.fetch_add(STALE, Ordering::SeqCst);
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 4);
    for id in &left {
        assert!(store.lease(id).await.unwrap().is_none());
    }
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 0);
}

#[tokio::test]
async fn the_sweep_keeps_live_and_unknown_owners_and_new_owners() {
    let dir = tempfile::tempdir().unwrap();
    let (store, env, _) = swept_store(dir.path());
    let id = leased(&store, &identity("standalone", 4242, 1)).await;
    env.now.fetch_add(STALE, Ordering::SeqCst);
    for (liveness, what) in [(1, "alive"), (2, "unknown")] {
        env.liveness.store(liveness, Ordering::SeqCst);
        assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 0, "{what}");
        assert!(store.lease(&id).await.unwrap().is_some(), "{what}");
    }
    // Replaced by a new owner: its row is fresh and stays.
    env.liveness.store(0, Ordering::SeqCst);
    let successor = identity("standalone", 5151, 2);
    store.takeover_lease(&id, &successor, false).await.unwrap();
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 0);
    assert_eq!(store.lease(&id).await.unwrap().unwrap().identity, successor);
}

#[tokio::test]
async fn runtime_rows_need_the_lock_of_this_database() {
    let dir = tempfile::tempdir().unwrap();
    let (store, env, path) = swept_store(dir.path());
    let id = leased(&store, &identity("runtime", 3131, 7)).await;
    env.now.fetch_add(STALE, Ordering::SeqCst);
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 0);
    let other = dir.path().join("other/sessions.db");
    assert_eq!(store.sweep_stale_leases(Some(&other)).await.unwrap(), 0);
    assert!(store.lease(&id).await.unwrap().is_some());
    assert_eq!(store.sweep_stale_leases(Some(&path)).await.unwrap(), 1);
    assert!(store.lease(&id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_racing_the_sweep_is_never_lost_silently() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let (store, env, _) = swept_store(dir.path());
        let store = Arc::new(store);
        for round in 0..20 {
            let owner = identity("standalone", 9000 + round, 1);
            let id = leased(&store, &owner).await;
            env.now.fetch_add(STALE, Ordering::SeqCst);
            let (swept, renewed) = tokio::join!(
                store.sweep_stale_leases(None),
                store.renew_lease(&id, &owner)
            );
            swept.unwrap();
            let row = store.lease(&id).await.unwrap();
            // A renewal that succeeded keeps its row; one that lost the race
            // is told so, and the owner stops writing.
            match renewed {
                Ok(()) => assert_eq!(row.map(|r| r.identity), Some(owner), "round {round}"),
                Err(_) => assert!(row.is_none(), "round {round}"),
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_acquisition_that_reaches_its_transaction_after_release_is_refused() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let base = SqliteSessionStore::open(&path).unwrap();
        let writer = base.standalone_writer();
        let a = writer
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let me = base.lease(&a).await.unwrap().unwrap().identity;
        // A Session this writer has not claimed yet.
        let b = base.create(None, None).await.unwrap().metadata().id.clone();
        writer.release_leases().await.unwrap();
        // `open_writer` passed its own check before the release; its claim
        // runs now, in its own transaction.
        let claimed = writer.takeover_lease(&b, &me, false).await;
        assert!(claimed.is_err(), "a released writer claimed {b}");
        assert!(writer.acquire_lease(&b, &me).await.is_err());
        assert!(base.lease(&b).await.unwrap().is_none());
        assert!(base.lease(&a).await.unwrap().is_none());
    })
    .await
    .unwrap();
}
