//! Defects found by probe E1 (see `docs/probes/e1-results.md`).
//!
//! Each test states an invariant E1 needs. The ignored ones fail on `staging`
//! today and stay ignored until their defect is fixed; run them with
//! `cargo test -p loop-runtime --test e1_defects -- --ignored`.
#![cfg(unix)]

use async_trait::async_trait;
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, ContentBlock, SessionKey, SessionSpec, SessionUpdate, StopReason, TurnId,
    TurnOutcome, UpdateEnvelope,
};
use loop_runtime::{
    log::{Event, EventKind, EventLog, LoggedEvent, Result as LogResult, SqliteEventLog},
    Runtime, RuntimeLimits,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

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

/// Another process holds the Session database's write lock.
type Held = Arc<Mutex<Option<rusqlite::Connection>>>;

fn hold_write_lock(db: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    conn
}

fn tree_has(db: &Path, needle: &str) -> bool {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM session_entries WHERE payload LIKE ?1",
        [format!("%{needle}%")],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// The model answers `reply` at once. Just before it does, the Session
/// database's write lock is taken, so the turn's tree writes must wait.
fn locking_model(db: PathBuf, held: Held, reply: &'static str) -> Arc<loop_ai::Models> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(move |_: &Context| {
        held.lock()
            .unwrap()
            .get_or_insert_with(|| hold_write_lock(&db));
        Box::pin(std::future::ready(FauxResponse::Text(reply.into())))
    })));
    models
}

/// The SQLite log; when a `turn_ended` commits, it records whether the tree
/// already had `needle`.
struct AtTurnEnd {
    inner: SqliteEventLog,
    db: PathBuf,
    needle: &'static str,
    tree_at_end: Mutex<Option<bool>>,
    /// Told when a `turn_ended` committed.
    ended: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}
