//! Probe E1, "Recoverable truth" (H1: ownership and recovery).
//!
//! Each trial runs three owners of one agent directory as separate processes:
//! a Runtime ("the daemon", with `events.db` and `sessions.db`) and two
//! embedded (standalone) owners that write only `sessions.db`. All of them use
//! the `faux` provider, and every turn runs one tool with a real side effect: it
//! appends the turn's token to `effects.log`.
//!
//! A trial is three rounds. Rounds 0 and 1 end with SIGKILL of all three
//! processes, at seeded random times and at seeded failpoints between the log
//! commit, the tree commit and the acknowledgement. Rounds 1 and 2 restart all
//! owners (two restarts). Round 2 stops cleanly, and a last Runtime start checks
//! that recovery does not repeat itself. While owners are alive, the trial makes
//! their leases look stale and asks the other owners to take them over.
//!
//! The full run is ignored by default:
//!
//! ```text
//! E1_TRIALS=200 E1_SEED=42 E1_PARALLEL=6 cargo test -p loop-runtime --test e1_recovery \
//!     -- --ignored --nocapture e1_recoverable_truth
//! ```
//!
//! `E1_ROOT` names the folder for trial directories (default: a temp folder).
//! `E1_BUDGET_SECS` stops starting new trials after that many seconds.
#![cfg(unix)]

use async_trait::async_trait;
use loop_agent::harness::{
    create_session_repository,
    session::{
        sqlite::{create_sqlite_session_store_for_resume, LeaseIdentity, SqliteSessionStore},
        PendingSessionWrite, SessionForkSelection, SessionMetadata, SessionReader, SessionStore,
        SessionTreeEntry,
    },
    types::SessionError,
    AgentHarness, AgentHarnessOptions, AgentHarnessResources, HostExecutionEnv, SandboxMode,
};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{AgentBackend, ContentBlock, SessionRef, SessionSpec, TurnId};
use loop_runtime::{
    log::{Event, EventKind, EventLog, LoggedEvent, SqliteEventLog},
    Runtime, RuntimeLimits,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
};

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
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// End this process the hard way: no destructors, no flushes, no releases.
fn sigkill_self() -> ! {
    let _ = std::io::stdout().flush();
    unsafe {
        kill(std::process::id() as i32, 9);
    }
    std::process::abort()
}

fn alive(pid: u32) -> bool {
    unsafe { kill(pid as i32, 0) == 0 }
}

fn say(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "E1|{line}");
    let _ = out.flush();
}

/// Token text is fixed width, so no token is a prefix of another.
fn token(role: &str, round: u32, n: u32) -> String {
    format!("{role}r{round}n{n:04}")
}

/// The token of the newest user prompt in a model request.
fn prompt_token(context: &Context) -> String {
    for message in context.messages.iter().rev() {
        if let Message::User(_) = message {
            let text = serde_json::to_string(message).unwrap_or_default();
            if let Some(start) = text.find("E1TOK[") {
                let rest = &text[start + 6..];
                if let Some(end) = rest.find(']') {
                    return rest[..end].to_owned();
                }
            }
        }
    }
    "unknown".into()
}

/// Every token in `text`.
fn tokens_in(text: &str) -> HashSet<String> {
    let bytes = text.as_bytes();
    let mut found = HashSet::new();
    // role (d0|d1|o1|o2) r<digit> n<4 digits>
    for i in 0..bytes.len().saturating_sub(9) {
        let w = &bytes[i..i + 9];
        if (w[0] == b'd' || w[0] == b'o')
            && w[1].is_ascii_digit()
            && w[2] == b'r'
            && w[3].is_ascii_digit()
            && w[4] == b'n'
            && w[5..9].iter().all(u8::is_ascii_digit)
            && (i + 9 == bytes.len() || !bytes[i + 9].is_ascii_digit())
        {
            found.insert(text[i..i + 9].to_owned());
        }
    }
    found
}

/// A seeded model: the prompt asks for one tool call that appends the token
/// to `effects.log`; the tool result is answered with `done <token>`.
fn responder(tool: &'static str, effects: PathBuf, seed: u64) -> loop_ai::providers::FauxResponder {
    let rng = Arc::new(Mutex::new(Rng(seed)));
    Arc::new(move |context: &Context| {
        let token = prompt_token(context);
        let asked = matches!(context.messages.last(), Some(Message::User(_)));
        let delay = rng.lock().unwrap().below(25);
        let response = if asked {
            let arguments = if tool == "bash" {
                json!({"command": format!("printf '%s\\n' {token} >> '{}'", effects.display())})
            } else {
                json!({"token": token})
            };
            FauxResponse::ToolCalls(vec![ToolCall {
                id: format!("call-{token}"),
                name: tool.into(),
                arguments,
                thought_signature: None,
            }])
        } else {
            FauxResponse::Text(format!("done {token}"))
        };
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            response
        })
    })
}

// ---------------------------------------------------------------- failpoints

/// `phase:target:nth`. Targets are update types (`turn_started`, `tool_call`,
/// `turn_ended`, ...), `prompt_sent`, `append` (an embedded tree write), `ack`,
/// and the recovery points `recovery_batch`, `mark_recovered`.
#[derive(Clone, Debug)]
struct Failpoint {
    before: bool,
    target: String,
    nth: u64,
}
impl Failpoint {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let phase = parts.next()?;
        let target = parts.next()?.to_owned();
        let nth = parts.next()?.parse().ok()?;
        Some(Self {
            before: phase == "before",
            target,
            nth,
        })
    }
}
struct Faults {
    point: Option<Failpoint>,
    armed: AtomicBool,
    seen: AtomicU64,
}
impl Faults {
    fn from_env() -> Arc<Self> {
        let point = Failpoint::parse(&env("E1_FAILPOINT"));
        // Recovery points run before anyone can arm them.
        let armed = point
            .as_ref()
            .is_some_and(|p| p.target == "recovery_batch" || p.target == "mark_recovered");
        Arc::new(Self {
            point,
            armed: AtomicBool::new(armed),
            seen: AtomicU64::new(0),
        })
    }
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    /// Count a matching step and die when it is the chosen one.
    fn hit(&self, before: bool, targets: &[&str]) {
        let Some(point) = &self.point else { return };
        if point.before != before || !self.armed.load(Ordering::SeqCst) {
            return;
        }
        if targets.iter().any(|t| *t == point.target) {
            let n = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
            if n == point.nth {
                sigkill_self();
            }
        }
    }
}

