//! Probe E7, "Turn queue" (H3: scheduling liveness).
//!
//! Each schedule builds one in-process Runtime over the `faux` provider and
//! drives it with several concurrent clients. A client runs a seeded list of
//! operations: open a Session, send a prompt, cancel, close (plain or forced),
//! load a closed Session, attach or detach a watching connection, and pause.
//! There are more Sessions than `max_sessions` and more prompts than
//! `max_active_turns`, so prompts queue and idle Sessions are unloaded. The
//! faux model waits a random time per call and may call a tool (`ls`, `bash`)
//! or the `subagent` tool (nested children, parked parents). The Event Log is a
//! wrapper that delays every append at random ("slow event-log writes").
//!
//! The Runtime has no steering or follow-up API (`capabilities().steering` is
//! false). The nearest operation is a prompt on a Session that already has a
//! turn: the Runtime refuses it with `Busy`. A share of prompts aims at busy
//! Sessions for this.
//!
//! Per schedule the probe checks: every prompt call and every outcome ends
//! before a deadline; after quiescence the Runtime shows no busy Session and
//! all permits are free (a gated probe: exactly `max_active_turns` turns start
//! and the next one waits); active turns never exceed the limit; loaded
//! Sessions never exceed the limit; every `PromptSent` has exactly one
//! terminal record (`turn_ended` or `prompt_settled`) and no prompt that the
//! Runtime refused leaves a `PromptSent`; no panic.
//!
//! The full run is ignored by default:
//!
//! ```text
//! E7_SCHEDULES=1000 E7_SEED=20261111 E7_PARALLEL=4 cargo test -p loop-runtime \
//!     --test e7_turn_queue -- --ignored --nocapture --test-threads=4 e7_turn_queue
//! ```
//!
//! `E7_ONLY=<index>` runs one schedule and prints it. `E7_LIMITS=default`
//! uses 16 Sessions and 8 active turns for every schedule (the default is a
//! mix with smaller limits). `E7_BUDGET_SECS` stops starting new schedules
//! after that many seconds. `E7_ROOT` names the folder for schedule
//! directories. The schedule is a pure function of the seed and the index, but
//! thread timing is not, so a replay may take another interleaving.
#![cfg(unix)]

use async_trait::async_trait;
use loop_ai::{
    providers::{faux_provider_fn, FauxResponder, FauxResponse},
    Context, Message, ToolCall,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, SessionRef, SessionSpec, SessionUpdate, StopReason,
    TurnOutcome,
};
use loop_runtime::{
    log::{Event, EventKind, EventLog, LoggedEvent, MemoryEventLog, Result as LogResult},
    Runtime, RuntimeLimits,
};
use loop_wire::RuntimeControl;
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle, time::timeout};

/// Longest wait for one prompt call, one outcome or one admin operation.
const DEADLINE: Duration = Duration::from_secs(30);
/// Longest wait for one whole schedule.
const SCHEDULE_DEADLINE: Duration = Duration::from_secs(240);

// ---------------------------------------------------------------- utilities

/// SplitMix64: small, seedable and good enough for schedules.
#[derive(Clone)]
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}
fn env_u64(name: &str, default: u64) -> u64 {
    env(name).parse().unwrap_or(default)
}

fn say(line: String) {
    println!("E7|{line}");
}

static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let text = format!(
            "{} at {}",
            info.payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_default(),
            info.location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default()
        );
        eprintln!("E7|PANIC|{text}");
        PANICS.lock().unwrap().push(text);
    }));
}
fn panic_count() -> usize {
    PANICS.lock().unwrap().len()
}

// ---------------------------------------------------------------- the model

/// State the faux model shares with the schedule.
struct Shared {
    next_call: AtomicU64,
    /// Model calls in flight: call number to Session number.
    inflight: Mutex<HashMap<u64, usize>>,
    /// Sessions that were cancelled or closed. Their model calls may outlive
    /// the turn (the faux stream is not cancelled), so they are not counted.
    tainted: Mutex<HashSet<usize>>,
    /// Most untainted model calls in flight at one time.
    peak: AtomicUsize,
    /// Most model calls of any kind in flight since the last reset.
    window_peak: AtomicUsize,
    /// Holds the probe turns (mode `gate`).
    gate: watch::Sender<bool>,
    /// Holds one turn open in the negative control (mode `hold`).
    hold: watch::Sender<bool>,
    model_calls: AtomicU64,
    /// Gated probe calls waiting on the gate.
    gate_calls: AtomicUsize,
}
impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_call: AtomicU64::new(0),
            inflight: Mutex::default(),
            tainted: Mutex::default(),
            peak: AtomicUsize::new(0),
            window_peak: AtomicUsize::new(0),
            gate: watch::channel(false).0,
            hold: watch::channel(false).0,
            model_calls: AtomicU64::new(0),
            gate_calls: AtomicUsize::new(0),
        })
    }
    fn taint(&self, session: usize) {
        self.tainted.lock().unwrap().insert(session);
    }
    fn enter(&self, session: usize) -> u64 {
        let call = self.next_call.fetch_add(1, Ordering::Relaxed);
        self.model_calls.fetch_add(1, Ordering::Relaxed);
        let mut map = self.inflight.lock().unwrap();
        map.insert(call, session);
        let tainted = self.tainted.lock().unwrap();
        let live = map.values().filter(|s| !tainted.contains(*s)).count();
        self.peak.fetch_max(live, Ordering::Relaxed);
        self.window_peak.fetch_max(map.len(), Ordering::Relaxed);
        call
    }
    fn leave(&self, call: u64) {
        self.inflight.lock().unwrap().remove(&call);
    }
    fn inflight_now(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }
}

/// `E7[<id>;<mode>;<delay ms>;<kids>]`. The id is `<schedule>-<session>-<n>`,
/// with `.<k>` appended for each Subagent level.
struct Spec {
    id: String,
    mode: String,
    delay: u64,
    kids: u64,
}
impl Spec {
    fn parse(text: &str) -> Option<Self> {
        let start = text.find("E7[")? + 3;
        let end = start + text[start..].find(']')?;
        let mut parts = text[start..end].split(';');
        Some(Self {
            id: parts.next()?.to_owned(),
            mode: parts.next()?.to_owned(),
            delay: parts.next()?.parse().ok()?,
            kids: parts.next()?.parse().ok()?,
        })
    }
    fn session(&self) -> usize {
        self.id
            .split('-')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(usize::MAX)
    }
}

fn first_spec(context: &Context) -> Option<Spec> {
    for message in &context.messages {
        if let Message::User(_) = message {
            let text = serde_json::to_string(message).unwrap_or_default();
            if let Some(spec) = Spec::parse(&text) {
                return Some(spec);
            }
        }
    }
    None
}

