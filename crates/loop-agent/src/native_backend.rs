//! [`NativeBackend`]: Loop's own agent loop behind the
//! [`loop_protocol::AgentBackend`] interface.
//!
//! It wraps one [`AgentHarness`], which holds one active session at a time.
//! Agent events are translated by [`NativeEventMapper`] and published as
//! [`UpdateEnvelope`]s on a broadcast stream.

#[cfg(feature = "orchestration")]
#[path = "native_workflows.rs"]
mod workflows;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::ImageContent;
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, Compaction,
    CompactionReport, ConfigChanged, ContentBlock, ErrorCategory, ErrorInfo, LocalShell,
    ModelChoice, ModelChoices, ModelControl, MutationResult, NoticeLevel, Origin, SessionConfig,
    SessionKey, SessionMetadata, SessionRef, SessionSpec, SessionStats, SessionUpdate, ShellResult,
    StatsSummary, StopReason, TurnHandle, TurnId, TurnOutcome, UpdateEnvelope,
};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::effort::{effective_effort, native_efforts, parse_thinking, NATIVE_EFFORTS};
use crate::harness::agent_harness::{AgentHarness, TurnWritten};
use crate::harness::session::format_session_stats;
use crate::harness::session::types::{
    PendingSessionWrite, PERMISSION_MODE_ENTRY, RESPONSE_TIMEOUT_ENTRY, SANDBOX_ENTRY,
};
use crate::harness::types::{AgentHarnessError, AgentHarnessPhase, ShellExecOptions};
use crate::protocol_events::{replay, turn_outcome, NativeEventMapper};
use crate::types::AgentThinkingLevel;
use crate::types::{AgentEvent, PromptInput};

/// Instance id of Loop's own agent.
pub const NATIVE_INSTANCE: &str = "loop";

/// Updates buffered per subscriber before a slow reader starts losing the oldest.
const UPDATE_BUFFER: usize = 4096;

/// Loop's own agent loop as an [`AgentBackend`].
pub struct NativeBackend {
    #[cfg(feature = "orchestration")]
    workflows: Mutex<std::collections::HashMap<String, workflows::WorkflowRun>>,
    instance: BackendInstanceId,
    harness: Arc<AgentHarness>,
    updates: broadcast::Sender<UpdateEnvelope>,
    stream: Arc<Mutex<StreamState>>,
    /// Set while a prompt sent through this backend runs, so two callers
    /// cannot both start one.
    prompting: Arc<AtomicBool>,
    /// The current Session's model and effort, as last announced.
    config: Arc<Mutex<ConfigState>>,
    /// Serializes model and effort changes, so each revision check and its
    /// write happen together.
    setting: tokio::sync::Mutex<()>,
    /// Effort for a Session that never chose one.
    default_effort: Mutex<AgentThinkingLevel>,
    /// Model for a new Session, or one that never chose one.
    default_model: Mutex<Option<loop_ai::Model>>,
    /// Response timeout for a Session without an override (0 = unlimited).
    default_response_timeout: Mutex<u64>,
    /// A model chosen during a turn, for the Session it was chosen in. The
    /// next turn starts on it; the harness keeps the running turn's model.
    pending_model: Mutex<Option<(SessionKey, loop_ai::Model)>>,
    /// Jobs that start with the next turn, by key; a newer job replaces the
    /// older one of its key.
    next_turn: Mutex<std::collections::HashMap<&'static str, NextTurnJob>>,
    /// Set by a cancel or shutdown that arrives while a prompt is still
    /// being prepared, before the harness has a turn to abort.
    cancel_requested: AtomicBool,
    /// Held while a turn is admitted or the model changes, so a model
    /// change never lands between the busy check and a turn's start.
    admission: tokio::sync::Mutex<()>,
    /// The Runtime's tool lock. Set once, never cleared: it refuses user
    /// commands and workflows for the life of this backend.
    tool_lock: std::sync::OnceLock<loop_protocol::ToolLock>,
    /// The Policy gate's check for user commands (`session/shell`). Set
    /// once, never cleared. Without one, every user command is refused.
    command_gate: std::sync::OnceLock<CommandGate>,
}

/// A command the user typed (`session/shell`, `!command`), for the Policy
/// gate to check before it runs.
#[derive(Debug, Clone)]
pub struct UserCommand {
    /// The Session it runs in.
    pub session: SessionKey,
    /// The command as typed.
    pub command: String,
    /// The caller of the connection that sent it; `None` for an in-process
    /// call.
    pub caller: Option<loop_protocol::Caller>,
}

/// Why the Policy gate refused a [`UserCommand`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CommandRefused(pub String);

/// The Policy gate's check for a [`UserCommand`]: `Ok` runs it, `Err` gives
/// the refusal. It records its decision before it returns.
pub type CommandGate = Arc<
    dyn Fn(UserCommand) -> futures::future::BoxFuture<'static, Result<(), CommandRefused>>
        + Send
        + Sync,
>;

/// Work that waits for the running turn to end. It is called with `true`
/// when the next turn is about to start, or `false` when a newer job for the
/// same key replaces it, so it can release what it prepared.
pub type NextTurnJob = Box<dyn FnOnce(bool) -> futures::future::BoxFuture<'static, ()> + Send>;

/// A Session's config as this backend last announced it.
#[derive(Debug, Clone)]
struct ConfigState {
    /// The Session it describes; another one means it must be read again.
    session: SessionKey,
    revision: u64,
    name: Option<String>,
    /// The effort level chosen for the next turn, before the model's limits.
    level: AgentThinkingLevel,
    /// What the next turn uses.
    config: SessionConfig,
    /// What the running turn uses.
    effective: SessionConfig,
    /// The running turn a change waits for.
    pending: Option<TurnId>,
    /// The full sandbox choice last recorded for the Session, if any.
    sandbox_choice: Option<serde_json::Value>,
    /// The recorded sandbox is queued for the next turn, so `effective`
    /// keeps the old one until it is installed.
    sandbox_deferred: bool,
}

impl ConfigState {
    fn update(&self) -> SessionUpdate {
        SessionUpdate::ConfigChanged(self.changed())
    }

    fn changed(&self) -> ConfigChanged {
        ConfigChanged {
            revision: self.revision,
            metadata: SessionMetadata {
                name: self.name.clone(),
            },
            config: self.config.clone(),
            effective: self.effective.clone(),
            pending: self.pending.is_some(),
            applies_after_turn: self.pending.clone(),
            workspace_revision: 0,
        }
    }
}

