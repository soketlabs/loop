use loop_protocol::SessionKey;
use loop_runtime::log::{Event, EventKind, EventLog, MemoryEventLog, SqliteEventLog};
use loop_store_testkit::event_log as kit;
use loop_store_testkit::Fixture;
use serde_json::json;

// The shared Event Log suite lives in `loop-store-testkit` so that stores
// outside this repository can run the same checks. This file runs it on the
// memory and SQLite logs and keeps the SQLite-only tests.

fn update(session: &str) -> Event {
    kit::update(session)
}

async fn memory_fixture() -> Fixture<dyn EventLog> {
    kit::fixture(MemoryEventLog::new())
}

async fn sqlite_fixture() -> Fixture<dyn EventLog> {
    let dir = tempfile::tempdir().unwrap();
    let log = SqliteEventLog::open(dir.path()).await.unwrap();
    kit::fixture(log).with_guard(dir)
}

macro_rules! shared_suite {
    ($memory:ident, $sqlite:ident, $suite:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $memory() {
            kit::$suite(&memory_fixture).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $sqlite() {
            kit::$suite(&sqlite_fixture).await;
        }
    };
}
shared_suite!(memory_round_trip, sqlite_round_trip, round_trip);
shared_suite!(
    memory_concurrent_producers,
    sqlite_concurrent_producers,
    concurrent_producers
);
shared_suite!(memory_cursors, sqlite_cursors, cursors);
shared_suite!(memory_generations, sqlite_generations, generations);
shared_suite!(
    memory_atomic_validation,
    sqlite_atomic_validation,
    atomic_validation
);
shared_suite!(
    memory_invalid_unknown,
    sqlite_invalid_unknown,
    invalid_unknown_is_atomic
);
shared_suite!(
    memory_runtime_records,
    sqlite_runtime_records,
    runtime_records_have_no_session_cursor
);
shared_suite!(
    memory_subagent_sessions,
    sqlite_subagent_sessions,
    subagent_sessions
);
shared_suite!(memory_prompts_sent, sqlite_prompts_sent, prompts_sent);
shared_suite!(memory_paged_reads, sqlite_paged_reads, paged_reads);
shared_suite!(
    memory_paged_read_under_appends,
    sqlite_paged_read_under_appends,
    paged_read_under_appends
);
shared_suite!(
    memory_read_your_writes,
    sqlite_read_your_writes,
    read_your_writes
);
shared_suite!(
    memory_long_read_does_not_delay_appends,
    sqlite_long_read_does_not_delay_appends,
    long_read_does_not_delay_appends
);
shared_suite!(
    memory_summary_matches_fold,
    sqlite_summary_matches_fold,
    summary_matches_fold
);
shared_suite!(memory_recovery_pages, sqlite_recovery_pages, recovery_pages);
shared_suite!(
    memory_identity_round_trip,
    sqlite_identity_round_trip,
    identity_round_trip
);
shared_suite!(
    memory_tenant_isolation,
    sqlite_tenant_isolation,
    tenant_isolation
);
shared_suite!(
    memory_owner_of_record,
    sqlite_owner_of_record,
    owner_of_record
);

#[tokio::test]
async fn sqlite_reopen_keeps_cursors_generations_and_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let original;
    {
        let log = SqliteEventLog::open(dir.path()).await.unwrap();
        assert_eq!(
            log.allocate_generation("runtime", 42, 100).await.unwrap(),
            1
        );
        kit::round_trip_on(&log).await;
        original = log
            .read_runtime_after(
                loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
                0,
            )
            .await
            .unwrap();
    }
    let log = SqliteEventLog::open(dir.path()).await.unwrap();
    assert_eq!(
        log.read_runtime_after(
            loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
            0
        )
        .await
        .unwrap(),
        original
    );
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
    let a = SqliteEventLog::open(dir.path()).await.unwrap();
    let b = SqliteEventLog::open(dir.path()).await.unwrap();
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

#[tokio::test]
async fn sqlite_transaction_failure_does_not_consume_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let log = SqliteEventLog::open(dir.path()).await.unwrap();
    // Fault injection only: all behavior is observed through the public Event Log.
    let conn = rusqlite::Connection::open(dir.path().join("events.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON events WHEN NEW.global_seq=2 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(log
        .append_batch(vec![update("s1"), update("s1")])
        .await
        .is_err());
    assert!(log
        .read_runtime_after(
            loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
            0
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        log.latest_seq(&loop_store::Tenant::local(), &"s1".into())
            .await
            .unwrap(),
        0
    );
    conn.execute_batch("DROP TRIGGER fail_second").unwrap();
    let records = log.append_batch(vec![update("s1")]).await.unwrap();
    assert_eq!(
        (records[0].global_seq, records[0].session_seq),
        (1, Some(1))
    );
}

/// Recovery needs only that a prompt was sent, not its content: the SQLite
/// log does not load `prompt_sent` payloads for it, and finds recovery
/// records by an index on (generation, kind).
#[tokio::test]
async fn sqlite_recovery_does_not_load_prompt_content() {
    let dir = tempfile::tempdir().unwrap();
    let log = SqliteEventLog::open(dir.path()).await.unwrap();
    let g = log.allocate_generation("runtime", 1, 0).await.unwrap();
    let prompt = |kind: EventKind| Event {
        generation: g,
        session_key: Some(SessionKey("s".into())),
        turn: None,
        ts_ms: 1,
        kind,
        identity: loop_store::Identity::unrecorded(),
    };
    log.append_batch(vec![
        prompt(EventKind::PromptSent(
            json!({"content": "x".repeat(10_000)}),
        )),
        prompt(EventKind::PromptSettled(json!({}))),
    ])
    .await
    .unwrap();
    let kinds: Vec<_> = log
        .recovery_records(
            loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
            1,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            EventKind::PromptSent(json!({})),
            EventKind::PromptSettled(json!({}))
        ]
    );
    let conn = rusqlite::Connection::open(dir.path().join("events.db")).unwrap();
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_index_info('events_generation_kind')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(columns, ["generation", "kind"]);
}

/// A `policy_decision` row in a SQLite Event Log: it takes no Session
/// sequence, so a Session read (attach, replay) skips it and the Session's
/// sequence stays contiguous; it reads back unchanged after a reopen; and
/// recovery's reads leave it out.
#[tokio::test]
async fn a_policy_decision_row_is_log_only_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let decision = json!({"id": "pd-1", "outcome": "deny", "by": "rule"});
    {
        let log = SqliteEventLog::open(dir.path()).await.unwrap();
        log.append_batch(vec![update("s")]).await.unwrap();
        log.append_batch(vec![Event {
            generation: 1,
            session_key: Some(SessionKey::from("s")),
            turn: None,
            ts_ms: 1,
            kind: EventKind::PolicyDecision(decision.clone()),
            identity: loop_store::Identity::unrecorded(),
        }])
        .await
        .unwrap();
        log.append_batch(vec![update("s")]).await.unwrap();
    }
    let log = SqliteEventLog::open(dir.path()).await.unwrap();
    let session = log
        .read_after(&loop_store::Tenant::local(), &SessionKey::from("s"), 0)
        .await
        .unwrap();
    let seqs: Vec<_> = session.iter().map(|r| r.session_seq).collect();
    assert_eq!(seqs, [Some(1), Some(2)]);
    let all = log
        .read_runtime_after(
            loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
            0,
        )
        .await
        .unwrap();
    let stored = all
        .iter()
        .find(|r| matches!(r.event.kind, EventKind::PolicyDecision(_)))
        .expect("the decision is stored");
    assert_eq!(stored.event.kind, EventKind::PolicyDecision(decision));
    assert_eq!(stored.session_seq, None);
    assert!(log
        .recovery_records(
            loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
            0
        )
        .await
        .unwrap()
        .iter()
        .all(|r| !matches!(r.event.kind, EventKind::PolicyDecision(_))));
}