fn tool_calls(id: &str, list: Vec<(&str, serde_json::Value)>) -> FauxResponse {
    FauxResponse::ToolCalls(
        list.into_iter()
            .enumerate()
            .map(|(k, (name, arguments))| ToolCall {
                id: format!("call-{id}-{k}"),
                name: name.into(),
                arguments,
                thought_signature: None,
            })
            .collect(),
    )
}

fn responder(shared: Arc<Shared>) -> FauxResponder {
    Arc::new(move |context: &Context| {
        let shared = shared.clone();
        let spec = first_spec(context);
        let calls = context
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();
        Box::pin(async move {
            let Some(spec) = spec else {
                return FauxResponse::Text("no spec".into());
            };
            let call = shared.enter(spec.session());
            match spec.mode.as_str() {
                "gate" => {
                    shared.gate_calls.fetch_add(1, Ordering::SeqCst);
                    let mut rx = shared.gate.subscribe();
                    let _ = rx.wait_for(|open| *open).await;
                    shared.gate_calls.fetch_sub(1, Ordering::SeqCst);
                }
                "hold" => {
                    let mut rx = shared.hold.subscribe();
                    let _ = rx.wait_for(|open| *open).await;
                }
                _ => tokio::time::sleep(Duration::from_millis(spec.delay)).await,
            }
            shared.leave(call);
            if calls > 0 {
                return FauxResponse::Text(format!("done {}", spec.id));
            }
            let id = &spec.id;
            match spec.mode.as_str() {
                "err" => FauxResponse::Error("e7 injected model error".into()),
                "ls" => tool_calls(id, vec![("ls", json!({"path": "."}))]),
                "bash" => tool_calls(id, vec![("bash", json!({"command": "sleep 0.05"}))]),
                "sub" | "nest" => {
                    let child = if spec.mode == "sub" { "text" } else { "sub" };
                    let list = (0..spec.kids.max(1))
                        .map(|k| {
                            let mode = if child == "text" && k % 2 == 1 {
                                "ls"
                            } else {
                                child
                            };
                            let task = format!("E7[{id}.{k};{mode};{};1]", spec.delay);
                            ("subagent", json!({ "task": task }))
                        })
                        .collect();
                    tool_calls(id, list)
                }
                _ => FauxResponse::Text(format!("done {id}")),
            }
        })
    })
}

// ------------------------------------------------------------------ the log

