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
use loop_agent::harness::session::{SessionFile, TOOL_APPROVAL_LABEL_NAMESPACE};
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
    takeover_stale_boundary(&factory).await;
    sweep_stale_boundary(&factory).await;
    claimant_pid_too_large(&factory).await;
    reclaim_same_pid_new_generation(&factory).await;
    takeover_missing_session(&factory).await;
    destinations(&factory).await;
    delete_removes_control_and_receipt(&factory).await;
    old_writer_fenced_after_takeover(&factory).await;
    check_resume_refuses_stale_lease(&factory).await;
    concurrent_acquires(&factory).await;
    renew_races_takeover(&factory).await;
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

/// Takeover exactly at the stale age and one millisecond below it.
pub async fn takeover_stale_boundary<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let old = owner("standalone", 5001, 1);
    let new = owner("standalone", 5002, 1);
    let id = session(store).await;
    store.acquire_lease(&id, &old).await.unwrap();
    env.set(5001, PidLiveness::Dead);
    env.advance(LEASE_STALE_AFTER_MS - 1);
    assert_in_use(
        store.takeover_lease(&id, &new, false).await,
        &id,
        "standalone",
        5001,
    );
    env.advance(1);
    store.takeover_lease(&id, &new, false).await.unwrap();
    assert_eq!(store.lease(&id).await.unwrap().unwrap().identity, new);
}

/// A sweep keeps a lease one millisecond below the stale age and removes it
/// exactly at the stale age.
pub async fn sweep_stale_boundary<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    store
        .acquire_lease(&id, &owner("standalone", 5101, 1))
        .await
        .unwrap();
    env.set(5101, PidLiveness::Dead);
    env.advance(LEASE_STALE_AFTER_MS - 1);
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 0);
    assert!(store.lease(&id).await.unwrap().is_some());
    env.advance(1);
    assert_eq!(store.sweep_stale_leases(None).await.unwrap(), 1);
    assert_eq!(store.lease(&id).await.unwrap(), None);
}

/// A claimant pid that does not fit pid_t is refused, for every claim kind.
pub async fn claimant_pid_too_large<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    let big = owner("runtime", i32::MAX as u32 + 1, 1);
    for result in [
        store.acquire_lease(&id, &big).await,
        store.takeover_lease(&id, &big, false).await,
        store.takeover_lease(&id, &big, true).await,
    ] {
        match result {
            Err(SessionError::Invalid(text)) => {
                assert_eq!(text, "lease requires an owner and a positive process id")
            }
            other => panic!("expected an invalid claimant, got {other:?}"),
        }
    }
    assert_eq!(store.lease(&id).await.unwrap(), None);
    // i32::MAX itself is accepted.
    store
        .acquire_lease(&id, &owner("runtime", i32::MAX as u32, 1))
        .await
        .unwrap();
}

/// A restarted Runtime with the same pid and a new generation is another
/// owner: it reclaims only under the Runtime lock.
pub async fn reclaim_same_pid_new_generation<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let earlier = owner("runtime", 5201, 1);
    let later = owner("runtime", 5201, 2);
    let id = session(store).await;
    store.acquire_lease(&id, &earlier).await.unwrap();
    assert_in_use(store.acquire_lease(&id, &later).await, &id, "Runtime", 5201);
    // Stale, but the pid is alive (it is ours): no ordinary takeover.
    env.advance(LEASE_STALE_AFTER_MS);
    assert_in_use(
        store.takeover_lease(&id, &later, false).await,
        &id,
        "Runtime",
        5201,
    );
    store.takeover_lease(&id, &later, true).await.unwrap();
    assert_eq!(store.lease(&id).await.unwrap().unwrap().identity, later);
    assert_lost(store.renew_lease(&id, &earlier).await, &id);
}

