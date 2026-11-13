//! Session lease conformance suite (the Runtime's `SessionLeases` trait).
//!
//! Each check takes a factory. The factory gets a [`ScriptedEnvironment`]
//! (the clock and the pid probe, which the check drives) and returns a fresh,
//! empty `Arc<dyn RuntimeSessionStore>` that uses it. The store must report a
//! `database_path`, so that the sweep can be checked with and without the
//! proof of the Runtime lock.
//!
//! The checks assert the error variant and the exact text, because the
//! Runtime and the Surfaces show these texts to people.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use loop_agent::harness::session::{
    LeaseEnvironment, LeaseIdentity, PendingSessionWrite, PidLiveness, RuntimeSessionStore,
    LEASE_STALE_AFTER_MS,
};
use loop_agent::harness::types::SessionError;
use loop_agent::AgentMessage;

use crate::Fixture;

/// A clock and a pid probe that a check sets. Every pid is alive until the
/// check says otherwise.
pub struct ScriptedEnvironment {
    now: AtomicI64,
    liveness: Mutex<HashMap<u32, PidLiveness>>,
}

impl ScriptedEnvironment {
    /// The clock starts at `now` ms.
    pub fn new(now: i64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(now),
            liveness: Mutex::new(HashMap::new()),
        })
    }
    /// Move the clock forward.
    pub fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }
    /// Set what the probe reports for `pid`.
    pub fn set(&self, pid: u32, liveness: PidLiveness) {
        self.liveness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid, liveness);
    }
}

impl LeaseEnvironment for ScriptedEnvironment {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn pid_liveness(&self, pid: u32) -> PidLiveness {
        self.liveness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .copied()
            .unwrap_or(PidLiveness::Alive)
    }
}

/// Run every check below. The factory is called once per check.
pub async fn run_all<F, Fut>(factory: F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    acquire(&factory).await;
    renew(&factory).await;
    takeover_after_stale(&factory).await;
    runtime_reclaim(&factory).await;
    release(&factory).await;
    sweep(&factory).await;
    two_owners(&factory).await;
    delete_removes_the_lease(&factory).await;
    control_markers(&factory).await;
}

const START: i64 = 1_700_000_000_000;

fn owner(owner: &str, pid: u32, generation: i64) -> LeaseIdentity {
    LeaseIdentity {
        owner: owner.into(),
        pid,
        generation,
    }
}

fn in_use(id: &str, owner: &str, pid: u32) -> String {
    format!("Session {id} is in use by {owner} {pid}. Use `loop session attach {id}` instead.")
}

fn lost(id: &str) -> String {
    format!("Session {id} lease ownership was lost; stop writing and cancel work.")
}

#[track_caller]
fn assert_in_use<T: std::fmt::Debug>(
    result: Result<T, SessionError>,
    id: &str,
    who: &str,
    pid: u32,
) {
    match result {
        Err(SessionError::InUse(text)) => assert_eq!(text, in_use(id, who, pid)),
        other => panic!("expected InUse for {id}, got {other:?}"),
    }
}

#[track_caller]
fn assert_lost<T: std::fmt::Debug>(result: Result<T, SessionError>, id: &str) {
    match result {
        Err(SessionError::Invalid(text)) => assert_eq!(text, lost(id)),
        other => panic!("expected lost ownership of {id}, got {other:?}"),
    }
}

fn text(label: &str) -> PendingSessionWrite {
    PendingSessionWrite::Message {
        message: AgentMessage::user_text(label),
    }
}

async fn session(store: &dyn RuntimeSessionStore) -> String {
    store
        .create(None, None)
        .await
        .unwrap()
        .metadata()
        .id
        .clone()
}

