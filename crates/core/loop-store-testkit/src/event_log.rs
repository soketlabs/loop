//! Event Log conformance suite.
//!
//! Each suite function takes a factory that returns a fresh, empty log in a
//! [`Fixture`]. [`run_all`] runs every check, each on its own fresh log.
//! The `*_on` functions run one check on a log you already hold; use them
//! when a test needs the log for more than one step (for example a reopen).
//!
//! The checks use these `loop_store` items: `Event`, `EventKind`,
//! `EventLog`, `MAX_BATCH_EVENTS` and `WRITER_QUEUE_CAPACITY`, and from step
//! B2 the paged reads (`Cursor`, `Page`, `PageLimit`) and the Session
//! summary (`SessionSummary`, `summarize`). A store that implements a
//! different queue capacity must still pass: the capacity is only a
//! constant of the contract. `loop_runtime::log` re-exports the same items.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_protocol::{
    ConfigChanged, ContentBlock, Origin, Role, SessionKey, SessionUpdate, StopReason, TurnId,
    UpdateEnvelope,
};
use loop_store::{
    summarize, Caller, Cursor, Event, EventKind, EventLog, Identity, LoggedEvent, Page, PageLimit,
    Principal, PrincipalKind, Scope, SystemScope, Tenant, Via, MAX_BATCH_EVENTS,
    SESSION_CREATED_KEY, WRITER_QUEUE_CAPACITY,
};
use serde_json::json;

use crate::Fixture;

/// Wrap a concrete log in a [`Fixture`] of the trait object a factory returns.
pub fn fixture(log: impl EventLog + 'static) -> Fixture<dyn EventLog> {
    let log: Arc<dyn EventLog> = Arc::new(log);
    Fixture::new(log)
}

/// The `local` tenant.
pub fn local() -> Tenant {
    Tenant::local()
}

/// A tenant from outside the host.
pub fn acme() -> Tenant {
    Tenant::new("acme").expect("valid tenant")
}

/// A system scope for the checks. Only tests and the Runtime's startup
/// code make one.
pub fn system() -> &'static SystemScope {
    static SCOPE: std::sync::OnceLock<SystemScope> = std::sync::OnceLock::new();
    SCOPE.get_or_init(SystemScope::for_runtime_startup)
}

/// Every tenant's rows.
pub fn all() -> Scope<'static> {
    Scope::System(system())
}

/// The host's user (`uid:501`) acting in the `local` tenant, the identity
/// of most records of the checks.
pub fn host() -> Identity {
    Identity::acting(&Caller::host(Principal::local_uid(501)), local())
}

/// An Edge Gateway user of tenant `acme`, acting in `tenant`.
pub fn edge_user(tenant: Tenant) -> Identity {
    let caller = Caller::asserted(
        Principal::new(PrincipalKind::Person, "https://idp.example", "u-7").expect("principal"),
        acme(),
    )
    .expect("caller");
    Identity::acting(&caller, tenant)
}

/// A `TurnStarted` Update for `session`, generation 1, turn `turn-1`.
pub fn update(session: &str) -> Event {
    Event::update(
        1,
        100,
        UpdateEnvelope::live(
            "loop".into(),
            0,
            SessionKey::from(session),
            Some("turn-1".into()),
            900,
            SessionUpdate::TurnStarted {
                turn_id: "turn-1".into(),
            },
        ),
        host(),
    )
}

/// Run every check below. The factory is called once per check.
pub async fn run_all<F, Fut>(factory: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    round_trip(&factory).await;
    concurrent_producers(&factory).await;
    cursors(&factory).await;
    generations(&factory).await;
    atomic_validation(&factory).await;
    invalid_unknown_is_atomic(&factory).await;
    runtime_records_have_no_session_cursor(&factory).await;
    subagent_sessions(&factory).await;
    prompts_sent(&factory).await;
    paged_reads(&factory).await;
    paged_read_under_appends(&factory).await;
    read_your_writes(&factory).await;
    long_read_does_not_delay_appends(&factory).await;
    summary_matches_fold(&factory).await;
    recovery_pages(&factory).await;
    identity_round_trip(&factory).await;
    tenant_isolation(&factory).await;
    owner_of_record(&factory).await;
}

/// Every kind, Unknown kinds included, keeps the identity it was appended
/// with: tenant, principal (also of an unknown kind), `via` and `actor`
/// (step B3).
pub async fn identity_round_trip<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    identity_round_trip_on(fixture.store.as_ref()).await;
}

/// A tenant's reads never return another tenant's rows, on every read
/// method; the same Session key in two tenants is two Sessions with their
/// own `session_seq`; a system scope reads all tenants (step B3).
pub async fn tenant_isolation<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    tenant_isolation_on(fixture.store.as_ref()).await;
}

/// The owner of record is the identity of the creating `session_opened`,
/// set once (step B3).
pub async fn owner_of_record<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    owner_of_record_on(fixture.store.as_ref()).await;
}

/// Every event kind survives a batch with cursors assigned in order.
pub async fn round_trip<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    round_trip_on(fixture.store.as_ref()).await;
}

/// 128 producers append at once; cursors stay contiguous and in order.
pub async fn concurrent_producers<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    concurrent_producers_on(Arc::clone(&fixture.store)).await;
}

/// Read cursors for sessions and the Runtime log.
pub async fn cursors<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    cursors_on(fixture.store.as_ref()).await;
}

/// Generation allocation and clean-stop records.
pub async fn generations<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    generations_on(fixture.store.as_ref()).await;
}

/// Invalid batches are rejected whole; the largest valid batch is accepted.
pub async fn atomic_validation<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    atomic_validation_on(fixture.store.as_ref()).await;
}

/// A batch with an invalid `Unknown` event commits nothing.
pub async fn invalid_unknown_is_atomic<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    invalid_unknown_is_atomic_on(fixture.store.as_ref()).await;
}

/// Runtime records carry no Session and consume no Session cursor, and a
/// lone append flushes without waiting for another producer.
pub async fn runtime_records_have_no_session_cursor<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    runtime_records_have_no_session_cursor_on(fixture.store.as_ref()).await;
}

