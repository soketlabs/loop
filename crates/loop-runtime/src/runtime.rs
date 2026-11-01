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
use loop_app_core::runtime::{
    BootstrapOpts, GlobalResources, OpenedSession, SessionFactory, WorkspaceResources,
};
use loop_protocol::{
    rpc::SessionInfo, AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities,
    ContentBlock, ErrorCategory, Origin, SessionKey, SessionRef, SessionSpec, SessionUpdate,
    TurnHandle, TurnOutcome, UpdateEnvelope,
};
use loop_wire::control::EscalationRecipient;
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
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
    log: Arc<dyn EventLog>,
    generation: u64,
    limits: RuntimeLimits,
    turns: Arc<Semaphore>,
    store: Arc<SqliteSessionStore>,
    identity: LeaseIdentity,
    updates: broadcast::Sender<UpdateEnvelope>,
    activity: Activity,
    close_grace: Duration,
}
/// Newest event timestamp per Session, kept in memory so listing never scans the log.
type Activity = Arc<Mutex<HashMap<SessionKey, i64>>>;
type Recipients = HashMap<u64, Arc<dyn EscalationRecipient>>;
fn touch(activity: &Activity, key: &SessionKey, ts_ms: i64) {
    let mut map = activity.lock().unwrap();
    let slot = map.entry(key.clone()).or_insert(ts_ms);
    *slot = (*slot).max(ts_ms);
}
/// How long a close waits for each settling step before giving up on a stuck turn.
const CLOSE_GRACE: Duration = Duration::from_secs(10);
/// Lease renewals run every interval; the lease turns stale at 30 s, so give up
/// on transient errors a little before that.
const RENEW_INTERVAL: Duration = Duration::from_secs(5);
const RENEW_GIVE_UP: Duration = Duration::from_secs(25);
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
}
enum Command {
    Cancel,
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
            log,
            generation,
            limits,
            turns: Arc::new(Semaphore::new(limits.max_active_turns)),
            store,
            identity,
            updates,
            activity,
            close_grace: CLOSE_GRACE,
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
                if live.closing.load(Ordering::Acquire) {
                    return Err(BackendError::Busy);
                }
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
        let created = existing.is_none();
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
        let mut lease = match OpeningLease::acquire(
            self.store.clone(),
            self.identity.clone(),
            id.clone(),
        )
        .await
        {
            Ok(lease) => lease,
            Err(error) => {
                self.discard_new_row(created, &id).await;
                return Err(error);
            }
        };
        opts.session_id = Some(id.clone());
        let result = async {
            let mut opened = self.factory.open(workspace, &opts).await.map_err(failed)?;
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
            let escalations =
                crate::EscalationHost::new(reference.clone(), self.log.clone(), self.generation);
            // Runtime grants are live Session state, never restored after a crash.
            opened.approval_bridge.clear_session_grants();
            Arc::make_mut(&mut opened.approval_bridge).set_host(Arc::new(escalations.clone()));
            opened
                .tool_gate
                .set_approver(Some(opened.approval_bridge.clone()));
            let receiver = opened.native.subscribe();
            let (commands, rx) = mpsc::unbounded_channel();
            let (cancel, _) = watch::channel(0);
            let ts_ms = now();
            self.log
                .append_batch(vec![Event {
                    generation: self.generation,
                    session_key: Some(reference.key.clone()),
                    turn: None,
                    ts_ms,
                    kind: EventKind::SessionOpened(json!({"cwd": opened.workspace.cwd})),
                }])
                .await
                .map_err(failed)?;
            touch(&self.activity, &reference.key, ts_ms);
            let live = Arc::new(LiveSession {
                opened,
                escalations,
                reference: reference.clone(),
                busy: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                failure: Mutex::new(None),
                permit: AsyncMutex::new(None),
                cancel,
                commands,
                pump: AsyncMutex::new(None),
                turn_task: Mutex::new(None),
                activity: Arc::clone(&self.activity),
                tail: Mutex::new(VecDeque::new()),
            });
            // Attachments outlive unloading. Serialize restoration with new
            // registration and disconnect, so none can miss this host.
            let recipients = self.recipients.lock().await;
            if let Some(attached) = recipients.get(&reference.key) {
                for (&connection, recipient) in attached {
                    live.escalations.attach(connection, recipient.clone()).await;
                }
            }
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
        if result.is_err() {
            self.discard_new_row(created, &id).await;
        }
        result
    }
    // A failed open of a Session this call created must not leave an empty row.
    async fn discard_new_row(&self, created: bool, id: &str) {
        if created {
            if let Err(error) = self.store.delete(id).await {
                eprintln!("Runtime could not discard Session {id}: {error}");
            }
        }
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
        let activity = self.activity.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        Ok(known
            .into_iter()
            .map(|meta| {
                let r = reference(&meta.id);
                let live = sessions.get(&r.key);
                let last_active_ms = activity
                    .get(&r.key)
                    .copied()
                    .unwrap_or(meta.created_at)
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
    /// Every wait is bounded by the close grace; a turn that ignores cancellation
    /// is aborted so the lease is always released.
    pub async fn close(&self, key: &SessionKey, force: bool) -> BackendResult<()> {
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
        if !force && live.busy.load(Ordering::Acquire) {
            return Err(BackendError::Busy);
        }
        if live.closing.swap(true, Ordering::AcqRel) {
            // Another close owns the teardown; wait for it instead of repeating it.
            while self.sessions.lock().unwrap().contains_key(key) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            return Ok(());
        }
        if force {
            live.cancel.send_modify(|n| *n += 1);
            live.opened.native.cancel(&live.reference);
        }
        let withdrawal = live
            .escalations
            .withdraw("Session closed", true)
            .await
            .map_err(failed);
        let grace = self.close_grace;
        if force && live.commands.send(Command::Cancel).is_err() {
            live.cancel.send_modify(|n| *n += 1);
            live.opened.native.cancel(&live.reference);
        }
        let settled = tokio::time::timeout(grace, async {
            while live.busy.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .is_ok();
        if !settled {
            eprintln!(
                "Runtime close: turn of Session {} ignored cancellation; aborting",
                key.0
            );
            if let Some(turn) = live.turn_task.lock().unwrap().take() {
                turn.abort();
            }
            live.permit.lock().await.take();
            live.busy.store(false, Ordering::Release);
        }
        let _ = tokio::time::timeout(grace, live.opened.native.shutdown()).await;
        let (tx, rx) = oneshot::channel();
        let _ = live.commands.send(Command::Stop(tx));
        let stopped = tokio::time::timeout(grace, rx).await.is_ok();
        let mut pump_error = None;
        if let Some(task) = live.pump.lock().await.take() {
            if !stopped {
                task.abort();
            }
            if let Err(error) = task.await {
                if !error.is_cancelled() {
                    pump_error = Some(failed(error));
                }
            }
        }
        let ts_ms = now();
        let logged = self
            .log
            .append_batch(vec![Event {
                generation: self.generation,
                session_key: Some(key.clone()),
                turn: None,
                ts_ms,
                kind: EventKind::SessionClosed(
                    json!({"force": force, "aborted": !settled || !stopped}),
                ),
            }])
            .await
            .map_err(failed);
        touch(&self.activity, key, ts_ms);
        let released = self
            .store
            .release_lease(&key.0, &self.identity)
            .await
            .map_err(failed);
        self.sessions.lock().unwrap().remove(key);
        if let Some(error) = pump_error {
            return Err(error);
        }
        withdrawal?;
        logged?;
        released?;
        if let Some(error) = live.failure.lock().unwrap().clone() {
            return Err(failed(error));
        }
        Ok(())
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
    async fn recipient(
        &self,
        session: &SessionRef,
        connection: u64,
        recipient: Option<Arc<dyn loop_wire::control::EscalationRecipient>>,
    ) -> BackendResult<()> {
        if session.instance != self.instance || session.key.0 != session.native_id {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let mut recipients = self.recipients.lock().await;
        match &recipient {
            Some(recipient) => {
                recipients
                    .entry(session.key.clone())
                    .or_default()
                    .insert(connection, recipient.clone());
            }
            None => {
                if let Some(attached) = recipients.get_mut(&session.key) {
                    attached.remove(&connection);
                }
            }
        }
        if let Ok(live) = self.route(session) {
            match recipient {
                Some(recipient) => live.escalations.attach(connection, recipient).await,
                None => live.escalations.detach(connection).await,
            }
        }
        Ok(())
    }
    async fn disconnected(&self, connection: u64) {
        let mut recipients = self.recipients.lock().await;
        recipients.retain(|_, attached| {
            attached.remove(&connection);
            !attached.is_empty()
        });
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        for live in sessions {
            live.escalations.detach(connection).await;
        }
    }

    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        Runtime::list(self).await
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
        let records = self
            .log
            .read_after(&session.key, after_seq)
            .await
            .map_err(failed)?;
        let mut replay = Vec::with_capacity(records.len());
        for record in records {
            if let EventKind::Update(envelope) = record.event.kind {
                replay.push(envelope);
            }
        }
        let latest_seq = match replay.last() {
            Some(last) => last.seq,
            None => self.log.latest_seq(&session.key).await.map_err(failed)?,
        };
        Ok(loop_wire::Attachment {
            latest_seq,
            generation: self.generation,
            replay: Box::new(replay.into_iter()),
        })
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
            attach: true,
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
        let task_live = Arc::clone(&live);
        let task = tokio::spawn(async move {
            let live = task_live;
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
    let ts_ms = now();
    let committed = log
        .append_batch(vec![Event::update(generation, ts_ms, envelope)])
        .await
        .map_err(failed)?;
    touch(&live.activity, &live.reference.key, ts_ms);
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
#[derive(Debug)]
enum RenewEnd {
    /// The lease is no longer ours.
    Lost(String),
    /// Renewals kept failing for the whole give-up window.
    Stale(String),
}
/// Renew on every tick. Only a lost lease is fatal at once; transient storage
/// errors are retried until no renewal has succeeded for `give_up`.
async fn renew_loop<F, Fut>(mut renew: F, interval: Duration, give_up: Duration) -> RenewEnd
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), SessionError>>,
{
    let mut ticker = tokio::time::interval(interval);
    let mut last_ok = tokio::time::Instant::now();
    loop {
        ticker.tick().await;
        match renew().await {
            Ok(()) => last_ok = tokio::time::Instant::now(),
            Err(SessionError::Storage(error)) => {
                eprintln!("Runtime: lease renewal failed, will retry: {error}");
                if last_ok.elapsed() >= give_up {
                    return RenewEnd::Stale(error);
                }
            }
            Err(error) => return RenewEnd::Lost(error.to_string()),
        }
    }
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
    let key = live.reference.key.clone();
    let renew_log = Arc::clone(&log);
    renewals.spawn(async move {
        let end = renew_loop(
            || store.renew_lease(&id, &identity),
            RENEW_INTERVAL,
            RENEW_GIVE_UP,
        )
        .await;
        let (state, reason) = match end {
            RenewEnd::Lost(reason) => ("lost", reason),
            RenewEnd::Stale(reason) => ("stale", reason),
        };
        eprintln!("Runtime: lease of Session {id} is {state}: {reason}");
        if let Some(live) = renewing.upgrade() {
            fail(&live, format!("lease {state}: {reason}"));
        }
        let _ = renew_log
            .append_batch(vec![Event {
                generation,
                session_key: Some(key),
                turn: None,
                ts_ms: now(),
                kind: EventKind::Cancelled(
                    json!({"reason": format!("lease {state}"), "detail": reason}),
                ),
            }])
            .await;
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
                        let withdrawn = live.escalations.withdraw("turn cancelled", false).await.map_err(failed);
                        result.map(|_| ()).and(withdrawn)
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