/// How [`SessionConfig::sandbox`] names a recorded sandbox choice: its mode,
/// then its isolation and runtime when it sets them, joined by `/`.
fn sandbox_label(choice: &serde_json::Value) -> Option<String> {
    let mode = choice["mode"].as_str()?;
    let parts = ["isolation", "runtime"]
        .into_iter()
        .filter_map(|key| choice[key].as_str());
    Some(
        std::iter::once(mode)
            .chain(parts)
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// `provider/id` of `model`.
fn model_id(model: &loop_ai::Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// The model and effort a Session with `recorded` choices opens with: its
/// own model while still available (else `default_model`), and its own
/// effort, including an explicit off (else `default_effort`). Loaded and
/// not-yet-loaded Sessions are described with the same rule.
pub fn restored_choice(
    models: &loop_ai::Models,
    recorded: &crate::harness::session::types::SessionConfigEntries,
    default_model: Option<loop_ai::Model>,
    default_effort: AgentThinkingLevel,
) -> (Option<loop_ai::Model>, AgentThinkingLevel) {
    let own = recorded.model.as_ref().and_then(|(provider, id)| {
        if models.credentials().is_logged_out(provider) {
            None
        } else {
            models.get_model(provider, id)
        }
    });
    (
        own.or(default_model),
        recorded.thinking_level.unwrap_or(default_effort),
    )
}

/// The config a Session runs with on `model` at `level`.
pub fn session_config(model: Option<&loop_ai::Model>, level: AgentThinkingLevel) -> SessionConfig {
    SessionConfig {
        model: model.map(model_id),
        effort: Some(effective_effort(model, level)),
        mode: None,
        response_timeout_ms: None,
        sandbox: None,
        active_skills: vec![],
    }
}

fn invalid(message: String) -> BackendError {
    BackendError::failed(ErrorCategory::Other, message)
}

/// Clears [`NativeBackend::prompting`] when the prompt is over.
struct PromptSlot(Arc<AtomicBool>);

impl Drop for PromptSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Translation state, shared with the harness subscription.
struct StreamState {
    session: SessionKey,
    seq: u64,
    /// Envelopes are numbered and sent under the same lock, so subscribers
    /// always see `seq` in increasing order.
    updates: broadcast::Sender<UpdateEnvelope>,
    mapper: NativeEventMapper,
    /// Turns already numbered per Session this backend streamed before, so
    /// switching back never repeats a turn id.
    turns: std::collections::HashMap<SessionKey, u64>,
    /// A prompted turn's `TurnEnded` and everything after it, kept back until
    /// the harness has written the turn to the Session tree. Whoever commits
    /// the stream (the Runtime's Event Log) then never records a turn end
    /// the tree does not hold.
    held: Vec<(Option<TurnId>, SessionUpdate)>,
    /// A prompt's turn is not yet in the tree: hold its end. Set and cleared
    /// only by [`TurnHold`], under this lock.
    holding: bool,
}

impl StreamState {
    /// Number and send one update.
    fn send(&mut self, instance: &BackendInstanceId, turn: Option<TurnId>, update: SessionUpdate) {
        self.seq += 1;
        let envelope = UpdateEnvelope::live(
            instance.clone(),
            0,
            self.session.clone(),
            turn,
            self.seq,
            update,
        );
        // No receivers is fine: nobody is listening yet.
        let _ = self.updates.send(envelope);
    }

    /// Stop holding and send the held updates. With `failure`, the turn's
    /// tree writes failed: the first held turn end (the prompt's own turn)
    /// reports that failure instead of its own stop. True when a turn end
    /// was held.
    fn release(&mut self, instance: &BackendInstanceId, failure: Option<&ErrorInfo>) -> bool {
        self.holding = false;
        let mut ended = false;
        for (turn, mut update) in std::mem::take(&mut self.held) {
            if let SessionUpdate::TurnEnded { stop, error, .. } = &mut update {
                if !ended {
                    if let Some(info) = failure {
                        *stop = StopReason::Error;
                        *error = Some(info.clone());
                    }
                }
                ended = true;
            }
            self.send(instance, turn, update);
        }
        ended
    }
}

/// Holds a prompted turn's end on the stream until the harness has written
/// the turn to the tree ([`TurnHold::written`]). If the prompt task ends any
/// other way (a panic, an abort) before that, dropping the last handle
/// releases the held updates with an internal error, so the stream never
/// stays blocked.
struct TurnHold {
    instance: BackendInstanceId,
    stream: Arc<Mutex<StreamState>>,
    config: Arc<Mutex<ConfigState>>,
    /// The prompt's turn end already reported a tree write failure.
    failure_reported: AtomicBool,
}

impl TurnHold {
    fn start(
        instance: BackendInstanceId,
        stream: Arc<Mutex<StreamState>>,
        config: Arc<Mutex<ConfigState>>,
    ) -> Arc<Self> {
        stream.lock().holding = true;
        Arc::new(Self {
            instance,
            stream,
            config,
            failure_reported: AtomicBool::new(false),
        })
    }

    /// The turn is in the tree (or its write failed with `failure`): send
    /// its end and what came after it.
    fn written(&self, failure: Option<&ErrorInfo>) {
        let released = self.stream.lock().release(&self.instance, failure);
        if released {
            if failure.is_some() {
                self.failure_reported.store(true, Ordering::Release);
            }
            apply_pending(&self.instance, &self.stream, &self.config);
        }
    }
}

impl Drop for TurnHold {
    fn drop(&mut self) {
        let unfinished = {
            let state = self.stream.lock();
            state.holding || !state.held.is_empty()
        };
        if unfinished {
            let info = ErrorInfo::new(
                ErrorCategory::Internal,
                "the turn stopped before it was saved",
            );
            self.written(Some(&info));
        }
    }
}

impl NativeBackend {
    /// Wrap `harness`. Subscribes to its events immediately, so call this
    /// before any turn runs. Session changes must go through this backend
    /// (`new_session`, `load_session`, [`NativeBackend::adopt_session`]) so
    /// updates carry the right session key.
    pub async fn new(harness: Arc<AgentHarness>) -> Arc<Self> {
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let session = SessionKey(harness.session_id().await);
        let stream = Arc::new(Mutex::new(StreamState {
            session,
            seq: 0,
            updates: updates.clone(),
            mapper: NativeEventMapper::new(),
            turns: Default::default(),
            held: Vec::new(),
            holding: false,
        }));
        let level = harness.thinking_level().await;
        let default_model = harness.model().await;
        let default_response_timeout = harness.response_header_timeout_ms().await;
        let backend = Arc::new(Self {
            #[cfg(feature = "orchestration")]
            workflows: Mutex::new(Default::default()),
            instance: BackendInstanceId::from(NATIVE_INSTANCE),
            harness: Arc::clone(&harness),
            updates: updates.clone(),
            stream: Arc::clone(&stream),
            prompting: Arc::new(AtomicBool::new(false)),
            config: Arc::new(Mutex::new(ConfigState {
                session: SessionKey(String::new()),
                revision: 0,
                name: None,
                level,
                config: SessionConfig::default(),
                effective: SessionConfig::default(),
                pending: None,
                sandbox_choice: None,
                sandbox_deferred: false,
            })),
            setting: tokio::sync::Mutex::new(()),
            default_effort: Mutex::new(level),
            default_model: Mutex::new(default_model),
            default_response_timeout: Mutex::new(default_response_timeout),
            pending_model: Mutex::new(None),
            next_turn: Mutex::new(Default::default()),
            cancel_requested: AtomicBool::new(false),
            admission: tokio::sync::Mutex::new(()),
            tool_lock: std::sync::OnceLock::new(),
            command_gate: std::sync::OnceLock::new(),
        });
        backend.read_config().await;
        let instance = backend.instance.clone();
        let config = Arc::clone(&backend.config);
        harness.subscribe(move |event: AgentEvent| {
            if translate(&instance, &stream, &event) {
                apply_pending(&instance, &stream, &config);
            }
            async {}
        });
        backend
    }

    /// The current Session's config, as a `config_changed` update would
    /// carry it.
    pub fn config(&self) -> ConfigChanged {
        self.config.lock().changed()
    }

    /// Check `expected` against the Session revision, for a change its
    /// owner commits itself (a rename or a branch move). The owner
    /// serializes it with this backend's setters.
    pub fn check_session_revision(&self, expected: Option<u64>) -> BackendResult<()> {
        self.check_revision(expected)
    }

    /// Count a committed rename or branch move in the Session revision and
    /// return the new revision. `name` is a rename's new name, which later
    /// announcements carry.
    pub fn record_change(&self, name: Option<Option<String>>) -> u64 {
        let mut state = self.config.lock();
        state.revision += 1;
        if let Some(name) = name {
            state.name = name;
        }
        state.revision
    }

    /// Effort for Sessions that never chose one, used when one is restored.
    pub fn set_default_effort(&self, level: AgentThinkingLevel) {
        *self.default_effort.lock() = level;
    }

    /// Model for new Sessions and Sessions that never chose one.
    pub fn set_default_model(&self, model: Option<loop_ai::Model>) {
        *self.default_model.lock() = model;
    }

    /// The effort level the current Session chose for its next turn, before
    /// the model's limits apply (see [`crate::effort::effective_effort`]).
    pub fn thinking_level(&self) -> AgentThinkingLevel {
        self.config.lock().level
    }

    /// Apply the model and effort the current Session recorded: its model
    /// when still available, and its effort, or the default effort when it
    /// never chose one (an explicit `off` stays off).
    pub async fn restore_config(&self) -> BackendResult<()> {
        let _setting = self.setting.lock().await;
        let recorded = self.harness.session_config().await.map_err(harness_error)?;
        let default_model = self.default_model.lock().clone();
        let default_effort = *self.default_effort.lock();
        let (model, level) = restored_choice(
            self.harness.models(),
            &recorded,
            default_model,
            default_effort,
        );
        match model {
            Some(model) => self.harness.set_model(model).await,
            None => self.harness.clear_model().await,
        }
        self.harness.set_thinking_level(level).await;
        self.read_config().await;
        Ok(())
    }

    /// Response timeout for Sessions without an override (0 = unlimited).
    /// The current Session picks it up unless it has its own override.
    pub async fn set_default_response_timeout(&self, ms: u64) {
        *self.default_response_timeout.lock() = ms;
        let own = self.config.lock().config.response_timeout_ms;
        self.harness
            .set_response_header_timeout_ms(own.unwrap_or(ms))
            .await;
    }

    /// Read the current Session's config from the harness and its tree.
    async fn read_config(&self) {
        let session = SessionKey(self.harness.session_id().await);
        // Work queued for another Session never starts in this one.
        if self.config.lock().session != session {
            self.discard_next_turn().await;
        }
        let recorded = self.harness.session_config().await.unwrap_or_default();
        let model = self.harness.model().await;
        let level = self.harness.thinking_level().await;
        self.harness.clear_active_skills().await;
        for name in &recorded.active_skills {
            self.harness.activate_skill(name).await;
        }
        let mut config = session_config(model.as_ref(), level);
        config.response_timeout_ms = recorded.response_timeout_ms;
        config.sandbox = recorded.sandbox.as_ref().and_then(sandbox_label);
        config.mode = recorded.permission_mode.clone();
        config.active_skills = self.harness.active_skills().await;
        let timeout = recorded
            .response_timeout_ms
            .unwrap_or(*self.default_response_timeout.lock());
        self.harness.set_response_header_timeout_ms(timeout).await;
        *self.pending_model.lock() = None;
        *self.config.lock() = ConfigState {
            session,
            revision: recorded.revision,
            name: recorded.name,
            level,
            effective: config.clone(),
            config,
            pending: None,
            sandbox_choice: recorded.sandbox,
            sandbox_deferred: false,
        };
    }

    /// Make sure the announced config is the harness's current Session's.
    async fn current_config(&self, session: &SessionRef) -> BackendResult<()> {
        self.check_session(session).await?;
        let stale = self.config.lock().session != session.key;
        if stale {
            self.read_config().await;
        }
        Ok(())
    }

    /// Run `job` when the next turn starts if a turn is running now. When
    /// none is, the job is handed back for the caller to run at once.
    pub async fn after_turn(&self, key: &'static str, job: NextTurnJob) -> Option<NextTurnJob> {
        // Holding admission keeps the turn from ending into a new one
        // between this check and the job being filed.
        let _admission = self.admission.lock().await;
        // Whichever way the new job goes, an older one of its key is stale.
        let older = self.next_turn.lock().remove(key);
        if let Some(older) = older {
            older(false).await;
        }
        if !self.is_busy_now() {
            return Some(job);
        }
        self.next_turn.lock().insert(key, job);
        None
    }

    /// Stop a prompt that is still preparing and release everything queued
    /// for the next turn, for a Session that is going away. Waits for the
    /// preparation to settle. Safe to repeat.
    pub async fn release_next_turn(&self) {
        self.cancel_requested.store(true, Ordering::Release);
        // A prompt still preparing holds admission.
        let _admission = self.admission.lock().await;
        self.discard_next_turn().await;
    }

    /// Release everything queued for the next turn without starting it.
    async fn discard_next_turn(&self) {
        let jobs: Vec<NextTurnJob> = self.next_turn.lock().drain().map(|(_, job)| job).collect();
        for job in jobs {
            job(false).await;
        }
    }

    /// The Session's recorded sandbox choice, for a caller that reopens it.
    pub fn recorded_sandbox(&self) -> Option<serde_json::Value> {
        self.config.lock().sandbox_choice.clone()
    }

    /// Record the Session's sandbox choice in its tree and announce it. The
    /// caller installs the sandbox; `deferred` says it waits for the next
    /// turn, so the effective config keeps the old one until
    /// [`Self::sandbox_applied`].
    pub async fn record_sandbox(
        &self,
        session: &SessionRef,
        choice: serde_json::Value,
        expected_revision: Option<u64>,
        deferred: bool,
    ) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        self.check_revision(expected_revision)?;
        self.record_sandbox_locked(choice, deferred).await
    }

    async fn record_sandbox_locked(
        &self,
        choice: serde_json::Value,
        deferred: bool,
    ) -> BackendResult<MutationResult> {
        self.harness
            .record_config(PendingSessionWrite::Custom {
                custom_type: SANDBOX_ENTRY.into(),
                data: serde_json::json!({ "choice": choice }),
            })
            .await
            .map_err(harness_error)?;
        {
            let mut state = self.config.lock();
            state.config.sandbox = sandbox_label(&choice);
            state.sandbox_choice = Some(choice);
            state.sandbox_deferred = deferred;
        }
        Ok(self.announce_change().await)
    }

    /// Choose a sandbox while a turn may be running. During a turn the
    /// choice is recorded, announced as pending and its `job` filed for the
    /// next turn, replacing an older job of its `key`: `Ok(Ok(result))`.
    /// With no turn running nothing is recorded; the older job is released
    /// and `job` is handed back for the caller to run and then record:
    /// `Ok(Err(job))`.
    pub async fn defer_sandbox(
        &self,
        session: &SessionRef,
        key: &'static str,
        choice: serde_json::Value,
        expected_revision: Option<u64>,
        job: NextTurnJob,
    ) -> BackendResult<Result<MutationResult, NextTurnJob>> {
        // The same order as the model setter: settings, then admission.
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        self.check_revision(expected_revision)?;
        let _admission = self.admission.lock().await;
        let busy = self.is_busy_now();
        let recorded = if busy {
            Some(self.record_sandbox_locked(choice, true).await?)
        } else {
            None
        };
        let older = self.next_turn.lock().remove(key);
        if let Some(older) = older {
            older(false).await;
        }
        match recorded {
            Some(result) => {
                self.next_turn.lock().insert(key, job);
                Ok(Ok(result))
            }
            None => Ok(Err(job)),
        }
    }

    /// A deferred sandbox is now installed: announce it as effective.
    pub fn sandbox_applied(&self) {
        let mut state = self.config.lock();
        if !state.sandbox_deferred || state.session != self.stream.lock().session {
            return;
        }
        state.sandbox_deferred = false;
        state.effective.sandbox = state.config.sandbox.clone();
        announce(&self.instance, &self.stream, state.update());
    }

    /// The model the next turn uses: one chosen during a turn, else the
    /// harness's.
    async fn chosen_model(&self) -> Option<loop_ai::Model> {
        let session = self.config.lock().session.clone();
        let pending = self
            .pending_model
            .lock()
            .as_ref()
            .filter(|(key, _)| *key == session)
            .map(|(_, model)| model.clone());
        match pending {
            Some(model) => Some(model),
            None => self.harness.model().await,
        }
    }

    /// Announce a change made under [`Self::setting`] and say when it
    /// applies.
    async fn announce_change(&self) -> MutationResult {
        let model = self.chosen_model().await;
        let level = self.harness.thinking_level().await;
        let skills = self.harness.active_skills().await;
        let running = self.is_busy_now();
        let mut state = self.config.lock();
        state.revision += 1;
        state.level = level;
        let timeout = state.config.response_timeout_ms;
        let sandbox = state.config.sandbox.clone();
        let mode = state.config.mode.clone();
        let applied_sandbox = state.effective.sandbox.clone();
        state.config = session_config(model.as_ref(), level);
        state.config.response_timeout_ms = timeout;
        state.config.sandbox = sandbox;
        state.config.mode = mode;
        state.config.active_skills = skills;
        let current_turn = self.stream.lock().mapper.current_turn().cloned();
        match current_turn.filter(|_| running) {
            Some(turn) => state.pending = Some(turn),
            None => {
                state.effective = state.config.clone();
                state.pending = None;
            }
        }
        if state.sandbox_deferred {
            state.effective.sandbox = applied_sandbox;
        }
        let seq = announce(&self.instance, &self.stream, state.update());
        MutationResult {
            revision: state.revision,
            seq,
            pending: state.pending.is_some(),
            applies_after_turn: state.pending.clone(),
        }
    }

    fn check_revision(&self, expected: Option<u64>) -> BackendResult<()> {
        let revision = self.config.lock().revision;
        match expected {
            Some(expected) if expected != revision => Err(BackendError::Conflict { revision }),
            _ => Ok(()),
        }
    }

    fn is_busy_now(&self) -> bool {
        self.prompting.load(Ordering::Acquire) || self.harness.phase() != AgentHarnessPhase::Idle
    }

    /// The wrapped harness, for native-only features (sandbox, skills, MCP,
    /// forking, workflows).
    pub fn harness(&self) -> &Arc<AgentHarness> {
        &self.harness
    }

    /// A reference to the harness's current session.
    pub async fn current_session(&self) -> SessionRef {
        self.session_ref(self.harness.session_id().await)
    }

    /// Number this Session's next turns after `turns` earlier ones, recorded
    /// elsewhere (for example in the Runtime's Event Log), so turn ids stay
    /// unique across process restarts.
    pub fn continue_turns_after(&self, turns: u64) {
        self.stream.lock().mapper.continue_after(turns);
    }

    /// Point the update stream at a session the harness switched to by
    /// itself (for example after forking).
    pub fn adopt_session(&self, native_id: &str) -> SessionRef {
        let mut stream = self.stream.lock();
        let (old, started) = (stream.session.clone(), stream.mapper.turns_started());
        let seen = stream.turns.entry(old).or_default();
        *seen = (*seen).max(started);
        stream.session = SessionKey::from(native_id);
        stream.seq = 0;
        stream.mapper = NativeEventMapper::new();
        let earlier = stream.turns.get(&stream.session).copied().unwrap_or(0);
        stream.mapper.continue_after(earlier);
        drop(stream);
        self.session_ref(native_id.to_owned())
    }

    fn session_ref(&self, native_id: String) -> SessionRef {
        SessionRef {
            instance: self.instance.clone(),
            key: SessionKey(native_id.clone()),
            native_id,
        }
    }

    async fn check_session(&self, session: &SessionRef) -> BackendResult<()> {
        if session.instance != self.instance || session.native_id != self.harness.session_id().await
        {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        Ok(())
    }
}

/// Translate and send `event`'s updates. True when a turn ended and its end
/// was sent. While a [`TurnHold`] holds (a prompt's turn is not yet in the
/// tree), a turn end and what follows it are kept back until it releases them.
fn translate(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    event: &AgentEvent,
) -> bool {
    let mut state = stream.lock();
    let updates = state.mapper.map(event);
    // Nothing holds any more: what is left goes out first, in order.
    let mut ended = !state.holding && !state.held.is_empty() && state.release(instance, None);
    for update in updates {
        let is_end = matches!(update, SessionUpdate::TurnEnded { .. });
        let turn = match &update {
            SessionUpdate::TurnStarted { turn_id } | SessionUpdate::TurnEnded { turn_id, .. } => {
                Some(turn_id.clone())
            }
            _ => state.mapper.current_turn().cloned(),
        };
        if state.holding && (is_end || !state.held.is_empty()) {
            state.held.push((turn, update));
            continue;
        }
        ended |= is_end;
        state.send(instance, turn, update);
    }
    ended
}

/// Send `update` as the current Session's next envelope, outside any turn.
/// Returns its sequence.
fn announce(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    update: SessionUpdate,
) -> u64 {
    let mut state = stream.lock();
    state.seq += 1;
    let envelope = UpdateEnvelope::live(
        instance.clone(),
        0,
        state.session.clone(),
        None,
        state.seq,
        update,
    );
    let _ = state.updates.send(envelope);
    state.seq
}

/// A turn ended: changes made during it now apply. Announce that.
fn apply_pending(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    config: &Mutex<ConfigState>,
) {
    let mut state = config.lock();
    if state.pending.take().is_none() || state.session != stream.lock().session {
        return;
    }
    let applied_sandbox = state.effective.sandbox.clone();
    state.effective = state.config.clone();
    if state.sandbox_deferred {
        state.effective.sandbox = applied_sandbox;
    }
    announce(instance, stream, state.update());
}

/// Send an error notice outside any turn.
fn error_notice(instance: &BackendInstanceId, stream: &Mutex<StreamState>, info: &ErrorInfo) {
    announce(
        instance,
        stream,
        SessionUpdate::Notice {
            level: NoticeLevel::Error,
            text: info.message.clone(),
        },
    );
}

/// Keep the stream complete when the harness fails outside a normal run, so
/// every prompt that ran produces exactly one `TurnEnded` (a prompt rejected
/// as busy gets only an [`error_notice`]):
/// - the run's turn is still open: end it as failed;
/// - the run never started a turn (`turns_before` unchanged): report a
///   started-and-failed turn;
/// - the run's turn already ended (for example the failure was while saving
///   it): report an error notice.
fn close_failed_turn(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    turns_before: u64,
    info: &ErrorInfo,
) {
    let mut state = stream.lock();
    let started = state.mapper.turns_started() > turns_before;
    let mut updates = Vec::new();
    if state.mapper.current_turn().is_some() {
        updates.extend(state.mapper.end_turn_with_error(info.clone()));
    } else if started {
        updates.push(SessionUpdate::Notice {
            level: NoticeLevel::Error,
            text: info.message.clone(),
        });
    } else {
        let turn_id = state.mapper.begin_turn();
        updates.push(SessionUpdate::TurnStarted { turn_id });
        updates.extend(state.mapper.end_turn_with_error(info.clone()));
    }
    let session = state.session.clone();
    for update in updates {
        let turn = match &update {
            SessionUpdate::TurnStarted { turn_id } | SessionUpdate::TurnEnded { turn_id, .. } => {
                Some(turn_id.clone())
            }
            _ => None,
        };
        state.seq += 1;
        let envelope = UpdateEnvelope::live(
            instance.clone(),
            0,
            session.clone(),
            turn,
            state.seq,
            update,
        );
        let _ = state.updates.send(envelope);
    }
}

fn harness_error(err: AgentHarnessError) -> BackendError {
    match err {
        AgentHarnessError::Busy => BackendError::Busy,
        AgentHarnessError::NoModelSelected => {
            BackendError::failed(ErrorCategory::Auth, err.to_string())
        }
        other => BackendError::failed(ErrorCategory::Internal, other.to_string()),
    }
}

fn prompt_input(input: Vec<ContentBlock>) -> BackendResult<PromptInput> {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in input {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t);
            }
            ContentBlock::Image { data, mime_type } => {
                images.push(ImageContent { data, mime_type })
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "prompt content {}",
                    serde_json::to_value(&other)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "of unknown type".into())
                )))
            }
        }
    }
    Ok(if images.is_empty() {
        PromptInput::Text(text)
    } else {
        PromptInput::TextWithImages { text, images }
    })
}