#[async_trait]
impl EventLog for AtTurnEnd {
    async fn append_batch(&self, events: Vec<Event>) -> LogResult<Vec<LoggedEvent>> {
        let ends = events.iter().any(|e| {
            matches!(&e.kind, EventKind::Update(u) if matches!(u.update, SessionUpdate::TurnEnded { .. }))
        });
        let result = self.inner.append_batch(events).await;
        if ends && result.is_ok() {
            self.tree_at_end
                .lock()
                .unwrap()
                .get_or_insert_with(|| tree_has(&self.db, self.needle));
            if let Some(ended) = self.ended.lock().unwrap().take() {
                let _ = ended.send(());
            }
        }
        result
    }
    async fn read_after(&self, s: &SessionKey, after: u64) -> LogResult<Vec<LoggedEvent>> {
        self.inner.read_after(s, after).await
    }
    async fn read_runtime_after(&self, g: u64) -> LogResult<Vec<LoggedEvent>> {
        self.inner.read_runtime_after(g).await
    }
    async fn latest_seq(&self, s: &SessionKey) -> LogResult<u64> {
        self.inner.latest_seq(s).await
    }
    async fn allocate_generation(&self, o: &str, p: u32, t: i64) -> LogResult<u64> {
        self.inner.allocate_generation(o, p, t).await
    }
    async fn finish_generation(&self, g: u64, t: i64) -> LogResult<()> {
        self.inner.finish_generation(g, t).await
    }
    async fn recovery_records(&self, g: u64) -> LogResult<Vec<LoggedEvent>> {
        self.inner.recovery_records(g).await
    }
    async fn last_recovered_generation(&self) -> LogResult<u64> {
        self.inner.last_recovered_generation().await
    }
    async fn mark_recovered(&self, g: u64, t: i64) -> LogResult<()> {
        self.inner.mark_recovered(g, t).await
    }
    async fn subagent_sessions(&self) -> LogResult<HashSet<SessionKey>> {
        self.inner.subagent_sessions().await
    }
    async fn prompts_sent(&self, s: &SessionKey) -> LogResult<u64> {
        self.inner.prompts_sent(s).await
    }
    async fn turn_ids(&self, s: &SessionKey) -> LogResult<Vec<TurnId>> {
        self.inner.turn_ids(s).await
    }
    async fn last_activity_ms(&self) -> LogResult<HashMap<SessionKey, i64>> {
        self.inner.last_activity_ms().await
    }
    async fn sync(&self) -> loop_runtime::log::Result<()> {
        self.inner.sync().await
    }
    async fn read_session_page(
        &self,
        session: &loop_protocol::SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<loop_runtime::log::LoggedEvent>> {
        self.inner
            .read_session_page(session, after_seq, through_seq, limit)
            .await
    }
    async fn session_summary(
        &self,
        session: &loop_protocol::SessionKey,
    ) -> loop_runtime::log::Result<Option<loop_runtime::log::SessionSummary>> {
        self.inner.session_summary(session).await
    }
    async fn recovery_page(
        &self,
        from_generation: u64,
        after_global: u64,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<loop_runtime::log::LoggedEvent>> {
        self.inner
            .recovery_page(from_generation, after_global, limit)
            .await
    }
}

async fn runtime_with(dir: &Path, models: Arc<loop_ai::Models>, log: Arc<dyn EventLog>) -> Runtime {
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(globals, opts(dir), log, store, RuntimeLimits::default())
    }
    .await
    .unwrap()
}

/// D1. The Event Log says a turn ended normally before the Session tree
/// holds that turn. A crash in that window leaves `turn_ended: end_turn` in
/// `events.db` and no reply in `sessions.db`; recovery sees a finished turn
/// and does not mark it uncertain. Surfaces that watch the stream have already
/// shown it as finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]

async fn d1_a_logged_turn_end_implies_the_tree_holds_the_turn() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let db = agent.join("sessions.db");
        let held: Held = Arc::default();
        let (ended, ended_rx) = std::sync::mpsc::channel();
        let log = Arc::new(AtTurnEnd {
            inner: SqliteEventLog::open(&agent).await.unwrap(),
            db: db.clone(),
            needle: "reply-d1",
            tree_at_end: Mutex::new(None),
            ended: Mutex::new(Some(ended)),
        });
        // The tree writes wait until the log has committed `turn_ended`. A
        // turn end that waits for the tree never commits first, so the lock
        // goes after 3 s, short of the 6 s write deadline, and the writes
        // then succeed. Either way the order is fixed, not a timing window.
        let releaser = held.clone();
        std::thread::spawn(move || {
            while releaser.lock().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = ended_rx.recv_timeout(Duration::from_secs(3));
            releaser.lock().unwrap().take();
        });
        let models = locking_model(db.clone(), held, "reply-d1");
        let rt = runtime_with(dir.path(), models, log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let outcome = rt
            .prompt(&session, vec![ContentBlock::text("go")])
            .await
            .unwrap()
            .outcome()
            .await;
        assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
        assert!(tree_has(&db, "reply-d1"), "the reply reached the tree in the end");
        let at_end = log.tree_at_end.lock().unwrap().expect("turn_ended was logged");
        rt.shutdown().await;
        assert!(
            at_end,
            "turn_ended (end_turn) was committed to events.db while sessions.db did not yet hold the turn"
        );
    })
    .await
    .expect("test timed out");
}

