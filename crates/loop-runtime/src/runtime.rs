//! Session ownership, routing and committed update publication.
use crate::log::{Event, EventKind, EventLog};
use async_trait::async_trait;
use loop_agent::{
    harness::{
        create_session_repository,
        session::{
            sqlite::{LeaseIdentity, SqliteSessionStore},
            SessionStore,
        },
        types::SessionError,
        Sandbox,
    },
    native_backend::NATIVE_INSTANCE,
};
use loop_app_core::{
    permissions::workspace_root,
    runtime::{BootstrapOpts, GlobalResources, OpenedSession, SessionFactory, WorkspaceResources},
};
use loop_protocol::{
    rpc::{SessionInfo, SessionSnapshot},
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, Compaction,
    CompactionReport, ConfigChanged, ContentBlock, ErrorCategory, LocalShell, ModelChoices,
    ModelControl, MutationResult, Origin, SessionKey, SessionMetadata, SessionRef, SessionSpec,
    SessionStats, SessionUpdate, ShellResult, StatsSummary, TurnHandle, TurnOutcome,
    UpdateEnvelope,
};
use loop_wire::control::EscalationRecipient;
use publish::{barrier, fail};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
    task::{AbortHandle, JoinHandle},
};

/// Admission limits. Active turns queue when all permits are occupied.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    pub max_sessions: usize,
    pub max_active_turns: usize,
}
impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_sessions: 16,
            max_active_turns: 8,
        }
    }
}

mod subagent;
#[cfg(test)]
mod subagent_tests;
#[cfg(test)]
mod unload_tests;

mod admin;
#[cfg(test)]
mod admin_tests;
mod attach;
mod leases;
mod pause;
mod publish;
mod sessions;
mod shell;
mod tree;
mod turns;

/// The host's opaque, stable id for the workspace at canonical `cwd`.
pub fn workspace_id(cwd: &std::path::Path) -> String {
    tree::workspace_id(cwd)
}