#[async_trait]
impl AgentBackend for NativeBackend {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            session_skills: true,
            workflow: cfg!(feature = "orchestration"),
            load_session: true,
            set_model: true,
            efforts: true,
            usage: true,
            images: true,
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

    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        let cwd = spec.cwd.map(|p| p.to_string_lossy().into_owned());
        let id = self
            .harness
            .start_new_session(cwd, spec.name)
            .await
            .map_err(harness_error)?;
        let session = self.adopt_session(&id);
        // A new Session starts from the defaults, not the last one's choices.
        let level = *self.default_effort.lock();
        let model = self.default_model.lock().clone();
        match model {
            Some(model) => self.harness.set_model(model).await,
            None => self.harness.clear_model().await,
        }
        self.harness.set_thinking_level(level).await;
        self.read_config().await;
        Ok(session)
    }

    async fn load_session(
        &self,
        session: &SessionRef,
        _spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        if session.instance != self.instance {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let id = self
            .harness
            .switch_session(&session.native_id)
            .await
            .map_err(harness_error)?;
        let session = self.adopt_session(&id);
        self.restore_config().await?;
        let history = self.transcript(&session).await?;
        let mut stream = self.stream.lock();
        for update in history {
            stream.seq += 1;
            let mut envelope = UpdateEnvelope::live(
                self.instance.clone(),
                0,
                session.key.clone(),
                None,
                stream.seq,
                update,
            );
            envelope.origin = Origin::Replay;
            let _ = self.updates.send(envelope);
        }
        Ok(session)
    }

    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        self.check_session(session).await?;
        let context = self
            .harness
            .session_context()
            .await
            .map_err(harness_error)?;
        Ok(replay(&context.messages))
    }

    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        self.check_session(session).await?;
        // A model change in progress finishes before this turn is admitted.
        let _admission = self.admission.lock().await;
        // Reserve the turn before checking the harness, so two callers
        // cannot both pass the check.
        if self
            .prompting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BackendError::Busy);
        }
        let slot = PromptSlot(Arc::clone(&self.prompting));
        if self.harness.phase() != AgentHarnessPhase::Idle {
            return Err(BackendError::Busy);
        }
        let input = prompt_input(input)?;
        self.cancel_requested.store(false, Ordering::Release);
        let jobs: Vec<NextTurnJob> = self.next_turn.lock().drain().map(|(_, job)| job).collect();
        for job in jobs {
            job(true).await;
        }
        // A model chosen during the last turn starts this one.
        let chosen = self.pending_model.lock().take();
        if let Some((key, model)) = chosen {
            if key == self.config.lock().session {
                self.harness.set_model(model).await;
            }
        }
        // A cancel during preparation found no turn to abort; it stops the
        // prompt here, before any model or tool call.
        if self.cancel_requested.load(Ordering::Acquire) {
            return Err(BackendError::failed(
                ErrorCategory::Cancelled,
                "the turn was cancelled before it started",
            ));
        }
        let (done, handle) = TurnHandle::channel();
        let harness = Arc::clone(&self.harness);
        let instance = self.instance.clone();
        let stream = Arc::clone(&self.stream);
        let turns_before = self.stream.lock().mapper.turns_started();
        let config = Arc::clone(&self.config);
        let hold = TurnHold::start(instance.clone(), Arc::clone(&stream), Arc::clone(&config));
        tokio::spawn(async move {
            let on_written = Arc::clone(&hold);
            let (counted, count_instance, count_stream, count_config) = (
                Arc::clone(&harness),
                instance.clone(),
                Arc::clone(&stream),
                Arc::clone(&config),
            );
            let written: TurnWritten = Box::new(move |failure| {
                // The same text `harness_error` gives the returned error.
                let info = failure
                    .map(|e| ErrorInfo::new(ErrorCategory::Internal, format!("session: {e}")));
                on_written.written(info.as_ref());
                // The tree counts each automatic compaction in the Session
                // revision, as it counts `session/compact`.
                let compactions = counted.take_turn_compactions();
                if compactions > 0 {
                    let update = {
                        let mut state = count_config.lock();
                        state.revision += compactions;
                        state.update()
                    };
                    announce(&count_instance, &count_stream, update);
                }
            });
            let result = harness.prompt_then_written(input, written).await;
            let rejected_busy = matches!(result, Err(AgentHarnessError::Busy));
            // A run that returned a message already ended its turn on the
            // stream, even when the message is an error.
            let ran = result.is_ok();
            let outcome = match result {
                Ok(message) => match message.as_assistant().map(turn_outcome) {
                    Some((StopReason::Cancelled, _)) => TurnOutcome::Cancelled,
                    Some((StopReason::Error, error)) => {
                        TurnOutcome::Failed(error.unwrap_or_else(|| {
                            ErrorInfo::new(ErrorCategory::Agent, "the turn failed")
                        }))
                    }
                    Some((stop, _)) => TurnOutcome::Completed(stop),
                    None => TurnOutcome::Completed(StopReason::EndTurn),
                },
                Err(AgentHarnessError::Busy) => TurnOutcome::Failed(ErrorInfo::new(
                    ErrorCategory::Internal,
                    "a turn is already running",
                )),
                Err(err) => TurnOutcome::Failed(match harness_error(err) {
                    BackendError::Failed(info) => info,
                    other => ErrorInfo::new(ErrorCategory::Internal, other.to_string()),
                }),
            };
            // Normally the harness released the turn's end after its tree
            // writes. A run that never wrote (refused before its turn) has
            // nothing held; anything left is sent now, as it is.
            hold.written(None);
            let reported = hold.failure_reported.load(Ordering::Acquire);
            drop(hold);
            drop(slot);
            match &outcome {
                // Another caller of the harness is running a turn; that
                // turn is not this prompt's to close.
                TurnOutcome::Failed(info) if rejected_busy => {
                    error_notice(&instance, &stream, info);
                }
                // The held turn end already reported this failure.
                TurnOutcome::Failed(_) if reported && !ran => {}
                TurnOutcome::Failed(info) if !ran => {
                    close_failed_turn(&instance, &stream, turns_before, info);
                    apply_pending(&instance, &stream, &config);
                }
                _ => {}
            }
            let _ = done.send(outcome);
        });
        Ok(handle)
    }

    fn cancel(&self, _session: &SessionRef) {
        if self.prompting.load(Ordering::Acquire) {
            self.cancel_requested.store(true, Ordering::Release);
        }
        self.harness.abort();
    }

    fn is_busy(&self, _session: &SessionRef) -> bool {
        self.is_busy_now()
    }

    async fn shutdown(&self) {
        self.harness.request_shutdown();
        self.release_next_turn().await;
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

    fn model_control(&self) -> Option<&dyn ModelControl> {
        Some(self)
    }
}

