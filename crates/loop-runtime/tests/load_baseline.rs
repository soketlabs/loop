//! Load baseline for the Runtime (task 0.3). Ignored by default.
//!
//! The test builds one in-process Runtime over the `faux` provider and runs
//! `LOAD_SESSIONS` Sessions through it. `LOAD_ACTIVE` client tasks drive
//! turns at the same time. Each model call streams `LOAD_CHUNKS` text deltas.
//! `LOAD_SUBSCRIBERS` tasks read the Runtime's one broadcast channel.
//!
//! It measures:
//!
//! * total wall time;
//! * Event Log commits: `append_batch` calls and events, per second. A
//!   counting wrapper around the Event Log counts them, so the Runtime source
//!   is not touched. The SQLite log may merge queued calls into one
//!   transaction. The wrapper counts the calls the Runtime makes, not
//!   transactions;
//! * delivery delay, two ways. `delivery_delay_us` runs from the return of
//!   the `append_batch` that committed an Update to the moment a subscriber
//!   receives it. The Runtime sends to the broadcast channel right after the
//!   commit returns, so this is the broadcast hop and the wait in the channel.
//!   `end_to_end_delay_us` runs from the start of that `append_batch` call, so
//!   it adds the commit itself (the queue for the writer and the write);
//! * observed concurrency: the peak number of turns that had started and not
//!   ended (counted from committed `TurnStarted` and `TurnEnded` Updates), and
//!   the peak number of `append_batch` calls in flight;
//! * peak resident set size of the process (`getrusage`, whole process life);
//! * lagged broadcast receivers (a receiver that got `Lagged`) and the number
//!   of updates they skipped.
//!
//! Parameters (environment variables, defaults in brackets):
//!
//! ```text
//! LOAD_SESSIONS [32]        Sessions to open and run
//! LOAD_ACTIVE [16]          client tasks that drive turns at once
//! LOAD_TURNS [2]            turns per Session, one after another
//! LOAD_CHUNKS [100]         text deltas per model call
//! LOAD_CHUNK_BYTES [16]     bytes per delta
//! LOAD_SUBSCRIBERS [4]      fast broadcast subscribers
//! LOAD_SLOW_SUBSCRIBERS [0] subscribers that sleep per update
//! LOAD_SLOW_MS [5]          sleep of a slow subscriber, per update
//! LOAD_MAX_SESSIONS [16]    RuntimeLimits.max_sessions (the default)
//! LOAD_MAX_ACTIVE_TURNS [8] RuntimeLimits.max_active_turns (the default)
//! LOAD_STORE [sqlite]       Event Log under the counter: sqlite or memory
//! LOAD_CLOSE [0]            1 closes each Session after its turns
//! LOAD_DEADLINE_SECS [100]  stop waiting after this long
//! ```
//!
//! Run it one configuration per process, so the peak RSS is per run:
//!
//! ```text
//! cargo test --locked -p loop-runtime --test load_baseline -- --ignored --nocapture
//! LOAD_SESSIONS=200 LOAD_ACTIVE=50 LOAD_MAX_SESSIONS=200 LOAD_MAX_ACTIVE_TURNS=50 \
//!     cargo test --locked -p loop-runtime --test load_baseline -- --ignored --nocapture
//! ```
//!
//! The result is one line that starts with `LOAD|RESULT|` and holds JSON,
//! plus a few `LOAD|` lines for people.
#![cfg(unix)]