/// One Runtime owns all live native Sessions and their durable update streams.
pub struct Runtime {
    instance: BackendInstanceId,
    factory: SessionFactory,
    defaults: BootstrapOpts,
    sessions: Mutex<HashMap<SessionKey, Arc<LiveSession>>>,
    workspaces: Mutex<HashMap<PathBuf, WorkspaceState>>,
    recipients: AsyncMutex<HashMap<SessionKey, Recipients>>,
    admission: AsyncMutex<()>,
    stopped: AtomicBool,
    idle_epoch: std::sync::atomic::AtomicU64,
    /// In-memory only: caller environments must never enter persistent logs or settings.
    launch_specs: Mutex<HashMap<SessionKey, SessionSpec>>,
    /// Tests start their own sandboxes instead of a platform one.
    #[cfg(test)]
    sandbox_starter: Mutex<Option<admin::SandboxStarter>>,
    log: Arc<dyn EventLog>,
    generation: u64,
    limits: RuntimeLimits,
    turns: Arc<Semaphore>,
    store: Arc<SqliteSessionStore>,
    identity: LeaseIdentity,
    updates: broadcast::Sender<UpdateEnvelope>,
    activity: Activity,
    close_grace: Duration,
    /// Set by [`Runtime::into_shared`]; the `subagent` tool calls back through it.
    me: OnceLock<Weak<Runtime>>,
    subagent_depth: u32,
    /// Sessions the Runtime opened as Subagents, read from the Event Log once.
    subagents: AsyncMutex<Option<HashSet<SessionKey>>>,
    /// Sessions unloaded to make room and not loaded since. A prompt or an
    /// attach for one loads it again instead of failing.
    unloaded: Mutex<HashSet<SessionKey>>,
    /// Connections attached to each Session, whether or not they answer
    /// Escalations. A watched Session is never unloaded.
    watchers: Mutex<HashMap<SessionKey, HashSet<u64>>>,
    views: Mutex<HashMap<String, tree::BranchView>>,
    destinations: Arc<AsyncMutex<()>>,
    pauses: Arc<pause::Pauses>,
    /// Serializes workspace admin changes (settings and trust writes, reloads).
    admin: AsyncMutex<()>,
    /// Reloads hold the write side; an opening Session publishes under the
    /// read side, so it either is seen by a reload or adopts its result.
    reload_gate: tokio::sync::RwLock<()>,
    /// Counts saved changes to new-Session defaults (`runtime/defaults`).
    defaults_revision: std::sync::atomic::AtomicU64,
}
/// Newest event timestamp per Session, kept in memory so listing never scans the log.
type Activity = Arc<Mutex<HashMap<SessionKey, i64>>>;
type Recipients = HashMap<u64, Arc<dyn EscalationRecipient>>;
/// Surfaces that should see Escalations of the first Session in `lineage`:
/// those attached to it or to any Session above it.
fn wanted(
    recipients: &HashMap<SessionKey, Recipients>,
    lineage: &[SessionKey],
) -> HashMap<u64, Arc<dyn EscalationRecipient>> {
    let mut wanted = HashMap::new();
    for key in lineage {
        for (&connection, recipient) in recipients.get(key).into_iter().flatten() {
            wanted
                .entry(connection)
                .or_insert_with(|| recipient.clone());
        }
    }
    wanted
}
fn touch(activity: &Activity, key: &SessionKey, ts_ms: i64) {
    let mut map = activity.lock().unwrap();
    let slot = map.entry(key.clone()).or_insert(ts_ms);
    *slot = (*slot).max(ts_ms);
}
/// How long a close waits for each settling step before giving up on a stuck turn.
const CLOSE_GRACE: Duration = Duration::from_secs(10);
struct WorkspaceState {
    resources: Arc<WorkspaceResources>,
    sandbox: Option<Arc<dyn Sandbox>>,
    /// The current generation, held while it is current.
    generation: Arc<admin::Generation>,
    /// Counts committed admin changes and reloads of this workspace.
    revision: u64,
}
struct LiveSession {
    /// Serializes the decisions that change who may use this Session:
    /// unloading, closing, prompt admission and attaching. Whoever holds it
    /// sees `busy`, `closing` and the attached Surfaces settled.
    lifecycle: AsyncMutex<()>,
    committed: AsyncMutex<Option<loop_agent::harness::session::SessionFile>>,
    turn_start_seq: std::sync::atomic::AtomicU64,
    pauses: Arc<pause::Pauses>,
    opened: OpenedSession,
    escalations: crate::EscalationHost,
    reference: SessionRef,
    busy: AtomicBool,
    closing: AtomicBool,
    failure: Mutex<Option<String>>,
    permit: AsyncMutex<Option<OwnedSemaphorePermit>>,
    cancel: watch::Sender<u64>,
    commands: mpsc::UnboundedSender<Command>,
    pump: AsyncMutex<Option<JoinHandle<()>>>,
    turn_task: Mutex<Option<AbortHandle>>,
    activity: Activity,
    tail: Mutex<VecDeque<UpdateEnvelope>>,
    /// Subagent nesting: 0 for a Session a Surface opened.
    depth: u32,
    /// The Session that opened this one as a Subagent.
    parent: Option<SessionKey>,
    /// The directory the gate treats as this Session's workspace.
    workspace: PathBuf,
    /// Present on a Subagent: its answer and model-call count.
    child: Option<Arc<subagent::ChildState>>,
    /// Subagent calls of the current turn that are waiting on a child.
    waiting: AsyncMutex<usize>,
    /// The cancellation count when the current turn began.
    turn_epoch: std::sync::atomic::AtomicU64,
    /// The Session's config as last committed to the Event Log.
    announced: Mutex<Announced>,
    /// A workspace reload waiting for the current turn or command to end.
    pending_workspace: Mutex<Option<Arc<admin::Generation>>>,
    credential_notice: Mutex<Option<String>>,
    /// The workspace generation the Session uses, held until it switches.
    generation: Mutex<Option<Arc<admin::Generation>>>,
    /// The sandbox the Session runs in (`session/sandbox`).
    sandbox_choice: Mutex<loop_protocol::admin::SandboxChoice>,
    /// A sandbox chosen during a turn, started and waiting for the turn to
    /// end; the running tool calls keep the one they started in.
    pending_sandbox: Mutex<Option<admin::PendingSandbox>>,
}
/// What the pump has committed and published of one Session. The pump is
/// the only writer of a loaded Session's updates, so `published` and
/// `latest`, read together, are a consistent point in its stream.
struct Announced {
    /// The newest update sequence the pump committed and is publishing.
    published: u64,
    /// The newest committed config (or the config the Session opened with).
    latest: ConfigChanged,
    /// Receipts of committed changes whose setters have not read them yet:
    /// revision to sequence. Each setter takes its own, so any number may be
    /// outstanding while the Event Log is slow.
    receipts: HashMap<u64, u64>,
    /// The newest revision committed. A later update with the same revision
    /// (a pending change taking effect) is not a new change.
    revision: u64,
}
impl Announced {
    fn commit(&mut self, seq: u64, changed: &ConfigChanged) {
        if changed.revision > self.revision {
            self.revision = changed.revision;
            self.receipts.insert(changed.revision, seq);
        }
        self.latest = changed.clone();
    }
}
/// Drops a setter's receipt if the setter goes away before reading it.
struct Receipt<'a> {
    live: &'a LiveSession,
    revision: u64,
}
impl Receipt<'_> {
    fn take(self) -> Option<u64> {
        self.live
            .announced
            .lock()
            .unwrap()
            .receipts
            .remove(&self.revision)
    }
}
impl Drop for Receipt<'_> {
    fn drop(&mut self) {
        if let Ok(mut announced) = self.live.announced.lock() {
            announced.receipts.remove(&self.revision);
        }
    }
}
enum Command {
    Cancel,
    Publish(Box<SessionUpdate>, oneshot::Sender<BackendResult<()>>),
    Barrier(oneshot::Sender<()>),
    Stop(oneshot::Sender<()>),
}
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn failed(error: impl std::fmt::Display) -> BackendError {
    BackendError::failed(ErrorCategory::Internal, error.to_string())
}
fn reference(id: &str) -> SessionRef {
    SessionRef {
        instance: NATIVE_INSTANCE.into(),
        key: id.into(),
        native_id: id.into(),
    }
}
impl Runtime {
    /// Construct a Runtime over injected resources and an Event Log. The caller owns
    /// the process lock; crash recovery/reclamation belongs to the startup layer.
    pub async fn new(
        mut globals: GlobalResources,
        defaults: BootstrapOpts,
        log: Arc<dyn EventLog>,
        limits: RuntimeLimits,
    ) -> BackendResult<Self> {
        if limits.max_sessions == 0 || limits.max_active_turns == 0 {
            return Err(failed("Runtime limits must be positive"));
        }
        if !defaults.cwd.is_absolute() {
            return Err(failed("Runtime cwd must be absolute"));
        }
        let generation = log
            .allocate_generation("runtime", std::process::id(), now())
            .await
            .map_err(failed)?;
        let identity = LeaseIdentity {
            owner: "runtime".into(),
            pid: std::process::id(),
            generation: i64::try_from(generation).map_err(failed)?,
        };
        let store = Arc::new(SqliteSessionStore::open(&globals.sessions_db).map_err(failed)?);
        globals.repository =
            create_session_repository(Arc::new(store.for_lease(identity.clone())), None);
        log.append_batch(vec![Event {
            generation,
            session_key: None,
            turn: None,
            ts_ms: now(),
            kind: EventKind::RuntimeStarted(json!({"pid": std::process::id()})),
        }])
        .await
        .map_err(failed)?;
        let activity = Arc::new(Mutex::new(log.last_activity_ms().await.map_err(failed)?));
        let (updates, _) = broadcast::channel(4096);
        Ok(Self {
            instance: NATIVE_INSTANCE.into(),
            factory: SessionFactory::new(Arc::new(globals)),
            defaults,
            sessions: Mutex::new(HashMap::new()),
            workspaces: Mutex::new(HashMap::new()),
            recipients: AsyncMutex::new(HashMap::new()),
            admission: AsyncMutex::new(()),
            stopped: AtomicBool::new(false),
            idle_epoch: std::sync::atomic::AtomicU64::new(0),
            launch_specs: Mutex::new(HashMap::new()),
            #[cfg(test)]
            sandbox_starter: Mutex::new(None),
            log,
            generation,
            limits,
            turns: Arc::new(Semaphore::new(limits.max_active_turns)),
            store,
            identity,
            updates,
            activity,
            close_grace: CLOSE_GRACE,
            me: OnceLock::new(),
            subagent_depth: subagent::DEFAULT_DEPTH,
            subagents: AsyncMutex::new(None),
            unloaded: Mutex::new(HashSet::new()),
            watchers: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            destinations: Arc::default(),
            pauses: Arc::default(),
            admin: AsyncMutex::new(()),
            reload_gate: tokio::sync::RwLock::new(()),
            defaults_revision: std::sync::atomic::AtomicU64::new(0),
        })
    }
    /// Share the Runtime. Only a shared Runtime offers its Sessions the
    /// `subagent` tool, because the tool opens child Sessions through it.
    pub fn into_shared(self) -> Arc<Self> {
        Arc::new_cyclic(|me| {
            let _ = self.me.set(me.clone());
            self
        })
    }
    /// The deepest Subagent level (the root is 0); Sessions at it cannot
    /// spawn another child. Defaults to 2.
    pub fn with_subagent_depth(mut self, depth: u32) -> Self {
        self.subagent_depth = depth;
        self
    }
    /// Recover from the previous Runtime before accepting clients (design §12).
    ///
    /// Ends every turn an earlier generation left open with `runtime_restarted`
    /// and withdraws its pending Escalations. Nothing is retried. A second pass
    /// changes nothing. It reads only turn boundaries and Escalation records
    /// since the newest generation whose recovery committed.
    ///
    /// With `lock` (the lock of this Runtime's own Session database), leases of
    /// an earlier Runtime are reclaimed and released.
    ///
    /// Precondition: call before serving any Session; turns of this generation
    /// are never ended here.
    pub async fn recover(
        &self,
        lock: Option<&loop_app_core::runtime_instance::SessionDatabaseLock>,
    ) -> BackendResult<crate::Recovery> {
        use crate::recovery::{unfinished, REASON};
        if let Some(lock) = lock {
            let ours = std::path::absolute(&self.factory.globals.sessions_db).map_err(failed)?;
            if lock.sessions_db() != ours {
                return Err(failed(format!(
                    "the held lock is for {}, not this Runtime's Session database {}",
                    lock.sessions_db().display(),
                    ours.display()
                )));
            }
        }
        let from = self.log.last_recovered_generation().await.map_err(failed)?;
        let records = self.log.recovery_records(from).await.map_err(failed)?;
        let (turns, escalations) = unfinished(&records, self.generation);
        let ts_ms = now();
        let mut events = Vec::with_capacity(2 * turns.len() + escalations.len());
        for turn in &turns {
            let update = |update| {
                Event::update(
                    self.generation,
                    ts_ms,
                    UpdateEnvelope::live(
                        self.instance.clone(),
                        self.generation,
                        turn.session.clone(),
                        Some(turn.turn_id.clone()),
                        0,
                        update,
                    ),
                )
            };
            // A prompt whose turn start never committed may still have run
            // tools: give it a turn, so it can end uncertain.
            if turn.unstarted {
                events.push(update(SessionUpdate::TurnStarted {
                    turn_id: turn.turn_id.clone(),
                }));
            }
            events.push(update(SessionUpdate::TurnEnded {
                turn_id: turn.turn_id.clone(),
                stop: loop_protocol::StopReason::RuntimeRestarted,
                error: None,
            }));
        }
        for open in &escalations {
            events.push(Event {
                generation: self.generation,
                session_key: Some(open.session.clone()),
                turn: None,
                ts_ms,
                kind: EventKind::EscalationWithdrawn(
                    json!({"id": open.id, "generation": open.generation, "reason": REASON}),
                ),
            });
        }
        for chunk in events.chunks(crate::log::MAX_BATCH_EVENTS) {
            for event in chunk {
                if let Some(key) = &event.session_key {
                    touch(&self.activity, key, ts_ms);
                }
            }
            self.log
                .append_batch(chunk.to_vec())
                .await
                .map_err(failed)?;
        }
        self.log
            .mark_recovered(self.generation, now())
            .await
            .map_err(failed)?;
        let mut leases_reclaimed = 0;
        let mut leases_swept = 0;
        if let Some(lock) = lock {
            // Terminal `loop` processes that died without releasing. A failed
            // sweep must not keep the Runtime from starting.
            match self
                .store
                .sweep_stale_leases(Some(lock.sessions_db()))
                .await
            {
                Ok(swept) => leases_swept = swept,
                Err(error) => eprintln!("Runtime recovery: lease sweep: {error}"),
            }
        }
        if lock.is_some() {
            for meta in self.store.list(None).await.map_err(failed)? {
                let Some(lease) = self.store.lease(&meta.id).await.map_err(failed)? else {
                    continue;
                };
                if lease.identity.owner != "runtime" || lease.identity == self.identity {
                    continue;
                }
                // One Session's lease must not keep the Runtime from starting;
                // opening it later reports the conflict.
                let reclaimed = async {
                    self.store
                        .takeover_lease(&meta.id, &self.identity, true)
                        .await?;
                    self.store.release_lease(&meta.id, &self.identity).await
                }
                .await;
                match reclaimed {
                    Ok(()) => leases_reclaimed += 1,
                    Err(error) => {
                        eprintln!("Runtime recovery: lease of Session {}: {error}", meta.id)
                    }
                }
            }
        }
        Ok(crate::Recovery {
            turns_ended: turns.len(),
            escalations_withdrawn: escalations.len(),
            leases_reclaimed,
            leases_swept,
        })
    }
    /// Bound each step of closing a Session whose turn ignores cancellation.
    pub fn with_close_grace(mut self, grace: Duration) -> Self {
        self.close_grace = grace;
        self
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// This process, its limits and how many Sessions are loaded and busy.
    /// `socket` is empty: the process serving the Runtime fills it in.
    pub fn status(&self) -> loop_protocol::rpc::RuntimeStatus {
        let sessions = self.sessions.lock().unwrap();
        let busy = sessions
            .values()
            .filter(|s| s.busy.load(Ordering::Acquire))
            .count();
        loop_protocol::rpc::RuntimeStatus {
            auto_started: false,
            pid: std::process::id(),
            socket: PathBuf::new(),
            listen_url: None,
            generation: self.generation,
            loaded: sessions.len() as u64,
            busy: busy as u64,
            max_sessions: self.limits.max_sessions as u64,
            max_active_turns: self.limits.max_active_turns as u64,
            max_sessions_from: None,
            max_active_turns_from: None,
        }
    }
    pub fn event_log(&self) -> &Arc<dyn EventLog> {
        &self.log
    }
    fn route(&self, session: &SessionRef) -> BackendResult<Arc<LiveSession>> {
        if session.instance != self.instance || session.key.0 != session.native_id {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        self.sessions
            .lock()
            .unwrap()
            .get(&session.key)
            .cloned()
            .ok_or_else(|| BackendError::SessionNotFound(session.key.clone()))
    }
    /// An auto-started owner may stop only after every Surface and work item leaves.
    pub async fn is_idle(&self) -> bool {
        let _admission = self.admission.lock().await;
        if self
            .watchers
            .lock()
            .unwrap()
            .values()
            .any(|set| !set.is_empty())
        {
            return false;
        }
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        for live in sessions {
            if live.busy.load(Ordering::Acquire) || live.escalations.has_pending().await {
                return false;
            }
        }
        true
    }

    /// Changes whenever work or a Surface enters, including between idle polls.
    pub fn idle_activity(&self) -> u64 {
        self.idle_epoch.load(Ordering::Acquire)
    }

    async fn credential_notice(
        &self,
        model: Option<&str>,
        env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Option<String> {
        let (provider, _) = model?.split_once('/')?;
        let preset = loop_ai::providers::PROVIDER_PRESETS
            .iter()
            .find(|preset| preset.id == provider)?;
        let exported = preset
            .api_key_envs
            .iter()
            .find_map(|key| env?.get(*key).filter(|value| !value.trim().is_empty()))?;
        let configured = self.factory.globals.models.get_provider(provider)?;
        let actual = loop_ai::auth::resolve_provider_auth(
            provider,
            &configured.auth,
            self.factory.globals.credentials.as_ref(),
            None,
        )
        .await
        .ok();
        if actual.as_ref().and_then(|auth| auth.api_key.as_deref()) == Some(exported.as_str()) {
            return None;
        }
        Some(format!("This shell's {provider} API key differs from the Runtime's credential; the Runtime credential is used."))
    }

    /// This live Session and the live Sessions above it, nearest first.
    fn lineage(&self, key: &SessionKey) -> Vec<SessionKey> {
        let sessions = self.sessions.lock().unwrap();
        let mut lineage = vec![key.clone()];
        let mut next = sessions.get(key).and_then(|live| live.parent.clone());
        while let Some(key) = next.filter(|key| !lineage.contains(key)) {
            next = sessions.get(&key).and_then(|live| live.parent.clone());
            lineage.push(key);
        }
        lineage
    }
    /// The parent of a Session the Runtime opened as a Subagent. Forks also
    /// record a parent, so only the Event Log's record of the child's opening
    /// tells them apart; the log is scanned only for Sessions with a parent.
    async fn subagent_parent(&self, session: &SessionRef) -> BackendResult<Option<String>> {
        let known = self
            .factory
            .globals
            .repository
            .list(None)
            .await
            .map_err(failed)?;
        let Some(parent) = known
            .into_iter()
            .find(|m| m.id == session.native_id)
            .and_then(|m| m.parent_session_id)
        else {
            return Ok(None);
        };
        let child = self.subagent_keys().await?.contains(&session.key);
        Ok(child.then_some(parent))
    }
    /// Keys of every Session opened as a Subagent. The Event Log is asked
    /// once; children opened later are added as they open.
    async fn subagent_keys(&self) -> BackendResult<HashSet<SessionKey>> {
        let mut known = self.subagents.lock().await;
        if let Some(keys) = known.as_ref() {
            return Ok(keys.clone());
        }
        let keys = self.log.subagent_sessions().await.map_err(failed)?;
        *known = Some(keys.clone());
        Ok(keys)
    }
    /// Read all known Sessions without acquiring their leases.
    pub async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        let known = self
            .factory
            .globals
            .repository
            .list(None)
            .await
            .map_err(failed)?;
        // Forks also record a parent; only Subagents report one here.
        let subagents = self.subagent_keys().await?;
        let mut summaries = HashMap::new();
        let mut owners = HashMap::new();
        for meta in &known {
            let reader = self.store.load(&meta.id).await.map_err(failed)?;
            let entries = reader.read_entries(None).await.map_err(failed)?;
            summaries.insert(
                meta.id.clone(),
                loop_agent::harness::session::summarize_session(meta, &entries),
            );
            owners.insert(
                meta.id.clone(),
                self.store.lease(&meta.id).await.map_err(failed)?,
            );
        }
        let activity = self.activity.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        Ok(known
            .into_iter()
            .map(|meta| {
                let r = reference(&meta.id);
                let subagent = subagents.contains(&r.key);
                let live = sessions.get(&r.key);
                let last_active_ms = activity
                    .get(&r.key)
                    .copied()
                    .unwrap_or(meta.created_at)
                    .max(0) as u64;
                let summary = summaries.remove(&meta.id).unwrap();
                let owner = owners.remove(&meta.id).flatten();
                let foreign = owner
                    .as_ref()
                    .is_some_and(|owner| owner.identity != self.identity);
                SessionInfo {
                    summary: loop_protocol::rpc::SessionListSummary {
                        workspace_id: meta
                            .cwd
                            .as_ref()
                            .map(|cwd| tree::workspace_id(std::path::Path::new(cwd))),
                        backend: Some("native".into()),
                        first_prompt: summary.title,
                        message_count: Some(summary.message_count as u64),
                        selected_model: summary.model,
                        owner_kind: owner.map(|owner| owner.identity.owner),
                        availability: Some(
                            if foreign {
                                "owned_elsewhere"
                            } else if live.is_some() {
                                "loaded"
                            } else {
                                "loadable"
                            }
                            .into(),
                        ),
                        fork_source: meta
                            .parent_session_id
                            .as_ref()
                            .filter(|_| !subagent)
                            .map(|id| reference(id)),
                    },
                    r#ref: r,
                    cwd: meta.cwd.map(PathBuf::from).unwrap_or_default(),
                    name: summary.name,
                    parent: meta
                        .parent_session_id
                        .filter(|_| subagent)
                        .map(|s| reference(&s)),
                    live: live.is_some(),
                    busy: live.is_some_and(|s| s.busy.load(Ordering::Acquire)),
                    created_ms: meta.created_at.max(0) as u64,
                    last_active_ms,
                }
            })
            .collect())
    }
    /// Stop admission, settle work, drain pumps and release all Session leases.
    /// Sessions are cancelled together and settle concurrently, each bounded.
    pub async fn shutdown(&self) {
        {
            let _admission = self.admission.lock().await;
            if self.stopped.swap(true, Ordering::AcqRel) {
                return;
            }
        }
        self.finish_shutdown().await;
    }

    /// Version replacement must not close a Session admitted after discovery.
    pub async fn shutdown_if_empty(&self) -> bool {
        {
            let _admission = self.admission.lock().await;
            if !self.sessions.lock().unwrap().is_empty()
                || self
                    .watchers
                    .lock()
                    .unwrap()
                    .values()
                    .any(|set| !set.is_empty())
            {
                return false;
            }
            if self.stopped.swap(true, Ordering::AcqRel) {
                return false;
            }
        }
        self.finish_shutdown().await;
        true
    }

    /// Fence admissions and watched Sessions before an automatic idle shutdown.
    pub async fn shutdown_if_idle(&self) -> bool {
        {
            let _admission = self.admission.lock().await;
            let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
            let mut guards = Vec::new();
            for live in &sessions {
                guards.push(live.lifecycle.lock().await);
            }
            if sessions
                .iter()
                .any(|live| live.busy.load(Ordering::Acquire))
            {
                return false;
            }
            for live in &sessions {
                if live.escalations.has_pending().await {
                    return false;
                }
            }
            let watchers = self.watchers.lock().unwrap();
            if watchers.values().any(|set| !set.is_empty())
                || self.stopped.swap(true, Ordering::AcqRel)
            {
                return false;
            }
        }
        self.finish_shutdown().await;
        true
    }

    async fn finish_shutdown(&self) {
        self.turns.close();
        let keys: Vec<_> = self.sessions.lock().unwrap().keys().cloned().collect();
        futures::future::join_all(keys.iter().map(|key| async move {
            if let Err(error) = self.close_inner(key, true).await {
                eprintln!("Runtime shutdown: {error}");
            }
        }))
        .await;
        let workspaces: Vec<_> = self
            .workspaces
            .lock()
            .unwrap()
            .drain()
            .map(|(_, value)| value)
            .collect();
        for workspace in workspaces {
            workspace.resources.mcp_client.disconnect_all().await;
            if let Some(sandbox) = workspace.sandbox {
                if let Err(error) = sandbox.destroy().await {
                    eprintln!("Runtime sandbox stop: {error}");
                }
            }
        }
        if let Err(error) = self
            .log
            .append_batch(vec![Event {
                generation: self.generation,
                session_key: None,
                turn: None,
                ts_ms: now(),
                kind: EventKind::RuntimeStopped(json!({})),
            }])
            .await
        {
            eprintln!("Runtime stop log: {error}");
        }
        if let Err(error) = self.log.finish_generation(self.generation, now()).await {
            eprintln!("Runtime generation stop: {error}");
        }
    }
}
#[async_trait]
impl loop_wire::RuntimeControl for Runtime {
    async fn config(&self, p: loop_protocol::rpc::SessionParams) -> BackendResult<SessionSnapshot> {
        self.snapshot(&p.session).await
    }

    async fn skill(
        &self,
        p: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<MutationResult> {
        let session = p.session.clone();
        self.set_config(&session, false, |live| async move {
            live.opened.native.skill(p).await
        })
        .await
    }
    #[cfg(feature = "orchestration")]
    async fn workflow(
        &self,
        p: loop_protocol::session_services::WorkflowParams,
    ) -> BackendResult<loop_protocol::session_services::WorkflowResult> {
        let live = self.loaded(&p.session).await?;
        if let Some(result) = live.opened.native.known_workflow(&p)? {
            return Ok(result);
        }
        let result = {
            let _lifecycle = live.lifecycle.lock().await;
            self.check_tree_live(&live)?;
            self.reserve(&live).await?;
            match live.opened.native.workflow(p).await {
                Ok(result) => result,
                Err(error) => {
                    live.busy.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        };
        let id = result.workflow_id.clone();
        tokio::spawn(async move {
            live.opened.native.wait_workflow(&id).await;
            let _ = barrier(&live).await;
            let _lifecycle = live.lifecycle.lock().await;
            live.busy.store(false, Ordering::Release);
            live.apply_pending().await;
        });
        Ok(result)
    }

    async fn tree(
        &self,
        params: loop_protocol::rpc::TreeParams,
    ) -> BackendResult<loop_protocol::rpc::TreeResult> {
        Runtime::tree(self, params).await
    }
    async fn view(
        &self,
        params: loop_protocol::rpc::ViewParams,
    ) -> BackendResult<loop_protocol::rpc::ViewResult> {
        Runtime::view(self, params).await
    }
    async fn navigate(
        &self,
        params: loop_protocol::rpc::NavigateParams,
    ) -> BackendResult<loop_protocol::rpc::NavigateResult> {
        Runtime::navigate(self, params).await
    }
    async fn rename(
        &self,
        params: loop_protocol::rpc::RenameParams,
    ) -> BackendResult<loop_protocol::rpc::MutationResult> {
        Runtime::rename(self, params).await
    }
    async fn fork(
        &self,
        params: loop_protocol::rpc::ForkParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        Runtime::fork(self, params).await
    }
    async fn clone_session(
        &self,
        params: loop_protocol::rpc::CloneParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        Runtime::clone_session(self, params).await
    }
    async fn export(
        &self,
        params: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<loop_protocol::rpc::ExportResult> {
        Runtime::export(self, &params.session).await
    }
    async fn import(
        &self,
        params: loop_protocol::rpc::ImportParams,
    ) -> BackendResult<loop_protocol::rpc::DestinationResult> {
        Runtime::import(self, params).await
    }
    async fn recipient(
        &self,
        session: &SessionRef,
        connection: u64,
        recipient: Option<Arc<dyn loop_wire::control::EscalationRecipient>>,
    ) -> BackendResult<()> {
        let attached = recipient.is_some();
        self.register_watch(session, connection, attached, recipient)
            .await
    }
    async fn register(
        &self,
        session: &SessionRef,
        connection: u64,
        attached: bool,
        recipient: Option<Arc<dyn loop_wire::control::EscalationRecipient>>,
    ) -> BackendResult<()> {
        self.register_watch(session, connection, attached, recipient)
            .await
    }
    async fn attach_watching(
        &self,
        connection: u64,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<loop_wire::Attachment> {
        self.attach_watched(connection, session, after_seq, false)
            .await
    }
    async fn attach_snapshot_watching(
        &self,
        connection: u64,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<loop_wire::Attachment> {
        self.attach_watched(connection, session, after_seq, true)
            .await
    }
    async fn disconnected(&self, connection: u64) {
        self.watchers.lock().unwrap().retain(|_, watching| {
            watching.remove(&connection);
            !watching.is_empty()
        });
        self.disconnect_recipients(connection).await;
    }

    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        Runtime::list(self).await
    }
    fn admin(&self) -> Option<&dyn loop_wire::RuntimeAdmin> {
        Some(self)
    }
    async fn status(&self) -> BackendResult<loop_protocol::rpc::RuntimeStatus> {
        Ok(Runtime::status(self))
    }
    async fn close(&self, session: &SessionRef, force: bool) -> BackendResult<()> {
        if session.instance != self.instance || session.key.0 != session.native_id {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        Runtime::close(self, &session.key, force).await
    }
    async fn attach(
        &self,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<loop_wire::Attachment> {
        if session.instance != self.instance || session.key.0 != session.native_id {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let live = self.sessions.lock().unwrap().contains_key(&session.key);
        if !live {
            let known = self
                .factory
                .globals
                .repository
                .list(None)
                .await
                .map_err(failed)?;
            if !known.iter().any(|m| m.id == session.native_id) {
                return Err(BackendError::SessionNotFound(session.key.clone()));
            }
        }
        // The caller already listens for live updates, so anything committed
        // after this read is delivered live; the caller removes the overlap.
        let snapshot = self.snapshot(session).await?;
        self.attachment(session, after_seq, snapshot).await
    }
}

impl Runtime {
    /// The Session's state at its newest published update. A loaded
    /// Session's comes from its pump without waiting for it, so a stalled
    /// Event Log does not hold up an attach; another one's is read from its
    /// tree.
    async fn snapshot(&self, session: &SessionRef) -> BackendResult<SessionSnapshot> {
        // The pump records each committed sequence and config together, so
        // they are a consistent point; the lifecycle lock orders the snapshot
        // with prompt admission and close. Updates committed after it are
        // delivered live; the caller removes the overlap by sequence.
        let committed = match self.route(session) {
            Ok(live) => {
                let _lifecycle = live.lifecycle.lock().await;
                Some(Self::announced(&live))
            }
            Err(_) => None,
        };
        let (at_seq, changed) = match committed {
            Some(committed) => committed,
            None => {
                let at_seq = self.log.latest_seq(&session.key).await.map_err(failed)?;
                (at_seq, self.recorded_config(session).await?)
            }
        };
        Ok(self.public_snapshot(session, at_seq, changed))
    }
    /// The newest published sequence and config. Caller holds the lifecycle lock.
    fn announced(live: &LiveSession) -> (u64, ConfigChanged) {
        let announced = live.announced.lock().unwrap();
        (announced.published, announced.latest.clone())
    }
    /// Public state at `at_seq`, without a transcript view.
    fn public_snapshot(
        &self,
        session: &SessionRef,
        at_seq: u64,
        changed: ConfigChanged,
    ) -> SessionSnapshot {
        SessionSnapshot {
            notices: self
                .route(session)
                .map(|live| {
                    let mut notices = live.opened.startup_notices.clone();
                    notices.extend(live.credential_notice.lock().unwrap().clone());
                    notices
                })
                .unwrap_or_default(),
            session: session.clone(),
            workspace_id: self
                .route(session)
                .map(|live| tree::workspace_id(&live.opened.workspace.cwd))
                .unwrap_or_default(),
            at_seq,
            revision: changed.revision,
            branch_revision: 0,
            view_id: None,
            metadata: changed.metadata,
            config: changed.config,
            effective: changed.effective,
            pending: changed.pending,
            capabilities: self.capabilities(),
            turn_state: None,
            stats: serde_json::Value::Null,
            workspace_revision: self
                .route(session)
                .ok()
                .map_or(changed.workspace_revision, |live| {
                    self.workspace_revision(&live.opened.workspace.cwd)
                }),
        }
    }
    /// A Session's config as its tree records it, without loading it.
    async fn recorded_config(&self, session: &SessionRef) -> BackendResult<ConfigChanged> {
        // A reader only: listing and attaching never take a writer lease.
        let reader = SessionStore::load(self.store.as_ref(), &session.native_id)
            .await
            .map_err(failed)?;
        let head = reader.read_head().await.map_err(failed)?;
        let entries = reader.read_entries(None).await.map_err(failed)?;
        let recorded =
            loop_agent::harness::session::types::config_from_entries(&entries, head.as_deref());
        // Described as loading it would set it up: the same defaults and
        // the same rule for the Session's own choices.
        let mut opts = self.session_opts();
        if let Some(cwd) = &reader.metadata().cwd {
            opts.cwd = PathBuf::from(cwd);
        }
        let workspace = self.factory.workspace(&opts).await.map_err(failed)?;
        let (default_model, default_effort) = self.factory.session_defaults(&workspace, &opts);
        let (model, level) = loop_agent::native_backend::restored_choice(
            &self.factory.globals.models,
            &recorded,
            default_model,
            default_effort,
        );
        let mut config = loop_agent::native_backend::session_config(model.as_ref(), level);
        config.mode = recorded.permission_mode.clone();
        Ok(ConfigChanged {
            revision: recorded.revision,
            metadata: SessionMetadata {
                name: recorded.name.or_else(|| reader.metadata().name.clone()),
            },
            effective: config.clone(),
            config,
            ..ConfigChanged::default()
        })
    }
    /// Options to open a Session with the Runtime's startup defaults.
    fn session_opts(&self) -> BootstrapOpts {
        let d = &self.defaults;
        BootstrapOpts {
            cwd: d.cwd.clone(),
            provider: d.provider.clone(),
            model: d.model.clone(),
            theme: d.theme.clone(),
            system_prompt: d.system_prompt.clone(),
            append_system_prompt: d.append_system_prompt.clone(),
            no_context_files: d.no_context_files,
            interactive: false,
            session_id: None,
            no_web: d.no_web,
            permission_mode: d.permission_mode.clone(),
            agent: d.agent.clone(),
        }
    }
    /// The newest Session revision the Event Log announced for `key`.
    async fn logged_revision(&self, key: &SessionKey) -> BackendResult<u64> {
        let records = self.log.read_after(key, 0).await.map_err(failed)?;
        Ok(records
            .iter()
            .rev()
            .find_map(|record| match &record.event.kind {
                EventKind::Update(UpdateEnvelope {
                    update: SessionUpdate::ConfigChanged(changed),
                    ..
                }) => Some(changed.revision),
                EventKind::Update(UpdateEnvelope {
                    update: SessionUpdate::BranchReset { revision, .. },
                    ..
                }) => Some(*revision),
                _ => None,
            })
            .unwrap_or(0))
    }
    /// The loaded Session, loading it again when it was unloaded to make room.
    async fn loaded(&self, session: &SessionRef) -> BackendResult<Arc<LiveSession>> {
        match self.route(session) {
            Err(BackendError::SessionNotFound(key)) if self.was_unloaded(&key) => {
                self.reload(session).await?;
                self.route(session)
            }
            other => other,
        }
    }
    /// Run a config setter under the Session's lifecycle lock and return
    /// once its `config_changed` update is committed (outside the lock). A setter that cannot
    /// wait for the next turn refuses a busy Session by passing `refuse_busy`.
    async fn set_config<F, Fut>(
        &self,
        session: &SessionRef,
        refuse_busy: bool,
        set: F,
    ) -> BackendResult<MutationResult>
    where
        F: FnOnce(Arc<LiveSession>) -> Fut,
        Fut: Future<Output = BackendResult<MutationResult>>,
    {
        let live = self.loaded(session).await?;
        let mut result = {
            let _lifecycle = live.lifecycle.lock().await;
            if live.closing.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire) {
                return Err(failed("Session closing"));
            }
            if let Some(error) = live.failure.lock().unwrap().clone() {
                return Err(failed(error));
            }
            if refuse_busy && live.busy.load(Ordering::Acquire) {
                return Err(BackendError::Busy);
            }
            set(Arc::clone(&live)).await?
        };
        let receipt = Receipt {
            live: &live,
            revision: result.revision,
        };
        // Not under the lock: a stalled Event Log must not hold up attach,
        // admission or close. The pump commits in the order changes were
        // made, so the reply still follows the commit.
        barrier(&live).await?;
        result.seq = receipt
            .take()
            .ok_or_else(|| failed("the change was not recorded in the Event Log"))?;
        Ok(result)
    }
    fn control(live: &LiveSession) -> BackendResult<&dyn ModelControl> {
        live.opened
            .native
            .model_control()
            .ok_or_else(|| BackendError::Unsupported("choosing a model".into()))
    }
}

#[async_trait]
impl Compaction for Runtime {
    async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<CompactionReport> {
        Runtime::compact(self, session, instructions).await
    }
}

#[async_trait]
impl SessionStats for Runtime {
    async fn stats(&self, session: &SessionRef) -> BackendResult<StatsSummary> {
        let live = self.loaded(session).await?;
        let mut stats = live.opened.native.stats(session).await?;
        let turn_state = if live.escalations.has_pending().await {
            "parked"
        } else if live.busy.load(Ordering::Acquire) {
            "busy"
        } else {
            "idle"
        };
        stats.turn_state = Some(turn_state.into());
        Ok(stats)
    }
}

#[async_trait]
impl LocalShell for Runtime {
    async fn run(
        &self,
        session: &SessionRef,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> BackendResult<ShellResult> {
        self.shell(session, command, timeout_ms).await
    }
}

#[async_trait]
impl ModelControl for Runtime {
    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        let live = self.loaded(session).await?;
        Self::control(&live)?.models(session).await
    }
    async fn set_model(
        &self,
        session: &SessionRef,
        model: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        // During a turn the choice waits: the running turn keeps its model.
        self.set_config(session, false, |live| async move {
            Self::control(&live)?
                .set_model(session, model, expected_revision)
                .await
        })
        .await
    }
    async fn efforts(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        let live = self.loaded(session).await?;
        Self::control(&live)?.efforts(session).await
    }
    async fn set_effort(
        &self,
        session: &SessionRef,
        effort: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        // During a turn the change waits for the next one.
        self.set_config(session, false, |live| async move {
            Self::control(&live)?
                .set_effort(session, effort, expected_revision)
                .await
        })
        .await
    }
    async fn modes(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        let live = self.loaded(session).await?;
        Self::control(&live)?.modes(session).await
    }
    async fn ttfb(
        &self,
        session: &SessionRef,
        change: Option<Option<u64>>,
        expected_revision: Option<u64>,
    ) -> BackendResult<loop_protocol::rpc::TtfbResult> {
        if change.is_none() {
            let live = self.loaded(session).await?;
            return Self::control(&live)?.ttfb(session, None, None).await;
        }
        // During a turn the change waits for the next one.
        let read = Mutex::new(None);
        let mutation = self
            .set_config(session, false, |live| {
                let read = &read;
                async move {
                    let result = Self::control(&live)?
                        .ttfb(session, change, expected_revision)
                        .await?;
                    let mutation = MutationResult {
                        revision: result.revision,
                        seq: result.seq.unwrap_or(0),
                        pending: result.pending,
                        applies_after_turn: None,
                    };
                    *read.lock().unwrap() = Some(result);
                    Ok(mutation)
                }
            })
            .await?;
        let mut result = read
            .into_inner()
            .unwrap()
            .ok_or_else(|| failed("the change was not recorded"))?;
        // The Runtime's own sequence, committed in its Event Log.
        result.revision = mutation.revision;
        result.seq = Some(mutation.seq);
        result.pending = mutation.pending;
        Ok(result)
    }
    async fn set_mode(
        &self,
        session: &SessionRef,
        mode: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        self.set_config(session, false, |live| async move {
            Self::control(&live)?
                .set_mode(session, mode, expected_revision)
                .await
        })
        .await
    }
}

#[async_trait]
impl AgentBackend for Runtime {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            session_environment: true,
            session_config: true,
            session_skills: true,
            workflow: cfg!(feature = "orchestration"),
            session_tree: true,
            session_rename: true,
            session_fork: true,
            session_transfer: true,
            branch_snapshot: true,
            multi_session: true,
            attach: true,
            attach_snapshot: true,
            load_session: true,
            images: true,
            set_model: true,
            efforts: true,
            compaction: true,
            session_stats: true,
            local_shell: true,
            session_ttfb: true,
            ..Capabilities::default()
        }
    }
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }
    fn model_control(&self) -> Option<&dyn ModelControl> {
        Some(self)
    }
    fn compaction(&self) -> Option<&dyn Compaction> {
        Some(self)
    }
    fn session_stats(&self) -> Option<&dyn SessionStats> {
        Some(self)
    }
    fn local_shell(&self) -> Option<&dyn LocalShell> {
        Some(self)
    }
    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        self.open(None, spec, None).await
    }
    async fn load_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        self.open(Some(session), spec, None).await
    }
    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        self.route(session)?.opened.native.transcript(session).await
    }
    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        let mut reloads = 0;
        let live = loop {
            let live = match self.route(session) {
                Ok(live) => live,
                // Unloaded to make room: load it again rather than refuse.
                Err(BackendError::SessionNotFound(key))
                    if self.was_unloaded(&key) && reloads < 3 =>
                {
                    reloads += 1;
                    self.reload(session).await?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Admission is decided under the lifecycle lock, so an unload
            // either sees this turn or has already begun.
            let lifecycle = live.lifecycle.lock().await;
            if live.closing.load(Ordering::Acquire) {
                drop(lifecycle);
                if self.was_unloaded(&session.key) && reloads < 3 {
                    reloads += 1;
                    self.wait_removed(&live).await;
                    self.reload(session).await?;
                    continue;
                }
                return Err(failed("Session closing"));
            }
            if self.stopped.load(Ordering::Acquire) {
                return Err(failed("Session closing"));
            }
            if let Some(error) = live.failure.lock().unwrap().clone() {
                return Err(failed(error));
            }
            self.reserve(&live).await?;
            drop(lifecycle);
            break live;
        };
        let (done, handle) = TurnHandle::channel();
        let log = self.log.clone();
        let generation = self.generation;
        let turns = self.turns.clone();
        // Admission and execution belong to the Runtime, even if a Surface drops
        // the request future or its TurnHandle while waiting for a permit.
        let mut cancelled = live.cancel.subscribe();
        live.turn_epoch
            .store(*cancelled.borrow(), Ordering::Release);
        let task_live = Arc::clone(&live);
        let task = tokio::spawn(async move {
            let live = task_live;
            // `prompt_sent` committed, and the backend has not taken the
            // prompt: an exit now must say the prompt never ran a turn.
            let mut unsettled = false;
            let result = async {
                let ts_ms = now();
                log.append_batch(vec![Event {
                    generation,
                    session_key: Some(live.reference.key.clone()),
                    turn: None,
                    ts_ms,
                    kind: EventKind::PromptSent(json!({"content": input})),
                }])
                .await
                .map_err(failed)?;
                unsettled = true;
                touch(&live.activity, &live.reference.key, ts_ms);
                let permit = tokio::select! {
                    p = turns.acquire_owned() => p.map_err(failed)?,
                    _ = cancelled.changed() => return Ok(TurnOutcome::Cancelled),
                };
                if live.closing.load(Ordering::Acquire) {
                    return Ok(TurnOutcome::Cancelled);
                }
                *live.permit.lock().await = Some(permit);
                if cancelled.has_changed().unwrap_or(true) || live.closing.load(Ordering::Acquire) {
                    return Ok(TurnOutcome::Cancelled);
                }
                let turn = live.opened.native.prompt(&live.reference, input).await?;
                unsettled = false;
                // A cancel that raced the turn's registration hit an idle backend.
                if cancelled.has_changed().unwrap_or(true) {
                    live.opened.native.cancel(&live.reference);
                }
                let outcome = turn.outcome().await;
                barrier(&live).await?;
                if let Some(error) = live.failure.lock().unwrap().clone() {
                    return Err(failed(error));
                }
                // Native cancellation while awaiting a tool can return its last
                // tool-use message. Runtime cancellation remains authoritative.
                Ok(if cancelled.has_changed().unwrap_or(true) {
                    TurnOutcome::Cancelled
                } else {
                    outcome
                })
            }
            .await;
            if unsettled {
                // Before `busy` clears, so it precedes the next prompt.
                let settled = log
                    .append_batch(vec![Event {
                        generation,
                        session_key: Some(live.reference.key.clone()),
                        turn: None,
                        ts_ms: now(),
                        kind: EventKind::PromptSettled(json!({})),
                    }])
                    .await;
                if let Err(error) = settled {
                    eprintln!("Runtime: could not log a prompt that never started: {error}");
                }
            }
            live.permit.lock().await.take();
            live.pauses.signal("turn_settling");
            {
                // Settle under the lifecycle lock: a snapshot or tree read
                // that holds it sees one consistent busy state.
                let _lifecycle = live.lifecycle.lock().await;
                live.busy.store(false, Ordering::Release);
                live.apply_pending().await;
            }
            let outcome = match result {
                Ok(outcome) => outcome,
                // A cancel during the backend's prompt preparation is a cancel.
                Err(BackendError::Failed(info)) if info.category == ErrorCategory::Cancelled => {
                    TurnOutcome::Cancelled
                }
                Err(BackendError::Failed(info)) => TurnOutcome::Failed(info),
                Err(error) => TurnOutcome::Failed(loop_protocol::ErrorInfo::new(
                    ErrorCategory::Internal,
                    error.to_string(),
                )),
            };
            let _ = done.send(outcome);
        });
        *live.turn_task.lock().unwrap() = Some(task.abort_handle());
        Ok(handle)
    }
    fn cancel(&self, session: &SessionRef) {
        if let Ok(live) = self.route(session) {
            let _ = live.commands.send(Command::Cancel);
        }
    }
    fn is_busy(&self, session: &SessionRef) -> bool {
        self.route(session)
            .is_ok_and(|s| s.busy.load(Ordering::Acquire))
    }
    async fn shutdown(&self) {
        Runtime::shutdown(self).await;
    }
}

#[cfg(test)]
mod tests {
    use super::leases::{renew_loop, RenewEnd};
    use super::*;
    use crate::log::MemoryEventLog;

    // Trigger the actual NativeBackend replay producer at the pump boundary.
    // Runtime loading uses the already-opened factory Session and does not need
    // to switch the harness to the same Session a second time.
    use std::sync::atomic::AtomicUsize;

    async fn run_renew(
        script: impl Fn(usize) -> Result<(), SessionError> + Send + 'static,
        within: Duration,
    ) -> (Option<RenewEnd>, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let end = tokio::time::timeout(
            within,
            renew_loop(
                move || {
                    let n = seen.fetch_add(1, Ordering::AcqRel);
                    let result = script(n);
                    async move { result }
                },
                Duration::from_millis(5),
                Duration::from_millis(200),
            ),
        )
        .await
        .ok();
        (end, calls.load(Ordering::Acquire))
    }

    #[tokio::test]
    async fn transient_renewal_errors_are_retried() {
        let (end, calls) = run_renew(
            |n| {
                if n < 3 {
                    Err(SessionError::Storage("busy".into()))
                } else {
                    Ok(())
                }
            },
            Duration::from_millis(150),
        )
        .await;
        assert!(end.is_none(), "must keep renewing: {end:?}");
        assert!(calls > 4);
    }

    #[tokio::test]
    async fn renewal_gives_up_only_once_the_lease_is_stale() {
        let started = std::time::Instant::now();
        let (end, _) = run_renew(
            |_| Err(SessionError::Storage("busy".into())),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(end, Some(RenewEnd::Stale(_))));
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn lost_ownership_is_fatal_immediately() {
        let (end, calls) = run_renew(
            |_| Err(SessionError::Invalid("lease ownership was lost".into())),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(end, Some(RenewEnd::Lost(_))));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn native_replay_never_becomes_new_event_log_activity() {
        let dir = tempfile::tempdir().unwrap();
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let globals = GlobalResources::with_models(dir.path().join("agent"), models).unwrap();
        let opts = BootstrapOpts {
            cwd: dir.path().into(),
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
        };
        let log = Arc::new(MemoryEventLog::new());
        let rt = Runtime::new(globals, opts, log.clone(), RuntimeLimits::default())
            .await
            .unwrap();
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.prompt(&session, vec![ContentBlock::text("history")])
            .await
            .unwrap()
            .outcome()
            .await;
        let before = log.read_runtime_after(0).await.unwrap();
        let tail = rt.tail(&session).unwrap();
        let mut updates = rt.subscribe();
        let live = rt.route(&session).unwrap();
        live.opened
            .native
            .load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        barrier(&live).await.unwrap();
        assert_eq!(log.read_runtime_after(0).await.unwrap(), before);
        assert_eq!(rt.tail(&session).unwrap(), tail);
        assert!(updates.try_recv().is_err());
        rt.shutdown().await;
    }
}