/// `subagent_sessions` finds Sessions whose opening recorded a parent.
pub async fn subagent_sessions<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    subagent_sessions_on(fixture.store.as_ref()).await;
}

/// `prompts_sent` counts prompts per Session.
pub async fn prompts_sent<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    prompts_sent_on(fixture.store.as_ref()).await;
}

/// Paged Session reads: page boundaries, size limits, empty pages, cursors
/// past the end, and `through_seq`.
pub async fn paged_reads<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    paged_reads_on(fixture.store.as_ref()).await;
}

/// A paged read stays contiguous and stable while appends continue (C6).
pub async fn paged_read_under_appends<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    paged_read_under_appends_on(Arc::clone(&fixture.store)).await;
}

/// Every read that starts after an append returned sees that append (C8).
pub async fn read_your_writes<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    read_your_writes_on(Arc::clone(&fixture.store)).await;
}

/// A multi-MiB replay that runs again and again does not delay appends (C7).
pub async fn long_read_does_not_delay_appends<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    long_read_does_not_delay_appends_on(Arc::clone(&fixture.store)).await;
}

/// The store's Session summaries equal a fold over the log.
pub async fn summary_matches_fold<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    summary_matches_fold_on(fixture.store.as_ref()).await;
}

/// Recovery pages concatenate to `recovery_records`.
pub async fn recovery_pages<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn EventLog>>,
{
    let fixture = factory().await;
    recovery_pages_on(fixture.store.as_ref()).await;
}

pub async fn round_trip_on(log: &dyn EventLog) {
    let kinds = vec![
        EventKind::PromptSent(json!({"prompt":"hello"})),
        EventKind::Cancelled(json!({"reason":"user"})),
        EventKind::EscalationRaised(json!({"id":"e1"})),
        EventKind::EscalationAnswered(json!({"id":"e1","answer":"allow"})),
        EventKind::EscalationWithdrawn(json!({"id":"e1","reason":"cancelled"})),
        EventKind::SessionOpened(json!({"cwd":"/work","parent":null})),
        EventKind::SessionClosed(json!({"reason":"done"})),
        EventKind::RuntimeStarted(json!({"pid":42})),
        EventKind::RuntimeStopped(json!({"reason":"shutdown"})),
        EventKind::Unknown(json!({"kind":"future_kind","payload":{"x":7}})),
    ];
    let mut events = vec![update("s1")];
    for kind in kinds {
        events.push(Event {
            generation: 1,
            session_key: if matches!(
                kind,
                EventKind::RuntimeStarted(_) | EventKind::RuntimeStopped(_)
            ) {
                None
            } else {
                Some("s1".into())
            },
            turn: None,
            ts_ms: 101,
            kind,
            identity: host(),
        });
    }
    let committed = log.append_batch(events.clone()).await.unwrap();
    assert_eq!(committed.len(), 11);
    assert_eq!(log.read_runtime_after(all(), 0).await.unwrap(), committed);
    for (index, (record, input)) in committed.iter().zip(events).enumerate() {
        assert_eq!(record.global_seq, index as u64 + 1);
        assert_eq!(record.session_seq, if index == 0 { Some(1) } else { None });
        if let EventKind::Update(envelope) = &record.event.kind {
            assert_eq!(envelope.seq, 1);
            assert_eq!(envelope.generation, 1);
        } else {
            assert_eq!(record.event, input);
        }
    }
}

pub async fn concurrent_producers_on(log: Arc<dyn EventLog>) {
    let barrier = Arc::new(tokio::sync::Barrier::new(129));
    let mut tasks = Vec::new();
    for _ in 0..128 {
        let log = log.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let records = log
                .append_batch(vec![update("s1"), update("s2"), update("s1")])
                .await
                .unwrap();
            assert_eq!(records.len(), 3);
            assert_eq!(records[1].global_seq, records[0].global_seq + 1);
            assert_eq!(records[2].global_seq, records[0].global_seq + 2);
            assert_eq!(
                records[2].session_seq,
                records[0].session_seq.map(|s| s + 1)
            );
        }));
    }
    barrier.wait().await;
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(log.latest_seq(&local(), &"s1".into()).await.unwrap(), 256);
    assert_eq!(log.latest_seq(&local(), &"s2".into()).await.unwrap(), 128);
    let all = log.read_runtime_after(all(), 0).await.unwrap();
    assert_eq!(all.len(), 384);
    for (i, record) in all.iter().enumerate() {
        assert_eq!(record.global_seq, i as u64 + 1);
    }
}

