//! Lease behavior at the SQLite store boundary, with deterministic time and pid checks.
#![cfg(feature = "sqlite")]

use loop_agent::harness::{
    session::{
        sqlite::{LeaseEnvironment, LeaseIdentity, PidLiveness, SqliteSessionStore},
        PendingSessionWrite,
    },
    SessionStore,
};
use std::sync::{
    atomic::{AtomicI64, AtomicU8, Ordering},
    Arc,
};

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
        "Session {id} is in use by Runtime 42. Use `loop attach {id}` instead."
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
    // The timer's storage operation runs on the blocking pool.
    for _ in 0..100 {
        if store.lease(id).await.unwrap().unwrap().heartbeat_ms == 36_000 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(store.lease(id).await.unwrap().unwrap().heartbeat_ms, 36_000);
    drop(resumed);
    for _ in 0..100 {
        if store.lease(id).await.unwrap().is_none() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("standalone resume lease was not released");
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
    resumed.load(&other.metadata().id).await.unwrap();
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
async fn runtime_lock_never_overrides_a_proven_live_owner() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(Environment {
        now: AtomicI64::new(1000),
        liveness: AtomicU8::new(1),
    });
    let store = SqliteSessionStore::with_environment(dir.path().join("live.db"), env).unwrap();
    let reader = store.create(None, None).await.unwrap();
    let id = &reader.metadata().id;
    store
        .acquire_lease(id, &identity("runtime", 42, 1))
        .await
        .unwrap();
    assert!(store
        .takeover_lease(id, &identity("runtime", 43, 2), true)
        .await
        .is_err());
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
    for _ in 0..100 {
        if cancelled.is_cancelled() {
            break;
        }
        store.lease(id).await.unwrap();
    }
    assert!(cancelled.is_cancelled());
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