#[async_trait]
impl ModelControl for NativeBackend {
    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.current_config(session).await?;
        let models = self.harness.models();
        let available = models.get_available().await;
        let state = self.config.lock();
        Ok(ModelChoices {
            current: state.config.model.clone(),
            models: available
                .iter()
                .filter(|m| !models.credentials().is_logged_out(&m.provider))
                .map(|m| ModelChoice {
                    id: model_id(m),
                    name: m.name.clone(),
                    description: None,
                })
                .collect(),
            revision: state.revision,
        })
    }

    async fn set_model(
        &self,
        session: &SessionRef,
        model: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        // Provider ids never contain `/`; model ids may.
        let (provider, id) = model
            .split_once('/')
            .ok_or_else(|| invalid(format!("choose a model as provider/model, not `{model}`")))?;
        if self.harness.models().credentials().is_logged_out(provider) {
            return Err(invalid(format!(
                "{provider} is logged out; run /login {provider}"
            )));
        }
        let resolved = self
            .harness
            .models()
            .get_model(provider, id)
            .ok_or_else(|| invalid(format!("model not found: {provider}/{id}")))?;
        self.check_revision(expected_revision)?;
        // A model change never lands in the middle of a turn; holding
        // admission keeps a turn from starting until it is done.
        let _admission = self.admission.lock().await;
        // Recorded first: a choice that cannot be saved is not made.
        self.harness
            .record_config(PendingSessionWrite::ModelChange {
                provider: provider.into(),
                model_id: id.into(),
            })
            .await
            .map_err(harness_error)?;
        if self.is_busy_now() {
            // The running turn keeps its model; the next one starts on this.
            let key = self.config.lock().session.clone();
            *self.pending_model.lock() = Some((key, resolved));
        } else {
            *self.pending_model.lock() = None;
            self.harness.set_model(resolved).await;
        }
        Ok(self.announce_change().await)
    }

    async fn efforts(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.current_config(session).await?;
        let model = self.chosen_model().await;
        let state = self.config.lock();
        Ok(ModelChoices {
            current: Some(effective_effort(model.as_ref(), state.level)),
            models: native_efforts(model.as_ref(), &NATIVE_EFFORTS)
                .into_iter()
                .map(|level| ModelChoice {
                    name: level.clone(),
                    id: level,
                    description: None,
                })
                .collect(),
            revision: state.revision,
        })
    }

    async fn set_effort(
        &self,
        session: &SessionRef,
        effort: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        let level = effort.trim().to_lowercase();
        let model = self.chosen_model().await;
        let levels = native_efforts(model.as_ref(), &NATIVE_EFFORTS);
        if !levels.contains(&level) {
            return Err(invalid(format!(
                "unknown effort `{level}`; choose one of: {}",
                levels.join(", ")
            )));
        }
        let thinking_level = parse_thinking(&level);
        self.check_revision(expected_revision)?;
        self.harness
            .record_config(PendingSessionWrite::ThinkingLevelChange { thinking_level })
            .await
            .map_err(harness_error)?;
        // A running turn keeps the level it started with.
        self.harness.set_thinking_level(thinking_level).await;
        Ok(self.announce_change().await)
    }

    async fn ttfb(
        &self,
        session: &SessionRef,
        change: Option<Option<u64>>,
        expected_revision: Option<u64>,
    ) -> BackendResult<loop_protocol::rpc::TtfbResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        let mut seq = None;
        let mut pending = self.config.lock().pending.is_some();
        if let Some(timeout) = change {
            self.check_revision(expected_revision)?;
            self.harness
                .record_config(PendingSessionWrite::Custom {
                    custom_type: RESPONSE_TIMEOUT_ENTRY.into(),
                    data: serde_json::json!({ "timeout_ms": timeout }),
                })
                .await
                .map_err(harness_error)?;
            let default = *self.default_response_timeout.lock();
            // A running turn keeps the timeout it started with.
            self.harness
                .set_response_header_timeout_ms(timeout.unwrap_or(default))
                .await;
            self.config.lock().config.response_timeout_ms = timeout;
            let result = self.announce_change().await;
            seq = Some(result.seq);
            pending = result.pending;
        }
        let state = self.config.lock();
        Ok(loop_protocol::rpc::TtfbResult {
            override_ms: state.config.response_timeout_ms,
            effective_ms: state
                .config
                .response_timeout_ms
                .unwrap_or(*self.default_response_timeout.lock()),
            revision: state.revision,
            seq,
            pending,
        })
    }
}