pub async fn cursors_on(log: &dyn EventLog) {
    assert_eq!(
        log.latest_seq(&local(), &"missing".into()).await.unwrap(),
        0
    );
    assert!(log
        .read_after(&local(), &"missing".into(), 0)
        .await
        .unwrap()
        .is_empty());
    let mut audit = update("s1");
    audit.kind = EventKind::PromptSent(json!({"prompt":"go"}));
    let mut runtime = audit.clone();
    runtime.session_key = None;
    runtime.turn = None;
    runtime.kind = EventKind::RuntimeStarted(json!({}));
    log.append_batch(vec![
        runtime,
        update("s1"),
        audit,
        update("s2"),
        update("s1"),
    ])
    .await
    .unwrap();
    let session = log.read_after(&local(), &"s1".into(), 1).await.unwrap();
    assert_eq!(session.len(), 1);
    assert_eq!(
        (session[0].global_seq, session[0].session_seq),
        (5, Some(2))
    );
    assert_eq!(log.latest_seq(&local(), &"s1".into()).await.unwrap(), 2);
    assert_eq!(log.latest_seq(&local(), &"s2".into()).await.unwrap(), 1);
    assert_eq!(
        log.read_runtime_after(all(), 3)
            .await
            .unwrap()
            .iter()
            .map(|e| e.global_seq)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert!(log
        .read_after(&local(), &"s1".into(), 2)
        .await
        .unwrap()
        .is_empty());
    assert!(log
        .read_after(&local(), &"s1".into(), u64::MAX)
        .await
        .unwrap()
        .is_empty());
    assert!(log
        .read_runtime_after(all(), u64::MAX)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        log.read_runtime_after(all(), 0).await.unwrap()[0]
            .event
            .session_key,
        None
    );
}

pub async fn generations_on(log: &dyn EventLog) {
    assert!(log.finish_generation(0, 20).await.is_err());
    assert_eq!(
        log.allocate_generation("runtime", 42, 100).await.unwrap(),
        1
    );
    log.append_batch(vec![update("s1")]).await.unwrap();
    log.finish_generation(1, 110).await.unwrap();
    assert_eq!(
        log.allocate_generation("runtime", 43, 120).await.unwrap(),
        2
    );
    let mut event = update("s1");
    event.generation = 2;
    let records = log.append_batch(vec![event]).await.unwrap();
    assert_eq!(records[0].session_seq, Some(2));
    if let EventKind::Update(envelope) = &records[0].event.kind {
        assert_eq!(envelope.generation, 2);
    } else {
        panic!("expected Update");
    }
    assert!(log.finish_generation(9, 130).await.is_err());
}

pub async fn atomic_validation_on(log: &dyn EventLog) {
    assert_eq!(MAX_BATCH_EVENTS, 256);
    assert_eq!(WRITER_QUEUE_CAPACITY, 64);
    assert!(log.append_batch(Vec::new()).await.unwrap().is_empty());
    let mut invalid = update("s1");
    invalid.session_key = None;
    assert!(log.append_batch(vec![update("s1"), invalid]).await.is_err());
    let mut replay = update("s1");
    if let EventKind::Update(envelope) = &mut replay.kind {
        envelope.origin = Origin::Replay;
    }
    assert!(log.append_batch(vec![replay]).await.is_err());
    assert!(log
        .append_batch(vec![update("s1"); MAX_BATCH_EVENTS + 1])
        .await
        .is_err());
    assert!(log.read_runtime_after(all(), 0).await.unwrap().is_empty());
    assert_eq!(
        log.append_batch(vec![update("s1"); MAX_BATCH_EVENTS])
            .await
            .unwrap()
            .len(),
        256
    );
    assert_eq!(log.latest_seq(&local(), &"s1".into()).await.unwrap(), 256);
}

pub async fn invalid_unknown_is_atomic_on(log: &dyn EventLog) {
    let mut bad = update("s1");
    bad.kind = EventKind::Unknown(json!({"payload":7}));
    assert!(log.append_batch(vec![update("s1"), bad]).await.is_err());
    assert!(log.read_runtime_after(all(), 0).await.unwrap().is_empty());
}

pub async fn runtime_records_have_no_session_cursor_on(log: &dyn EventLog) {
    let mut event = update("s1");
    event.kind = EventKind::RuntimeStarted(json!({}));
    assert!(log.append_batch(vec![event.clone()]).await.is_err());
    event.session_key = None;
    event.turn = None;
    let result = tokio::time::timeout(Duration::from_secs(2), log.append_batch(vec![event]))
        .await
        .expect("a lone append must flush without waiting for another producer")
        .unwrap();
    assert_eq!(result[0].session_seq, None);
    assert_eq!(result[0].event.session_key, None);
}

pub async fn subagent_sessions_on(log: &dyn EventLog) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let opened = |key: &str, parent: serde_json::Value| Event {
            generation: 1,
            session_key: Some(key.into()),
            turn: None,
            ts_ms: 1,
            kind: EventKind::SessionOpened(json!({"cwd": "/w", "parent": parent, "depth": 1})),
            identity: host(),
        };
        log.append_batch(vec![
            opened("root", json!(null)),
            opened("kid", json!("root")),
            update("kid"),
            opened("kid", json!("root")),
            opened("other", json!(null)),
        ])
        .await
        .unwrap();
        let keys = log.subagent_sessions(all()).await.unwrap();
        assert_eq!(keys, HashSet::from([(local(), SessionKey::from("kid"))]));
    })
    .await
    .unwrap();
}

pub async fn prompts_sent_on(log: &dyn EventLog) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let prompt = |key: &str| Event {
            generation: 1,
            session_key: Some(key.into()),
            turn: None,
            ts_ms: 1,
            kind: EventKind::PromptSent(json!({"content": []})),
            identity: host(),
        };
        log.append_batch(vec![prompt("a"), update("a"), prompt("a"), prompt("b")])
            .await
            .unwrap();
        assert_eq!(log.prompts_sent(&local(), &"a".into()).await.unwrap(), 2);
        assert_eq!(log.prompts_sent(&local(), &"b".into()).await.unwrap(), 1);
        assert_eq!(log.prompts_sent(&local(), &"c".into()).await.unwrap(), 0);
    })
    .await
    .unwrap();
}

// ------------------------------------------------------------------ step B2

/// An Update of `session` in `turn`, at `ts_ms`.
pub fn update_with(session: &str, turn: &str, ts_ms: i64, update: SessionUpdate) -> Event {
    Event::update(
        1,
        ts_ms,
        UpdateEnvelope::live(
            "loop".into(),
            0,
            SessionKey::from(session),
            Some(turn.into()),
            0,
            update,
        ),
        host(),
    )
}

/// A text chunk Update of `session` with `bytes` bytes of text.
pub fn chunk(session: &str, bytes: usize) -> Event {
    update_with(
        session,
        "turn-1",
        100,
        SessionUpdate::MessageChunk {
            message_id: "msg-1".into(),
            role: Role::Agent,
            content: ContentBlock::Text {
                text: "x".repeat(bytes),
            },
        },
    )
}

fn record(session: Option<&str>, turn: Option<&str>, ts_ms: i64, kind: EventKind) -> Event {
    Event {
        generation: 1,
        session_key: session.map(SessionKey::from),
        turn: turn.map(TurnId::from),
        ts_ms,
        kind,
        identity: host(),
    }
}

fn config(revision: u64) -> SessionUpdate {
    SessionUpdate::ConfigChanged(ConfigChanged {
        revision,
        ..ConfigChanged::default()
    })
}