/// The Memory log with a random delay on every append, and a count of open
/// `turn_started` .. `turn_ended` windows seen at append entry. The window of
/// a turn lies inside the time its turn holds a permit, so the count never
/// exceeds the limit, except while a parent waits for a Subagent.
struct SlowLog {
    inner: MemoryEventLog,
    max_ms: u64,
    seed: u64,
    n: AtomicU64,
    open: AtomicI64,
    window_peak: AtomicI64,
}
#[async_trait]
impl EventLog for SlowLog {
    async fn append_batch(&self, events: Vec<Event>) -> LogResult<Vec<LoggedEvent>> {
        for event in &events {
            if let EventKind::Update(envelope) = &event.kind {
                match envelope.update {
                    SessionUpdate::TurnStarted { .. } => {
                        let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
                        self.window_peak.fetch_max(now, Ordering::SeqCst);
                    }
                    SessionUpdate::TurnEnded { .. } => {
                        self.open.fetch_sub(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        }
        if self.max_ms > 0 {
            let n = self.n.fetch_add(1, Ordering::Relaxed);
            let delay =
                Rng(self.seed ^ n.wrapping_mul(0xD6E8_FEB8_6659_FD93)).below(self.max_ms + 1);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.inner.append_batch(events).await
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

// ------------------------------------------------------------- the schedule

#[derive(Clone, Debug)]
enum Op {
    NewSession,
    Prompt {
        pick: u64,
        mode: &'static str,
        delay: u64,
        kids: u64,
        cancel_after: Option<u64>,
        busy_bias: bool,
    },
    Cancel {
        pick: u64,
    },
    Close {
        pick: u64,
        force: bool,
    },
    Load {
        pick: u64,
    },
    Attach {
        pick: u64,
    },
    Detach,
    Pause(u64),
}

#[derive(Clone, Debug)]
struct Schedule {
    index: u64,
    seed: u64,
    max_sessions: usize,
    max_active: usize,
    log_delay_ms: u64,
    want_sessions: usize,
    shutdown_in_flight: bool,
    has_subagents: bool,
    clients: Vec<Vec<Op>>,
}

const LIMIT_MIX: [(usize, usize); 6] = [(2, 1), (3, 2), (4, 2), (5, 3), (8, 4), (16, 8)];

fn schedule(seed: u64, index: u64, only_default: bool) -> Schedule {
    let mut rng = Rng(seed ^ (index + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let (max_sessions, max_active) = if only_default || index.is_multiple_of(2) {
        (16, 8)
    } else {
        LIMIT_MIX[rng.below(LIMIT_MIX.len() as u64) as usize]
    };
    let log_delay_ms = [0, 2, 10, 30][rng.below(4) as usize];
    let want_sessions = (max_sessions + 1 + rng.below(max_sessions as u64) as usize).min(40);
    let subs = rng.chance(40);
    let clients_n = rng.range(3, 10);
    let mut has_subagents = false;
    let mut clients = Vec::new();
    for _ in 0..clients_n {
        let mut ops: Vec<Op> = (0..rng.range(1, 3)).map(|_| Op::NewSession).collect();
        for _ in 0..rng.range(15, 45) {
            let roll = rng.below(100);
            let op = match roll {
                0..=9 => Op::NewSession,
                10..=62 => {
                    let mode = match rng.below(100) {
                        0..=34 => "text",
                        35..=54 => "ls",
                        55..=62 => "bash",
                        63..=66 => "err",
                        67..=84 if subs => "sub",
                        67..=84 => "text",
                        _ if subs => "nest",
                        _ => "ls",
                    };
                    has_subagents |= mode == "sub" || mode == "nest";
                    Op::Prompt {
                        pick: rng.next(),
                        mode,
                        delay: if rng.chance(10) {
                            rng.range(100, 200)
                        } else {
                            rng.below(60)
                        },
                        kids: rng.range(1, 3),
                        cancel_after: rng.chance(25).then(|| rng.below(90)),
                        busy_bias: rng.chance(10),
                    }
                }
                63..=66 => Op::Cancel { pick: rng.next() },
                67..=71 => Op::Close {
                    pick: rng.next(),
                    force: rng.chance(60),
                },
                72..=77 => Op::Load { pick: rng.next() },
                78..=85 => Op::Attach { pick: rng.next() },
                86..=90 => Op::Detach,
                _ => Op::Pause(rng.below(15)),
            };
            ops.push(op);
            if rng.chance(50) {
                ops.push(Op::Pause(rng.below(6)));
            }
        }
        clients.push(ops);
    }
    Schedule {
        index,
        seed,
        max_sessions,
        max_active,
        log_delay_ms,
        want_sessions,
        shutdown_in_flight: rng.chance(15),
        has_subagents,
        clients,
    }
}

// ------------------------------------------------------------ the execution

#[derive(Clone, Debug)]
enum Kind {
    Completed(StopReason),
    Cancelled,
    Failed(String),
    Uncertain,
    Hang,
}

#[derive(Debug)]
struct PromptRec {
    id: String,
    key: String,
    mode: &'static str,
    acked: bool,
    refused: Option<String>,
    outcome: Option<Kind>,
}

struct Sess {
    no: usize,
    sref: SessionRef,
}

struct Watching;
#[async_trait]
impl loop_wire::control::EscalationRecipient for Watching {
    fn caller(&self) -> Option<loop_protocol::Caller> {
        None
    }

    fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
    async fn interact(
        &self,
        _: loop_protocol::rpc::InteractParams,
        withdrawn: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<loop_protocol::InteractionOutcome, ()> {
        let _ = withdrawn.await;
        Err(())
    }
}

struct Ctx {
    sched: Schedule,
    rt: Arc<Runtime>,
    shared: Arc<Shared>,
    registry: Mutex<Vec<Arc<Sess>>>,
    /// Acknowledged prompts not yet ended, per Session number.
    outstanding_by: Mutex<HashMap<usize, usize>>,
    recs: Mutex<Vec<PromptRec>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    violations: Mutex<Vec<String>>,
    counters: Mutex<BTreeMap<&'static str, u64>>,
    cancel_ms: Mutex<Vec<u64>>,
    next_prompt: AtomicU64,
    outstanding: AtomicI64,
    outstanding_max: AtomicI64,
    loaded_max: AtomicU64,
    started: Instant,
}
impl Ctx {
    fn bump(&self, name: &'static str) {
        *self.counters.lock().unwrap().entry(name).or_default() += 1;
    }
    fn violate(&self, text: String) {
        self.violations.lock().unwrap().push(text);
    }
    fn sample_loaded(&self) {
        let status = self.rt.status();
        self.loaded_max.fetch_max(status.loaded, Ordering::Relaxed);
        if status.loaded > self.sched.max_sessions as u64 {
            self.violate(format!(
                "LOADED_OVER_LIMIT loaded={} limit={}",
                status.loaded, self.sched.max_sessions
            ));
        }
    }
    fn pick(&self, pick: u64, busy_bias: bool) -> Option<Arc<Sess>> {
        let registry = self.registry.lock().unwrap();
        if registry.is_empty() {
            return None;
        }
        if busy_bias {
            let outstanding = self.outstanding_by.lock().unwrap();
            let busy: Vec<_> = registry
                .iter()
                .filter(|s| outstanding.get(&s.no).copied().unwrap_or(0) > 0)
                .collect();
            if !busy.is_empty() {
                return Some(busy[(pick % busy.len() as u64) as usize].clone());
            }
        }
        Some(registry[(pick % registry.len() as u64) as usize].clone())
    }
}

/// Errors a refusal may carry without being a finding.
fn expected_refusal(error: &BackendError) -> bool {
    match error {
        BackendError::Busy | BackendError::SessionNotFound(_) => true,
        BackendError::Failed(info) => {
            let text = format!("{info:?}");
            text.contains("Session closing") || text.contains("Runtime stopped")
        }
        _ => false,
    }
}

async fn new_session(ctx: &Arc<Ctx>) {
    if ctx.registry.lock().unwrap().len() >= ctx.sched.want_sessions {
        return;
    }
    match timeout(DEADLINE, ctx.rt.new_session(SessionSpec::default())).await {
        Err(_) => ctx.violate("HANG new_session".into()),
        Ok(Ok(sref)) => {
            ctx.bump("new_session_ok");
            let mut registry = ctx.registry.lock().unwrap();
            let no = registry.len();
            registry.push(Arc::new(Sess { no, sref }));
        }
        Ok(Err(BackendError::Busy)) => ctx.bump("new_session_busy_at_limit"),
        Ok(Err(error)) if expected_refusal(&error) => ctx.bump("new_session_refused"),
        Ok(Err(error)) => ctx.violate(format!("NEW_SESSION_ERROR {error:?}")),
    }
    ctx.sample_loaded();
}

#[allow(clippy::too_many_arguments)]
async fn prompt(
    ctx: &Arc<Ctx>,
    pick: u64,
    mode: &'static str,
    delay: u64,
    kids: u64,
    cancel_after: Option<u64>,
    busy_bias: bool,
) {
    let Some(sess) = ctx.pick(pick, busy_bias) else {
        return;
    };
    let n = ctx.next_prompt.fetch_add(1, Ordering::Relaxed);
    let id = format!("{}-{}-{n}", ctx.sched.index, sess.no);
    let spec = format!("E7[{id};{mode};{delay};{kids}]");
    ctx.bump("prompt_calls");
    let index = {
        let mut recs = ctx.recs.lock().unwrap();
        recs.push(PromptRec {
            id: id.clone(),
            key: sess.sref.key.0.clone(),
            mode,
            acked: false,
            refused: None,
            outcome: None,
        });
        recs.len() - 1
    };
    let result = timeout(
        DEADLINE,
        ctx.rt.prompt(&sess.sref, vec![ContentBlock::text(spec)]),
    )
    .await;
    let handle = match result {
        Err(_) => {
            ctx.violate(format!("HANG prompt call {id}"));
            ctx.recs.lock().unwrap()[index].refused = Some("hang".into());
            return;
        }
        Ok(Err(error)) => {
            let busy = matches!(error, BackendError::Busy);
            ctx.bump(if busy {
                "refused_busy"
            } else {
                "refused_other"
            });
            if !expected_refusal(&error) {
                ctx.violate(format!("PROMPT_ERROR {id} {error:?}"));
            }
            ctx.recs.lock().unwrap()[index].refused = Some(format!("{error:?}"));
            return;
        }
        Ok(Ok(handle)) => handle,
    };
    ctx.recs.lock().unwrap()[index].acked = true;
    ctx.bump("acked");
    *ctx.outstanding_by
        .lock()
        .unwrap()
        .entry(sess.no)
        .or_default() += 1;
    let now = ctx.outstanding.fetch_add(1, Ordering::SeqCst) + 1;
    ctx.outstanding_max.fetch_max(now, Ordering::SeqCst);
    let cancelled_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    if let Some(after) = cancel_after {
        let ctx = ctx.clone();
        let sess = sess.clone();
        let cancelled_at = cancelled_at.clone();
        ctx.clone()
            .tasks
            .lock()
            .unwrap()
            .push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(after)).await;
                ctx.shared.taint(sess.no);
                *cancelled_at.lock().unwrap() = Some(Instant::now());
                ctx.rt.cancel(&sess.sref);
                ctx.bump("cancel_after_ack");
            }));
    }
    let task = {
        let ctx = ctx.clone();
        let sess = sess.clone();
        tokio::spawn(async move {
            let kind = match timeout(DEADLINE, handle.outcome()).await {
                Err(_) => Kind::Hang,
                Ok(TurnOutcome::Completed(stop)) => Kind::Completed(stop),
                Ok(TurnOutcome::Cancelled) => Kind::Cancelled,
                Ok(TurnOutcome::Failed(info)) => Kind::Failed(format!("{info:?}")),
                Ok(TurnOutcome::Uncertain) => Kind::Uncertain,
            };
            if let (Some(at), Kind::Cancelled) = (*cancelled_at.lock().unwrap(), &kind) {
                ctx.cancel_ms
                    .lock()
                    .unwrap()
                    .push(at.elapsed().as_millis() as u64);
            }
            match &kind {
                Kind::Hang => ctx.violate(format!("HANG outcome {id} (session {})", sess.no)),
                Kind::Uncertain => ctx.violate(format!("UNCERTAIN_OUTCOME {id}")),
                Kind::Failed(info)
                    if ctx.sched.shutdown_in_flight && info.contains("semaphore closed") =>
                {
                    // Shutdown closes the permit queue first: a queued prompt fails.
                    ctx.bump("failed_at_shutdown");
                }
                Kind::Failed(info) if mode != "err" => {
                    ctx.violate(format!("UNEXPECTED_FAILED {id} {info}"))
                }
                _ => {}
            }
            ctx.bump(match &kind {
                Kind::Completed(_) => "outcome_completed",
                Kind::Cancelled => "outcome_cancelled",
                Kind::Failed(_) => "outcome_failed",
                Kind::Uncertain => "outcome_uncertain",
                Kind::Hang => "outcome_hang",
            });
            ctx.recs.lock().unwrap()[index].outcome = Some(kind);
            if let Some(count) = ctx.outstanding_by.lock().unwrap().get_mut(&sess.no) {
                *count -= 1;
            }
            ctx.outstanding.fetch_sub(1, Ordering::SeqCst);
        })
    };
    ctx.tasks.lock().unwrap().push(task);
}

async fn run_op(ctx: &Arc<Ctx>, client: u64, op: Op) {
    match op {
        Op::NewSession => new_session(ctx).await,
        Op::Prompt {
            pick,
            mode,
            delay,
            kids,
            cancel_after,
            busy_bias,
        } => prompt(ctx, pick, mode, delay, kids, cancel_after, busy_bias).await,
        Op::Cancel { pick } => {
            if let Some(sess) = ctx.pick(pick, true) {
                ctx.shared.taint(sess.no);
                ctx.rt.cancel(&sess.sref);
                ctx.bump("cancel_op");
            }
        }
        Op::Close { pick, force } => {
            if let Some(sess) = ctx.pick(pick, false) {
                if force {
                    ctx.shared.taint(sess.no);
                }
                match timeout(DEADLINE, ctx.rt.close(&sess.sref.key, force)).await {
                    Err(_) => ctx.violate(format!("HANG close session {}", sess.no)),
                    Ok(Ok(())) => ctx.bump(if force { "close_force_ok" } else { "close_ok" }),
                    Ok(Err(BackendError::Busy)) => ctx.bump("close_busy"),
                    Ok(Err(BackendError::SessionNotFound(_))) => ctx.bump("close_not_loaded"),
                    Ok(Err(error)) => {
                        ctx.violate(format!("CLOSE_ERROR session {} {error:?}", sess.no))
                    }
                }
                ctx.sample_loaded();
            }
        }
        Op::Load { pick } => {
            if let Some(sess) = ctx.pick(pick, false) {
                match timeout(
                    DEADLINE,
                    ctx.rt.load_session(&sess.sref, SessionSpec::default()),
                )
                .await
                {
                    Err(_) => ctx.violate(format!("HANG load session {}", sess.no)),
                    Ok(Ok(_)) => ctx.bump("load_ok"),
                    Ok(Err(BackendError::Busy)) => ctx.bump("load_busy_at_limit"),
                    Ok(Err(error)) if expected_refusal(&error) => ctx.bump("load_refused"),
                    Ok(Err(error)) => {
                        ctx.violate(format!("LOAD_ERROR session {} {error:?}", sess.no))
                    }
                }
                ctx.sample_loaded();
            }
        }
        Op::Attach { pick } => {
            if let Some(sess) = ctx.pick(pick, false) {
                match timeout(
                    DEADLINE,
                    RuntimeControl::recipient(
                        &*ctx.rt,
                        &sess.sref,
                        client,
                        Some(Arc::new(Watching)),
                    ),
                )
                .await
                {
                    Err(_) => ctx.violate(format!("HANG attach session {}", sess.no)),
                    Ok(Ok(())) => ctx.bump("attach_ok"),
                    Ok(Err(error)) if expected_refusal(&error) => ctx.bump("attach_refused"),
                    Ok(Err(error)) => {
                        ctx.violate(format!("ATTACH_ERROR session {} {error:?}", sess.no))
                    }
                }
            }
        }
        Op::Detach => {
            if timeout(DEADLINE, RuntimeControl::disconnected(&*ctx.rt, client))
                .await
                .is_err()
            {
                ctx.violate("HANG detach".into());
            }
            ctx.bump("detach");
        }
        Op::Pause(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
    }
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

struct Built {
    rt: Arc<Runtime>,
    shared: Arc<Shared>,
    log: Arc<SlowLog>,
}
async fn build(dir: &Path, limits: RuntimeLimits, log_delay_ms: u64, seed: u64) -> Built {
    std::fs::create_dir_all(dir.join("agent")).unwrap();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::write(dir.join("work/file.txt"), "e7").unwrap();
    std::fs::write(
        dir.join("agent/settings.json"),
        r#"{"sandbox":{"mode":"off"}}"#,
    )
    .unwrap();
    let shared = Shared::new();
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(responder(shared.clone())));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let log = Arc::new(SlowLog {
        inner: MemoryEventLog::new(),
        max_ms: log_delay_ms,
        seed,
        n: AtomicU64::new(0),
        open: AtomicI64::new(0),
        window_peak: AtomicI64::new(0),
    });
    let rt = {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(
            globals,
            make_opts(&dir.join("work")),
            log.clone(),
            store,
            limits,
        )
    }
    .await
    .unwrap()
    .into_shared();
    Built { rt, shared, log }
}

/// The permit probe. `active + 1` idle Sessions each get a prompt whose model
/// call waits on the gate. Exactly `active` calls must start, and the next one
/// must wait. With `hold`, one more turn is left open first (the negative
/// control), so the probe must find `active - 1`. Returns (reached, peak).
async fn permit_probe(
    rt: &Arc<Runtime>,
    shared: &Arc<Shared>,
    active: usize,
    hold: bool,
    violations: &mut Vec<String>,
) -> Option<(usize, usize)> {
    let mut sessions = Vec::new();
    for _ in 0..=active {
        match timeout(DEADLINE, rt.new_session(SessionSpec::default())).await {
            Ok(Ok(sref)) => sessions.push(sref),
            other => {
                violations.push(format!(
                    "PROBE_NEW_SESSION_REFUSED at quiescence (idle, unwatched Sessions): {other:?}"
                ));
                return None;
            }
        }
    }
    let _ = shared.gate.send(false);
    let _ = shared.hold.send(false);
    let mut handles = Vec::new();
    let mut take = sessions.iter();
    if hold {
        let sref = take.next().unwrap();
        let spec = "E7[900-9000-0;hold;0;0]";
        handles.push(rt.prompt(sref, vec![ContentBlock::text(spec)]).await);
    }
    // Let the held turn start, then reset the window.
    tokio::time::sleep(Duration::from_millis(100)).await;
    shared
        .window_peak
        .store(shared.inflight_now(), Ordering::Relaxed);
    for (i, sref) in take.enumerate() {
        let spec = format!("E7[900-{}-{i};gate;0;0]", 9001 + i);
        handles.push(rt.prompt(sref, vec![ContentBlock::text(spec)]).await);
    }
    let target = if hold { active - 1 } else { active };
    let until = Instant::now() + Duration::from_secs(15);
    while shared.gate_calls.load(Ordering::SeqCst) < target && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let reached = shared.gate_calls.load(Ordering::SeqCst);
    let peak = shared.window_peak.load(Ordering::Relaxed);
    let _ = shared.gate.send(true);
    let _ = shared.hold.send(true);
    for handle in handles {
        match handle {
            Ok(handle) => match timeout(DEADLINE, handle.outcome()).await {
                Ok(TurnOutcome::Completed(_)) => {}
                other => violations.push(format!("PROBE_TURN_DID_NOT_COMPLETE {other:?}")),
            },
            Err(error) => violations.push(format!("PROBE_PROMPT_ERROR {error:?}")),
        }
    }
    Some((reached, peak))
}

/// Terminal state of one `PromptSent` in the Event Log.
#[derive(Debug, Clone, PartialEq)]
enum Terminal {
    None,
    Settled,
    Ended(StopReason),
}

#[derive(Default)]
struct LogFacts {
    prompt_sent: usize,
    settled: usize,
    ended: usize,
    aborted_closes: usize,
    terminals: HashMap<String, Terminal>,
    sent_ids: HashMap<String, usize>,
    opened: HashSet<String>,
    errors: Vec<String>,
}

fn spec_id(value: &serde_json::Value) -> String {
    let text = value.to_string();
    text.find("E7[")
        .map(|start| {
            let rest = &text[start + 3..];
            rest[..rest.find(';').unwrap_or(rest.len())].to_owned()
        })
        .unwrap_or_else(|| "?".into())
}

/// Per Session: `PromptSent`, then either `turn_started` + `turn_ended` or
/// `prompt_settled`, and nothing else, in `global_seq` order.
fn account_log(records: &[LoggedEvent]) -> LogFacts {
    #[derive(Clone)]
    enum State {
        Idle,
        Sent(String),
        InTurn(String, String),
    }
    let mut facts = LogFacts::default();
    let mut states: HashMap<String, State> = HashMap::new();
    for record in records {
        let Some(key) = record.event.session_key.as_ref().map(|k| k.0.clone()) else {
            continue;
        };
        let state = states.entry(key.clone()).or_insert(State::Idle).clone();
        let next = match &record.event.kind {
            EventKind::PromptSent(value) => {
                facts.prompt_sent += 1;
                let id = spec_id(value);
                *facts.sent_ids.entry(id.clone()).or_default() += 1;
                facts.terminals.insert(id.clone(), Terminal::None);
                if !matches!(state, State::Idle) {
                    facts.errors.push(format!(
                        "prompt_sent {id} on session {key} while an earlier prompt has no terminal record"
                    ));
                }
                State::Sent(id)
            }
            EventKind::PromptSettled(_) => {
                facts.settled += 1;
                match state {
                    State::Sent(id) => {
                        facts.terminals.insert(id, Terminal::Settled);
                    }
                    _ => facts.errors.push(format!(
                        "prompt_settled on session {key} without a pending prompt"
                    )),
                }
                State::Idle
            }
            EventKind::Update(envelope) => match &envelope.update {
                SessionUpdate::TurnStarted { turn_id } => match state {
                    State::Sent(id) => State::InTurn(id, format!("{turn_id:?}")),
                    _ => {
                        facts.errors.push(format!(
                            "turn_started on session {key} without a pending prompt"
                        ));
                        state
                    }
                },
                SessionUpdate::TurnEnded { turn_id, stop, .. } => {
                    facts.ended += 1;
                    match state {
                        State::InTurn(id, turn) => {
                            if turn != format!("{turn_id:?}") {
                                facts.errors.push(format!(
                                    "turn_ended of another turn on session {key} (prompt {id})"
                                ));
                            }
                            facts.terminals.insert(id, Terminal::Ended(*stop));
                        }
                        _ => facts.errors.push(format!(
                            "turn_ended on session {key} without a started turn"
                        )),
                    }
                    State::Idle
                }
                _ => state,
            },
            EventKind::SessionOpened(_) => {
                facts.opened.insert(key.clone());
                state
            }
            EventKind::SessionClosed(value) => {
                if value.get("aborted").and_then(|v| v.as_bool()) == Some(true) {
                    facts.aborted_closes += 1;
                }
                state
            }
            _ => state,
        };
        states.insert(key, next);
    }
    for (key, state) in states {
        if !matches!(state, State::Idle) {
            facts.errors.push(format!(
                "session {key} ends with a prompt that has no terminal record"
            ));
        }
    }
    facts
}

struct Outcome {
    violations: Vec<String>,
    counters: BTreeMap<&'static str, u64>,
    cancel_ms: Vec<u64>,
    loaded_max: u64,
    inflight_peak: usize,
    window_peak: i64,
    outstanding_max: i64,
    probe: Option<(usize, usize)>,
    prompt_sent: usize,
    settled: usize,
    ended: usize,
    aborted_closes: usize,
    model_calls: u64,
    millis: u128,
    dump: String,
}

async fn run_schedule(sched: Schedule, dir: PathBuf) -> Outcome {
    let started = Instant::now();
    let panics_before = panic_count();
    let limits = RuntimeLimits {
        max_sessions: sched.max_sessions,
        max_active_turns: sched.max_active,
    };
    let Built { rt, shared, log } =
        build(&dir, limits, sched.log_delay_ms, sched.seed ^ sched.index).await;
    assert!(
        !rt.capabilities().steering,
        "the Runtime now offers steering: extend the probe"
    );
    let ctx = Arc::new(Ctx {
        sched: sched.clone(),
        rt: rt.clone(),
        shared: shared.clone(),
        registry: Mutex::default(),
        outstanding_by: Mutex::default(),
        recs: Mutex::default(),
        tasks: Mutex::default(),
        violations: Mutex::default(),
        counters: Mutex::default(),
        cancel_ms: Mutex::default(),
        next_prompt: AtomicU64::new(0),
        outstanding: AtomicI64::new(0),
        outstanding_max: AtomicI64::new(0),
        loaded_max: AtomicU64::new(0),
        started,
    });
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let ctx = ctx.clone();
        let sampling = sampling.clone();
        tokio::spawn(async move {
            while sampling.load(Ordering::Relaxed) {
                ctx.sample_loaded();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    let mut probe = None;
    let mut probe_violations = Vec::new();

    let body = async {
        // The action phase.
        let mut clients = Vec::new();
        for (client, ops) in sched.clients.iter().cloned().enumerate() {
            let ctx = ctx.clone();
            clients.push(tokio::spawn(async move {
                for op in ops {
                    run_op(&ctx, client as u64, op).await;
                }
            }));
        }
        for client in clients {
            if let Err(error) = client.await {
                ctx.violate(format!("CLIENT_TASK_PANIC {error}"));
            }
        }
        if sched.shutdown_in_flight {
            ctx.bump("shutdown_in_flight");
            if timeout(DEADLINE, rt.shutdown()).await.is_err() {
                ctx.violate("HANG shutdown with turns in flight".into());
            }
        }
        // Every prompt must end.
        loop {
            let task = ctx.tasks.lock().unwrap().pop();
            let Some(task) = task else { break };
            if let Err(error) = task.await {
                ctx.violate(format!("PROMPT_TASK_PANIC {error}"));
            }
        }
        if !sched.shutdown_in_flight {
            // Quiescence.
            for client in 0..sched.clients.len() {
                let _ = timeout(DEADLINE, RuntimeControl::disconnected(&*rt, client as u64)).await;
            }
            let status = rt.status();
            if status.busy != 0 {
                ctx.violate(format!("BUSY_AFTER_QUIESCENCE busy={}", status.busy));
            }
            let sessions: Vec<_> = ctx.registry.lock().unwrap().clone();
            for sess in &sessions {
                if rt.is_busy(&sess.sref) {
                    ctx.violate(format!("TURN_STILL_ACTIVE session {}", sess.no));
                }
            }
            match timeout(DEADLINE, rt.is_idle()).await {
                Ok(true) => {}
                other => ctx.violate(format!("RUNTIME_NOT_IDLE after quiescence {other:?}")),
            }
            let until = Instant::now() + Duration::from_secs(5);
            while shared.inflight_now() > 0 && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            probe =
                permit_probe(&rt, &shared, sched.max_active, false, &mut probe_violations).await;
            {
                if let Some((reached, peak)) = probe {
                    if reached < sched.max_active {
                        ctx.violate(format!(
                            "LOST_PERMIT only {reached} of {} turns could start",
                            sched.max_active
                        ));
                    }
                    if reached > sched.max_active || peak > sched.max_active {
                        ctx.violate(format!(
                            "OVER_RELEASE {reached} (peak {peak}) turns active, limit {}",
                            sched.max_active
                        ));
                    }
                }
            }
            if rt.status().busy != 0 {
                ctx.violate("BUSY_AFTER_PROBE".into());
            }
            if timeout(Duration::from_secs(60), rt.shutdown())
                .await
                .is_err()
            {
                ctx.violate("HANG shutdown".into());
            }
        }
    };
    let finished = timeout(SCHEDULE_DEADLINE, body).await.is_ok();
    sampling.store(false, Ordering::Relaxed);
    let _ = sampler.await;
    let mut violations: Vec<String> = ctx.violations.lock().unwrap().clone();
    violations.extend(probe_violations);
    if !finished {
        violations.push("HANG schedule exceeded its deadline".into());
    }
    let inflight_peak = shared.peak.load(Ordering::Relaxed);
    if inflight_peak > sched.max_active {
        violations.push(format!(
            "ACTIVE_OVER_LIMIT {inflight_peak} model calls of live prompts at once, limit {}",
            sched.max_active
        ));
    }
    let window_peak = log.window_peak.load(Ordering::SeqCst);
    if !sched.has_subagents && window_peak > sched.max_active as i64 {
        violations.push(format!(
            "ACTIVE_WINDOWS_OVER_LIMIT {window_peak} open turns in the log, limit {}",
            sched.max_active
        ));
    }
    if finished {
        // After shutdown: loaded Sessions, leases and the log.
        if rt.status().loaded != 0 {
            violations.push(format!("LOADED_AFTER_SHUTDOWN {}", rt.status().loaded));
        }
        let records = log.inner.read_runtime_after(0).await.unwrap_or_default();
        let facts = account_log(&records);
        violations.extend(facts.errors.iter().map(|e| format!("LOG_ACCOUNTING {e}")));
        {
            let recs = ctx.recs.lock().unwrap();
            for rec in recs.iter() {
                let terminal = facts.terminals.get(&rec.id);
                if rec.acked {
                    match (facts.sent_ids.get(&rec.id), terminal) {
                    (Some(1), Some(Terminal::Settled | Terminal::Ended(_))) => {}
                    (Some(1), other) => violations.push(format!(
                        "NO_TERMINAL_RECORD acknowledged prompt {} has {other:?}",
                        rec.id
                    )),
                    (count, _) => violations.push(format!(
                        "PROMPT_SENT_COUNT acknowledged prompt {} has {count:?} prompt_sent records",
                        rec.id
                    )),
                }
                    match (&rec.outcome, terminal) {
                    (Some(Kind::Completed(stop)), Some(Terminal::Ended(ended))) if stop != ended => {
                        violations.push(format!(
                            "STOP_MISMATCH prompt {} outcome {stop:?} but turn_ended {ended:?}",
                            rec.id
                        ))
                    }
                    (Some(Kind::Completed(_)), Some(Terminal::Settled)) => violations.push(format!(
                        "COMPLETED_WITHOUT_TURN prompt {} completed but the log says prompt_settled",
                        rec.id
                    )),
                    (Some(Kind::Completed(_)), Some(Terminal::None)) => {}
                    _ => {}
                }
                } else if facts.sent_ids.contains_key(&rec.id) {
                    violations.push(format!(
                        "REFUSED_BUT_LOGGED prompt {} was refused ({:?}) but has prompt_sent",
                        rec.id, rec.refused
                    ));
                }
            }
            let known: HashSet<&String> = recs.iter().map(|r| &r.id).collect();
            for id in facts.sent_ids.keys() {
                if !id.contains('.') && !id.starts_with("900-") && !known.contains(id) {
                    violations.push(format!("UNKNOWN_PROMPT_SENT {id}"));
                }
            }
        }
        if !facts.opened.is_empty() {
            if let Ok(store) = loop_agent::harness::session::sqlite::SqliteSessionStore::open(
                dir.join("agent/sessions.db"),
            ) {
                for key in &facts.opened {
                    if let Ok(Some(lease)) = store.lease(key).await {
                        violations.push(format!(
                            "LEASE_NOT_RELEASED session {key} owner {}",
                            lease.identity.owner
                        ));
                    }
                }
            }
        }
        let mut counters = ctx.counters.lock().unwrap().clone();
        counters.insert("log_prompt_sent", facts.prompt_sent as u64);
        counters.insert(
            "log_child_prompts",
            facts.sent_ids.keys().filter(|id| id.contains('.')).count() as u64,
        );
        *ctx.counters.lock().unwrap() = counters;
        if panic_count() > panics_before {
            violations.push(format!(
                "PANIC (possibly from a concurrent schedule): {:?}",
                &PANICS.lock().unwrap()[panics_before..]
            ));
        }
        let dump = if violations.is_empty() {
            String::new()
        } else {
            let recs = ctx.recs.lock().unwrap();
            let mut text = String::new();
            for rec in recs.iter() {
                text.push_str(&format!(
                    "  prompt {} session {} mode {} acked {} refused {:?} outcome {:?} terminal {:?}\n",
                    rec.id,
                    rec.key,
                    rec.mode,
                    rec.acked,
                    rec.refused,
                    rec.outcome,
                    facts.terminals.get(&rec.id)
                ));
            }
            text
        };
        return Outcome {
            violations,
            counters: ctx.counters.lock().unwrap().clone(),
            cancel_ms: ctx.cancel_ms.lock().unwrap().clone(),
            loaded_max: ctx.loaded_max.load(Ordering::Relaxed),
            inflight_peak,
            window_peak,
            outstanding_max: ctx.outstanding_max.load(Ordering::SeqCst),
            probe,
            prompt_sent: facts.prompt_sent,
            settled: facts.settled,
            ended: facts.ended,
            aborted_closes: facts.aborted_closes,
            model_calls: shared.model_calls.load(Ordering::Relaxed),
            millis: started.elapsed().as_millis(),
            dump,
        };
    }
    let dump = {
        let status = rt.status();
        format!(
            "  status loaded {} busy {} outstanding {} inflight {}\n  recs without outcome: {:?}\n",
            status.loaded,
            status.busy,
            ctx.outstanding.load(Ordering::SeqCst),
            shared.inflight_now(),
            ctx.recs
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.acked && r.outcome.is_none())
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
        )
    };
    let outcome = Outcome {
        violations,
        counters: ctx.counters.lock().unwrap().clone(),
        cancel_ms: ctx.cancel_ms.lock().unwrap().clone(),
        loaded_max: ctx.loaded_max.load(Ordering::Relaxed),
        inflight_peak,
        window_peak,
        outstanding_max: ctx.outstanding_max.load(Ordering::SeqCst),
        probe,
        prompt_sent: 0,
        settled: 0,
        ended: 0,
        aborted_closes: 0,
        model_calls: shared.model_calls.load(Ordering::Relaxed),
        millis: ctx.started.elapsed().as_millis(),
        dump,
    };
    outcome
}

// ------------------------------------------------------------------- driver

#[derive(Default)]
struct Totals {
    counters: BTreeMap<&'static str, u64>,
    cancel_ms: Vec<u64>,
    millis: Vec<u128>,
    loaded_max: u64,
    inflight_peak: usize,
    window_peak: i64,
    outstanding_max: i64,
    probes: u64,
    probe_ok: u64,
    prompt_sent: u64,
    settled: u64,
    ended: u64,
    aborted_closes: u64,
    model_calls: u64,
    failed: Vec<u64>,
    run: u64,
    default_limit_schedules: u64,
    subagent_schedules: u64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress probe: run explicitly (see the module doc)"]
async fn e7_turn_queue() {
    install_panic_hook();
    let schedules = env_u64("E7_SCHEDULES", 1000);
    let seed = env_u64("E7_SEED", 20261111);
    let parallel = env_u64("E7_PARALLEL", 4) as usize;
    let budget = Duration::from_secs(env_u64("E7_BUDGET_SECS", 15 * 60 - 60));
    let only_default = env("E7_LIMITS") == "default";
    let only: Option<u64> = env("E7_ONLY").parse().ok();
    let root = if env("E7_ROOT").is_empty() {
        std::env::temp_dir().join(format!("e7-{}", std::process::id()))
    } else {
        PathBuf::from(env("E7_ROOT"))
    };
    std::fs::create_dir_all(&root).unwrap();
    let started = Instant::now();
    let gate = Arc::new(tokio::sync::Semaphore::new(parallel));
    let totals = Arc::new(Mutex::new(Totals::default()));
    let indexes: Vec<u64> = match only {
        Some(index) => vec![index],
        None => (0..schedules).collect(),
    };
    let mut joins = Vec::new();
    for index in indexes {
        let permit = gate.clone().acquire_owned().await.unwrap();
        if started.elapsed() > budget {
            say(format!(
                "BUDGET|stopped starting schedules at index {index}"
            ));
            drop(permit);
            break;
        }
        let sched = schedule(seed, index, only_default);
        if only.is_some() {
            say(format!("SCHEDULE|{sched:#?}"));
        }
        let totals = totals.clone();
        let dir = root.join(format!("s{index:04}"));
        joins.push(tokio::spawn(async move {
            let _permit = permit;
            let outcome = {
                let sched = sched.clone();
                let dir = dir.clone();
                tokio::spawn(run_schedule(sched, dir)).await
            };
            let mut totals = totals.lock().unwrap();
            totals.run += 1;
            if sched.max_sessions == 16 && sched.max_active == 8 {
                totals.default_limit_schedules += 1;
            }
            if sched.has_subagents {
                totals.subagent_schedules += 1;
            }
            match outcome {
                Err(error) => {
                    totals.failed.push(sched.index);
                    say(format!(
                        "FAIL|seed={seed}|index={}|schedule task panicked: {error}",
                        sched.index
                    ));
                    say(format!("SCHEDULE|{sched:?}"));
                }
                Ok(outcome) => {
                    for (name, count) in &outcome.counters {
                        *totals.counters.entry(name).or_default() += count;
                    }
                    totals.cancel_ms.extend(&outcome.cancel_ms);
                    totals.millis.push(outcome.millis);
                    totals.loaded_max = totals.loaded_max.max(outcome.loaded_max);
                    totals.inflight_peak = totals.inflight_peak.max(outcome.inflight_peak);
                    totals.window_peak = totals.window_peak.max(outcome.window_peak);
                    totals.outstanding_max = totals.outstanding_max.max(outcome.outstanding_max);
                    if let Some((reached, peak)) = outcome.probe {
                        totals.probes += 1;
                        if reached == sched.max_active && peak == sched.max_active {
                            totals.probe_ok += 1;
                        }
                    }
                    totals.prompt_sent += outcome.prompt_sent as u64;
                    totals.settled += outcome.settled as u64;
                    totals.ended += outcome.ended as u64;
                    totals.aborted_closes += outcome.aborted_closes as u64;
                    totals.model_calls += outcome.model_calls;
                    if outcome.violations.is_empty() {
                        let _ = std::fs::remove_dir_all(&dir);
                    } else {
                        totals.failed.push(sched.index);
                        say(format!(
                            "FAIL|seed={seed}|index={}|limits={}/{}|violations={}",
                            sched.index,
                            sched.max_sessions,
                            sched.max_active,
                            outcome.violations.len()
                        ));
                        for violation in outcome.violations.iter().take(12) {
                            say(format!("  {violation}"));
                        }
                        say(format!("PROMPTS|\n{}", outcome.dump));
                        say(format!("SCHEDULE|{sched:?}"));
                        say(format!(
                            "REPLAY|E7_SEED={seed} E7_ONLY={} E7_PARALLEL=1 cargo test -p loop-runtime --test e7_turn_queue -- --ignored --nocapture e7_turn_queue (kept {})",
                            sched.index,
                            dir.display()
                        ));
                    }
                    if totals.run % 50 == 0 {
                        say(format!(
                            "PROGRESS|{} schedules, {} failed, {}s",
                            totals.run,
                            totals.failed.len(),
                            started.elapsed().as_secs()
                        ));
                    }
                }
            }
        }));
    }
    for join in joins {
        join.await.unwrap();
    }
    let totals = totals.lock().unwrap();
    let mut cancel = totals.cancel_ms.clone();
    cancel.sort_unstable();
    let mut millis: Vec<u64> = totals.millis.iter().map(|m| *m as u64).collect();
    millis.sort_unstable();
    say(format!("COUNTERS|{:?}", totals.counters));
    say(format!(
        "NUMBERS|schedules={}|default_limit={}|with_subagents={}|model_calls={}|prompt_sent_records={}|turn_ended={}|prompt_settled={}|aborted_closes={}",
        totals.run,
        totals.default_limit_schedules,
        totals.subagent_schedules,
        totals.model_calls,
        totals.prompt_sent,
        totals.ended,
        totals.settled,
        totals.aborted_closes
    ));
    say(format!(
        "PEAKS|loaded={}|live_model_calls={}|open_turn_windows={}|outstanding_prompts={}",
        totals.loaded_max, totals.inflight_peak, totals.window_peak, totals.outstanding_max
    ));
    say(format!(
        "PROBE|runs={}|exact={}",
        totals.probes, totals.probe_ok
    ));
    say(format!(
        "CANCEL_LATENCY_MS|n={}|p50={}|p99={}|max={}|over_1s={}",
        cancel.len(),
        percentile(&cancel, 0.5),
        percentile(&cancel, 0.99),
        cancel.last().copied().unwrap_or(0),
        cancel.iter().filter(|m| **m > 1000).count()
    ));
    say(format!(
        "SCHEDULE_MS|p50={}|p99={}|max={}",
        percentile(&millis, 0.5),
        percentile(&millis, 0.99),
        millis.last().copied().unwrap_or(0)
    ));
    let wall = started.elapsed();
    if totals.failed.is_empty() && panic_count() == 0 {
        say(format!(
            "PASS|schedules={}|seed={seed}|wall={}s",
            totals.run,
            wall.as_secs()
        ));
        let _ = std::fs::remove_dir_all(&root);
    } else {
        say(format!(
            "FAIL_SUMMARY|schedules={}|failed={}|indexes={:?}|panics={}|seed={seed}|wall={}s",
            totals.run,
            totals.failed.len(),
            totals.failed,
            panic_count(),
            wall.as_secs()
        ));
        panic!("E7 failed: {} schedules", totals.failed.len());
    }
}

/// The probe must be able to fail: with one turn left open, one permit is
/// taken, so the permit probe must find one turn fewer than the limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "negative control for the permit probe: run explicitly"]
async fn e7_negative_control() {
    let dir = std::env::temp_dir().join(format!("e7-control-{}", std::process::id()));
    let limits = RuntimeLimits {
        max_sessions: 16,
        max_active_turns: 8,
    };
    let Built { rt, shared, .. } = build(&dir, limits, 0, 1).await;
    let mut violations = Vec::new();
    let (reached, _) = permit_probe(&rt, &shared, 8, true, &mut violations)
        .await
        .expect("probe sessions");
    say(format!(
        "CONTROL|held one turn open: probe reached {reached} of 8 (a detected leak shows 7)"
    ));
    assert_eq!(reached, 7, "the probe did not see the held permit");
    assert!(violations.is_empty(), "{violations:?}");
    rt.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Defect D1. A cancel that arrives while the native backend prepares a prompt
/// (`native_backend.rs`, "the turn was cancelled before it started") ends the
/// prompt as `TurnOutcome::Failed` with category `Cancelled`. The Runtime's own
/// cancel paths (queued, running) report `TurnOutcome::Cancelled`, so a Surface
/// sees "failed" for a cancel it asked for. The test cancels at once after the
/// prompt is accepted, many times, and fails when it sees `Failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "regression test for defect D1 (fixed in the Runtime); run explicitly"]
async fn e7_defect_cancel_during_prompt_start_is_not_failed() {
    let dir = std::env::temp_dir().join(format!("e7-d1-{}", std::process::id()));
    let limits = RuntimeLimits {
        max_sessions: 4,
        max_active_turns: 2,
    };
    let Built { rt, .. } = build(&dir, limits, 0, 7).await;
    let session = rt.new_session(SessionSpec::default()).await.unwrap();
    let mut rng = Rng(7);
    let mut failed = Vec::new();
    for round in 0..1500 {
        let spec = format!("E7[{round}-0-{round};text;1;0]");
        let handle = rt
            .prompt(&session, vec![ContentBlock::text(spec)])
            .await
            .unwrap();
        for _ in 0..rng.below(40) {
            tokio::task::yield_now().await;
        }
        rt.cancel(&session);
        if let TurnOutcome::Failed(info) = handle.outcome().await {
            failed.push(format!("round {round}: {info:?}"));
        }
    }
    rt.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failed.is_empty(),
        "{} of 1500 cancelled prompts ended as Failed, first: {:?}",
        failed.len(),
        failed.first()
    );
}