/// Takeover and reclaim of a Session that does not exist.
pub async fn takeover_missing_session<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let a = owner("runtime", 5301, 1);
    for held in [false, true] {
        match store.takeover_lease("no-such-session", &a, held).await {
            Err(SessionError::NotFound(id)) => assert_eq!(id, "no-such-session"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
    assert_eq!(store.lease("no-such-session").await.unwrap(), None);
}

fn file(entries: Vec<loop_agent::harness::session::SessionTreeEntry>) -> SessionFile {
    SessionFile {
        id: "source".into(),
        name: Some("copy".into()),
        cwd: Some("/work".into()),
        created_at: START,
        entries,
    }
}

async fn source_entries(
    store: &dyn RuntimeSessionStore,
) -> Vec<loop_agent::harness::session::SessionTreeEntry> {
    let id = session(store).await;
    store.append_entry(&id, text("one")).await.unwrap();
    store.append_entry(&id, text("two")).await.unwrap();
    store
        .append_entry(
            &id,
            PendingSessionWrite::Label {
                label: format!("{TOOL_APPROVAL_LABEL_NAMESPACE}bash"),
            },
        )
        .await
        .unwrap();
    store
        .load(&id)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap()
}

/// `create_destination` and `destination_receipt`: create, retry with the
/// same key, a different fingerprint, and rollback.
pub async fn destinations<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let a = owner("runtime", 5401, 1);
    let entries = source_entries(store).await;
    assert_eq!(
        store.destination_receipt("copy:1", "fp").await.unwrap(),
        None
    );

    let mut created = store
        .create_destination(
            file(entries.clone()),
            "/work".into(),
            Some("parent".into()),
            "copy:1".into(),
            "fp".into(),
            "receipt-1".into(),
            a.clone(),
        )
        .await
        .unwrap();
    let id = created.id.clone();
    assert_eq!(created.receipt, "receipt-1");
    // The copy is leased to the creator and carries no approval grant.
    assert_eq!(store.lease(&id).await.unwrap().unwrap().identity, a);
    let reader = store.load(&id).await.unwrap();
    assert_eq!(reader.metadata().name.as_deref(), Some("copy"));
    assert_eq!(
        reader.metadata().parent_session_id.as_deref(),
        Some("parent")
    );
    assert_eq!(reader.read_entries(None).await.unwrap().len(), 2);
    assert_eq!(
        store.destination_receipt("copy:1", "fp").await.unwrap(),
        Some((id.clone(), "receipt-1".into()))
    );
    created.admitted();

    // The same key and fingerprint: the same Session and receipt, nothing new.
    let sessions = store.list(None).await.unwrap().len();
    let mut again = store
        .create_destination(
            file(entries.clone()),
            "/work".into(),
            None,
            "copy:1".into(),
            "fp".into(),
            "receipt-2".into(),
            a.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        (again.id.as_str(), again.receipt.as_str()),
        (id.as_str(), "receipt-1")
    );
    assert_eq!(store.list(None).await.unwrap().len(), sessions);
    // Found by its receipt, so not armed: rollback removes nothing.
    again.rollback().await.unwrap();
    assert!(store.load(&id).await.is_ok());

    // The same key with another fingerprint is refused, by both calls.
    let reused = "request id already used with different parameters";
    match store.destination_receipt("copy:1", "other").await {
        Err(SessionError::Invalid(text)) => assert_eq!(text, reused),
        other => panic!("expected a reused request id, got {other:?}"),
    }
    match store
        .create_destination(
            file(entries.clone()),
            "/work".into(),
            None,
            "copy:1".into(),
            "other".into(),
            "receipt-3".into(),
            a.clone(),
        )
        .await
    {
        Err(SessionError::Invalid(text)) => assert_eq!(text, reused),
        Err(other) => panic!("expected a reused request id, got {other:?}"),
        Ok(_) => panic!("expected a reused request id, got a destination"),
    }

    // Rollback of an armed destination removes its Session, lease and receipt.
    let mut rolled = store
        .create_destination(
            file(entries),
            "/work".into(),
            None,
            "import:2".into(),
            "fp".into(),
            "receipt-4".into(),
            a.clone(),
        )
        .await
        .unwrap();
    let rolled_id = rolled.id.clone();
    rolled.rollback().await.unwrap();
    assert!(store.load(&rolled_id).await.is_err());
    assert_eq!(store.lease(&rolled_id).await.unwrap(), None);
    assert_eq!(
        store.destination_receipt("import:2", "fp").await.unwrap(),
        None
    );
}

/// `delete` removes the Session's control row and destination receipt.
pub async fn delete_removes_control_and_receipt<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let a = owner("runtime", 5501, 1);
    let mut created = store
        .create_destination(
            file(Vec::new()),
            "/work".into(),
            None,
            "import:1".into(),
            "fp".into(),
            "receipt".into(),
            a.clone(),
        )
        .await
        .unwrap();
    created.admitted();
    let id = created.id.clone();
    store
        .control_mutation(
            &id,
            a.clone(),
            false,
            PendingSessionWrite::SessionInfo {
                name: "named".into(),
            },
            "{}".into(),
        )
        .await
        .unwrap();
    assert!(store.control_state(&id).await.unwrap().pending.is_some());
    store.for_lease(a.clone()).delete(&id).await.unwrap();
    let state = store.control_state(&id).await.unwrap();
    assert_eq!((state.branch_revision, state.pending), (0, None));
    assert_eq!(
        store.destination_receipt("import:1", "fp").await.unwrap(),
        None
    );
    assert_eq!(store.lease(&id).await.unwrap(), None);
}