#[async_trait]
impl Compaction for NativeBackend {
    async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<CompactionReport> {
        self.check_session(session).await?;
        let result = self
            .harness
            .compact(instructions)
            .await
            .map_err(harness_error)?;
        // The tree counts the compaction in the Session revision.
        self.record_change(None);
        Ok(CompactionReport {
            tokens_before: Some(result.tokens_before),
            summary: result.summary,
            ..CompactionReport::default()
        })
    }
}

impl NativeBackend {
    /// The current Session's revision.
    pub fn revision(&self) -> u64 {
        self.config.lock().revision
    }

    /// Record the Session's permission mode in its tree, counted in its
    /// revision. The caller applies the mode to its own gate.
    pub async fn record_permission_mode(
        &self,
        session: &SessionRef,
        mode: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        self.check_revision(expected_revision)?;
        self.write_permission_mode(mode).await
    }

    /// Record a mode the tool gate changed on its own (approving a plan
    /// returns to the mode it was entered from), so every Surface sees it.
    pub async fn record_gate_mode(&self, mode: &str) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.write_permission_mode(mode).await
    }

    async fn write_permission_mode(&self, mode: &str) -> BackendResult<MutationResult> {
        self.harness
            .record_config(PendingSessionWrite::Custom {
                custom_type: PERMISSION_MODE_ENTRY.into(),
                data: serde_json::json!({ "mode": mode }),
            })
            .await
            .map_err(harness_error)?;
        self.config.lock().config.mode = Some(mode.to_owned());
        Ok(self.announce_change().await)
    }

    /// Check a user command's timeout: by default, and at most, the
    /// `bash.maxTimeoutMs` limit. Under a tool lock every command is
    /// refused here, before the Runtime reserves the Session.
    pub fn shell_timeout(&self, requested: Option<u64>) -> BackendResult<u64> {
        self.refuse_if_locked("session/shell")?;
        loop_protocol::shell_timeout(requested, self.harness.bash_limits().max_timeout_ms)
    }

    /// Lock this backend's tools for good: user commands (`session/shell`)
    /// and workflows are refused from now on. A second lock is ignored; the
    /// first one stays.
    pub fn lock_tools(&self, lock: loop_protocol::ToolLock) {
        let _ = self.tool_lock.set(lock);
    }

    /// The tool lock in force, if any.
    pub fn tool_lock(&self) -> Option<loop_protocol::ToolLock> {
        self.tool_lock.get().copied()
    }

    /// The turn running now, if any: for decision records.
    pub fn current_turn(&self) -> Option<TurnId> {
        self.stream.lock().mapper.current_turn().cloned()
    }

    /// Check every user command with `gate` from now on. A second gate is
    /// ignored; the first one stays.
    pub fn set_command_gate(&self, gate: CommandGate) {
        let _ = self.command_gate.set(gate);
    }

    /// Run a user command for `caller`: [`LocalShell::run`] with the caller
    /// passed in, for a caller that read [`loop_protocol::Caller::current`]
    /// before it spawned a task.
    ///
    /// The outer checks come first: the tool lock refuses every command.
    /// Then the Policy gate decides as it would for the `bash` tool, and
    /// records the decision; with no gate installed the command is refused.
    /// Then the `bash` block list applies, as it does to the tool.
    pub async fn run_as(
        &self,
        session: &SessionRef,
        command: &str,
        timeout_ms: Option<u64>,
        caller: Option<loop_protocol::Caller>,
    ) -> BackendResult<ShellResult> {
        // `shell_timeout` refuses under a tool lock; checked again here so
        // no later change to it can let a command through.
        self.refuse_if_locked("session/shell")?;
        let timeout_ms = self.shell_timeout(timeout_ms)?;
        self.check_session(session).await?;
        let refused = |reason: String| {
            BackendError::failed(
                ErrorCategory::Agent,
                format!("the command is refused: {reason}"),
            )
        };
        let Some(gate) = self.command_gate.get() else {
            return Err(refused(
                "no permission gate checks user commands in this Session".into(),
            ));
        };
        gate(UserCommand {
            session: session.key.clone(),
            command: command.to_owned(),
            caller,
        })
        .await
        .map_err(|refusal| refused(refusal.0))?;
        crate::harness::tools::check_command_policy(command, &[]).map_err(refused)?;
        let env = self.harness.tool_env().await.map_err(harness_error)?;
        let out = env
            .exec(
                command,
                ShellExecOptions {
                    timeout_ms: Some(timeout_ms),
                    ..ShellExecOptions::default()
                },
            )
            .await
            .map_err(|e| BackendError::failed(ErrorCategory::Agent, e.to_string()))?;
        Ok(ShellResult {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
            truncated: out.truncated,
        }
        .bounded())
    }

    /// [`BackendError::ToolLocked`] for `what` when a lock is set.
    pub(crate) fn refuse_if_locked(&self, what: &str) -> BackendResult<()> {
        match self.tool_lock() {
            Some(lock) => Err(BackendError::ToolLocked {
                what: what.to_owned(),
                lock,
            }),
            None => Ok(()),
        }
    }

    /// Prepare a compaction for a caller that commits the returned tree
    /// write itself and then calls [`Self::record_change`].
    pub async fn prepare_compaction(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<(CompactionReport, PendingSessionWrite)> {
        self.check_session(session).await?;
        let (result, write) = self
            .harness
            .prepare_compaction(instructions)
            .await
            .map_err(harness_error)?;
        Ok((
            CompactionReport {
                tokens_before: Some(result.tokens_before),
                summary: result.summary,
                ..CompactionReport::default()
            },
            write,
        ))
    }
}