/// D3 (fixed). A turn whose tree writes fail must not be acknowledged as
/// completed. `AgentHarness::run_turn` used to log the failed `append_entry`
/// and return the reply, so the prompt resolved `Completed` while the tree had
/// no trace of the turn. Here the writes fail because another process holds
/// the database's write lock past the 6 s write deadline; a full disk or a
/// lost lease does the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d3_a_completed_turn_is_in_the_tree() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let db = agent.join("sessions.db");
        let held: Held = Arc::default();
        let log: Arc<dyn EventLog> = Arc::new(SqliteEventLog::open(&agent).await.unwrap());
        let models = locking_model(db.clone(), held.clone(), "reply-d3");
        let rt = runtime_with(dir.path(), models, log.clone()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let outcome = rt
            .prompt(&session, vec![ContentBlock::text("go")])
            .await
            .unwrap()
            .outcome()
            .await;
        held.lock().unwrap().take();
        let durable = tree_has(&db, "reply-d3");
        rt.shutdown().await;
        assert!(
            !matches!(outcome, TurnOutcome::Completed(_)) || durable,
            "the prompt was acknowledged {outcome:?}, but sessions.db has no trace of the turn"
        );
        // The turn's end reports the failed write; no separate error notice.
        let updates: Vec<_> = log
            .read_after(&session.key, 0)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|r| match r.event.kind {
                EventKind::Update(e) => Some(e.update),
                _ => None,
            })
            .collect();
        let ends: Vec<_> = updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::TurnEnded { stop, error, .. } => Some((*stop, error.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(ends, [(StopReason::Error, true)], "{updates:?}");
        assert!(
            !updates.iter().any(|u| matches!(
                u,
                SessionUpdate::Notice {
                    level: loop_protocol::NoticeLevel::Error,
                    ..
                }
            )),
            "{updates:?}"
        );
    })
    .await
    .expect("test timed out");
}

/// D2. A prompt the log accepted, whose `turn_started` never committed, is
/// not recovered. The turn's tools do not wait for the log, so with a slow
/// writer such a turn can already have run them (E1 saw side effects, and even
/// partial trees, with only `prompt_sent` in the log). Recovery reads only turn
/// boundaries, so it records nothing for this turn.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]

async fn d2_a_prompt_whose_turn_start_was_not_logged_is_recovered_as_uncertain() {
    tokio::time::timeout(LIMIT, async {
        use loop_agent::harness::session::{sqlite::SqliteSessionStore, SessionStore};
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let store = SqliteSessionStore::open(agent.join("sessions.db")).unwrap();
        let a: SessionKey = store
            .create(Some(dir.path().to_string_lossy().into_owned()), None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone()
            .into();
        let log = Arc::new(SqliteEventLog::open(&agent).await.unwrap());
        let g = log.allocate_generation("runtime", 1, 0).await.unwrap();
        let update = |turn: &str, update: SessionUpdate| {
            Event::update(
                g,
                1,
                UpdateEnvelope::live(
                    "loop".into(),
                    g,
                    a.clone(),
                    Some(TurnId(turn.into())),
                    0,
                    update,
                ),
            )
        };
        let prompt = Event {
            generation: g,
            session_key: Some(a.clone()),
            turn: None,
            ts_ms: 1,
            kind: EventKind::PromptSent(json!({"content": [{"type": "text", "text": "x"}]})),
        };
        // One finished turn, then a prompt whose turn start never committed.
        log.append_batch(vec![
            prompt.clone(),
            update(
                "turn-1",
                SessionUpdate::TurnStarted {
                    turn_id: TurnId("turn-1".into()),
                },
            ),
            update(
                "turn-1",
                SessionUpdate::TurnEnded {
                    turn_id: TurnId("turn-1".into()),
                    stop: StopReason::EndTurn,
                    error: None,
                },
            ),
            prompt,
        ])
        .await
        .unwrap();
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let rt = runtime_with(dir.path(), models, log.clone()).await;
        let lock = loop_app_core::runtime_instance::SessionDatabaseLock::acquire(
            &agent.join("sessions.db"),
        )
        .unwrap();
        let report = rt.recover(Some(&lock)).await.unwrap();
        assert_eq!(
            report.turns_ended, 1,
            "the second prompt may have run its tools; it must end uncertain ({report:?})"
        );
        // The recovered turn is named after the prompt's global sequence (4).
        let started: Vec<_> = log
            .read_after(&a, 0)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|r| match r.event.kind {
                EventKind::Update(e) => match e.update {
                    SessionUpdate::TurnStarted { turn_id } => Some(turn_id.0),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(started, ["turn-1", "recovered-prompt-4"]);
        // A second pass finds nothing more.
        let again = rt.recover(Some(&lock)).await.unwrap();
        rt.shutdown().await;
        assert_eq!(again.turns_ended, 0, "{again:?}");
    })
    .await
    .expect("test timed out");
}

async fn runtime_limited(
    dir: &Path,
    models: Arc<loop_ai::Models>,
    log: Arc<dyn EventLog>,
    limits: RuntimeLimits,
) -> Runtime {
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(globals, opts(dir), log, store, limits)
    }
    .await
    .unwrap()
}

/// The model waits for `gate` before it answers; `entered` is told first.
fn gated_model(
    gate: Arc<tokio::sync::Semaphore>,
    entered: Arc<tokio::sync::Notify>,
) -> Arc<loop_ai::Models> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(move |_: &Context| {
        let (gate, entered) = (gate.clone(), entered.clone());
        Box::pin(async move {
            entered.notify_one();
            gate.acquire().await.unwrap().forget();
            FauxResponse::Text("done".into())
        })
    })));
    models
}