/// Decision rows and the Session projection: a decision counts as Session
/// activity, and its turn is the turn already running, so it adds no turn,
/// no Session sequence, no prompt and no revision. A decision with no turn
/// (a typed command between turns) adds no turn either.
#[tokio::test]
async fn policy_decision_rows_add_no_phantom_turns_to_the_projection() {
    async fn check(log: &dyn EventLog) {
        let s = SessionKey::from("s");
        log.append_batch(vec![update("s")]).await.unwrap();
        let before = log
            .session_summary(&loop_store::Tenant::local(), &s)
            .await
            .unwrap()
            .unwrap();
        let decision = |turn: Option<&str>, ts_ms| Event {
            generation: 1,
            session_key: Some(s.clone()),
            turn: turn.map(Into::into),
            ts_ms,
            kind: EventKind::PolicyDecision(json!({"id": "pd", "outcome": "allow"})),
            identity: loop_store::Identity::unrecorded(),
        };
        log.append_batch(vec![decision(Some("turn-1"), 5_000)])
            .await
            .unwrap();
        log.append_batch(vec![decision(None, 6_000)]).await.unwrap();
        let after = log
            .session_summary(&loop_store::Tenant::local(), &s)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.last_seq, before.last_seq);
        assert_eq!(after.last_revision, before.last_revision);
        assert_eq!(after.prompts_sent, before.prompts_sent);
        assert_eq!(after.turn_count, before.turn_count);
        assert_eq!(after.parent, before.parent);
        assert_eq!(after.last_activity_ms, 6_000);
        let turns: Vec<String> = log
            .turn_ids(&loop_store::Tenant::local(), &s)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(turns, ["turn-1"]);
    }
    check(&MemoryEventLog::new()).await;
    let dir = tempfile::tempdir().unwrap();
    check(&SqliteEventLog::open(dir.path()).await.unwrap()).await;
    // The same after a reopen, from the stored projection.
    let log = SqliteEventLog::open(dir.path()).await.unwrap();
    let summary = log
        .session_summary(&loop_store::Tenant::local(), &SessionKey::from("s"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.turn_count, 1);
    assert_eq!(summary.last_activity_ms, 6_000);
}