/// A mixed workload in append batches. It covers each rule of the Session
/// summary: the last revision wins (not the largest), activity is the
/// newest timestamp of any record, turns of non-Update records count, the
/// first parent stays, and Runtime records have no Session.
pub fn workload() -> Vec<Vec<Event>> {
    let opened = |session: &str, parent: serde_json::Value, ts| {
        record(
            Some(session),
            None,
            ts,
            EventKind::SessionOpened(json!({"cwd": "/w", "parent": parent, "depth": 1})),
        )
    };
    let created = |session: &str, ts| {
        record(
            Some(session),
            None,
            ts,
            EventKind::SessionOpened(
                json!({"cwd": "/w", "parent": null, "depth": 0, SESSION_CREATED_KEY: true}),
            ),
        )
    };
    let prompt = |session: &str, turn: Option<&str>, ts| {
        record(
            Some(session),
            turn,
            ts,
            EventKind::PromptSent(json!({"content": [] })),
        )
    };
    let started = |session: &str, turn: &str, ts| {
        update_with(
            session,
            turn,
            ts,
            SessionUpdate::TurnStarted {
                turn_id: turn.into(),
            },
        )
    };
    let ended = |session: &str, turn: &str, ts| {
        update_with(
            session,
            turn,
            ts,
            SessionUpdate::TurnEnded {
                turn_id: turn.into(),
                stop: StopReason::EndTurn,
                error: None,
            },
        )
    };
    vec![
        vec![
            record(None, None, 10, EventKind::RuntimeStarted(json!({"pid": 1}))),
            created("a", 10),
            opened("kid", json!("a"), 11),
            prompt("a", None, 12),
            started("a", "turn-1", 13),
            update_with("a", "turn-1", 14, config(5)),
            update_with("a", "turn-1", 15, config(3)),
            Event {
                ts_ms: 18,
                ..chunk("a", 40)
            },
            prompt("b", Some("turn-7"), 16),
            record(
                Some("a"),
                Some("turn-1"),
                17,
                EventKind::Unknown(json!({"kind": "future_kind", "payload": {"x": 1}})),
            ),
        ],
        vec![
            ended("a", "turn-1", 30),
            update_with(
                "a",
                "turn-2",
                25,
                SessionUpdate::BranchReset {
                    revision: 9,
                    branch_revision: 1,
                    view_id: "v".into(),
                    reason: "navigate".into(),
                },
            ),
            opened("kid", json!(null), 31),
            opened("other", json!("b"), 32),
            started("b", "turn-2", 20),
            prompt("a", Some("turn-2"), 33),
            prompt("kid", None, 34),
            started("kid", "turn-1", 35),
            record(
                Some("a"),
                None,
                28,
                EventKind::EscalationRaised(json!({"id": "e1"})),
            ),
        ],
        vec![
            update_with("a", "turn-2", 40, config(4)),
            ended("b", "turn-2", 41),
            record(None, None, 42, EventKind::RuntimeStopped(json!({}))),
        ],
    ]
}

/// All pages of one Session's read, with their `next` values.
pub async fn all_pages(
    log: &dyn EventLog,
    session: &SessionKey,
    after: u64,
    through: Option<u64>,
    limit: PageLimit,
) -> Vec<Page<LoggedEvent>> {
    let mut pages = Vec::new();
    let mut after = after;
    loop {
        let page = log
            .read_session_page(&local(), session, after, through, limit)
            .await
            .unwrap();
        let next = page.next;
        match (next, page.items.last()) {
            (Some(cursor), Some(last)) => {
                assert_eq!(
                    cursor,
                    Cursor(last.session_seq.unwrap()),
                    "next is the last seq"
                )
            }
            (Some(_), None) => panic!("an empty page has no next"),
            _ => {}
        }
        pages.push(page);
        match next {
            Some(Cursor(seq)) => after = seq,
            None => return pages,
        }
    }
}

fn seqs(records: &[LoggedEvent]) -> Vec<u64> {
    records.iter().map(|r| r.session_seq.unwrap()).collect()
}

pub async fn paged_reads_on(log: &dyn EventLog) {
    let session = SessionKey::from("p");
    // Ten Updates of growing size, with other records between them.
    for i in 1..=10usize {
        log.append_batch(vec![
            chunk("p", 100 * i),
            update("other"),
            record(Some("p"), None, 1, EventKind::PromptSent(json!({}))),
        ])
        .await
        .unwrap();
    }
    let all = log.read_after(&local(), &session, 0).await.unwrap();
    assert_eq!(seqs(&all), (1..=10).collect::<Vec<_>>());

    // Event limit: 3 + 3 + 3 + 1. `next` is the last seq of a full page.
    let pages = all_pages(log, &session, 0, None, PageLimit::events(3)).await;
    let sizes: Vec<_> = pages.iter().map(|p| p.items.len()).collect();
    assert_eq!(sizes, vec![3, 3, 3, 1]);
    assert_eq!(
        pages.iter().map(|p| p.next).collect::<Vec<_>>(),
        vec![Some(Cursor(3)), Some(Cursor(6)), Some(Cursor(9)), None]
    );
    let joined: Vec<_> = pages.into_iter().flat_map(|p| p.items).collect();
    assert_eq!(joined, all, "pages join to the unpaged read");

    // A page that ends exactly at the last record has no `next`.
    let pages = all_pages(log, &session, 0, None, PageLimit::events(5)).await;
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[1].next, None);

    // `through_seq` bounds the read.
    let page = log
        .read_session_page(&local(), &session, 2, Some(5), PageLimit::events(10))
        .await
        .unwrap();
    assert_eq!(seqs(&page.items), vec![3, 4, 5]);
    assert_eq!(page.next, None);
    let page = log
        .read_session_page(&local(), &session, 2, Some(5), PageLimit::events(2))
        .await
        .unwrap();
    assert_eq!(seqs(&page.items), vec![3, 4]);
    assert_eq!(page.next, Some(Cursor(4)));

    // Byte limit: a page always holds one record, however large.
    let tiny = PageLimit {
        events: 100,
        bytes: 1,
    };
    let pages = all_pages(log, &session, 0, None, tiny).await;
    assert_eq!(pages.len(), 10);
    assert!(pages.iter().all(|p| p.items.len() == 1));
    // A limit between record sizes cuts by bytes before the event limit.
    let mid = PageLimit {
        events: 100,
        bytes: 2_500,
    };
    let pages = all_pages(log, &session, 0, None, mid).await;
    assert!(pages.len() > 1 && pages.len() < 10, "{} pages", pages.len());
    let joined: Vec<_> = pages.into_iter().flat_map(|p| p.items).collect();
    assert_eq!(joined, all);

    // Empty pages.
    for (after, through) in [
        (10, None),
        (u64::MAX, None),
        (5, Some(5)),
        (7, Some(3)),
        (0, Some(0)),
    ] {
        let page = log
            .read_session_page(&local(), &session, after, through, PageLimit::events(4))
            .await
            .unwrap();
        assert_eq!(
            page,
            Page::last(Vec::new()),
            "after {after} through {through:?}"
        );
    }
    let missing = log
        .read_session_page(&local(), &"missing".into(), 0, None, PageLimit::DEFAULT)
        .await
        .unwrap();
    assert_eq!(missing, Page::last(Vec::new()));
    // A limit of 0 records is read as 1.
    let page = log
        .read_session_page(&local(), &session, 0, None, PageLimit::events(0))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