/// Review of the D2 fix. A prompt cancelled while it waited for a turn
/// permit never started a turn. After a crash it must not be marked
/// uncertain: the Runtime logs that it settled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_cancelled_in_the_queue_is_not_recovered_as_uncertain() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let log: Arc<dyn EventLog> = Arc::new(SqliteEventLog::open(&agent).await.unwrap());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let limits = RuntimeLimits {
            max_active_turns: 1,
            ..RuntimeLimits::default()
        };
        let rt = runtime_limited(
            dir.path(),
            gated_model(gate.clone(), entered.clone()),
            log.clone(),
            limits,
        )
        .await;
        let a = rt.new_session(SessionSpec::default()).await.unwrap();
        let b = rt.new_session(SessionSpec::default()).await.unwrap();
        let first = rt.prompt(&a, vec![ContentBlock::text("a")]).await.unwrap();
        entered.notified().await;
        // A holds the only permit: B waits for it, then is cancelled.
        let queued = rt.prompt(&b, vec![ContentBlock::text("b")]).await.unwrap();
        rt.cancel(&b);
        assert!(matches!(queued.outcome().await, TurnOutcome::Cancelled));
        gate.add_permits(1);
        assert!(matches!(first.outcome().await, TurnOutcome::Completed(_)));
        // Crash: no clean stop.
        drop(rt);
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let rt = runtime_with(dir.path(), models, log).await;
        let report = rt.recover(None).await.unwrap();
        rt.shutdown().await;
        assert_eq!(report.turns_ended, 0, "{report:?}");
    })
    .await
    .expect("test timed out");
}

/// Review of the D2 fix. A prompt the backend refused (busy, invalid input,
/// unknown Session) never started a turn; after a crash it must not be
/// marked uncertain. Invalid input takes the same path as a busy refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_the_backend_refused_is_not_recovered_as_uncertain() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let log: Arc<dyn EventLog> = Arc::new(SqliteEventLog::open(&agent).await.unwrap());
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let rt = runtime_with(dir.path(), models.clone(), log.clone()).await;
        let a = rt.new_session(SessionSpec::default()).await.unwrap();
        let refused = rt
            .prompt(
                &a,
                vec![ContentBlock::ResourceLink {
                    uri: "file:///x".into(),
                    name: None,
                }],
            )
            .await
            .unwrap();
        assert!(matches!(refused.outcome().await, TurnOutcome::Failed(_)));
        drop(rt);
        let rt = runtime_with(dir.path(), models, log).await;
        let report = rt.recover(None).await.unwrap();
        rt.shutdown().await;
        assert_eq!(report.turns_ended, 0, "{report:?}");
    })
    .await
    .expect("test timed out");
}