/// Acquire: checks of the claimant, an unknown Session, idempotence and a
/// second owner.
pub async fn acquire<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    let a = owner("runtime", 4101, 1);

    for bad in [owner("", 4101, 1), owner("runtime", 0, 1)] {
        match store.acquire_lease(&id, &bad).await {
            Err(SessionError::Invalid(text)) => {
                assert_eq!(text, "lease requires an owner and a positive process id")
            }
            other => panic!("expected an invalid claimant, got {other:?}"),
        }
    }
    assert!(matches!(
        store.acquire_lease("no-such-session", &a).await,
        Err(SessionError::NotFound(_))
    ));
    assert_eq!(store.lease(&id).await.unwrap(), None);
    store.check_resume(&id).await.unwrap();

    store.acquire_lease(&id, &a).await.unwrap();
    let lease = store.lease(&id).await.unwrap().unwrap();
    assert_eq!(lease.identity, a);
    assert_eq!(lease.heartbeat_ms, START);

    // The same identity again changes nothing, not even the heartbeat.
    env.advance(1_000);
    store.acquire_lease(&id, &a).await.unwrap();
    assert_eq!(store.lease(&id).await.unwrap().unwrap().heartbeat_ms, START);

    let b = owner("standalone", 4102, 7);
    assert_in_use(store.acquire_lease(&id, &b).await, &id, "Runtime", 4101);
    assert_in_use(store.check_resume(&id).await, &id, "Runtime", 4101);
    // A plain acquire never replaces, however old the lease.
    env.advance(LEASE_STALE_AFTER_MS * 2);
    env.set(4101, PidLiveness::Dead);
    assert_in_use(store.acquire_lease(&id, &b).await, &id, "Runtime", 4101);
}

/// Renew: only the full identity renews, and it moves the heartbeat.
pub async fn renew<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    let a = owner("runtime", 4201, 3);
    store.acquire_lease(&id, &a).await.unwrap();
    env.advance(5_000);
    store.renew_lease(&id, &a).await.unwrap();
    assert_eq!(
        store.lease(&id).await.unwrap().unwrap().heartbeat_ms,
        START + 5_000
    );
    // A different generation of the same process is another owner.
    assert_lost(
        store.renew_lease(&id, &owner("runtime", 4201, 4)).await,
        &id,
    );
    assert_lost(
        store.renew_lease(&id, &owner("standalone", 4201, 3)).await,
        &id,
    );
    // No lease at all.
    let free = session(store).await;
    assert_lost(store.renew_lease(&free, &a).await, &free);
}

/// Takeover: only of a stale lease whose pid is proven dead.
pub async fn takeover_after_stale<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    let old = owner("standalone", 4301, 1);
    let new = owner("runtime", 4302, 2);

    // A free Session: takeover acquires.
    let free = session(store).await;
    store.takeover_lease(&free, &new, false).await.unwrap();
    assert_eq!(store.lease(&free).await.unwrap().unwrap().identity, new);

    store.acquire_lease(&id, &old).await.unwrap();
    // Fresh and dead: not yet.
    env.set(4301, PidLiveness::Dead);
    assert_in_use(
        store.takeover_lease(&id, &new, false).await,
        &id,
        "standalone",
        4301,
    );
    // Stale, but alive or unknown: never.
    env.advance(LEASE_STALE_AFTER_MS);
    env.set(4301, PidLiveness::Alive);
    assert_in_use(
        store.takeover_lease(&id, &new, false).await,
        &id,
        "standalone",
        4301,
    );
    env.set(4301, PidLiveness::Unknown);
    assert_in_use(
        store.takeover_lease(&id, &new, false).await,
        &id,
        "standalone",
        4301,
    );
    // Just under the stale age and dead: not yet.
    let edge = session(store).await;
    store.acquire_lease(&edge, &old).await.unwrap();
    env.advance(LEASE_STALE_AFTER_MS - 1);
    env.set(4301, PidLiveness::Dead);
    assert_in_use(
        store.takeover_lease(&edge, &new, false).await,
        &edge,
        "standalone",
        4301,
    );
    // Stale and dead: replaced, with a new heartbeat.
    store.takeover_lease(&id, &new, false).await.unwrap();
    let lease = store.lease(&id).await.unwrap().unwrap();
    assert_eq!(lease.identity, new);
    assert_eq!(lease.heartbeat_ms, env.now_ms());
    // The old owner is fenced out.
    assert_lost(store.renew_lease(&id, &old).await, &id);
    assert_lost(store.release_lease(&id, &old).await, &id);
}

