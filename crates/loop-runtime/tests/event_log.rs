use loop_protocol::{SessionKey, SessionUpdate, UpdateEnvelope};
use loop_runtime::log::{Event, EventKind, EventLog, MemoryEventLog};
use serde_json::json;

fn update(session: &str) -> Event {
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

async fn round_trip(log: &dyn EventLog) {
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

#[tokio::test]
async fn memory_round_trip() {
    round_trip(&MemoryEventLog::new()).await;
}

#[tokio::test]
async fn sqlite_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let log = loop_runtime::log::SqliteEventLog::open(dir.path())
        .await
        .unwrap();
    round_trip(&log).await;
}

async fn concurrent_producers(log: std::sync::Arc<dyn EventLog>) {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(129));
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

#[tokio::test]
async fn sqlite_concurrent_producers() {
    let dir = tempfile::tempdir().unwrap();
    concurrent_producers(std::sync::Arc::new(
        loop_runtime::log::SqliteEventLog::open(dir.path())
            .await
            .unwrap(),
    ))
    .await;
}
#[tokio::test]
async fn memory_concurrent_producers() {
    concurrent_producers(std::sync::Arc::new(MemoryEventLog::new())).await;
}

async fn cursors(log: &dyn EventLog) {
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

async fn generations(log: &dyn EventLog) {
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

async fn atomic_validation(log: &dyn EventLog) {
    use loop_runtime::log::MAX_BATCH_EVENTS;
    assert_eq!(MAX_BATCH_EVENTS, 256);
    assert_eq!(loop_runtime::log::WRITER_QUEUE_CAPACITY, 64);
    assert!(log.append_batch(Vec::new()).await.unwrap().is_empty());
    let mut invalid = update("s1");
    invalid.session_key = None;
    assert!(log.append_batch(vec![update("s1"), invalid]).await.is_err());
    let mut replay = update("s1");
    if let EventKind::Update(envelope) = &mut replay.kind {
        envelope.origin = loop_protocol::Origin::Replay;
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

macro_rules! shared_suite {
    ($memory:ident, $sqlite:ident, $suite:ident) => {
        #[tokio::test]
        async fn $memory() {
            $suite(&MemoryEventLog::new()).await;
        }
        #[tokio::test]
        async fn $sqlite() {
            let dir = tempfile::tempdir().unwrap();
            let log = loop_runtime::log::SqliteEventLog::open(dir.path())
                .await
                .unwrap();
            $suite(&log).await;
        }
    };
}
shared_suite!(memory_cursors, sqlite_cursors, cursors);
shared_suite!(memory_generations, sqlite_generations, generations);
shared_suite!(
    memory_atomic_validation,
    sqlite_atomic_validation,
    atomic_validation
);

#[tokio::test]
async fn sqlite_reopen_keeps_cursors_generations_and_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let original;
    {
        let log = loop_runtime::log::SqliteEventLog::open(dir.path())
            .await
            .unwrap();
        assert_eq!(
            log.allocate_generation("runtime", 42, 100).await.unwrap(),
            1
        );
        round_trip(&log).await;
        original = log.read_runtime_after(0).await.unwrap();
    }
    let log = loop_runtime::log::SqliteEventLog::open(dir.path())
        .await
        .unwrap();
    assert_eq!(log.read_runtime_after(0).await.unwrap(), original);
    assert_eq!(
        log.allocate_generation("runtime", 43, 200).await.unwrap(),
        2
    );
    assert_eq!(
        log.append_batch(vec![update("s1")]).await.unwrap()[0].session_seq,
        Some(2)
    );
}

#[tokio::test]
async fn sqlite_generation_allocation_is_transactional_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let a = loop_runtime::log::SqliteEventLog::open(dir.path())
        .await
        .unwrap();
    let b = loop_runtime::log::SqliteEventLog::open(dir.path())
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        a.allocate_generation("runtime", 1, 10),
        b.allocate_generation("runtime", 2, 10)
    );
    let mut generations = vec![a.unwrap(), b.unwrap()];
    generations.sort();
    assert_eq!(generations, vec![1, 2]);
}

#[test]
fn unknown_kind_decode_preserves_future_payload() {
    let raw = json!({"kind":"new_activity", "payload":{"nested":[1,2,3]}, "extra":"retained"});
    let kind: EventKind = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(kind, EventKind::Unknown(raw.clone()));
    assert_eq!(serde_json::to_value(kind).unwrap(), raw);
}

async fn invalid_unknown_is_atomic(log: &dyn EventLog) {
    let mut bad = update("s1");
    bad.kind = EventKind::Unknown(json!({"payload":7}));
    assert!(log.append_batch(vec![update("s1"), bad]).await.is_err());
    assert!(log.read_runtime_after(0).await.unwrap().is_empty());
}
shared_suite!(
    memory_invalid_unknown,
    sqlite_invalid_unknown,
    invalid_unknown_is_atomic
);

#[tokio::test]
async fn sqlite_transaction_failure_does_not_consume_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let log = loop_runtime::log::SqliteEventLog::open(dir.path())
        .await
        .unwrap();
    // Fault injection only: all behavior is observed through the public Event Log.
    let conn = rusqlite::Connection::open(dir.path().join("events.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON events WHEN NEW.global_seq=2 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(log
        .append_batch(vec![update("s1"), update("s1")])
        .await
        .is_err());
    assert!(log.read_runtime_after(0).await.unwrap().is_empty());
    assert_eq!(log.latest_seq(&"s1".into()).await.unwrap(), 0);
    conn.execute_batch("DROP TRIGGER fail_second").unwrap();
    let records = log.append_batch(vec![update("s1")]).await.unwrap();
    assert_eq!(
        (records[0].global_seq, records[0].session_seq),
        (1, Some(1))
    );
}

async fn runtime_records_have_no_session_cursor(log: &dyn EventLog) {
    let mut event = update("s1");
    event.kind = EventKind::RuntimeStarted(json!({}));
    assert!(log.append_batch(vec![event.clone()]).await.is_err());
    event.session_key = None;
    event.turn = None;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        log.append_batch(vec![event]),
    )
    .await
    .expect("a lone append must flush without waiting for another producer")
    .unwrap();
    assert_eq!(result[0].session_seq, None);
    assert_eq!(result[0].event.session_key, None);
}
shared_suite!(
    memory_runtime_records,
    sqlite_runtime_records,
    runtime_records_have_no_session_cursor
);
