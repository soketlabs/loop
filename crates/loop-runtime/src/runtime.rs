//! Session ownership, routing and committed update publication.
use crate::log::{Event, EventKind, EventLog};
use async_trait::async_trait;
use loop_agent::{
    harness::{
        create_session_repository,
        session::sqlite::{LeaseIdentity, SqliteSessionStore},
        Sandbox,
    },
    native_backend::NATIVE_INSTANCE,
};
use loop_app_core::runtime::{
    BootstrapOpts, GlobalResources, OpenedSession, SessionFactory, WorkspaceResources,
};
use loop_protocol::{
    rpc::SessionInfo, AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities,
    ContentBlock, ErrorCategory, Origin, SessionKey, SessionRef, SessionSpec, SessionUpdate,
    TurnHandle, TurnOutcome, UpdateEnvelope,
};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
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

/// One Runtime owns all live native Sessions and their durable update streams.
pub struct Runtime {
    instance: BackendInstanceId,
    factory: SessionFactory,
    defaults: BootstrapOpts,
    sessions: Mutex<HashMap<SessionKey, Arc<LiveSession>>>,
    workspaces: Mutex<HashMap<PathBuf, WorkspaceState>>,
    admission: AsyncMutex<()>,
    stopped: AtomicBool,
    log: Arc<dyn EventLog>,
    generation: u64,
    limits: RuntimeLimits,
    turns: Arc<Semaphore>,
    store: Arc<SqliteSessionStore>,
    identity: LeaseIdentity,
    updates: broadcast::Sender<UpdateEnvelope>,
}
// Own admission's lease until the pump and Session map take responsibility.
// A Surface can drop its request future at any await point during construction.
struct OpeningLease {
    store: Arc<SqliteSessionStore>,
    identity: LeaseIdentity,
    id: String,
    armed: bool,
}
impl OpeningLease {
    async fn acquire(
        store: Arc<SqliteSessionStore>,
        identity: LeaseIdentity,
        id: String,
    ) -> BackendResult<Self> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = store
                .acquire_lease(&id, &identity)
                .await
                .map_err(failed)
                .map(|()| Self {
                    store,
                    identity,
                    id,
                    armed: true,
                });
            let _ = tx.send(result);
        });
        rx.await.map_err(failed)?
    }
}
impl Drop for OpeningLease {
    fn drop(&mut self) {
        if self.armed {
            let store = self.store.clone();
            let identity = self.identity.clone();
            let id = self.id.clone();
            tokio::spawn(async move {
                if let Err(error) = store.release_lease(&id, &identity).await {
                    eprintln!("Runtime interrupted Session admission: {error}");
                }
            });
        }
    }
}
struct WorkspaceState {
    resources: Arc<WorkspaceResources>,
    sandbox: Option<Arc<dyn Sandbox>>,
}
struct LiveSession {
    opened: OpenedSession,
    reference: SessionRef,
    busy: AtomicBool,
    closing: AtomicBool,
    failure: Mutex<Option<String>>,
    permit: AsyncMutex<Option<OwnedSemaphorePermit>>,
    cancel: watch::Sender<u64>,
    commands: mpsc::UnboundedSender<Command>,
    pump: AsyncMutex<Option<JoinHandle<()>>>,
    tail: Mutex<VecDeque<UpdateEnvelope>>,
}
enum Command {
    Cancel,
    Barrier(oneshot::Sender<()>),
    Stop(oneshot::Sender<()>),
}
fn now() -> i64 {
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
        let (updates, _) = broadcast::channel(4096);
        Ok(Self {
            instance: NATIVE_INSTANCE.into(),
            factory: SessionFactory::new(Arc::new(globals)),
            defaults,
            sessions: Mutex::new(HashMap::new()),
            workspaces: Mutex::new(HashMap::new()),
            admission: AsyncMutex::new(()),
            stopped: AtomicBool::new(false),
            log,
            generation,
            limits,
            turns: Arc::new(Semaphore::new(limits.max_active_turns)),
            store,
            identity,
            updates,
        })
    }
    pub fn generation(&self) -> u64 {
        self.generation
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
    async fn open(
        &self,
        existing: Option<&SessionRef>,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        let _admission = self.admission.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Err(failed("Runtime stopped"));
        }
        if let Some(session) = existing {
            if session.instance != self.instance || session.key.0 != session.native_id {
                return Err(BackendError::SessionNotFound(session.key.clone()));
            }
            if let Ok(live) = self.route(session) {
                return Ok(live.reference.clone());
            }
        }
        if self.sessions.lock().unwrap().len() >= self.limits.max_sessions {
            return Err(BackendError::Busy);
        }
        let d = &self.defaults;
        let mut opts = BootstrapOpts {
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
        };
        if let Some(cwd) = spec.cwd {
            opts.cwd = cwd;
        } else if let Some(session) = existing {
            let known = self
                .factory
                .globals
                .repository
                .list(None)
                .await
                .map_err(failed)?;
            let meta = known
                .iter()
                .find(|m| m.id == session.native_id)
                .ok_or_else(|| BackendError::SessionNotFound(session.key.clone()))?;
            if let Some(cwd) = &meta.cwd {
                opts.cwd = PathBuf::from(cwd);
            }
        }
        let workspace = self.factory.workspace(&opts).await.map_err(failed)?;
        let id = match existing {
            Some(session) => session.native_id.clone(),
            None => self
                .factory
                .globals
                .repository
                .create(
                    Some(workspace.cwd.to_string_lossy().into_owned()),
                    spec.name,
                )
                .await
                .map_err(failed)?
                .metadata()
                .id
                .clone(),
        };
        let mut lease =
            OpeningLease::acquire(self.store.clone(), self.identity.clone(), id.clone()).await?;
        opts.session_id = Some(id.clone());
        let result = async {
            let opened = self.factory.open(workspace, &opts).await.map_err(failed)?;
            let sandbox = opened
                .harness
                .ensure_sandbox_ready()
                .await
                .map_err(failed)?;
            self.workspaces.lock().unwrap().insert(
                opened.workspace.cwd.clone(),
                WorkspaceState {
                    resources: opened.workspace.clone(),
                    sandbox,
                },
            );
            let reference = opened.native.current_session().await;
            let receiver = opened.native.subscribe();
            let (commands, rx) = mpsc::unbounded_channel();
            let (cancel, _) = watch::channel(0);
            self.log
                .append_batch(vec![Event {
                    generation: self.generation,
                    session_key: Some(reference.key.clone()),
                    turn: None,
                    ts_ms: now(),
                    kind: EventKind::SessionOpened(json!({"cwd": opened.workspace.cwd})),
                }])
                .await
                .map_err(failed)?;
            let live = Arc::new(LiveSession {
                opened,
                reference: reference.clone(),
                busy: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                failure: Mutex::new(None),
                permit: AsyncMutex::new(None),
                cancel,
                commands,
                pump: AsyncMutex::new(None),
                tail: Mutex::new(VecDeque::new()),
            });
            let task = tokio::spawn(pump(
                Arc::clone(&live),
                receiver,
                rx,
                Arc::clone(&self.log),
                self.generation,
                self.updates.clone(),
                Arc::clone(&self.store),
                self.identity.clone(),
            ));
            *live.pump.lock().await = Some(task);
            self.sessions
                .lock()
                .unwrap()
                .insert(reference.key.clone(), live.clone());
            lease.armed = false;
            Ok(reference)
        }
        .await;
        if result.is_err()
            && self
                .store
                .lease(&id)
                .await
                .map_err(failed)?
                .is_some_and(|lease| lease.identity == self.identity)
        {
            self.store
                .release_lease(&id, &self.identity)
                .await
                .map_err(failed)?;
            lease.armed = false;
        }
        result
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
        let events = self.log.read_runtime_after(0).await.map_err(failed)?;
        let sessions = self.sessions.lock().unwrap();
        Ok(known
            .into_iter()
            .map(|meta| {
                let r = reference(&meta.id);
                let live = sessions.get(&r.key);
                let last_active_ms = events
                    .iter()
                    .rev()
                    .find(|e| e.event.session_key.as_ref() == Some(&r.key))
                    .map_or(meta.created_at, |e| e.event.ts_ms)
                    .max(0) as u64;
                SessionInfo {
                    r#ref: r,
                    cwd: meta.cwd.map(PathBuf::from).unwrap_or_default(),
                    name: meta.name,
                    parent: meta.parent_session_id.map(|s| reference(&s)),
                    live: live.is_some(),
                    busy: live.is_some_and(|s| s.busy.load(Ordering::Acquire)),
                    created_ms: meta.created_at.max(0) as u64,
                    last_active_ms,
                }
            })
            .collect())
    }
    /// Recent committed updates; the Event Log remains the replay authority.
    pub fn tail(&self, session: &SessionRef) -> BackendResult<Vec<UpdateEnvelope>> {
        Ok(self
            .route(session)?
            .tail
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect())
    }
    /// A parent waiting for a Subagent releases its active-turn permit.
    pub async fn release_turn_permit(&self, session: &SessionRef) -> BackendResult<()> {
        let live = self.route(session)?;
        live.permit.lock().await.take();
        Ok(())
    }
    /// Rejoin turn admission before the parent continues executing.
    pub async fn reacquire_turn_permit(&self, session: &SessionRef) -> BackendResult<()> {
        let live = self.route(session)?;
        let mut permit = live.permit.lock().await;
        if permit.is_none() {
            let mut cancelled = live.cancel.subscribe();
            if live.closing.load(Ordering::Acquire) || !live.busy.load(Ordering::Acquire) {
                return Err(failed("Session turn ended"));
            }
            let acquired = tokio::select! {
                p = self.turns.clone().acquire_owned() => p.map_err(failed)?,
                _ = cancelled.changed() => return Err(failed("Session turn cancelled")),
            };
            if live.closing.load(Ordering::Acquire) || !live.busy.load(Ordering::Acquire) {
                return Err(failed("Session turn ended"));
            }
            *permit = Some(acquired);
        }
        Ok(())
    }
    /// Unload a Session, settling any forced cancellation before releasing ownership.
    pub async fn close(&self, key: &SessionKey, force: bool) -> BackendResult<()> {
        let _admission = self.admission.lock().await;
        self.close_inner(key, force).await
    }
    async fn close_inner(&self, key: &SessionKey, force: bool) -> BackendResult<()> {
        let live = self
            .sessions
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or_else(|| BackendError::SessionNotFound(key.clone()))?;
        live.closing.store(true, Ordering::Release);
        if live.busy.load(Ordering::Acquire) && !force {
            live.closing.store(false, Ordering::Release);
            return Err(BackendError::Busy);
        }
        if force && live.commands.send(Command::Cancel).is_err() {
            live.cancel.send_modify(|n| *n += 1);
            live.opened.native.cancel(&live.reference);
        }
        while live.busy.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        live.opened.native.shutdown().await;
        let (tx, rx) = oneshot::channel();
        let _ = live.commands.send(Command::Stop(tx));
        let _ = rx.await;
        if let Some(task) = live.pump.lock().await.take() {
            task.await.map_err(failed)?;
        }
        let logged = self
            .log
            .append_batch(vec![Event {
                generation: self.generation,
                session_key: Some(key.clone()),
                turn: None,
                ts_ms: now(),
                kind: EventKind::SessionClosed(json!({"force": force})),
            }])
            .await
            .map_err(failed);
        let released = self
            .store
            .release_lease(&key.0, &self.identity)
            .await
            .map_err(failed);
        self.sessions.lock().unwrap().remove(key);
        logged?;
        released?;
        if let Some(error) = live.failure.lock().unwrap().clone() {
            return Err(failed(error));
        }
        Ok(())
    }
    /// Stop admission, settle work, drain pumps and release all Session leases.
    pub async fn shutdown(&self) {
        let _admission = self.admission.lock().await;
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.turns.close();
        let keys: Vec<_> = self.sessions.lock().unwrap().keys().cloned().collect();
        for key in keys {
            if let Err(error) = self.close_inner(&key, true).await {
                eprintln!("Runtime shutdown: {error}");
            }
        }
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
impl AgentBackend for Runtime {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            multi_session: true,
            load_session: true,
            images: true,
            ..Capabilities::default()
        }
    }
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }
    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        self.open(None, spec).await
    }
    async fn load_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        self.open(Some(session), spec).await
    }
    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        self.route(session)?.opened.native.transcript(session).await
    }
    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        let live = self.route(session)?;
        if self.stopped.load(Ordering::Acquire) || live.closing.load(Ordering::Acquire) {
            return Err(failed("Session closing"));
        }
        if let Some(error) = live.failure.lock().unwrap().clone() {
            return Err(failed(error));
        }
        if live
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BackendError::Busy);
        }
        let (done, handle) = TurnHandle::channel();
        let log = self.log.clone();
        let generation = self.generation;
        let turns = self.turns.clone();
        // Admission and execution belong to the Runtime, even if a Surface drops
        // the request future or its TurnHandle while waiting for a permit.
        let mut cancelled = live.cancel.subscribe();
        tokio::spawn(async move {
            let result = async {
                log.append_batch(vec![Event {
                    generation,
                    session_key: Some(live.reference.key.clone()),
                    turn: None,
                    ts_ms: now(),
                    kind: EventKind::PromptSent(json!({"content": input})),
                }])
                .await
                .map_err(failed)?;
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
                let outcome = turn.outcome().await;
                barrier(&live).await?;
                if let Some(error) = live.failure.lock().unwrap().clone() {
                    return Err(failed(error));
                }
                Ok(outcome)
            }
            .await;
            live.permit.lock().await.take();
            live.busy.store(false, Ordering::Release);
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(BackendError::Failed(info)) => TurnOutcome::Failed(info),
                Err(error) => TurnOutcome::Failed(loop_protocol::ErrorInfo::new(
                    ErrorCategory::Internal,
                    error.to_string(),
                )),
            };
            let _ = done.send(outcome);
        });
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
async fn barrier(live: &LiveSession) -> BackendResult<()> {
    let (tx, rx) = oneshot::channel();
    let result = match live.commands.send(Command::Barrier(tx)) {
        Ok(()) => rx.await.map_err(failed),
        Err(error) => Err(failed(error)),
    };
    if let Some(error) = live.failure.lock().unwrap().clone() {
        return Err(failed(error));
    }
    result
}
async fn publish(
    live: &LiveSession,
    envelope: UpdateEnvelope,
    log: &dyn EventLog,
    generation: u64,
    updates: &broadcast::Sender<UpdateEnvelope>,
) -> BackendResult<()> {
    if envelope.origin == Origin::Replay {
        return Ok(());
    }
    let committed = log
        .append_batch(vec![Event::update(generation, now(), envelope)])
        .await
        .map_err(failed)?;
    if let Some(error) = live.failure.lock().unwrap().clone() {
        return Err(failed(error));
    }
    for record in committed {
        if let EventKind::Update(envelope) = record.event.kind {
            let mut tail = live.tail.lock().unwrap();
            if tail.len() == 256 {
                tail.pop_front();
            }
            tail.push_back(envelope.clone());
            let _ = updates.send(envelope);
        }
    }
    Ok(())
}
fn fail(live: &LiveSession, error: impl std::fmt::Display) {
    *live.failure.lock().unwrap() = Some(error.to_string());
    live.cancel.send_modify(|n| *n += 1);
    live.opened.native.cancel(&live.reference);
}
#[allow(clippy::too_many_arguments)]
async fn pump(
    live: Arc<LiveSession>,
    mut receiver: broadcast::Receiver<UpdateEnvelope>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    log: Arc<dyn EventLog>,
    generation: u64,
    updates: broadcast::Sender<UpdateEnvelope>,
    store: Arc<SqliteSessionStore>,
    identity: LeaseIdentity,
) {
    // Lease maintenance must not wait behind Event Log backpressure.
    // Dropping the JoinSet stops renewal when the pump drains or fails.
    let mut renewals = tokio::task::JoinSet::new();
    let renewing = Arc::downgrade(&live);
    let id = live.reference.native_id.clone();
    renewals.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(error) = store.renew_lease(&id, &identity).await {
                if let Some(live) = renewing.upgrade() {
                    fail(&live, error);
                }
                return;
            }
        }
    });
    loop {
        let result: BackendResult<()> = tokio::select! {
            envelope = receiver.recv() => match envelope {
                Ok(envelope) => publish(&live, envelope, log.as_ref(), generation, &updates).await,
                Err(error) => Err(failed(format!("Session update pump lost activity: {error}"))),
            },
            _ = renewals.join_next() => return,
            command = commands.recv() => {
                let stop = matches!(&command, Some(Command::Stop(_)));
                match command {
                    Some(Command::Cancel) => {
                        let result = log.append_batch(vec![Event { generation, session_key: Some(live.reference.key.clone()), turn: None, ts_ms: now(), kind: EventKind::Cancelled(json!({})) }]).await.map_err(failed);
                        live.cancel.send_modify(|n| *n += 1);
                        live.opened.native.cancel(&live.reference);
                        result.map(|_| ())
                    }
                    Some(Command::Barrier(tx) | Command::Stop(tx)) => {
                        let result = async {
                            loop { match receiver.try_recv() {
                                Ok(e) => publish(&live, e, log.as_ref(), generation, &updates).await?,
                                Err(broadcast::error::TryRecvError::Empty) => break,
                                Err(error) => return Err(failed(format!("Session update pump lost activity: {error}"))),
                            } }
                            Ok(())
                        }.await;
                        if let Err(error) = &result { fail(&live, error); }
                        let _ = tx.send(());
                        if stop { return; }
                        result
                    }
                    None => return,
                }
            }
        };
        if let Err(error) = result {
            fail(&live, error);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::MemoryEventLog;

    // Trigger the actual NativeBackend replay producer at the pump boundary.
    // Runtime loading uses the already-opened factory Session and does not need
    // to switch the harness to the same Session a second time.
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