/// A Runtime under the held Runtime lock reclaims another Runtime's lease
/// without the age and pid checks; nothing else does.
pub async fn runtime_reclaim<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let earlier = owner("runtime", 4401, 10);
    let later = owner("runtime", 4402, 11);
    let standalone = owner("standalone", 4403, 12);

    let id = session(store).await;
    store.acquire_lease(&id, &earlier).await.unwrap();
    assert_in_use(
        store.takeover_lease(&id, &later, false).await,
        &id,
        "Runtime",
        4401,
    );
    // Fresh and alive, but the lock proves the earlier Runtime is gone.
    store.takeover_lease(&id, &later, true).await.unwrap();
    assert_eq!(store.lease(&id).await.unwrap().unwrap().identity, later);

    // The lock proves nothing about a standalone owner, in either direction.
    let theirs = session(store).await;
    store.acquire_lease(&theirs, &standalone).await.unwrap();
    assert_in_use(
        store.takeover_lease(&theirs, &later, true).await,
        &theirs,
        "standalone",
        4403,
    );
    let ours = session(store).await;
    store.acquire_lease(&ours, &earlier).await.unwrap();
    assert_in_use(
        store.takeover_lease(&ours, &standalone, true).await,
        &ours,
        "Runtime",
        4401,
    );
}

/// Release: only the full identity, and only once.
pub async fn release<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    let a = owner("runtime", 4501, 1);
    store.acquire_lease(&id, &a).await.unwrap();
    assert_lost(
        store.release_lease(&id, &owner("runtime", 4501, 2)).await,
        &id,
    );
    assert!(store.lease(&id).await.unwrap().is_some());
    store.release_lease(&id, &a).await.unwrap();
    assert_eq!(store.lease(&id).await.unwrap(), None);
    assert_lost(store.release_lease(&id, &a).await, &id);
    // A released Session is free again.
    let b = owner("standalone", 4502, 1);
    store.acquire_lease(&id, &b).await.unwrap();
}

/// Sweep: stale and dead leases go; a Runtime's lease needs the lock proof.
pub async fn sweep<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let database = store
        .database_path()
        .expect("the lease suite needs a store with a database path")
        .to_path_buf();

    let dead = session(store).await;
    let alive = session(store).await;
    let unknown = session(store).await;
    let runtime = session(store).await;
    let fresh = session(store).await;
    store
        .acquire_lease(&dead, &owner("standalone", 4601, 1))
        .await
        .unwrap();
    store
        .acquire_lease(&alive, &owner("standalone", 4602, 1))
        .await
        .unwrap();
    store
        .acquire_lease(&unknown, &owner("standalone", 4603, 1))
        .await
        .unwrap();
    store
        .acquire_lease(&runtime, &owner("runtime", 4604, 1))
        .await
        .unwrap();
    env.set(4601, PidLiveness::Dead);
    env.set(4603, PidLiveness::Unknown);
    env.set(4604, PidLiveness::Dead);
    env.set(4605, PidLiveness::Dead);
    // Nothing is stale yet.
    assert_eq!(store.sweep_stale_leases(Some(&database)).await.unwrap(), 0);

    env.advance(LEASE_STALE_AFTER_MS);
    store
        .acquire_lease(&fresh, &owner("standalone", 4605, 1))
        .await
        .unwrap();
    // Without the proof, the Runtime's lease stays.
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 1);
    assert_eq!(store.lease(&dead).await.unwrap(), None);
    assert!(store.lease(&runtime).await.unwrap().is_some());
    // A lock of another database is no proof.
    let other = database.with_file_name("another-sessions.db");
    assert_eq!(store.sweep_stale_leases(Some(&other)).await.unwrap(), 0);
    // With the proof it goes too. Alive, unknown and fresh leases stay.
    assert_eq!(store.sweep_stale_leases(Some(&database)).await.unwrap(), 1);
    assert_eq!(store.lease(&runtime).await.unwrap(), None);
    for kept in [&alive, &unknown, &fresh] {
        assert!(store.lease(kept).await.unwrap().is_some(), "{kept}");
    }
}

