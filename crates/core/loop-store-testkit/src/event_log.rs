//! Event Log conformance suite.
//!
//! Each suite function takes a factory that returns a fresh, empty log in a
//! [`Fixture`]. [`run_all`] runs every check, each on its own fresh log.
//! The `*_on` functions run one check on a log you already hold; use them
//! when a test needs the log for more than one step (for example a reopen).
//!
//! The checks use these `loop_store` items: `Event`, `EventKind`,
//! `EventLog`, `MAX_BATCH_EVENTS` and `WRITER_QUEUE_CAPACITY`. A store that
//! implements a different queue capacity must still pass: the capacity is
//! only a constant of the contract. `loop_runtime::log` re-exports the same
//! items.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use loop_protocol::{Origin, SessionKey, SessionUpdate, UpdateEnvelope};
use loop_store::{Event, EventKind, EventLog, MAX_BATCH_EVENTS, WRITER_QUEUE_CAPACITY};
use serde_json::json;

use crate::Fixture;

/// Wrap a concrete log in a [`Fixture`] of the trait object a factory returns.
pub fn fixture(log: impl EventLog + 'static) -> Fixture<dyn EventLog> {
    let log: Arc<dyn EventLog> = Arc::new(log);
    Fixture::new(log)
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
        });
    }
    let committed = log.append_batch(events.clone()).await.unwrap();
    assert_eq!(committed.len(), 11);
    assert_eq!(log.read_runtime_after(0).await.unwrap(), committed);
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
    assert_eq!(log.latest_seq(&"s1".into()).await.unwrap(), 256);
    assert_eq!(log.latest_seq(&"s2".into()).await.unwrap(), 128);
    let all = log.read_runtime_after(0).await.unwrap();
    assert_eq!(all.len(), 384);
    for (i, record) in all.iter().enumerate() {
        assert_eq!(record.global_seq, i as u64 + 1);
    }
}

pub async fn cursors_on(log: &dyn EventLog) {
    assert_eq!(log.latest_seq(&"missing".into()).await.unwrap(), 0);
    assert!(log
        .read_after(&"missing".into(), 0)
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
    let session = log.read_after(&"s1".into(), 1).await.unwrap();
    assert_eq!(session.len(), 1);
    assert_eq!(
        (session[0].global_seq, session[0].session_seq),
        (5, Some(2))
    );
    assert_eq!(log.latest_seq(&"s1".into()).await.unwrap(), 2);
    assert_eq!(log.latest_seq(&"s2".into()).await.unwrap(), 1);
    assert_eq!(
        log.read_runtime_after(3)
            .await
            .unwrap()
            .iter()
            .map(|e| e.global_seq)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert!(log.read_after(&"s1".into(), 2).await.unwrap().is_empty());
    assert!(log
        .read_after(&"s1".into(), u64::MAX)
        .await
        .unwrap()
        .is_empty());
    assert!(log.read_runtime_after(u64::MAX).await.unwrap().is_empty());
    assert_eq!(
        log.read_runtime_after(0).await.unwrap()[0]
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
    assert!(log.read_runtime_after(0).await.unwrap().is_empty());
    assert_eq!(
        log.append_batch(vec![update("s1"); MAX_BATCH_EVENTS])
            .await
            .unwrap()
            .len(),
        256
    );
    assert_eq!(log.latest_seq(&"s1".into()).await.unwrap(), 256);
}

pub async fn invalid_unknown_is_atomic_on(log: &dyn EventLog) {
    let mut bad = update("s1");
    bad.kind = EventKind::Unknown(json!({"payload":7}));
    assert!(log.append_batch(vec![update("s1"), bad]).await.is_err());
    assert!(log.read_runtime_after(0).await.unwrap().is_empty());
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
        let keys = log.subagent_sessions().await.unwrap();
        assert_eq!(keys, HashSet::from([SessionKey::from("kid")]));
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
        };
        log.append_batch(vec![prompt("a"), update("a"), prompt("a"), prompt("b")])
            .await
            .unwrap();
        assert_eq!(log.prompts_sent(&"a".into()).await.unwrap(), 2);
        assert_eq!(log.prompts_sent(&"b".into()).await.unwrap(), 1);
        assert_eq!(log.prompts_sent(&"c".into()).await.unwrap(), 0);
    })
    .await
    .unwrap();
}