#[async_trait]
impl SessionStats for NativeBackend {
    async fn stats(&self, session: &SessionRef) -> BackendResult<StatsSummary> {
        self.check_session(session).await?;
        let stats = self.harness.session_stats().await.map_err(harness_error)?;
        // The window is the Session model's, known before any usage is.
        let window = match stats.context_usage.as_ref() {
            Some(usage) => Some(usage.context_window),
            None => self.harness.model().await.map(|m| m.context_window),
        };
        Ok(StatsSummary {
            total_tokens: stats.tokens.total_tokens(),
            context_tokens: stats.context_usage.as_ref().and_then(|c| c.tokens),
            context_window: window.filter(|w| *w > 0),
            report: format_session_stats(&stats),
            turn_state: Some(if self.is_busy_now() { "busy" } else { "idle" }.into()),
            usage: None,
        })
    }
}

#[async_trait]
impl LocalShell for NativeBackend {
    async fn run(
        &self,
        session: &SessionRef,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> BackendResult<ShellResult> {
        // The caller of the request in scope; an in-process call has none.
        self.run_as(
            session,
            command,
            timeout_ms,
            loop_protocol::Caller::current(),
        )
        .await
    }
}

impl NativeBackend {
    /// Activate or deactivate a skill under the Session admission lock.
    pub async fn skill(
        &self,
        p: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<MutationResult> {
        let _admission = self.admission.lock().await;
        let _setting = self.setting.lock().await;
        self.current_config(&p.session).await?;
        self.check_revision(p.expected_revision)?;
        if p.active {
            if !self.harness.activate_skill(&p.name).await {
                return Err(invalid(format!("skill not found: {}", p.name)));
            }
        } else {
            let active = self.harness.active_skills().await;
            self.harness.clear_active_skills().await;
            for name in active.into_iter().filter(|n| n != &p.name) {
                self.harness.activate_skill(&name).await;
            }
        }
        self.harness
            .record_config(PendingSessionWrite::Custom {
                custom_type: "active_skills".into(),
                data: serde_json::json!({"names": self.harness.active_skills().await}),
            })
            .await
            .map_err(harness_error)?;
        Ok(self.announce_change().await)
    }
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    type Parts = (
        BackendInstanceId,
        Arc<Mutex<StreamState>>,
        Arc<Mutex<ConfigState>>,
        broadcast::Receiver<UpdateEnvelope>,
    );

    fn parts() -> Parts {
        let (updates, rx) = broadcast::channel(64);
        let stream = Arc::new(Mutex::new(StreamState {
            session: SessionKey("s".into()),
            seq: 0,
            updates,
            mapper: NativeEventMapper::new(),
            turns: Default::default(),
            held: Vec::new(),
            holding: false,
        }));
        let config = Arc::new(Mutex::new(ConfigState {
            session: SessionKey("s".into()),
            revision: 0,
            name: None,
            level: AgentThinkingLevel::Off,
            config: SessionConfig::default(),
            effective: SessionConfig::default(),
            pending: None,
            sandbox_choice: None,
            sandbox_deferred: false,
        }));
        (BackendInstanceId::from(NATIVE_INSTANCE), stream, config, rx)
    }

    fn drain(rx: &mut broadcast::Receiver<UpdateEnvelope>) -> Vec<SessionUpdate> {
        std::iter::from_fn(|| rx.try_recv().ok().map(|e| e.update)).collect()
    }

    fn ends(updates: &[SessionUpdate]) -> Vec<(StopReason, bool)> {
        updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::TurnEnded { stop, error, .. } => Some((*stop, error.is_some())),
                _ => None,
            })
            .collect()
    }