fn targets(events: &[Event]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for event in events {
        match &event.kind {
            EventKind::PromptSent(_) => out.push("prompt_sent"),
            EventKind::Update(envelope) => {
                let update = serde_json::to_value(&envelope.update).unwrap_or_default();
                let stop = update["stop"].as_str() == Some("runtime_restarted");
                out.push(match update["type"].as_str().unwrap_or("") {
                    "turn_started" => "turn_started",
                    "turn_ended" if stop => "recovery_batch",
                    "turn_ended" => "turn_ended",
                    "tool_call" => "tool_call",
                    "tool_call_update" => "tool_call_update",
                    "message_ended" => "message_ended",
                    _ => "other",
                });
            }
            _ => {}
        }
    }
    out
}

/// The SQLite Event Log with failpoints and an optional slow writer.
struct FaultyLog {
    inner: SqliteEventLog,
    faults: Arc<Faults>,
    slow_ms: u64,
    rng: Mutex<Rng>,
}
#[async_trait]
impl EventLog for FaultyLog {
    async fn append_batch(
        &self,
        events: Vec<Event>,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        let kinds = targets(&events);
        if self.slow_ms > 0 {
            let delay = self.rng.lock().unwrap().below(self.slow_ms);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.faults.hit(true, &kinds);
        let result = self.inner.append_batch(events).await;
        if result.is_ok() {
            self.faults.hit(false, &kinds);
        }
        result
    }
    async fn read_after(
        &self,
        session: &loop_protocol::SessionKey,
        after_seq: u64,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.read_after(session, after_seq).await
    }
    async fn read_runtime_after(
        &self,
        global_seq: u64,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.read_runtime_after(global_seq).await
    }
    async fn latest_seq(
        &self,
        session: &loop_protocol::SessionKey,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(session).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        started_ms: i64,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.allocate_generation(owner, pid, started_ms).await
    }
    async fn finish_generation(
        &self,
        generation: u64,
        stopped_ms: i64,
    ) -> loop_runtime::log::Result<()> {
        self.inner.finish_generation(generation, stopped_ms).await
    }
    async fn recovery_records(&self, from: u64) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.recovery_records(from).await
    }
    async fn last_recovered_generation(&self) -> loop_runtime::log::Result<u64> {
        self.inner.last_recovered_generation().await
    }
    async fn mark_recovered(&self, generation: u64, at: i64) -> loop_runtime::log::Result<()> {
        self.faults.hit(true, &["mark_recovered"]);
        let result = self.inner.mark_recovered(generation, at).await;
        self.faults.hit(false, &["mark_recovered"]);
        result
    }
    async fn subagent_sessions(
        &self,
    ) -> loop_runtime::log::Result<HashSet<loop_protocol::SessionKey>> {
        self.inner.subagent_sessions().await
    }
    async fn prompts_sent(
        &self,
        session: &loop_protocol::SessionKey,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.prompts_sent(session).await
    }
    async fn turn_ids(
        &self,
        session: &loop_protocol::SessionKey,
    ) -> loop_runtime::log::Result<Vec<TurnId>> {
        self.inner.turn_ids(session).await
    }
    async fn last_activity_ms(
        &self,
    ) -> loop_runtime::log::Result<HashMap<loop_protocol::SessionKey, i64>> {
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

/// An embedded owner's Session store with failpoints around tree writes.
struct FaultyStore {
    inner: Arc<dyn SessionStore>,
    faults: Arc<Faults>,
}
#[async_trait]
impl SessionStore for FaultyStore {
    fn write_cancellation(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.inner.write_cancellation()
    }
    async fn release_leases(&self) -> Result<(), SessionError> {
        self.inner.release_leases().await
    }
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.create(cwd, name).await
    }
    async fn open_writer(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.open_writer(id).await
    }
    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.load(id).await
    }
    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        self.inner.list(cwd).await
    }
    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        self.faults.hit(true, &["append"]);
        let result = self.inner.append_entry(session_id, pending).await;
        self.faults.hit(false, &["append"]);
        result
    }
    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.delete(id).await
    }
    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner
            .fork(source_id, selection, through_entry_id, name)
            .await
    }
}

// ---------------------------------------------------------------- children

fn reference(id: &str) -> SessionRef {
    SessionRef {
        instance: "loop".into(),
        key: id.into(),
        native_id: id.into(),
    }
}

/// Lines from the parent: `ARM`, `PROBE id..`, `STOP`.
fn commands() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