pub async fn paged_read_under_appends_on(log: Arc<dyn EventLog>) {
    tokio::time::timeout(Duration::from_secs(60), async {
        let session = SessionKey::from("s");
        for _ in 0..5 {
            log.append_batch(vec![update("s"); 10]).await.unwrap();
        }
        let pinned = log.latest_seq(&local(), &session).await.unwrap();
        assert_eq!(pinned, 50);
        let before = log.read_after(&local(), &session, 0).await.unwrap();
        let writer = {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                for i in 0..100usize {
                    log.append_batch(vec![update("s"); 1 + i % 4])
                        .await
                        .unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        let mut pinned_pages = Vec::new();
        let mut open_pages = Vec::new();
        let mut rounds = 0;
        while !writer.is_finished() || rounds < 2 {
            rounds += 1;
            pinned_pages = all_pages(
                log.as_ref(),
                &session,
                0,
                Some(pinned),
                PageLimit::events(7),
            )
            .await;
            open_pages = all_pages(log.as_ref(), &session, 0, None, PageLimit::events(7)).await;
            tokio::task::yield_now().await;
        }
        writer.await.unwrap();
        // With a fixed `through_seq`, the pages equal the unpaged read.
        let pinned_read: Vec<_> = pinned_pages.into_iter().flat_map(|p| p.items).collect();
        assert_eq!(pinned_read, before);
        // Without one, the pages are a contiguous prefix of the final log.
        let open_read: Vec<_> = open_pages.into_iter().flat_map(|p| p.items).collect();
        let final_read = log.read_after(&local(), &session, 0).await.unwrap();
        assert!(open_read.len() >= 50);
        assert_eq!(
            seqs(&open_read),
            (1..=open_read.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(open_read[..], final_read[..open_read.len()]);
    })
    .await
    .unwrap();
}

pub async fn read_your_writes_on(log: Arc<dyn EventLog>) {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut tasks = Vec::new();
        for task in 0..32 {
            let log = Arc::clone(&log);
            tasks.push(tokio::spawn(async move {
                let key = SessionKey::from(format!("ryw-{task}"));
                for i in 1..=20u64 {
                    let records = log
                        .append_batch(vec![
                            record(
                                Some(key.as_str()),
                                None,
                                i as i64,
                                EventKind::PromptSent(json!({})),
                            ),
                            update(key.as_str()),
                        ])
                        .await
                        .unwrap();
                    let seq = records[1].session_seq.unwrap();
                    assert_eq!(seq, i);
                    assert_eq!(log.latest_seq(&local(), &key).await.unwrap(), seq);
                    let page = log
                        .read_session_page(&local(), &key, seq - 1, None, PageLimit::DEFAULT)
                        .await
                        .unwrap();
                    assert_eq!(page.items, records[1..].to_vec());
                    let tail = log.read_after(&local(), &key, seq - 1).await.unwrap();
                    assert_eq!(tail, records[1..].to_vec());
                    let summary = log.session_summary(&local(), &key).await.unwrap().unwrap();
                    assert_eq!((summary.last_seq, summary.prompts_sent), (seq, i));
                    assert_eq!(log.prompts_sent(&local(), &key).await.unwrap(), i);
                    let global = log
                        .read_runtime_after(all(), records[0].global_seq - 1)
                        .await
                        .unwrap();
                    assert!(global.starts_with(&records));
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    })
    .await
    .unwrap();
}

/// How many times slower than idle an append may be while replays run.
pub const APPEND_LATENCY_FACTOR: u32 = 10;
/// The bound never goes below this. The ADR asks for 50 ms (C7). Before the
/// reader/writer split, appends here waited about 500 ms at the median.
pub const APPEND_LATENCY_FLOOR: Duration = Duration::from_millis(50);
/// Appends measured in each phase (idle, then during replays).
const LATENCY_SAMPLES: usize = 300;
/// A phase stops after this time even with fewer samples, so a store that
/// fails does not run for minutes.
const LATENCY_PHASE: Duration = Duration::from_secs(20);

/// Sorted latencies of single appends to another Session, one every 1 ms.
async fn append_latencies(log: &dyn EventLog) -> Vec<Duration> {
    let mut latencies = Vec::with_capacity(LATENCY_SAMPLES);
    let phase = Instant::now();
    while latencies.len() < LATENCY_SAMPLES && phase.elapsed() < LATENCY_PHASE {
        let start = Instant::now();
        log.append_batch(vec![update("small")]).await.unwrap();
        latencies.push(start.elapsed());
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    latencies.sort();
    latencies
}

/// The value at quantile `q` of sorted samples.
fn quantile(sorted: &[Duration], q: f64) -> Duration {
    let index = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[index]
}

/// The rule: while replays run, the median and the p95 append latency each
/// stay under `max(APPEND_LATENCY_FACTOR x the same statistic when idle,
/// APPEND_LATENCY_FLOOR)`. Both phases use the same store in the same test,
/// so a slow machine raises both. The median and the p95 ignore a few
/// scheduler pauses; a store that serves reads on its writer fails both.
pub async fn long_read_does_not_delay_appends_on(log: Arc<dyn EventLog>) {
    tokio::time::timeout(Duration::from_secs(120), async {
        let big = SessionKey::from("big");
        // 6 MiB of history in one Session.
        let events = 6 * 1024;
        for _ in 0..events / MAX_BATCH_EVENTS {
            log.append_batch(vec![chunk("big", 1024); MAX_BATCH_EVENTS])
                .await
                .unwrap();
        }
        let start = Instant::now();
        assert_eq!(log.read_after(&local(), &big, 0).await.unwrap().len(), events);
        let replay = start.elapsed();
        let idle = append_latencies(log.as_ref()).await;
        let stop = Arc::new(AtomicBool::new(false));
        let mut readers = Vec::new();
        for _ in 0..2 {
            let log = Arc::clone(&log);
            let stop = Arc::clone(&stop);
            let big = big.clone();
            readers.push(tokio::spawn(async move {
                let mut replays = 0u32;
                while !stop.load(Ordering::Acquire) {
                    assert!(log.read_after(&local(), &big, 0).await.unwrap().len() >= events);
                    replays += 1;
                }
                replays
            }));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let busy = append_latencies(log.as_ref()).await;
        stop.store(true, Ordering::Release);
        let mut replays = 0;
        for reader in readers {
            replays += reader.await.unwrap();
        }
        assert!(replays > 0, "the replays ran during the appends");
        let report = format!(
            "idle p50 {:?} p95 {:?}; during {replays} replays p50 {:?} p95 {:?} ({} samples); one replay takes {replay:?}",
            quantile(&idle, 0.5),
            quantile(&idle, 0.95),
            quantile(&busy, 0.5),
            quantile(&busy, 0.95),
            busy.len()
        );
        println!("append latency: {report}");
        for q in [0.5, 0.95] {
            let bound = (quantile(&idle, q) * APPEND_LATENCY_FACTOR).max(APPEND_LATENCY_FLOOR);
            assert!(
                quantile(&busy, q) <= bound,
                "append q{q} above {bound:?}: {report}"
            );
        }
    })
    .await
    .unwrap();
}

pub async fn summary_matches_fold_on(log: &dyn EventLog) {
    for batch in workload() {
        log.append_batch(batch).await.unwrap();
    }
    let records = log.read_runtime_after(all(), 0).await.unwrap();
    let expected = summarize(&records);
    assert!(expected.len() >= 4);
    for ((tenant, session), (summary, turns)) in &expected {
        assert_eq!(
            log.session_summary(tenant, session).await.unwrap().as_ref(),
            Some(summary),
            "summary of {session:?}"
        );
        assert_eq!(&log.turn_ids(tenant, session).await.unwrap(), turns);
        assert_eq!(
            log.prompts_sent(tenant, session).await.unwrap(),
            summary.prompts_sent
        );
        assert_eq!(
            log.latest_seq(tenant, session).await.unwrap(),
            summary.last_seq
        );
        assert_eq!(
            log.session_owner(tenant, session).await.unwrap(),
            summary.owner
        );
    }
    let key = |session: &str| (local(), SessionKey::from(session));
    // The workload's rules, spelled out.
    let a = &expected[&key("a")].0;
    assert_eq!(
        a.last_revision, 4,
        "the last revision wins, not the largest"
    );
    assert_eq!(a.last_activity_ms, 40);
    assert_eq!(a.prompts_sent, 2);
    assert_eq!(a.turn_count, 2);
    assert_eq!(a.parent, None);
    assert_eq!(a.owner, Some(host()), "the creating opening is the owner");
    assert_eq!(
        expected[&key("kid")].0.parent,
        Some(SessionKey::from("a")),
        "a later opening without a parent does not clear it"
    );
    assert_eq!(
        expected[&key("kid")].0.owner,
        None,
        "an opening that does not create sets no owner"
    );
    assert_eq!(
        expected[&key("b")].1,
        vec![TurnId::from("turn-7"), TurnId::from("turn-2")],
        "a prompt's turn counts"
    );
    assert_eq!(
        log.session_summary(&local(), &"missing".into())
            .await
            .unwrap(),
        None
    );
    let subagents: HashSet<_> = expected
        .iter()
        .filter(|(_, (summary, _))| summary.parent.is_some())
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(log.subagent_sessions(all()).await.unwrap(), subagents);
    let activity: HashMap<_, _> = expected
        .iter()
        .map(|(key, (summary, _))| (key.clone(), summary.last_activity_ms))
        .collect();
    assert_eq!(log.last_activity_ms(all()).await.unwrap(), activity);
}

pub async fn recovery_pages_on(log: &dyn EventLog) {
    for batch in workload() {
        log.append_batch(batch).await.unwrap();
    }
    for _ in 0..3 {
        for batch in workload() {
            let batch = batch
                .into_iter()
                .map(|mut event| {
                    event.generation = 2;
                    if let EventKind::Update(envelope) = &mut event.kind {
                        envelope.generation = 2;
                    }
                    event
                })
                .collect();
            log.append_batch(batch).await.unwrap();
        }
    }
    for from in [0, 1, 2, 3] {
        let whole = log.recovery_records(all(), from).await.unwrap();
        let mut paged = Vec::new();
        let mut after = 0;
        loop {
            let page = log
                .recovery_page(all(), from, after, PageLimit::events(4))
                .await
                .unwrap();
            assert!(page.items.len() <= 4);
            paged.extend(page.items);
            match page.next {
                Some(Cursor(next)) => after = next,
                None => break,
            }
        }
        assert_eq!(paged, whole, "from generation {from}");
        if from <= 2 {
            assert!(!whole.is_empty());
        } else {
            assert!(whole.is_empty());
        }
    }
}

// ------------------------------------------------------------------ step B3

/// One record of each kind (an Update first), all of `session`.
fn every_kind(session: &str, identity: &Identity) -> Vec<Event> {
    let kinds = vec![
        EventKind::PromptSent(json!({"prompt":"hello"})),
        EventKind::PromptSettled(json!({})),
        EventKind::Cancelled(json!({"reason":"user"})),
        EventKind::EscalationRaised(json!({"id":"e1"})),
        EventKind::EscalationAnswered(json!({"id":"e1","answer":"allow"})),
        EventKind::EscalationWithdrawn(json!({"id":"e1","reason":"cancelled"})),
        EventKind::SessionOpened(json!({"cwd":"/work","parent":null, SESSION_CREATED_KEY: true})),
        EventKind::SessionClosed(json!({"reason":"done"})),
        EventKind::SubagentStarted(json!({"child":"k"})),
        EventKind::SubagentEnded(json!({"child":"k"})),
        EventKind::PolicyDecision(json!({"id":"pd-1"})),
        EventKind::Unknown(json!({"kind":"future_kind","payload":{"x":7}})),
    ];
    let mut first = update(session);
    first.identity = identity.clone();
    let mut events = vec![first];
    for kind in kinds {
        events.push(Event::new(
            1,
            Some(session.into()),
            Some("turn-1".into()),
            101,
            kind,
            identity.clone(),
        ));
    }
    events
}

pub async fn identity_round_trip_on(log: &dyn EventLog) {
    let robot = Principal::new(
        PrincipalKind::Unknown("robot".into()),
        "https://idp.example",
        "r-1",
    )
    .expect("principal");
    let identities = [
        host(),
        Identity::acting(&Caller::local_token(Principal::local_uid(501)), local()),
        edge_user(acme()),
        edge_user(local()).with_actor(Some("parent-session".into())),
        Identity {
            tenant: acme(),
            principal: robot,
            via: Some(Via::edge()),
            actor: Some("agent:x".into()),
        },
    ];
    let mut appended = Vec::new();
    for (i, identity) in identities.iter().enumerate() {
        let session = format!("id-{i}");
        let records = log
            .append_batch(every_kind(&session, identity))
            .await
            .unwrap();
        for record in &records {
            assert_eq!(&record.event.identity, identity, "{:?}", record.event.kind);
        }
        appended.extend(records);
    }
    // Runtime records of both deployments.
    for identity in [
        loop_store::Deployment::SingleNode.runtime_identity(),
        loop_store::Deployment::SharedCell.runtime_identity(),
    ] {
        appended.extend(
            log.append_batch(vec![
                Event::new(
                    1,
                    None,
                    None,
                    1,
                    EventKind::RuntimeStarted(json!({"pid": 1})),
                    identity.clone(),
                ),
                Event::new(
                    1,
                    None,
                    None,
                    2,
                    EventKind::RuntimeStopped(json!({})),
                    identity,
                ),
            ])
            .await
            .unwrap(),
        );
    }
    // Read back by every path that returns records.
    assert_eq!(log.read_runtime_after(all(), 0).await.unwrap(), appended);
    for (i, identity) in identities.iter().enumerate() {
        let session = SessionKey::from(format!("id-{i}"));
        let updates = log.read_after(&identity.tenant, &session, 0).await.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(&updates[0].event.identity, identity);
        let page = log
            .read_session_page(&identity.tenant, &session, 0, None, PageLimit::DEFAULT)
            .await
            .unwrap();
        assert_eq!(page.items, updates);
    }
    let recovery = log.recovery_records(all(), 0).await.unwrap();
    assert!(!recovery.is_empty());
    for record in &recovery {
        let original = appended
            .iter()
            .find(|r| r.global_seq == record.global_seq)
            .expect("recovery record was appended");
        assert_eq!(record.event.identity, original.event.identity);
    }
}

pub async fn tenant_isolation_on(log: &dyn EventLog) {
    let (l, a) = (local(), acme());
    let in_tenant = |tenant: &Tenant, mut event: Event| {
        event.identity.tenant = tenant.clone();
        event
    };
    let opened = |tenant: &Tenant, key: &str, parent: serde_json::Value| {
        in_tenant(
            tenant,
            record(
                Some(key),
                None,
                5,
                EventKind::SessionOpened(
                    json!({"cwd": "/w", "parent": parent, SESSION_CREATED_KEY: true}),
                ),
            ),
        )
    };
    let prompt = |tenant: &Tenant, key: &str, turn: &str, ts: i64| {
        in_tenant(
            tenant,
            record(Some(key), Some(turn), ts, EventKind::PromptSent(json!({}))),
        )
    };
    let started = |tenant: &Tenant, key: &str, turn: &str, ts: i64| {
        in_tenant(
            tenant,
            update_with(
                key,
                turn,
                ts,
                SessionUpdate::TurnStarted {
                    turn_id: turn.into(),
                },
            ),
        )
    };
    // The same key "s" in both tenants, interleaved, plus a key and a
    // Subagent that exist in one tenant only.
    log.append_batch(vec![
        opened(&l, "s", json!(null)),
        opened(&a, "s", json!(null)),
        prompt(&l, "s", "turn-1", 10),
        started(&l, "s", "turn-1", 11),
        prompt(&a, "s", "turn-9", 50),
        started(&a, "s", "turn-9", 51),
        started(&a, "s", "turn-9", 52),
        opened(&a, "only-acme", json!(null)),
        opened(&a, "kid", json!("s")),
        started(&a, "only-acme", "turn-1", 60),
        in_tenant(
            &a,
            record(
                Some("s"),
                None,
                53,
                EventKind::EscalationRaised(json!({"id":"e1"})),
            ),
        ),
    ])
    .await
    .unwrap();
    log.append_batch(vec![started(&l, "s", "turn-1", 12)])
        .await
        .unwrap();
    let s = SessionKey::from("s");
    let only = SessionKey::from("only-acme");
    // Own `session_seq` space per tenant.
    assert_eq!(log.latest_seq(&l, &s).await.unwrap(), 2);
    assert_eq!(log.latest_seq(&a, &s).await.unwrap(), 2);
    assert_eq!(log.latest_seq(&l, &only).await.unwrap(), 0);
    assert_eq!(log.latest_seq(&a, &only).await.unwrap(), 1);
    let check_tenant = |records: &[LoggedEvent], tenant: &Tenant| {
        assert!(!records.is_empty());
        for record in records {
            assert_eq!(record.event.tenant(), tenant, "{record:?}");
        }
    };
    for tenant in [&l, &a] {
        let read = log.read_after(tenant, &s, 0).await.unwrap();
        check_tenant(&read, tenant);
        assert_eq!(seqs(&read), vec![1, 2]);
        let page = log
            .read_session_page(tenant, &s, 0, None, PageLimit::events(1))
            .await
            .unwrap();
        check_tenant(&page.items, tenant);
        assert_eq!(page.next, Some(Cursor(1)));
        let pages = all_pages_in(log, tenant, &s, 0, None, PageLimit::events(1)).await;
        let joined: Vec<_> = pages.into_iter().flat_map(|p| p.items).collect();
        assert_eq!(joined, read);
        let runtime = log
            .read_runtime_after(Scope::Tenant(tenant), 0)
            .await
            .unwrap();
        check_tenant(&runtime, tenant);
        let recovery = log
            .recovery_records(Scope::Tenant(tenant), 0)
            .await
            .unwrap();
        check_tenant(&recovery, tenant);
        let mut paged = Vec::new();
        let mut after = 0;
        loop {
            let page = log
                .recovery_page(Scope::Tenant(tenant), 0, after, PageLimit::events(1))
                .await
                .unwrap();
            paged.extend(page.items);
            match page.next {
                Some(Cursor(next)) => after = next,
                None => break,
            }
        }
        assert_eq!(paged, recovery);
        let summary = log.session_summary(tenant, &s).await.unwrap().unwrap();
        assert_eq!(&summary.tenant, tenant);
        assert_eq!(summary.last_seq, 2);
        assert_eq!(summary.prompts_sent, 1);
        assert_eq!(summary.turn_count, 1);
        assert_eq!(
            summary.owner.as_ref().map(|o| &o.tenant),
            Some(tenant),
            "each tenant's Session has its own owner"
        );
        for (key, _) in log.last_activity_ms(Scope::Tenant(tenant)).await.unwrap() {
            assert_eq!(&key.0, tenant, "activity of {key:?}");
        }
        for (key_tenant, _) in log.subagent_sessions(Scope::Tenant(tenant)).await.unwrap() {
            assert_eq!(&key_tenant, tenant);
        }
    }
    assert_eq!(
        log.turn_ids(&l, &s).await.unwrap(),
        vec![TurnId::from("turn-1")]
    );
    assert_eq!(
        log.turn_ids(&a, &s).await.unwrap(),
        vec![TurnId::from("turn-9")]
    );
    assert_eq!(log.prompts_sent(&l, &only).await.unwrap(), 0);
    assert!(log.read_after(&l, &only, 0).await.unwrap().is_empty());
    assert_eq!(
        log.read_session_page(&l, &only, 0, None, PageLimit::DEFAULT)
            .await
            .unwrap(),
        Page::last(Vec::new())
    );
    assert_eq!(log.session_summary(&l, &only).await.unwrap(), None);
    assert_eq!(log.session_owner(&l, &only).await.unwrap(), None);
    assert!(log.turn_ids(&l, &only).await.unwrap().is_empty());
    assert_eq!(
        log.subagent_sessions(Scope::Tenant(&l)).await.unwrap(),
        HashSet::new()
    );
    assert_eq!(
        log.subagent_sessions(Scope::Tenant(&a)).await.unwrap(),
        HashSet::from([(a.clone(), SessionKey::from("kid"))])
    );
    let local_activity = log.last_activity_ms(Scope::Tenant(&l)).await.unwrap();
    assert_eq!(
        local_activity,
        HashMap::from([((l.clone(), s.clone()), 12)])
    );
    // The system scope sees both tenants' Sessions apart.
    let activity = log.last_activity_ms(all()).await.unwrap();
    assert_eq!(activity[&(l.clone(), s.clone())], 12);
    assert_eq!(activity[&(a.clone(), s.clone())], 53);
    assert_eq!(activity[&(a.clone(), only.clone())], 60);
    let everything = log.read_runtime_after(all(), 0).await.unwrap();
    assert_eq!(everything.len(), 12);
    let recovery = log.recovery_records(all(), 0).await.unwrap();
    assert!(recovery.iter().any(|r| r.event.tenant() == &l));
    assert!(recovery.iter().any(|r| r.event.tenant() == &a));
}

/// All pages of `tenant`'s Session's read.
pub async fn all_pages_in(
    log: &dyn EventLog,
    tenant: &Tenant,
    session: &SessionKey,
    after: u64,
    through: Option<u64>,
    limit: PageLimit,
) -> Vec<Page<LoggedEvent>> {
    let mut pages = Vec::new();
    let mut after = after;
    loop {
        let page = log
            .read_session_page(tenant, session, after, through, limit)
            .await
            .unwrap();
        let next = page.next;
        pages.push(page);
        match next {
            Some(Cursor(seq)) => after = seq,
            None => return pages,
        }
    }
}

pub async fn owner_of_record_on(log: &dyn EventLog) {
    let opened = |identity: Identity, created: bool| {
        let mut payload = json!({"cwd": "/w", "parent": null});
        if created {
            payload[SESSION_CREATED_KEY] = json!(true);
        }
        Event::new(
            1,
            Some("o".into()),
            None,
            1,
            EventKind::SessionOpened(payload),
            identity,
        )
    };
    let o = SessionKey::from("o");
    // A load before any creating record sets no owner.
    log.append_batch(vec![opened(edge_user(local()), false)])
        .await
        .unwrap();
    assert_eq!(log.session_owner(&local(), &o).await.unwrap(), None);
    // The creating opening sets it; later loads by others do not change it.
    log.append_batch(vec![opened(host(), true)]).await.unwrap();
    log.append_batch(vec![
        opened(edge_user(local()), false),
        opened(edge_user(local()), true),
    ])
    .await
    .unwrap();
    assert_eq!(log.session_owner(&local(), &o).await.unwrap(), Some(host()));
    // The same key in another tenant has its own owner.
    log.append_batch(vec![opened(edge_user(acme()), true)])
        .await
        .unwrap();
    assert_eq!(
        log.session_owner(&acme(), &o).await.unwrap(),
        Some(edge_user(acme()))
    );
    assert_eq!(log.session_owner(&local(), &o).await.unwrap(), Some(host()));
    assert_eq!(
        log.session_summary(&local(), &o)
            .await
            .unwrap()
            .and_then(|s| s.owner),
        Some(host())
    );
}