/// After a takeover the old owner's `delete` and `control_mutation` are
/// fenced too, not only its appends.
pub async fn old_writer_fenced_after_takeover<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let old = owner("standalone", 5601, 1);
    let new = owner("runtime", 5602, 1);
    let id = session(store).await;
    store.acquire_lease(&id, &old).await.unwrap();
    let old_writer = store.for_lease(old.clone());
    env.advance(LEASE_STALE_AFTER_MS);
    env.set(5601, PidLiveness::Dead);
    store.takeover_lease(&id, &new, false).await.unwrap();
    assert_lost(old_writer.append_entry(&id, text("late")).await, &id);
    assert_lost(old_writer.delete(&id).await, &id);
    assert_lost(
        store
            .control_mutation(
                &id,
                old.clone(),
                false,
                PendingSessionWrite::SessionInfo {
                    name: "late".into(),
                },
                "{}".into(),
            )
            .await,
        &id,
    );
    // Nothing of the old owner landed.
    assert!(store
        .load(&id)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(store.control_state(&id).await.unwrap().pending, None);
}

/// `check_resume` refuses every lease, also a stale one whose pid is dead:
/// a standalone resume must go through `takeover_lease`.
pub async fn check_resume_refuses_stale_lease<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    let store = fixture.store.as_ref();
    let id = session(store).await;
    store
        .acquire_lease(&id, &owner("standalone", 5701, 1))
        .await
        .unwrap();
    env.advance(LEASE_STALE_AFTER_MS * 10);
    env.set(5701, PidLiveness::Dead);
    assert_in_use(store.check_resume(&id).await, &id, "standalone", 5701);
}

/// Two owners acquire one free Session at once: exactly one wins.
pub async fn concurrent_acquires<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    for round in 0..8 {
        let a = owner("runtime", 5801, round);
        let b = owner("standalone", 5802, round);
        let id = session(fixture.store.as_ref()).await;
        let (sa, sb) = (Arc::clone(&fixture.store), Arc::clone(&fixture.store));
        let (ia, ib, ida, idb) = (a.clone(), b.clone(), id.clone(), id.clone());
        let (ra, rb) = tokio::join!(
            tokio::spawn(async move { sa.acquire_lease(&ida, &ia).await }),
            tokio::spawn(async move { sb.acquire_lease(&idb, &ib).await }),
        );
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        let holder = fixture.store.lease(&id).await.unwrap().unwrap().identity;
        match (&ra, &rb) {
            (Ok(()), Err(SessionError::InUse(_))) => assert_eq!(holder, a),
            (Err(SessionError::InUse(_)), Ok(())) => assert_eq!(holder, b),
            other => panic!("round {round}: expected exactly one winner, got {other:?}"),
        }
    }
}

/// A renewal races a takeover of a stale lease whose pid is dead. Either the
/// renewal wins (the lease is fresh again and the takeover is refused) or
/// the takeover wins (the renewal finds its lease lost). Nothing else.
pub async fn renew_races_takeover<F, Fut>(factory: &F)
where
    F: Fn(Arc<ScriptedEnvironment>) -> Fut,
    Fut: Future<Output = Fixture<dyn RuntimeSessionStore>>,
{
    let env = ScriptedEnvironment::new(START);
    let fixture = factory(Arc::clone(&env)).await;
    for round in 0..8 {
        let old = owner("standalone", 5901, round);
        let new = owner("runtime", 5902, round);
        let id = session(fixture.store.as_ref()).await;
        fixture.store.acquire_lease(&id, &old).await.unwrap();
        env.set(5901, PidLiveness::Dead);
        env.advance(LEASE_STALE_AFTER_MS);
        let (sa, sb) = (Arc::clone(&fixture.store), Arc::clone(&fixture.store));
        let (io, inew, ida, idb) = (old.clone(), new.clone(), id.clone(), id.clone());
        let (renewed, taken) = tokio::join!(
            tokio::spawn(async move { sa.renew_lease(&ida, &io).await }),
            tokio::spawn(async move { sb.takeover_lease(&idb, &inew, false).await }),
        );
        let (renewed, taken) = (renewed.unwrap(), taken.unwrap());
        let holder = fixture.store.lease(&id).await.unwrap().unwrap();
        match (&renewed, &taken) {
            (Ok(()), Err(SessionError::InUse(_))) => {
                assert_eq!(holder.identity, old);
                assert_eq!(holder.heartbeat_ms, env.now_ms());
            }
            (Err(SessionError::Invalid(text)), Ok(())) => {
                assert_eq!(text, &lost(&id));
                assert_eq!(holder.identity, new);
            }
            other => panic!("round {round}: inconsistent race outcome {other:?}"),
        }
    }
}