/// Two owners on two Sessions, and the fenced writes of each writer.
pub async fn two_owners<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let a = owner("runtime", 4701, 1);
    let b = owner("standalone", 4702, 1);
    let ours = session(store).await;
    let theirs = session(store).await;
    let free = session(store).await;
    store.acquire_lease(&ours, &a).await.unwrap();
    store.acquire_lease(&theirs, &b).await.unwrap();

    // Neither owner renews or releases the other's lease.
    assert_lost(store.renew_lease(&theirs, &a).await, &theirs);
    assert_lost(store.release_lease(&ours, &b).await, &ours);

    let writer_a = store.for_lease(a.clone());
    let writer_b = store.for_lease(b.clone());
    writer_a.append_entry(&ours, text("a")).await.unwrap();
    writer_b.append_entry(&theirs, text("b")).await.unwrap();
    // A leased writer writes only its own Sessions, and never a free one.
    assert_lost(writer_a.append_entry(&theirs, text("x")).await, &theirs);
    assert_lost(writer_a.append_entry(&free, text("x")).await, &free);
    assert_lost(writer_b.delete(&ours).await, &ours);
    // An unleased writer writes only free Sessions.
    assert_in_use(
        store.append_entry(&ours, text("x")).await,
        &ours,
        "Runtime",
        4701,
    );
    assert_in_use(store.delete(&theirs).await, &theirs, "standalone", 4702);
    store.append_entry(&free, text("free")).await.unwrap();
    // The fenced writes changed nothing.
    let entries = store
        .load(&ours)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);

    // After a takeover the old owner's writer is fenced out.
    env.advance(LEASE_STALE_AFTER_MS);
    env.set(4702, PidLiveness::Dead);
    store.takeover_lease(&theirs, &a, false).await.unwrap();
    assert_lost(writer_b.append_entry(&theirs, text("late")).await, &theirs);
    writer_a
        .append_entry(&theirs, text("now ours"))
        .await
        .unwrap();
}

/// Deleting a Session removes its lease.
pub async fn delete_removes_the_lease<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let a = owner("runtime", 4801, 1);
    let id = session(store).await;
    store.acquire_lease(&id, &a).await.unwrap();
    store.for_lease(a.clone()).delete(&id).await.unwrap();
    assert_eq!(store.lease(&id).await.unwrap(), None);
    assert!(matches!(
        store.acquire_lease(&id, &a).await,
        Err(SessionError::NotFound(_))
    ));
}

/// Control markers: a fenced mutation, the pending marker, acknowledgement.
pub async fn control_markers<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let a = owner("runtime", 4901, 1);
    let id = session(store).await;
    let state = store.control_state(&id).await.unwrap();
    assert_eq!((state.branch_revision, state.pending), (0, None));

    let rename = PendingSessionWrite::SessionInfo {
        name: "renamed".into(),
    };
    // Without the lease the mutation is fenced.
    assert_lost(
        store
            .control_mutation(&id, a.clone(), false, rename.clone(), "{}".into())
            .await,
        &id,
    );
    store.acquire_lease(&id, &a).await.unwrap();
    let marker = r#"{"kind":"rename"}"#.to_owned();
    let state = store
        .control_mutation(&id, a.clone(), false, rename.clone(), marker.clone())
        .await
        .unwrap();
    assert_eq!(
        (state.branch_revision, state.pending.as_deref()),
        (0, Some(marker.as_str()))
    );
    // A second mutation waits for the announcement of the first.
    match store
        .control_mutation(&id, a.clone(), false, rename.clone(), "{}".into())
        .await
    {
        Err(SessionError::Invalid(text)) => {
            assert_eq!(text, "Session has an unannounced mutation; reopen it")
        }
        other => panic!("expected a pending marker, got {other:?}"),
    }
    // Only the announced marker is cleared.
    store.acknowledge_control(&id, "{}").await.unwrap();
    assert!(store.control_state(&id).await.unwrap().pending.is_some());
    store.acknowledge_control(&id, &marker).await.unwrap();
    assert_eq!(store.control_state(&id).await.unwrap().pending, None);

    // A branch move to an unknown entry is refused; a known one counts.
    let unknown = PendingSessionWrite::Leaf {
        target_id: Some("no-such-entry".into()),
    };
    match store
        .control_mutation(&id, a.clone(), true, unknown, "{}".into())
        .await
    {
        Err(SessionError::Invalid(text)) => assert_eq!(text, "unknown branch target"),
        other => panic!("expected an unknown target, got {other:?}"),
    }
    let target = store
        .for_lease(a.clone())
        .append_entry(&id, text("target"))
        .await
        .unwrap();
    let state = store
        .control_mutation(
            &id,
            a.clone(),
            true,
            PendingSessionWrite::Leaf {
                target_id: Some(target.id().to_owned()),
            },
            r#"{"summary":"left behind"}"#.into(),
        )
        .await
        .unwrap();
    assert_eq!(state.branch_revision, 1);
    let entries = store
        .load(&id)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap();
    // The rename, the target, the branch summary of the marker, the move.
    assert_eq!(entries.len(), 4);
    let head = store.load(&id).await.unwrap().read_head().await.unwrap();
    assert_eq!(head, Some(target.id().to_owned()));
}