async fn daemon_main() {
    let dir = PathBuf::from(env("E1_DIR"));
    let agent = dir.join("agent");
    let work = dir.join("work");
    let effects = dir.join("effects.log");
    let seed: u64 = env("E1_SEED").parse().unwrap_or(1);
    let round: u32 = env("E1_ROUND").parse().unwrap_or(0);
    let faults = Faults::from_env();
    let _instance = loop_app_core::runtime_instance::RuntimeInstanceLock::acquire(&agent)
        .expect("runtime.lock");
    let sessions_lock =
        loop_app_core::runtime_instance::SessionDatabaseLock::acquire(&agent.join("sessions.db"))
            .expect("sessions lock");
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(responder("bash", effects, seed)));
    let globals = GlobalResources::with_models(agent.clone(), models).unwrap();
    let log = Arc::new(FaultyLog {
        inner: SqliteEventLog::open(&agent).await.unwrap(),
        faults: faults.clone(),
        slow_ms: env("E1_SLOW_LOG").parse().unwrap_or(0),
        rng: Mutex::new(Rng(seed ^ 0x51)),
    });
    let opts = BootstrapOpts {
        cwd: work.clone(),
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
    };
    let rt = Runtime::new(globals, opts, log, RuntimeLimits::default())
        .await
        .unwrap()
        .into_shared();
    let r = rt.recover(Some(&sessions_lock)).await.unwrap();
    say(format!(
        "READY|{}|{}|{}|{}|{}",
        rt.generation(),
        r.turns_ended,
        r.escalations_withdrawn,
        r.leases_reclaimed,
        r.leases_swept
    ));
    let mut commands = commands();
    if env("E1_VERIFY") == "1" {
        while let Some(line) = commands.recv().await {
            if line == "STOP" {
                break;
            }
        }
        rt.shutdown().await;
        say("STOPPED".into());
        return;
    }
    let mut sessions = Vec::new();
    let wanted: Vec<String> = env("E1_SESSIONS")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if wanted.is_empty() {
        for _ in 0..2 {
            let spec = SessionSpec {
                cwd: Some(work.clone()),
                ..Default::default()
            };
            sessions.push(rt.new_session(spec).await.unwrap());
        }
    } else {
        for id in wanted {
            match rt
                .load_session(&reference(&id), SessionSpec::default())
                .await
            {
                Ok(session) => sessions.push(session),
                Err(error) => say(format!("LOADFAIL|{id}|{error}")),
            }
        }
    }
    for (i, session) in sessions.iter().enumerate() {
        say(format!("SESSION|{i}|{}", session.native_id));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut drivers = Vec::new();
    for (i, session) in sessions.into_iter().enumerate() {
        let (rt, stop, faults) = (rt.clone(), stop.clone(), faults.clone());
        let role = format!("d{i}");
        drivers.push(tokio::spawn(async move {
            for n in 0..9999 {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let tok = token(&role, round, n);
                say(format!("SENT|{}|{tok}", session.native_id));
                match rt
                    .prompt(
                        &session,
                        vec![ContentBlock::text(format!("E1TOK[{tok}] go"))],
                    )
                    .await
                {
                    Ok(turn) => {
                        let outcome = turn.outcome().await;
                        faults.hit(true, &["ack"]);
                        let outcome = match outcome {
                            loop_protocol::TurnOutcome::Completed(_) => "completed".to_owned(),
                            other => format!("{other:?}").replace('|', "/"),
                        };
                        say(format!("ACK|{}|{tok}|{outcome}", session.native_id));
                    }
                    Err(error) => {
                        say(format!("PERR|{}|{tok}|{error}", session.native_id));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }));
    }
    while let Some(line) = commands.recv().await {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("ARM") => faults.arm(),
            Some("PROBE") => {
                for id in words {
                    match rt
                        .load_session(&reference(id), SessionSpec::default())
                        .await
                    {
                        Ok(_) => {
                            say(format!("PROBE|{id}|TAKEN"));
                            let _ = rt.close(&id.into(), true).await;
                        }
                        Err(error) => say(format!("PROBE|{id}|refused|{error}")),
                    }
                }
            }
            Some("STOP") => break,
            _ => {}
        }
    }
    stop.store(true, Ordering::SeqCst);
    for driver in drivers {
        let _ = driver.await;
    }
    rt.shutdown().await;
    say("STOPPED".into());
}

async fn owner_main() {
    let dir = PathBuf::from(env("E1_DIR"));
    let db = dir.join("agent").join("sessions.db");
    let work = dir.join("work");
    let effects = dir.join("effects.log");
    let seed: u64 = env("E1_SEED").parse().unwrap_or(1);
    let round: u32 = env("E1_ROUND").parse().unwrap_or(0);
    let role = env("E1_NAME");
    let resume = env("E1_SESSION");
    let faults = Faults::from_env();
    // The terminal `loop` path: sweep stale rows, then claim the Session.
    let inner = match create_sqlite_session_store_for_resume(
        &db,
        (!resume.is_empty()).then_some(resume.as_str()),
    )
    .await
    {
        Ok(store) => store,
        Err(error) => {
            say(format!("RESUMEFAIL|{resume}|{error}"));
            return;
        }
    };
    let store: Arc<dyn SessionStore> = Arc::new(FaultyStore {
        inner,
        faults: faults.clone(),
    });
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = if resume.is_empty() {
        repo.create(Some(work.to_string_lossy().into_owned()), None)
            .await
            .unwrap()
    } else {
        match repo.open(&resume).await {
            Ok(session) => session,
            Err(error) => {
                say(format!("RESUMEFAIL|{resume}|{error}"));
                return;
            }
        }
    };
    let id = session.metadata().id.clone();
    let models = Arc::new(loop_ai::Models::new());
    models.set_provider(faux_provider_fn(responder("effect", effects.clone(), seed)));
    let tool = loop_agent::AgentTool::simple(
        "effect",
        "effect",
        "append the token to effects.log",
        json!({"type": "object", "properties": {"token": {"type": "string"}}}),
        move |_, args, _, _| {
            let effects = effects.clone();
            async move {
                let token = args["token"].as_str().unwrap_or("none").to_owned();
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&effects)
                    .map_err(|e| e.to_string())?;
                file.write_all(format!("{token}\n").as_bytes())
                    .map_err(|e| e.to_string())?;
                Ok(loop_agent::AgentToolResult::text("appended"))
            }
        },
    );
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        model: models.get_model("faux", "faux-model"),
        models,
        session,
        host_env: Arc::new(HostExecutionEnv::new(&work)),
        tools: vec![tool],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: AgentHarnessResources::default(),
    }));
    say(format!("READY|{id}"));
    let stop = Arc::new(AtomicBool::new(false));
    let driver = {
        let (harness, stop, faults, id) =
            (harness.clone(), stop.clone(), faults.clone(), id.clone());
        tokio::spawn(async move {
            for n in 0..9999 {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let tok = token(&role, round, n);
                say(format!("SENT|{id}|{tok}"));
                match harness.prompt(format!("E1TOK[{tok}] go")).await {
                    Ok(_) => {
                        faults.hit(true, &["ack"]);
                        say(format!("ACK|{id}|{tok}|completed"));
                    }
                    Err(error) => {
                        say(format!(
                            "PERR|{id}|{tok}|{}",
                            error.to_string().replace('|', "/")
                        ));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        })
    };
    let base = SqliteSessionStore::open(&db).unwrap();
    let mut commands = commands();
    let mut probe_generation = 1;
    while let Some(line) = commands.recv().await {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("ARM") => faults.arm(),
            Some("PROBE") => {
                for target in words {
                    // What `loop --resume <id>` does: a takeover, never a reclaim.
                    probe_generation += 1;
                    let me = LeaseIdentity {
                        owner: "standalone".into(),
                        pid: std::process::id(),
                        generation: -probe_generation,
                    };
                    match base.takeover_lease(target, &me, false).await {
                        Ok(()) => {
                            say(format!("PROBE|{target}|TAKEN"));
                            let _ = base.release_lease(target, &me).await;
                        }
                        Err(error) => say(format!("PROBE|{target}|refused|{error}")),
                    }
                }
            }
            Some("STOP") => break,
            _ => {}
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = driver.await;
    let _ = store.release_leases().await;
    say("STOPPED".into());
}

/// The child entry point. It does nothing unless the parent set `E1_ROLE`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "child process of e1_recoverable_truth"]
async fn e1_child() {
    let role = env("E1_ROLE");
    match role.as_str() {
        "daemon" => daemon_main().await,
        "owner" => owner_main().await,
        _ => return,
    }
    // The stdin reader is a blocking task; do not wait for it on the way out.
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

// ---------------------------------------------------------------- parent

struct Kid {
    role: String,
    child: Child,
    stdin: Option<ChildStdin>,
    pid: u32,
    lines: Arc<Mutex<Vec<String>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Kid {
    fn spawn(role: &str, vars: &[(&str, String)]) -> Kid {
        let exe = std::env::current_exe().unwrap();
        let stderr = vars
            .iter()
            .find(|(k, _)| *k == "E1_DIR")
            .and_then(|(_, dir)| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(Path::new(dir).join("stderr.log"))
                    .ok()
            });
        let mut cmd = Command::new(exe);
        cmd.args([
            "e1_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            "E1_ROLE",
            if role.starts_with('d') || role == "verify" {
                "daemon"
            } else {
                "owner"
            },
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(match stderr {
            Some(file) => std::process::Stdio::from(file),
            None => std::process::Stdio::null(),
        })
        .kill_on_drop(true);
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn child");
        let pid = child.id().unwrap_or(0);
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let reader = tokio::spawn(async move {
            let mut out = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = out.next_line().await {
                // libtest may print "test e1_child ... " before the first line.
                if let Some(at) = line.find("E1|") {
                    sink.lock().unwrap().push(line[at + 3..].to_owned());
                }
            }
        });
        Kid {
            role: role.into(),
            child,
            stdin,
            pid,
            lines,
            reader,
        }
    }
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    /// Wait for a line with `prefix`; None when the child exits or time runs out.
    async fn wait(&mut self, prefix: &str, count: usize, limit: Duration) -> bool {
        let start = Instant::now();
        loop {
            let n = self
                .lines()
                .iter()
                .filter(|l| l.starts_with(prefix))
                .count();
            if n >= count {
                return true;
            }
            if start.elapsed() > limit || !self.running() {
                // Let the reader drain what the child printed before it ended.
                tokio::time::sleep(Duration::from_millis(30)).await;
                return self
                    .lines()
                    .iter()
                    .filter(|l| l.starts_with(prefix))
                    .count()
                    >= count;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn send(&mut self, line: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(format!("{line}\n").as_bytes()).await;
            let _ = stdin.flush().await;
        }
    }
    async fn sigkill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
    /// Wait for exit and collect every line.
    async fn finish(mut self, limit: Duration) -> (String, Vec<String>) {
        if tokio::time::timeout(limit, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.reader).await;
        (self.role.clone(), self.lines())
    }
}

#[derive(Default, Debug, Clone, serde::Serialize)]
struct Tally {
    trials: u64,
    // Pass criteria.
    acked_lost: u64,
    duplicate_effects: u64,
    duplicate_recovery_records: u64,
    live_takeovers: u64,
    interrupted_not_uncertain: u64,
    // Context.
    acked_daemon: u64,
    acked_embedded: u64,
    interrupted_daemon: u64,
    interrupted_marked_uncertain: u64,
    interrupted_completed_unacked: u64,
    interrupted_never_started: u64,
    interrupted_embedded: u64,
    embedded_unrecorded_effects: u64,
    embedded_partial_tree: u64,
    embedded_completed_unacked: u64,
    log_ok_tree_missing: u64,
    effect_without_turn_record: u64,
    probes: u64,
    probes_refused: u64,
    daemon_restarts: u64,
    recovery_failpoint_deaths: u64,
    load_failures: u64,
    resume_failures: u64,
    prompt_errors: u64,
    verify_turns_ended: u64,
    kills: u64,
    failpoint_deaths: u64,
    recovered_turns: u64,
    trial_failures: u64,
    notes: Vec<String>,
    #[serde(skip)]
    plan: Vec<String>,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        macro_rules! sum { ($($f:ident),*) => { $( self.$f += other.$f; )* } }
        sum!(
            trials,
            acked_lost,
            duplicate_effects,
            duplicate_recovery_records,
            live_takeovers,
            interrupted_not_uncertain,
            acked_daemon,
            acked_embedded,
            interrupted_daemon,
            interrupted_marked_uncertain,
            interrupted_completed_unacked,
            interrupted_never_started,
            interrupted_embedded,
            embedded_unrecorded_effects,
            embedded_partial_tree,
            embedded_completed_unacked,
            log_ok_tree_missing,
            effect_without_turn_record,
            probes,
            probes_refused,
            daemon_restarts,
            recovery_failpoint_deaths,
            load_failures,
            resume_failures,
            prompt_errors,
            verify_turns_ended,
            kills,
            failpoint_deaths,
            recovered_turns,
            trial_failures
        );
        self.notes.extend(other.notes.iter().cloned());
    }
    fn passed(&self) -> bool {
        self.acked_lost == 0
            && self.duplicate_effects == 0
            && self.duplicate_recovery_records == 0
            && self.live_takeovers == 0
            && self.interrupted_not_uncertain == 0
    }
}

const DAEMON_FAILPOINTS: &[&str] = &[
    "prompt_sent",
    "turn_started",
    "tool_call",
    "tool_call_update",
    "message_ended",
    "turn_ended",
    "ack",
];

fn pick_daemon_failpoint(rng: &mut Rng, restart: bool) -> String {
    if restart && rng.chance(20) {
        let phase = if rng.chance(50) { "before" } else { "after" };
        let target = if rng.chance(50) {
            "recovery_batch"
        } else {
            "mark_recovered"
        };
        return format!("{phase}:{target}:1");
    }
    if rng.chance(25) {
        return String::new(); // a plain random-time SIGKILL
    }
    let target = DAEMON_FAILPOINTS[rng.below(DAEMON_FAILPOINTS.len() as u64) as usize];
    let phase = if target == "ack" || rng.chance(50) {
        "before"
    } else {
        "after"
    };
    format!("{phase}:{target}:{}", 1 + rng.below(4))
}

fn pick_owner_failpoint(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 => String::new(),
        1 => format!("before:append:{}", 1 + rng.below(8)),
        2 => format!("after:append:{}", 1 + rng.below(8)),
        _ => format!("before:ack:{}", 1 + rng.below(3)),
    }
}

fn sessions_db(dir: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(dir.join("agent/sessions.db")).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn
}

/// Make the leases of these pids look 60 s old (as if their owners paused).
fn age_leases(dir: &Path, pids: &[u32]) {
    let conn = sessions_db(dir);
    for pid in pids {
        let _ = conn.execute(
            "UPDATE session_leases SET heartbeat_ms = heartbeat_ms - 60000 WHERE pid = ?1",
            [pid],
        );
    }
}

fn lease_rows(dir: &Path) -> BTreeMap<String, (String, u32, i64)> {
    let conn = sessions_db(dir);
    let mut stmt = conn
        .prepare("SELECT session_id, owner, pid, generation FROM session_leases")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

struct RoundLog {
    lines: Vec<(String, Vec<String>)>,
}

/// One trial. Returns its tally; the trial directory is deleted on success.
async fn trial(root: PathBuf, index: u64, seed: u64) -> Tally {
    let mut rng = Rng(seed);
    let mut tally = Tally {
        trials: 1,
        ..Default::default()
    };
    let dir = root.join(format!("t{index:04}"));
    std::fs::create_dir_all(dir.join("agent")).unwrap();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::write(
        dir.join("agent/settings.json"),
        r#"{"sandbox":{"mode":"off"}}"#,
    )
    .unwrap();
    let base = |round: u32, extra: Vec<(&'static str, String)>| {
        let mut vars = vec![
            ("E1_DIR", dir.to_string_lossy().into_owned()),
            ("E1_ROUND", round.to_string()),
            (
                "LOOP_CODING_AGENT_DIR",
                dir.join("agent").to_string_lossy().into_owned(),
            ),
            ("HOME", dir.to_string_lossy().into_owned()),
        ];
        vars.extend(extra);
        vars
    };
    let slow = if rng.chance(30) { 15 } else { 0 };
    tally.plan.push(format!("slow={slow}"));
    let mut logs = RoundLog { lines: Vec::new() };
    let mut daemon_sessions: Vec<String> = Vec::new();
    let mut owner_sessions: [String; 2] = [String::new(), String::new()];
    let mut dead_owner_pids: Vec<u32> = Vec::new();

    for round in 0..3u32 {
        let restart = round > 0;
        // Simulate 30 s passing for the owners that were SIGKILLed.
        age_leases(&dir, &dead_owner_pids);
        // 1. The embedded owners, resuming their Sessions after a restart.
        let mut owners = Vec::new();
        for (i, name) in ["o1", "o2"].iter().enumerate() {
            let failpoint = if round < 2 {
                pick_owner_failpoint(&mut rng)
            } else {
                String::new()
            };
            tally.plan.push(format!("r{round}:{name}={failpoint}"));
            let kid = Kid::spawn(
                name,
                &base(
                    round,
                    vec![
                        ("E1_NAME", name.to_string()),
                        ("E1_SESSION", owner_sessions[i].clone()),
                        ("E1_SEED", rng.next().to_string()),
                        ("E1_FAILPOINT", failpoint),
                    ],
                ),
            );
            owners.push(kid);
        }
        for (i, owner) in owners.iter_mut().enumerate() {
            if owner.wait("READY|", 1, Duration::from_secs(30)).await {
                let line = owner
                    .lines()
                    .into_iter()
                    .find(|l| l.starts_with("READY|"))
                    .unwrap();
                owner_sessions[i] = line["READY|".len()..].to_owned();
            } else {
                tally.resume_failures += 1;
                tally.notes.push(format!(
                    "trial {index} round {round}: {} did not start: {:?}",
                    owner.role,
                    owner.lines()
                ));
            }
        }
        let owner_pids: Vec<u32> = owners.iter().map(|o| o.pid).collect();
        // 2. Owners alive with stale-looking heartbeats when the daemon sweeps.
        age_leases(&dir, &owner_pids);
        let before_daemon = lease_rows(&dir);
        // 3. The daemon; a recovery failpoint kills it, and it is started again.
        let mut daemon = None;
        for attempt in 0..3 {
            let failpoint = if attempt == 0 && round < 2 {
                pick_daemon_failpoint(&mut rng, restart)
            } else if attempt == 0 && round == 2 && rng.chance(20) {
                if pick_daemon_failpoint(&mut rng, true).contains("recovery") {
                    "after:recovery_batch:1".to_owned()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            tally.plan.push(format!("r{round}:d#{attempt}={failpoint}"));
            let mut kid = Kid::spawn(
                "daemon",
                &base(
                    round * 3 + attempt,
                    vec![
                        ("E1_SEED", rng.next().to_string()),
                        ("E1_FAILPOINT", failpoint.clone()),
                        ("E1_SESSIONS", daemon_sessions.join(",")),
                        ("E1_SLOW_LOG", slow.to_string()),
                    ],
                ),
            );
            if restart || attempt > 0 {
                tally.daemon_restarts += 1;
            }
            let ready = kid.wait("READY|", 1, Duration::from_secs(30)).await
                && kid
                    .wait(
                        "SESSION|",
                        daemon_sessions.len().max(2),
                        Duration::from_secs(30),
                    )
                    .await;
            if ready {
                daemon = Some(kid);
                break;
            }
            let (_, lines) = kid.finish(Duration::from_secs(5)).await;
            if failpoint.contains("recovery") || failpoint.contains("mark_recovered") {
                tally.recovery_failpoint_deaths += 1;
            } else {
                tally.notes.push(format!(
                    "trial {index} round {round}: daemon start failed: {lines:?}"
                ));
            }
            for line in lines.iter().filter(|l| l.starts_with("LOADFAIL|")) {
                tally.load_failures += 1;
                tally
                    .notes
                    .push(format!("trial {index} round {round}: {line}"));
            }
            logs.lines.push(("daemon".into(), lines));
        }
        let Some(mut daemon) = daemon else {
            tally.trial_failures += 1;
            for o in owners {
                logs.lines.push(o.finish(Duration::from_millis(1)).await);
            }
            break;
        };
        for line in daemon.lines() {
            if let Some(rest) = line.strip_prefix("READY|") {
                let parts: Vec<u64> = rest.split('|').map(|p| p.parse().unwrap_or(0)).collect();
                tally.recovered_turns += parts.get(1).copied().unwrap_or(0);
            }
            if let Some(rest) = line.strip_prefix("SESSION|") {
                let id = rest.split('|').nth(1).unwrap().to_owned();
                if !daemon_sessions.contains(&id) {
                    daemon_sessions.push(id);
                }
            }
        }
        // The daemon's sweep must keep the rows of live owners.
        let after_daemon = lease_rows(&dir);
        for (session, row) in &before_daemon {
            if owner_pids.contains(&row.1) && alive(row.1) && after_daemon.get(session) != Some(row)
            {
                tally.live_takeovers += 1;
                tally.notes.push(format!(
                    "trial {index} round {round}: daemon start changed live lease {session} {row:?} -> {:?}",
                    after_daemon.get(session)
                ));
            }
        }
        // 4. Let work flow, then probe takeovers of live owners.
        daemon.wait("ACK|", 2, Duration::from_secs(5)).await;
        for owner in owners.iter_mut() {
            owner.wait("ACK|", 1, Duration::from_secs(5)).await;
        }
        let mut live: Vec<u32> = owners
            .iter_mut()
            .filter_map(|o| o.running().then_some(o.pid))
            .collect();
        if daemon.running() {
            live.push(daemon.pid);
        }
        age_leases(&dir, &live);
        let rows = lease_rows(&dir);
        let mut expected = 0;
        if daemon.running() {
            let targets: Vec<String> = owner_sessions
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect();
            expected += targets.len();
            daemon.send(&format!("PROBE {}", targets.join(" "))).await;
        }
        let mut owner_expected = Vec::new();
        for (i, owner) in owners.iter_mut().enumerate() {
            let mut targets: Vec<String> = daemon_sessions.clone();
            targets.push(owner_sessions[1 - i].clone());
            targets.retain(|s| !s.is_empty());
            owner_expected.push(targets.len());
            if owner.running() {
                owner.send(&format!("PROBE {}", targets.join(" "))).await;
            }
        }
        daemon
            .wait("PROBE|", expected, Duration::from_secs(10))
            .await;
        for (i, owner) in owners.iter_mut().enumerate() {
            owner
                .wait("PROBE|", owner_expected[i], Duration::from_secs(10))
                .await;
        }
        let after = lease_rows(&dir);
        for (session, row) in &rows {
            if alive(row.1) && after.get(session).is_some_and(|now| now.1 != row.1) {
                tally.live_takeovers += 1;
                tally.notes.push(format!(
                    "trial {index} round {round}: live lease {session} {row:?} -> {:?}",
                    after.get(session)
                ));
            }
        }
        // 5. Kill all three at random boundaries, or stop cleanly in the last round.
        if round < 2 {
            daemon.send("ARM").await;
            for owner in owners.iter_mut() {
                owner.send("ARM").await;
            }
            let mut plan: Vec<(u64, usize)> = (0..3).map(|i| (rng.below(1200), i)).collect();
            plan.sort();
            tally.plan.push(format!("r{round}:kill={plan:?}"));
            let start = Instant::now();
            for (at, who) in plan {
                let wait = Duration::from_millis(at).saturating_sub(start.elapsed());
                tokio::time::sleep(wait).await;
                let kid = if who == 0 {
                    &mut daemon
                } else {
                    &mut owners[who - 1]
                };
                if kid.running() {
                    tally.kills += 1;
                    kid.sigkill().await;
                } else {
                    tally.failpoint_deaths += 1;
                }
            }
        } else {
            tokio::time::sleep(Duration::from_millis(200 + rng.below(400))).await;
            daemon.send("STOP").await;
            for owner in owners.iter_mut() {
                owner.send("STOP").await;
            }
        }
        dead_owner_pids = owner_pids.clone();
        let (_, lines) = daemon.finish(Duration::from_secs(30)).await;
        for line in lines.iter().filter(|l| l.starts_with("LOADFAIL|")) {
            tally.load_failures += 1;
            tally
                .notes
                .push(format!("trial {index} round {round}: {line}"));
        }
        logs.lines.push(("daemon".into(), lines));
        for owner in owners {
            let (role, lines) = owner.finish(Duration::from_secs(30)).await;
            for line in lines.iter().filter(|l| l.starts_with("RESUMEFAIL|")) {
                tally.resume_failures += 1;
                tally
                    .notes
                    .push(format!("trial {index} round {round}: {role} {line}"));
            }
            logs.lines.push((role, lines));
        }
    }
    // 6. One more start: recovery after a clean stop must find nothing.
    let mut verify = Kid::spawn("verify", &base(3, vec![("E1_VERIFY", "1".into())]));
    if verify.wait("READY|", 1, Duration::from_secs(30)).await {
        let line = verify
            .lines()
            .into_iter()
            .find(|l| l.starts_with("READY|"))
            .unwrap();
        let turns: u64 = line.split('|').nth(2).unwrap_or("0").parse().unwrap_or(0);
        tally.verify_turns_ended += turns;
    }
    verify.send("STOP").await;
    let _ = verify.finish(Duration::from_secs(30)).await;

    analyse(&dir, index, &logs, &mut tally);
    if tally.passed() && tally.trial_failures == 0 {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        tally.notes.push(format!(
            "trial {index} (seed {seed}) kept at {}",
            dir.display()
        ));
    }
    tally
}

/// One Runtime turn as the Event Log records it.
#[derive(Default, Debug)]
struct LogTurn {
    session: String,
    token: Option<String>,
    ends: Vec<String>,
}

fn analyse(dir: &Path, index: u64, logs: &RoundLog, tally: &mut Tally) {
    // What the processes said.
    let mut sent: HashMap<String, String> = HashMap::new(); // token -> session
    let mut acked: HashMap<String, String> = HashMap::new(); // token -> outcome
    for (role, lines) in &logs.lines {
        for line in lines {
            let parts: Vec<&str> = line.split('|').collect();
            match parts.first() {
                Some(&"SENT") => {
                    sent.insert(parts[2].to_owned(), parts[1].to_owned());
                }
                Some(&"ACK") => {
                    acked.insert(parts[2].to_owned(), parts[3].to_owned());
                }
                Some(&"PERR") => tally.prompt_errors += 1,
                Some(&"PROBE") => {
                    tally.probes += 1;
                    if parts[2] == "refused" {
                        tally.probes_refused += 1;
                    } else {
                        tally.live_takeovers += 1;
                        tally.notes.push(format!("trial {index}: {role} {line}"));
                    }
                }
                _ => {}
            }
        }
    }
    // Side effects.
    let mut effects: HashMap<String, u64> = HashMap::new();
    for line in std::fs::read_to_string(dir.join("effects.log"))
        .unwrap_or_default()
        .lines()
    {
        *effects.entry(line.trim().to_owned()).or_default() += 1;
    }
    for (tok, n) in &effects {
        if *n > 1 {
            tally.duplicate_effects += 1;
            tally
                .notes
                .push(format!("trial {index}: side effect {tok} ran {n} times"));
        }
    }
    // The Session tree.
    let conn = sessions_db(dir);
    let mut tree: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT session_id, payload FROM session_entries ORDER BY session_id, entry_seq",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for row in rows {
            let (session, payload) = row.unwrap();
            tree.entry(session).or_default().push_str(&payload);
        }
    }
    let in_tree =
        |session: &str, needle: &str| tree.get(session).is_some_and(|t| t.contains(needle));
    // The Event Log, grouped into turns.
    let events = rusqlite::Connection::open(dir.join("agent/events.db")).unwrap();
    events.busy_timeout(Duration::from_secs(10)).unwrap();
    let mut turns: Vec<LogTurn> = Vec::new();
    let mut open: HashMap<(String, String), usize> = HashMap::new();
    let mut prompted: HashMap<String, Option<String>> = HashMap::new(); // session -> token
    let mut logged_tokens: HashSet<String> = HashSet::new();
    {
        let mut stmt = events
            .prepare("SELECT session_key, kind, payload FROM events WHERE session_key IS NOT NULL ORDER BY global_seq")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (session, kind, payload) = row.unwrap();
            let value: Value = serde_json::from_str(&payload).unwrap();
            if kind == "prompt_sent" {
                let tok = tokens_in(&payload).into_iter().next();
                prompted.insert(session.clone(), tok);
                continue;
            }
            if kind != "update" {
                continue;
            }
            let update = &value["event"]["payload"]["update"];
            let turn_id = update["turn_id"].as_str().unwrap_or("").to_owned();
            match update["type"].as_str() {
                Some("turn_started") => {
                    let tok = prompted.get_mut(&session).and_then(Option::take);
                    if let Some(tok) = &tok {
                        logged_tokens.insert(tok.clone());
                    }
                    turns.push(LogTurn {
                        session: session.clone(),
                        token: tok,
                        ends: Vec::new(),
                    });
                    open.insert((session, turn_id), turns.len() - 1);
                }
                Some("turn_ended") => {
                    let stop = update["stop"].as_str().unwrap_or("?").to_owned();
                    match open.get(&(session.clone(), turn_id.clone())) {
                        Some(&i) => turns[i].ends.push(stop),
                        None => tally.notes.push(format!(
                            "trial {index}: turn_ended {stop} for unknown turn {session}/{turn_id}"
                        )),
                    }
                }
                _ => {}
            }
        }
    }
    // Every logged turn has exactly one end; recovery never ends one twice.
    for turn in &turns {
        let restarted = turn
            .ends
            .iter()
            .filter(|s| *s == "runtime_restarted")
            .count();
        if turn.ends.len() > 1 || restarted > 1 {
            tally.duplicate_recovery_records += 1;
            tally.notes.push(format!(
                "trial {index}: turn {:?} has ends {:?}",
                turn.token, turn.ends
            ));
        }
        if turn.ends.is_empty() {
            tally.interrupted_not_uncertain += 1;
            tally.notes.push(format!(
                "trial {index}: turn {:?} in {} never ended",
                turn.token, turn.session
            ));
        }
    }
    let by_token: HashMap<&str, &LogTurn> = turns
        .iter()
        .filter_map(|t| t.token.as_deref().map(|k| (k, t)))
        .collect();
    let daemon_sessions: HashSet<&str> = turns.iter().map(|t| t.session.as_str()).collect();
    for (tok, session) in &sent {
        let daemon = tok.starts_with('d');
        let done = in_tree(session, &format!("done {tok}"));
        let effect = effects.get(tok).copied().unwrap_or(0);
        let ack = acked.get(tok);
        let completed = ack.is_some_and(|a| a == "completed");
        if daemon {
            let logged = by_token.get(tok.as_str());
            let log_ok =
                logged.is_some_and(|t| t.ends.first().map(String::as_str) == Some("end_turn"));
            if completed {
                tally.acked_daemon += 1;
                if !done || !log_ok || effect != 1 {
                    tally.acked_lost += 1;
                    tally.notes.push(format!(
                        "trial {index}: acked {tok} lost: tree={done} log_end_turn={log_ok} effects={effect}"
                    ));
                }
                continue;
            }
            if ack.is_some() {
                continue; // cancelled or failed and said so
            }
            tally.interrupted_daemon += 1;
            match logged {
                Some(turn)
                    if turn.ends.first().map(String::as_str) == Some("runtime_restarted") =>
                {
                    tally.interrupted_marked_uncertain += 1
                }
                Some(turn) if turn.ends.first().map(String::as_str) == Some("end_turn") && done => {
                    tally.interrupted_completed_unacked += 1
                }
                Some(turn) if turn.ends.first().map(String::as_str) == Some("end_turn") => {
                    tally.interrupted_not_uncertain += 1;
                    tally.log_ok_tree_missing += 1;
                    tally.notes.push(format!(
                        "trial {index}: {tok} ended end_turn in the log, tree has no reply (effects={effect}), not marked uncertain"
                    ));
                }
                Some(turn) => {
                    if !turn.ends.is_empty() {
                        tally.notes.push(format!(
                            "trial {index}: {tok} interrupted, ended {:?}",
                            turn.ends
                        ));
                    }
                }
                None if effect > 0 || in_tree(session, tok) => {
                    tally.interrupted_not_uncertain += 1;
                    tally.effect_without_turn_record += 1;
                    tally.notes.push(format!(
                        "trial {index}: {tok} ran (effects={effect}, tree={}) but the log has no turn for it",
                        in_tree(session, tok)
                    ));
                }
                None => tally.interrupted_never_started += 1,
            }
            let _ = logged_tokens.contains(tok);
        } else {
            debug_assert!(!daemon_sessions.contains(session.as_str()));
            if completed {
                tally.acked_embedded += 1;
                if !done || effect != 1 {
                    tally.acked_lost += 1;
                    tally.notes.push(format!(
                        "trial {index}: embedded acked {tok} lost: tree={done} effects={effect}"
                    ));
                }
                continue;
            }
            if ack.is_some() {
                continue;
            }
            tally.interrupted_embedded += 1;
            let any = in_tree(session, tok);
            if done {
                tally.embedded_completed_unacked += 1;
            } else if any {
                tally.embedded_partial_tree += 1;
            } else if effect > 0 {
                tally.embedded_unrecorded_effects += 1;
            }
        }
    }
}

#[test]
#[ignore = "E1 probe: long; run explicitly"]
fn e1_recoverable_truth() {
    let trials: u64 = env("E1_TRIALS").parse().unwrap_or(200);
    let parallel: usize = env("E1_PARALLEL").parse().unwrap_or(4);
    let budget = Duration::from_secs(env("E1_BUDGET_SECS").parse().unwrap_or(1500));
    let seed: u64 = env("E1_SEED").parse().unwrap_or_else(|_| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });
    let root = match env("E1_ROOT").as_str() {
        "" => std::env::temp_dir().join(format!("loop-e1-{seed}")),
        path => PathBuf::from(path),
    };
    std::fs::create_dir_all(&root).unwrap();
    println!(
        "E1 seed={seed} trials={trials} parallel={parallel} root={}",
        root.display()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let started = Instant::now();
    let total = runtime.block_on(async {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallel));
        let total = Arc::new(Mutex::new(Tally::default()));
        let mut tasks = Vec::new();
        for index in 0..trials {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            if started.elapsed() > budget {
                drop(permit);
                println!("E1 budget reached after {index} trials started");
                break;
            }
            let trial_seed = Rng(seed ^ index.wrapping_mul(0x9E37_79B9)).next();
            let (root, total) = (root.clone(), total.clone());
            tasks.push(tokio::spawn(async move {
                let t0 = Instant::now();
                let tally = trial(root, index, trial_seed).await;
                println!(
                    "E1 trial {index} seed={trial_seed} {:.1}s pass={} acked={}+{} interrupted={}/{} uncertain={} kills={} fp={}",
                    t0.elapsed().as_secs_f64(),
                    tally.passed(),
                    tally.acked_daemon,
                    tally.acked_embedded,
                    tally.interrupted_daemon,
                    tally.interrupted_embedded,
                    tally.interrupted_marked_uncertain,
                    tally.kills,
                    tally.failpoint_deaths,
                );
                if !tally.passed() {
                    println!("  plan: {}", tally.plan.join(" "));
                }
                for note in &tally.notes {
                    println!("  note: {note}");
                }
                total.lock().unwrap().add(&tally);
                drop(permit);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let total = total.lock().unwrap().clone();
        total
    });
    let mut summary = serde_json::to_value(&total).unwrap();
    summary["notes"] = json!(total.notes.len());
    summary["seed"] = json!(seed);
    summary["seconds"] = json!(started.elapsed().as_secs());
    println!(
        "E1 SUMMARY {}",
        serde_json::to_string_pretty(&summary).unwrap()
    );
    println!("E1 PASS={}", total.passed());
    if total.passed() && total.trial_failures == 0 {
        let _ = std::fs::remove_dir_all(&root);
    }
    assert!(total.passed(), "E1 criteria failed; see notes above");
}