    fn turn(instance: &BackendInstanceId, stream: &Mutex<StreamState>) {
        translate(instance, stream, &AgentEvent::AgentStart);
        translate(instance, stream, &AgentEvent::AgentEnd { messages: vec![] });
    }

    #[test]
    fn a_held_end_waits_for_the_tree_and_later_turns_flow() {
        let (instance, stream, config, mut rx) = parts();
        let hold = TurnHold::start(instance.clone(), Arc::clone(&stream), config);
        turn(&instance, &stream);
        assert!(
            ends(&drain(&mut rx)).is_empty(),
            "the end waits for the tree"
        );
        hold.written(None);
        assert_eq!(ends(&drain(&mut rx)), [(StopReason::EndTurn, false)]);
        // A turn end after the release (before the prompt slot drops) flows.
        turn(&instance, &stream);
        assert_eq!(ends(&drain(&mut rx)), [(StopReason::EndTurn, false)]);
        drop(hold);
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn a_panic_in_the_held_window_releases_the_end_as_failed() {
        let (instance, stream, config, mut rx) = parts();
        let hold = TurnHold::start(instance.clone(), Arc::clone(&stream), config);
        let (i, s) = (instance.clone(), Arc::clone(&stream));
        let task = tokio::spawn(async move {
            let _hold = hold;
            turn(&i, &s);
            panic!("the harness panicked before its tree writes");
        });
        assert!(task.await.unwrap_err().is_panic());
        let updates = drain(&mut rx);
        assert_eq!(ends(&updates), [(StopReason::Error, true)]);
        // Later updates are not held.
        turn(&instance, &stream);
        assert_eq!(ends(&drain(&mut rx)), [(StopReason::EndTurn, false)]);
    }

    #[test]
    fn a_write_failure_rewrites_only_the_first_held_end() {
        let (instance, stream, config, mut rx) = parts();
        let hold = TurnHold::start(instance.clone(), Arc::clone(&stream), config);
        turn(&instance, &stream);
        turn(&instance, &stream);
        let info = ErrorInfo::new(ErrorCategory::Internal, "session: storage: full");
        hold.written(Some(&info));
        assert!(hold.failure_reported.load(Ordering::Acquire));
        assert_eq!(
            ends(&drain(&mut rx)),
            [(StopReason::Error, true), (StopReason::EndTurn, false)]
        );
    }
}