use async_trait::async_trait;
use loop_ai::{
    providers::{faux_provider_fn, FauxResponder, FauxResponse},
    AssistantContent, AssistantMessage, AssistantMessageEvent, InputModality, Model, ModelCost,
    StopReason as AiStopReason, TextContent,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, ContentBlock, SessionKey, SessionSpec, SessionUpdate, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::{
    log::{
        Event, EventKind, EventLog, LoggedEvent, MemoryEventLog, Result as LogResult,
        SqliteEventLog,
    },
    Runtime, RuntimeLimits,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{sync::watch, time::timeout};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn env_str(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}

// ------------------------------------------------------------------ RSS

#[repr(C)]
struct TimeVal {
    sec: i64,
    usec: i64,
}
#[repr(C)]
struct RUsage {
    utime: TimeVal,
    stime: TimeVal,
    maxrss: i64,
    rest: [i64; 13],
}
extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}
/// Peak resident set size of this process so far, in bytes.
fn peak_rss_bytes() -> u64 {
    let mut usage = RUsage {
        utime: TimeVal { sec: 0, usec: 0 },
        stime: TimeVal { sec: 0, usec: 0 },
        maxrss: 0,
        rest: [0; 13],
    };
    // RUSAGE_SELF is 0 on Linux and macOS.
    let rc = unsafe { getrusage(0, &mut usage) };
    if rc != 0 {
        return 0;
    }
    // macOS reports bytes; Linux reports kilobytes.
    if cfg!(target_os = "macos") {
        usage.maxrss as u64
    } else {
        usage.maxrss as u64 * 1024
    }
}

// --------------------------------------------------------- counting log

/// Wraps an Event Log, counts commits and notes when each Update committed.
struct CountingLog {
    inner: Arc<dyn EventLog>,
    calls: AtomicU64,
    events: AtomicU64,
    updates: AtomicU64,
    failures: AtomicU64,
    /// Time each `append_batch` call took, in microseconds.
    call_micros: Mutex<Vec<u64>>,
    /// Largest batch seen.
    max_batch: AtomicUsize,
    /// For each Update: when its `append_batch` began and when it returned.
    committed_at: Mutex<HashMap<(SessionKey, u64), (Instant, Instant)>>,
    inflight: AtomicI64,
    peak_inflight: AtomicI64,
    active_turns: AtomicI64,
    peak_active_turns: AtomicI64,
}
impl CountingLog {
    fn new(inner: Arc<dyn EventLog>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            calls: AtomicU64::new(0),
            events: AtomicU64::new(0),
            updates: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            call_micros: Mutex::new(Vec::new()),
            max_batch: AtomicUsize::new(0),
            committed_at: Mutex::new(HashMap::new()),
            inflight: AtomicI64::new(0),
            peak_inflight: AtomicI64::new(0),
            active_turns: AtomicI64::new(0),
            peak_active_turns: AtomicI64::new(0),
        })
    }
}
#[async_trait]
impl EventLog for CountingLog {
    async fn append_batch(&self, events: Vec<Event>) -> LogResult<Vec<LoggedEvent>> {
        let count = events.len();
        let now_inflight = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_inflight
            .fetch_max(now_inflight, Ordering::Relaxed);
        let start = Instant::now();
        let result = self.inner.append_batch(events).await;
        let done = Instant::now();
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.max_batch.fetch_max(count, Ordering::Relaxed);
        self.call_micros
            .lock()
            .unwrap()
            .push((done - start).as_micros() as u64);
        match &result {
            Ok(records) => {
                self.events
                    .fetch_add(records.len() as u64, Ordering::Relaxed);
                let mut times = self.committed_at.lock().unwrap();
                for record in records {
                    if let EventKind::Update(envelope) = &record.event.kind {
                        self.updates.fetch_add(1, Ordering::Relaxed);
                        times.insert((envelope.session.clone(), envelope.seq), (start, done));
                        match envelope.update {
                            SessionUpdate::TurnStarted { .. } => {
                                let now = self.active_turns.fetch_add(1, Ordering::Relaxed) + 1;
                                self.peak_active_turns.fetch_max(now, Ordering::Relaxed);
                            }
                            SessionUpdate::TurnEnded { .. } => {
                                self.active_turns.fetch_sub(1, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(_) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }
    async fn read_after(
        &self,
        session: &loop_protocol::SessionKey,
        after_seq: u64,
    ) -> LogResult<Vec<LoggedEvent>> {
        self.inner.read_after(session, after_seq).await
    }
    async fn read_runtime_after(&self, global_seq: u64) -> LogResult<Vec<LoggedEvent>> {
        self.inner.read_runtime_after(global_seq).await
    }
    async fn latest_seq(&self, session: &loop_protocol::SessionKey) -> LogResult<u64> {
        self.inner.latest_seq(session).await
    }
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> LogResult<u64> {
        self.inner.allocate_generation(owner, pid, started_ms).await
    }
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> LogResult<()> {
        self.inner.finish_generation(generation, stopped_ms).await
    }
    async fn recovery_records(&self, from_generation: u64) -> LogResult<Vec<LoggedEvent>> {
        self.inner.recovery_records(from_generation).await
    }
    async fn last_recovered_generation(&self) -> LogResult<u64> {
        self.inner.last_recovered_generation().await
    }
    async fn mark_recovered(&self, generation: u64, recovered_ms: i64) -> LogResult<()> {
        self.inner.mark_recovered(generation, recovered_ms).await
    }
    async fn subagent_sessions(&self) -> LogResult<HashSet<loop_protocol::SessionKey>> {
        self.inner.subagent_sessions().await
    }
    async fn prompts_sent(&self, session: &loop_protocol::SessionKey) -> LogResult<u64> {
        self.inner.prompts_sent(session).await
    }
    async fn turn_ids(
        &self,
        session: &loop_protocol::SessionKey,
    ) -> LogResult<Vec<loop_protocol::TurnId>> {
        self.inner.turn_ids(session).await
    }
    async fn last_activity_ms(&self) -> LogResult<HashMap<loop_protocol::SessionKey, i64>> {
        self.inner.last_activity_ms().await
    }
    fn capabilities(&self) -> loop_store::StoreCapabilities {
        self.inner.capabilities()
    }
    async fn read_session_page(
        &self,
        session: &loop_protocol::SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: loop_runtime::log::PageLimit,
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<LoggedEvent>> {
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
    ) -> loop_runtime::log::Result<loop_runtime::log::Page<LoggedEvent>> {
        self.inner
            .recovery_page(from_generation, after_global, limit)
            .await
    }
}

// ------------------------------------------------------------- the model

fn faux_model() -> Model {
    Model {
        id: "faux-model".into(),
        name: "Faux Model".into(),
        api: loop_ai::API_OPENAI_COMPLETIONS.to_string(),
        provider: "faux".into(),
        base_url: "http://faux.local".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

/// A model call that streams `chunks` text deltas of `bytes` bytes each.
fn streaming_responder(chunks: usize, bytes: usize) -> FauxResponder {
    let model = faux_model();
    Arc::new(move |_context| {
        let model = model.clone();
        Box::pin(async move {
            let delta = "x".repeat(bytes);
            let mut partial = AssistantMessage::pending(&model);
            let mut events = vec![AssistantMessageEvent::Start {
                partial: partial.clone(),
            }];
            partial.content.push(AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }));
            events.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            });
            for _ in 0..chunks {
                if let AssistantContent::Text(t) = &mut partial.content[0] {
                    t.text.push_str(&delta);
                }
                events.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.clone(),
                    partial: partial.clone(),
                });
            }
            let full = match &partial.content[0] {
                AssistantContent::Text(t) => t.text.clone(),
                _ => String::new(),
            };
            events.push(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: full,
                partial: partial.clone(),
            });
            partial.usage.output = 1;
            partial.usage.total_tokens = 1;
            partial.stop_reason = AiStopReason::Stop;
            events.push(AssistantMessageEvent::Done {
                reason: AiStopReason::Stop,
                message: partial,
            });
            FauxResponse::Events(events)
        })
    })
}

fn make_opts(cwd: &Path) -> BootstrapOpts {
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
        permission_mode: Some("full-access".into()),
        agent: None,
    }
}

// ------------------------------------------------------------- statistics

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}
fn summary(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    json!({
        "count": values.len(),
        "p50": percentile(&values, 0.50),
        "p95": percentile(&values, 0.95),
        "p99": percentile(&values, 0.99),
        "max": values.last().copied().unwrap_or(0),
    })
}

fn short(text: impl std::fmt::Debug) -> String {
    let text = format!("{text:?}");
    text.chars().take(110).collect()
}

// ------------------------------------------------------------ subscribers

#[derive(Default)]
struct SubscriberReport {
    received: u64,
    skipped: u64,
    lagged: bool,
    delays_us: Vec<u64>,
    end_to_end_us: Vec<u64>,
}

async fn subscriber(
    mut receiver: tokio::sync::broadcast::Receiver<UpdateEnvelope>,
    log: Arc<CountingLog>,
    mut stop: watch::Receiver<bool>,
    slow: Option<Duration>,
) -> SubscriberReport {
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    let mut report = SubscriberReport::default();
    let note = |report: &mut SubscriberReport, envelope: &UpdateEnvelope| {
        report.received += 1;
        let key = (envelope.session.clone(), envelope.seq);
        let at = log.committed_at.lock().unwrap().get(&key).copied();
        if let Some((start, done)) = at {
            let now = Instant::now();
            report.delays_us.push((now - done).as_micros() as u64);
            report.end_to_end_us.push((now - start).as_micros() as u64);
        }
    };
    loop {
        tokio::select! {
            received = receiver.recv() => match received {
                Ok(envelope) => {
                    note(&mut report, &envelope);
                    if let Some(slow) = slow {
                        tokio::time::sleep(slow).await;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    report.lagged = true;
                    report.skipped += n;
                }
                Err(RecvError::Closed) => break,
            },
            _ = stop.changed() => break,
        }
    }
    // Drain what is already queued.
    loop {
        match receiver.try_recv() {
            Ok(envelope) => note(&mut report, &envelope),
            Err(TryRecvError::Lagged(n)) => {
                report.lagged = true;
                report.skipped += n;
            }
            Err(_) => break,
        }
    }
    report
}

// ------------------------------------------------------------------- test

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load baseline; run by hand, see the module comment"]
async fn load_baseline() {
    let sessions = env_u64("LOAD_SESSIONS", 32) as usize;
    let active = env_u64("LOAD_ACTIVE", 16) as usize;
    let turns = env_u64("LOAD_TURNS", 2) as usize;
    let chunks = env_u64("LOAD_CHUNKS", 100) as usize;
    let chunk_bytes = env_u64("LOAD_CHUNK_BYTES", 16) as usize;
    let fast = env_u64("LOAD_SUBSCRIBERS", 4) as usize;
    let slow_count = env_u64("LOAD_SLOW_SUBSCRIBERS", 0) as usize;
    let slow_ms = env_u64("LOAD_SLOW_MS", 5);
    let defaults = RuntimeLimits::default();
    let limits = RuntimeLimits {
        max_sessions: env_u64("LOAD_MAX_SESSIONS", defaults.max_sessions as u64) as usize,
        max_active_turns: env_u64("LOAD_MAX_ACTIVE_TURNS", defaults.max_active_turns as u64)
            as usize,
    };
    let store = env_str("LOAD_STORE", "sqlite");
    let close_after = env_u64("LOAD_CLOSE", 0) == 1;
    let deadline = Duration::from_secs(env_u64("LOAD_DEADLINE_SECS", 100));

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::create_dir_all(dir.path().join("work")).unwrap();
    std::fs::write(
        dir.path().join("agent/settings.json"),
        r#"{"sandbox":{"mode":"off"}}"#,
    )
    .unwrap();
    let inner: Arc<dyn EventLog> = match store.as_str() {
        "memory" => Arc::new(MemoryEventLog::new()),
        _ => Arc::new(
            SqliteEventLog::open(dir.path().join("agent"))
                .await
                .unwrap(),
        ),
    };
    let log = CountingLog::new(inner);

    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(streaming_responder(chunks, chunk_bytes)));
    let globals = GlobalResources::with_models(dir.path().join("agent"), models).unwrap();
    let rt = {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(
            globals,
            make_opts(&dir.path().join("work")),
            log.clone(),
            store,
            limits,
        )
    }
    .await
    .unwrap()
    .into_shared();

    let rss_before = peak_rss_bytes();
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut subscribers = Vec::new();
    for i in 0..fast + slow_count {
        let slow = (i >= fast).then(|| Duration::from_millis(slow_ms));
        subscribers.push(tokio::spawn(subscriber(
            rt.subscribe(),
            log.clone(),
            stop_rx.clone(),
            slow,
        )));
    }

    // Baseline commits (RuntimeStarted and the like) are not part of the load.
    let calls_before = log.calls.load(Ordering::Relaxed);
    let events_before = log.events.load(Ordering::Relaxed);
    log.call_micros.lock().unwrap().clear();
    log.peak_inflight.store(0, Ordering::Relaxed);

    let next = Arc::new(AtomicUsize::new(0));
    let timed_out = Arc::new(AtomicBool::new(false));
    let opened = Arc::new(AtomicUsize::new(0));
    let open_errors: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
    let outcomes: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
    let turn_micros: Arc<Mutex<Vec<u64>>> = Arc::default();

    let started = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..active {
        let rt = rt.clone();
        let next = next.clone();
        let opened = opened.clone();
        let open_errors = open_errors.clone();
        let outcomes = outcomes.clone();
        let turn_micros = turn_micros.clone();
        workers.push(tokio::spawn(async move {
            loop {
                if next.fetch_add(1, Ordering::Relaxed) >= sessions {
                    return;
                }
                let session = match timeout(deadline, rt.new_session(SessionSpec::default())).await
                {
                    Ok(Ok(session)) => {
                        opened.fetch_add(1, Ordering::Relaxed);
                        session
                    }
                    Ok(Err(error)) => {
                        *open_errors.lock().unwrap().entry(short(error)).or_default() += 1;
                        continue;
                    }
                    Err(_) => {
                        *open_errors
                            .lock()
                            .unwrap()
                            .entry("new_session timed out".into())
                            .or_default() += 1;
                        continue;
                    }
                };
                for _ in 0..turns {
                    let turn_start = Instant::now();
                    let label = match timeout(
                        deadline,
                        rt.prompt(&session, vec![ContentBlock::text("go")]),
                    )
                    .await
                    {
                        Ok(Ok(handle)) => match timeout(deadline, handle.outcome()).await {
                            Ok(TurnOutcome::Completed(_)) => "completed".to_string(),
                            Ok(TurnOutcome::Cancelled) => "cancelled".into(),
                            Ok(TurnOutcome::Uncertain) => "uncertain".into(),
                            Ok(TurnOutcome::Failed(info)) => format!("failed: {}", short(info)),
                            Err(_) => "outcome timed out".into(),
                        },
                        Ok(Err(error)) => format!("prompt refused: {}", short(error)),
                        Err(_) => "prompt call timed out".into(),
                    };
                    turn_micros
                        .lock()
                        .unwrap()
                        .push(turn_start.elapsed().as_micros() as u64);
                    let ok = label == "completed";
                    *outcomes.lock().unwrap().entry(label).or_default() += 1;
                    if !ok {
                        break;
                    }
                }
                if close_after {
                    let _ = timeout(deadline, rt.close(&session.key, false)).await;
                }
            }
        }));
    }
    let all_done = async {
        for worker in workers {
            let _ = worker.await;
        }
    };
    if timeout(deadline, all_done).await.is_err() {
        timed_out.store(true, Ordering::Relaxed);
    }
    let wall = started.elapsed();

    // Let the last updates reach the subscribers, then stop them.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = stop_tx.send(true);
    let mut delays = Vec::new();
    let mut end_to_end = Vec::new();
    let mut lagged_receivers = 0;
    let mut skipped_total = 0;
    let mut received_total = 0;
    let mut per_subscriber = Vec::new();
    for subscriber in subscribers {
        let report = subscriber.await.unwrap();
        lagged_receivers += report.lagged as u64;
        skipped_total += report.skipped;
        received_total += report.received;
        per_subscriber.push(json!({
            "received": report.received,
            "skipped": report.skipped,
            "lagged": report.lagged,
        }));
        delays.extend(report.delays_us);
        end_to_end.extend(report.end_to_end_us);
    }
    let peak_rss = peak_rss_bytes();
    let calls = log.calls.load(Ordering::Relaxed) - calls_before;
    let events = log.events.load(Ordering::Relaxed) - events_before;
    let updates = log.updates.load(Ordering::Relaxed);
    let secs = wall.as_secs_f64().max(f64::EPSILON);
    let turns_done = turn_micros.lock().unwrap().len();
    let result = json!({
        "config": {
            "sessions": sessions, "active_clients": active, "turns_per_session": turns,
            "chunks": chunks, "chunk_bytes": chunk_bytes,
            "fast_subscribers": fast, "slow_subscribers": slow_count, "slow_ms": slow_ms,
            "max_sessions": limits.max_sessions, "max_active_turns": limits.max_active_turns,
            "store": store, "close_after": close_after,
        },
        "timed_out": timed_out.load(Ordering::Relaxed),
        "wall_secs": secs,
        "sessions_opened": opened.load(Ordering::Relaxed),
        "open_errors": *open_errors.lock().unwrap(),
        "turn_outcomes": *outcomes.lock().unwrap(),
        "turns_run": turns_done,
        "commits": {
            "append_batch_calls": calls,
            "events": events,
            "updates": updates,
            "failed_calls": log.failures.load(Ordering::Relaxed),
            "calls_per_sec": calls as f64 / secs,
            "events_per_sec": events as f64 / secs,
            "max_batch": log.max_batch.load(Ordering::Relaxed),
            "updates_per_turn": if turns_done > 0 { updates as f64 / turns_done as f64 } else { 0.0 },
            "call_latency_us": summary(log.call_micros.lock().unwrap().clone()),
        },
        "delivery_delay_us": summary(delays),
        "end_to_end_delay_us": summary(end_to_end),
        "observed": {
            "peak_active_turns": log.peak_active_turns.load(Ordering::Relaxed),
            "peak_append_calls_in_flight": log.peak_inflight.load(Ordering::Relaxed),
        },
        "turn_latency_us": summary(turn_micros.lock().unwrap().clone()),
        "subscribers": {
            "lagged_receivers": lagged_receivers,
            "skipped_updates": skipped_total,
            "received_updates": received_total,
            "each": per_subscriber,
        },
        "rss_bytes": { "peak": peak_rss, "before_run": rss_before },
    });
    println!("LOAD|RESULT|{result}");
    println!(
        "LOAD|wall {:.2}s, {} calls ({:.0}/s), {} events ({:.0}/s), lagged receivers {}, peak RSS {} MiB",
        secs,
        calls,
        calls as f64 / secs,
        events,
        events as f64 / secs,
        lagged_receivers,
        peak_rss / (1024 * 1024)
    );

    rt.shutdown().await;
}
