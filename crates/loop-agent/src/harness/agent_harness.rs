//! Production orchestration: sessions, turn snapshots, tools, sandbox.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::run_agent_loop;
use crate::harness::compaction::{
    estimate_tokens, generate_branch_summary_fallback, generate_summary_fallback,
    prepare_compaction, CompactionSettings,
};
use crate::harness::hooks::{HarnessHookEvent, HookRegistry};
use crate::harness::prompt_templates::format_prompt_template_invocation;
use crate::harness::sandbox::{Sandbox, SandboxMode, SandboxStatus};
use crate::harness::session::types::{PendingSessionWrite, Session, SessionTreeEntry};
use crate::harness::skills::format_skill_invocation;
use crate::harness::types::{
    AgentHarnessError, AgentHarnessPhase, AgentHarnessResources, CompactResult, ExecutionEnv,
    NavigateTreeResult,
};
use crate::harness::system_prompt::format_skills_for_system_prompt;
use crate::messages::{convert_to_llm, user_message_with_images};
use crate::stream_fn::{stream_fn_from_models, StreamFn};
use crate::types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentEvent, AgentEventSink,
    AgentLoopConfig, AgentMessage, AgentThinkingLevel, AgentTool, BeforeToolCallContext,
    BeforeToolCallResult, PromptInput, QueueMode, ToolExecutionMode,
};

type BeforeToolHook = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>
        + Send
        + Sync,
>;

type AfterToolHook = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;
use loop_ai::{ImageContent, Model, Models, SimpleStreamOptions};

type Sub = Arc<dyn Fn(AgentEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Options for [`AgentHarness`].
pub struct AgentHarnessOptions {
    /// Models collection.
    pub models: Arc<Models>,
    /// Initial model.
    pub model: Model,
    /// Session.
    pub session: Session,
    /// Host execution env.
    pub host_env: Arc<dyn ExecutionEnv>,
    /// Tools.
    pub tools: Vec<AgentTool>,
    /// System prompt.
    pub system_prompt: String,
    /// Sandbox mode.
    pub sandbox: SandboxMode,
    /// Resources.
    pub resources: AgentHarnessResources,
}

/// Turn snapshot used for one LLM turn.
#[derive(Clone)]
pub struct TurnSnapshot {
    /// Messages.
    pub messages: Vec<AgentMessage>,
    /// System prompt.
    pub system_prompt: String,
    /// Model.
    pub model: Model,
    /// Thinking level.
    pub thinking_level: AgentThinkingLevel,
    /// Active tools.
    pub tools: Vec<AgentTool>,
    /// Stream options.
    pub stream_options: SimpleStreamOptions,
    /// Tool execution env for this turn.
    pub tool_env: Arc<dyn ExecutionEnv>,
}

/// Agent harness.
pub struct AgentHarness {
    #[allow(dead_code)]
    models: Arc<Models>,
    stream_fn: StreamFn,
    session: Arc<tokio::sync::Mutex<Session>>,
    host_env: Arc<dyn ExecutionEnv>,
    model: RwLock<Model>,
    thinking_level: RwLock<AgentThinkingLevel>,
    tools: RwLock<Vec<AgentTool>>,
    active_tool_names: RwLock<Option<Vec<String>>>,
    system_prompt: RwLock<String>,
    resources: RwLock<AgentHarnessResources>,
    /// Skills activated via `/skill:name` (included in `<available_skills>` even
    /// when `disable-model-invocation` is set). Does not trigger a prompt.
    active_skill_names: RwLock<Vec<String>>,
    stream_options: RwLock<SimpleStreamOptions>,
    sandbox: RwLock<SandboxMode>,
    phase: Mutex<AgentHarnessPhase>,
    pending_writes: Arc<Mutex<Vec<PendingSessionWrite>>>,
    steering: Arc<Mutex<VecDeque<AgentMessage>>>,
    follow_up: Arc<Mutex<VecDeque<AgentMessage>>>,
    next_turn: Mutex<VecDeque<AgentMessage>>,
    subscribers: Mutex<Vec<Sub>>,
    cancel: Mutex<Option<CancellationToken>>,
    idle: Notify,
    hooks: HookRegistry,
    shutting_down: AtomicBool,
    shutdown_notify: Notify,
    compaction_settings: RwLock<CompactionSettings>,
    tool_execution: Mutex<ToolExecutionMode>,
    before_tool_call: Mutex<Option<BeforeToolHook>>,
    after_tool_call: Mutex<Option<AfterToolHook>>,
    #[allow(dead_code)]
    steering_mode: Mutex<QueueMode>,
    #[allow(dead_code)]
    follow_up_mode: Mutex<QueueMode>,
}

impl AgentHarness {
    /// Create a harness.
    pub fn new(options: AgentHarnessOptions) -> Self {
        let stream_fn = stream_fn_from_models(Arc::clone(&options.models));
        let mut stream_options = SimpleStreamOptions::default();
        stream_options.base.session_id = Some(options.session.metadata().id.clone());
        Self {
            models: options.models,
            stream_fn,
            session: Arc::new(tokio::sync::Mutex::new(options.session)),
            host_env: options.host_env,
            model: RwLock::new(options.model),
            thinking_level: RwLock::new(AgentThinkingLevel::Off),
            tools: RwLock::new(options.tools),
            active_tool_names: RwLock::new(None),
            system_prompt: RwLock::new(options.system_prompt),
            resources: RwLock::new(options.resources),
            active_skill_names: RwLock::new(Vec::new()),
            stream_options: RwLock::new(stream_options),
            sandbox: RwLock::new(options.sandbox),
            phase: Mutex::new(AgentHarnessPhase::Idle),
            pending_writes: Arc::new(Mutex::new(Vec::new())),
            steering: Arc::new(Mutex::new(VecDeque::new())),
            follow_up: Arc::new(Mutex::new(VecDeque::new())),
            next_turn: Mutex::new(VecDeque::new()),
            subscribers: Mutex::new(Vec::new()),
            cancel: Mutex::new(None),
            idle: Notify::new(),
            hooks: HookRegistry::default(),
            shutting_down: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            compaction_settings: RwLock::new(CompactionSettings::default()),
            tool_execution: Mutex::new(ToolExecutionMode::Parallel),
            before_tool_call: Mutex::new(None),
            after_tool_call: Mutex::new(None),
            steering_mode: Mutex::new(QueueMode::OneAtATime),
            follow_up_mode: Mutex::new(QueueMode::OneAtATime),
        }
    }

    /// Current session id (also sent on provider API requests).
    pub async fn session_id(&self) -> String {
        self.session.lock().await.metadata().id.clone()
    }

    /// Build agent context from the active session branch (messages, model, thinking).
    /// Used by the CLI to restore the transcript on `--resume`.
    pub async fn session_context(
        &self,
    ) -> Result<crate::harness::session::SessionContext, AgentHarnessError> {
        let session = self.session.lock().await;
        session
            .build_context()
            .await
            .map_err(AgentHarnessError::Session)
    }

    /// Abort any in-flight turn, create an empty session on the same store, and
    /// switch the harness to it. Clears steering / follow-up / pending writes.
    /// Returns the new session id.
    pub async fn start_new_session(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<String, AgentHarnessError> {
        self.assert_not_shut_down()?;
        self.abort();
        self.wait_for_idle().await;

        let store = self.session.lock().await.store();
        let reader = store
            .create(cwd, name)
            .await
            .map_err(AgentHarnessError::Session)?;
        let new_session = Session::new(store, reader);
        let id = new_session.metadata().id.clone();

        *self.session.lock().await = new_session;
        {
            let mut opts = self.stream_options.write().await;
            opts.base.session_id = Some(id.clone());
        }
        self.steering.lock().clear();
        self.follow_up.lock().clear();
        self.next_turn.lock().clear();
        self.pending_writes.lock().clear();
        self.active_skill_names.write().await.clear();

        Ok(id)
    }

    /// List user-message fork points on the active branch (oldest → newest).
    pub async fn fork_points(
        &self,
    ) -> Result<Vec<crate::harness::session::SessionForkPoint>, AgentHarnessError> {
        let session = self.session.lock().await;
        let branch = session
            .read_branch()
            .await
            .map_err(AgentHarnessError::Session)?;
        Ok(crate::harness::session::fork_points_from_branch(&branch))
    }

    /// Abort any in-flight turn, fork the current session into a new one, and
    /// switch the harness to it. Clears steering / follow-up / pending writes.
    /// Returns the new session id.
    pub async fn fork_session(
        &self,
        selection: crate::harness::session::SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<String, AgentHarnessError> {
        self.assert_not_shut_down()?;
        self.abort();
        self.wait_for_idle().await;

        let (store, source_id) = {
            let session = self.session.lock().await;
            (session.store(), session.metadata().id.clone())
        };
        let reader = store
            .fork(&source_id, selection, through_entry_id, name)
            .await
            .map_err(AgentHarnessError::Session)?;
        let new_session = Session::new(store, reader);
        let id = new_session.metadata().id.clone();

        *self.session.lock().await = new_session;
        {
            let mut opts = self.stream_options.write().await;
            opts.base.session_id = Some(id.clone());
        }
        self.steering.lock().clear();
        self.follow_up.lock().clear();
        self.next_turn.lock().clear();
        self.pending_writes.lock().clear();
        self.active_skill_names.write().await.clear();

        Ok(id)
    }

    /// Current phase.
    pub fn phase(&self) -> AgentHarnessPhase {
        *self.phase.lock()
    }

    /// Set sandbox mode (applies on next turn). Destroys any previous sandbox.
    pub async fn set_sandbox(&self, mode: SandboxMode) {
        let prev = std::mem::replace(&mut *self.sandbox.write().await, SandboxMode::Disabled);
        if let SandboxMode::Enabled { sandbox } = prev {
            let _ = sandbox.destroy().await;
        }
        *self.sandbox.write().await = mode;
    }

    /// Disable sandbox and destroy the previous instance if any.
    pub async fn clear_sandbox(&self) {
        let prev = std::mem::replace(&mut *self.sandbox.write().await, SandboxMode::Disabled);
        if let SandboxMode::Enabled { sandbox } = prev {
            let _ = sandbox.destroy().await;
        }
    }

    /// Ensure sandbox ready when enabled.
    pub async fn ensure_sandbox_ready(
        &self,
    ) -> Result<Option<Arc<dyn Sandbox>>, AgentHarnessError> {
        let mode = self.sandbox.read().await.clone();
        match mode {
            SandboxMode::Disabled => Ok(None),
            SandboxMode::Enabled { sandbox } => {
                if sandbox.status() != SandboxStatus::Ready {
                    sandbox
                        .start()
                        .await
                        .map_err(|e| AgentHarnessError::Sandbox(e.to_string()))?;
                }
                Ok(Some(sandbox))
            }
        }
    }

    /// Set model.
    pub async fn set_model(&self, model: Model) {
        *self.model.write().await = model;
    }

    /// Set thinking level.
    pub async fn set_thinking_level(&self, level: AgentThinkingLevel) {
        *self.thinking_level.write().await = level;
    }

    /// Set tools (rejects duplicate names).
    pub async fn set_tools(&self, tools: Vec<AgentTool>) -> Result<(), AgentHarnessError> {
        let mut names = std::collections::HashSet::new();
        for t in &tools {
            if !names.insert(t.name.clone()) {
                return Err(AgentHarnessError::Other(format!(
                    "duplicate tool name: {}",
                    t.name
                )));
            }
        }
        *self.tools.write().await = tools;
        Ok(())
    }

    /// Set active tool names filter.
    pub async fn set_active_tools(&self, names: Option<Vec<String>>) {
        *self.active_tool_names.write().await = names;
    }

    /// Set resources.
    pub async fn set_resources(&self, resources: AgentHarnessResources) {
        *self.resources.write().await = resources;
    }

    /// Get resources (clone).
    pub async fn get_resources(&self) -> AgentHarnessResources {
        self.resources.read().await.clone()
    }

    /// Activate a skill for subsequent turns without prompting.
    ///
    /// The skill is listed under `<available_skills>` (including skills with
    /// `disable-model-invocation`). Returns `false` if the skill is unknown.
    pub async fn activate_skill(&self, name: &str) -> bool {
        let resources = self.resources.read().await;
        if !resources.skills.iter().any(|s| s.name == name) {
            return false;
        }
        drop(resources);
        let mut active = self.active_skill_names.write().await;
        if !active.iter().any(|n| n == name) {
            active.push(name.to_string());
        }
        true
    }

    /// Names of skills activated via [`Self::activate_skill`].
    pub async fn active_skills(&self) -> Vec<String> {
        self.active_skill_names.read().await.clone()
    }

    /// Clear user-activated skills.
    pub async fn clear_active_skills(&self) {
        self.active_skill_names.write().await.clear();
    }

    /// Subscribe to events.
    pub fn subscribe<F, Fut>(&self, handler: F)
    where
        F: Fn(AgentEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.subscribers
            .lock()
            .push(Arc::new(move |e| Box::pin(handler(e))));
    }

    /// Set before-tool-call hook (optional gate / transform).
    pub fn set_before_tool_call(&self, hook: Option<BeforeToolHook>) {
        *self.before_tool_call.lock() = hook;
    }

    /// Set after-tool-call hook (e.g. interactive file-edit review).
    pub fn set_after_tool_call(&self, hook: Option<AfterToolHook>) {
        *self.after_tool_call.lock() = hook;
    }

    /// Register a harness hook.
    pub fn on<F, Fut>(&self, handler: F)
    where
        F: Fn(crate::harness::hooks::HarnessHookEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::harness::hooks::HookOutcome> + Send + 'static,
    {
        self.hooks.on(handler);
    }

    /// Whether shutdown has been requested.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    fn assert_not_shut_down(&self) -> Result<(), AgentHarnessError> {
        if self.shutting_down.load(Ordering::Relaxed) {
            return Err(AgentHarnessError::ShuttingDown);
        }
        Ok(())
    }

    fn emit_queue_update(&self) {
        let hooks = self.hooks.clone();
        tokio::spawn(async move {
            let _ = hooks.emit(HarnessHookEvent::QueueUpdate).await;
        });
    }

    async fn acquire_idle_phase(
        &self,
        target: AgentHarnessPhase,
    ) -> Result<(), AgentHarnessError> {
        self.assert_not_shut_down()?;
        let mut phase = self.phase.lock();
        if *phase != AgentHarnessPhase::Idle {
            return Err(AgentHarnessError::Busy);
        }
        *phase = target;
        Ok(())
    }

    fn release_to_idle(&self) {
        *self.phase.lock() = AgentHarnessPhase::Idle;
        self.idle.notify_waiters();
        if self.shutting_down.load(Ordering::Relaxed) {
            self.shutdown_notify.notify_waiters();
        }
    }

    /// Set compaction settings.
    pub async fn set_compaction_settings(&self, settings: CompactionSettings) {
        *self.compaction_settings.write().await = settings;
    }

    /// Get compaction settings (clone).
    pub async fn compaction_settings(&self) -> CompactionSettings {
        self.compaction_settings.read().await.clone()
    }

    /// Append a label to the session (immediate when idle, deferred during a turn).
    pub async fn append_label(&self, label: impl Into<String>) -> Result<(), AgentHarnessError> {
        let label = label.into();
        if *self.phase.lock() == AgentHarnessPhase::Idle {
            let session = self.session.lock().await;
            session
                .store()
                .append_entry(
                    &session.metadata().id,
                    PendingSessionWrite::Label { label },
                )
                .await
                .map_err(AgentHarnessError::Session)?;
        } else {
            self.pending_writes
                .lock()
                .push(PendingSessionWrite::Label { label });
        }
        Ok(())
    }

    /// Read all session entries (full tree).
    pub async fn read_session_entries(
        &self,
    ) -> Result<Vec<SessionTreeEntry>, AgentHarnessError> {
        let session = self.session.lock().await;
        session
            .read_entries()
            .await
            .map_err(AgentHarnessError::Session)
    }

    /// Append a message to the session (immediate when idle, deferred during a turn).
    pub async fn append_message(
        &self,
        message: AgentMessage,
    ) -> Result<(), AgentHarnessError> {
        if *self.phase.lock() == AgentHarnessPhase::Idle {
            let session = self.session.lock().await;
            session
                .append_message(message)
                .await
                .map_err(AgentHarnessError::Session)?;
        } else {
            self.pending_writes
                .lock()
                .push(PendingSessionWrite::Message { message });
        }
        Ok(())
    }

    /// Invoke a skill by name (idle only).
    pub async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> Result<AgentMessage, AgentHarnessError> {
        self.assert_not_shut_down()?;
        let resources = self.resources.read().await;
        let skill = resources
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| AgentHarnessError::Other(format!("skill not found: {name}")))?;
        let text = format_skill_invocation(skill, additional_instructions.unwrap_or(""));
        drop(resources);
        self.prompt(PromptInput::Text(text)).await
    }

    /// Prompt from a named template (idle only).
    pub async fn prompt_from_template(
        &self,
        name: &str,
        args: &str,
    ) -> Result<AgentMessage, AgentHarnessError> {
        self.assert_not_shut_down()?;
        let resources = self.resources.read().await;
        let template = resources
            .prompt_templates
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| AgentHarnessError::Other(format!("template not found: {name}")))?;
        let text = format_prompt_template_invocation(template, args);
        drop(resources);
        self.prompt(PromptInput::Text(text)).await
    }

    /// Compact session context (idle only).
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactResult, AgentHarnessError> {
        let settings = self.compaction_settings.read().await.clone();
        self.compact_with_settings(&settings, custom_instructions)
            .await
    }

    /// Compact with explicit settings (idle only).
    pub async fn compact_with_settings(
        &self,
        settings: &CompactionSettings,
        custom_instructions: Option<&str>,
    ) -> Result<CompactResult, AgentHarnessError> {
        self.acquire_idle_phase(AgentHarnessPhase::Compaction)
            .await?;

        let result = self
            .compact_inner(settings, custom_instructions)
            .await
            .map_err(|e| {
                self.release_to_idle();
                e
            });

        self.release_to_idle();
        result
    }

    /// Aggregate session / token / cost statistics for `/session`.
    pub async fn session_stats(
        &self,
    ) -> Result<crate::harness::session::SessionStats, AgentHarnessError> {
        use crate::harness::session::{compute_session_stats, SessionStatsInput};

        let (meta, all_entries, branch_entries, branch_messages) = {
            let session = self.session.lock().await;
            let meta = session.metadata().clone();
            let all_entries = session
                .read_entries()
                .await
                .map_err(AgentHarnessError::Session)?;
            let branch_entries = session
                .read_branch()
                .await
                .map_err(AgentHarnessError::Session)?;
            let ctx = session
                .build_context()
                .await
                .map_err(AgentHarnessError::Session)?;
            (meta, all_entries, branch_entries, ctx.messages)
        };

        let model = self.model.read().await.clone();
        let system_prompt = self.system_prompt.read().await.clone();
        let tools = self.tools.read().await.clone();
        let llm_tools: Vec<loop_ai::Tool> = tools.iter().map(|t| t.to_llm_tool()).collect();

        Ok(compute_session_stats(SessionStatsInput {
            all_entries: &all_entries,
            branch_entries: &branch_entries,
            branch_messages: &branch_messages,
            session_id: &meta.id,
            session_name: meta.name.as_deref(),
            session_path: meta.path.as_deref(),
            cwd: meta.cwd.as_deref(),
            created_at: meta.created_at,
            parent_session_id: meta.parent_session_id.as_deref(),
            model: &model,
            system_prompt: &system_prompt,
            tools: Some(llm_tools.as_slice()),
            models: Some(&self.models),
        }))
    }

    async fn compact_inner(
        &self,
        settings: &CompactionSettings,
        custom_instructions: Option<&str>,
    ) -> Result<CompactResult, AgentHarnessError> {
        let ctx = {
            let session = self.session.lock().await;
            session
                .build_context()
                .await
                .map_err(AgentHarnessError::Session)?
        };

        let llm = convert_to_llm(&ctx.messages);
        let tokens_before = estimate_tokens(&llm);

        let prep = prepare_compaction(&ctx.messages, &llm, settings).map_err(|e| {
            AgentHarnessError::Compaction(e.to_string())
        })?;

        let hook = self
            .hooks
            .emit(HarnessHookEvent::SessionBeforeCompact {
                preparation_cut: prep.cut_index,
                custom_instructions: custom_instructions.map(|s| s.to_string()),
            })
            .await;

        if hook.cancel {
            return Err(AgentHarnessError::Hook("compaction cancelled".into()));
        }

        let summary = hook
            .summary
            .unwrap_or_else(|| generate_summary_fallback(&prep.to_summarize));

        let branch = {
            let session = self.session.lock().await;
            session
                .read_branch()
                .await
                .map_err(AgentHarnessError::Session)?
        };
        let first_kept_entry_id = first_kept_entry_id_for_cut(&branch, prep.cut_index);

        {
            let session = self.session.lock().await;
            session
                .store()
                .append_entry(
                    &session.metadata().id,
                    PendingSessionWrite::Compaction {
                        summary: summary.clone(),
                        first_kept_entry_id,
                        details: None,
                    },
                )
                .await
                .map_err(AgentHarnessError::Session)?;
        }

        Ok(CompactResult {
            summary,
            tokens_before,
        })
    }

    /// Navigate the session tree to a target entry (idle only).
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        summarize: bool,
    ) -> Result<NavigateTreeResult, AgentHarnessError> {
        self.acquire_idle_phase(AgentHarnessPhase::BranchSummary)
            .await?;

        let result = self
            .navigate_tree_inner(target_id, summarize)
            .await
            .map_err(|e| {
                self.release_to_idle();
                e
            });

        self.release_to_idle();
        result
    }

    async fn navigate_tree_inner(
        &self,
        target_id: &str,
        summarize: bool,
    ) -> Result<NavigateTreeResult, AgentHarnessError> {
        let hook = self
            .hooks
            .emit(HarnessHookEvent::SessionBeforeTree {
                target_id: target_id.to_string(),
            })
            .await;

        if hook.cancel {
            return Ok(NavigateTreeResult {
                cancelled: true,
                summary: None,
            });
        }

        let mut summary = hook.summary;

        if summarize && summary.is_none() {
            let session = self.session.lock().await;
            let entries = session
                .reader()
                .read_entries(None)
                .await
                .map_err(AgentHarnessError::Session)?;
            summary = Some(generate_branch_summary_fallback(entries.len()));
        }

        if summarize {
            if let Some(ref s) = summary {
                let session = self.session.lock().await;
                session
                    .store()
                    .append_entry(
                        &session.metadata().id,
                        PendingSessionWrite::BranchSummary {
                            summary: s.clone(),
                        },
                    )
                    .await
                    .map_err(AgentHarnessError::Session)?;
            }
        }

        {
            let session = self.session.lock().await;
            session
                .move_to(Some(target_id.to_string()))
                .await
                .map_err(AgentHarnessError::Session)?;
        }

        Ok(NavigateTreeResult {
            cancelled: false,
            summary,
        })
    }

    /// Request graceful shutdown: abort current work and reject new prompts.
    pub fn request_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        self.steering.lock().clear();
        self.follow_up.lock().clear();
        self.next_turn.lock().clear();
        self.pending_writes.lock().clear();
        self.abort();
        self.shutdown_notify.notify_waiters();
        let hooks = self.hooks.clone();
        tokio::spawn(async move {
            let _ = hooks.emit(HarnessHookEvent::ShutdownRequested).await;
        });
    }

    /// Wait until shutdown was requested and the harness is idle.
    pub async fn wait_for_shutdown(&self) {
        loop {
            if self.shutting_down.load(Ordering::Relaxed)
                && *self.phase.lock() == AgentHarnessPhase::Idle
            {
                return;
            }
            tokio::select! {
                _ = self.idle.notified() => {}
                _ = self.shutdown_notify.notified() => {}
            }
        }
    }

    /// Steer.
    pub fn steer(&self, message: AgentMessage) {
        self.steering.lock().push_back(message);
        self.emit_queue_update();
    }

    /// Follow up.
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up.lock().push_back(message);
        self.emit_queue_update();
    }

    /// Queue work that survives abort.
    pub fn next_turn(&self, message: AgentMessage) {
        self.next_turn.lock().push_back(message);
        self.emit_queue_update();
    }

    /// Abort current turn.
    pub fn abort(&self) {
        if let Some(t) = self.cancel.lock().as_ref() {
            t.cancel();
        }
        self.steering.lock().clear();
        self.follow_up.lock().clear();
    }

    /// Wait until idle.
    pub async fn wait_for_idle(&self) {
        loop {
            if *self.phase.lock() == AgentHarnessPhase::Idle {
                return;
            }
            self.idle.notified().await;
        }
    }

    /// Create turn snapshot.
    pub async fn create_turn_state(&self) -> Result<TurnSnapshot, AgentHarnessError> {
        let sandbox = self.ensure_sandbox_ready().await?;
        let tool_env = if let Some(sb) = sandbox {
            sb.env()
        } else {
            Arc::clone(&self.host_env)
        };

        let ctx = {
            let session = self.session.lock().await;
            session
                .build_context()
                .await
                .map_err(AgentHarnessError::Session)?
        };

        let resources = self.resources.read().await.clone();
        let all_tools = self.tools.read().await.clone();
        let active = self.active_tool_names.read().await.clone();
        let tools = if let Some(names) = active {
            all_tools
                .into_iter()
                .filter(|t| names.iter().any(|n| n == &t.name))
                .collect()
        } else {
            all_tools
        };

        // Progressive disclosure (pi): only advertise skills when `read` is available
        // so the model can load SKILL.md on demand. User-activated skills
        // (`/skill:name`) are always included, even with disable-model-invocation.
        let mut system_prompt = self.system_prompt.read().await.clone();
        let has_read = tools.iter().any(|t| t.name == "read");
        if has_read && !resources.skills.is_empty() {
            let active = self.active_skill_names.read().await.clone();
            let skills_block = format_skills_for_system_prompt(&resources.skills, &active);
            if !skills_block.is_empty() {
                system_prompt = format!("{system_prompt}\n\n{skills_block}");
            }
        }

        let mut stream_options = self.stream_options.read().await.clone();
        {
            let session = self.session.lock().await;
            stream_options.base.session_id = Some(session.metadata().id.clone());
        }

        Ok(TurnSnapshot {
            messages: ctx.messages,
            system_prompt,
            model: self.model.read().await.clone(),
            thinking_level: *self.thinking_level.read().await,
            tools,
            stream_options,
            tool_env,
        })
    }

    /// Prompt the harness; returns last assistant message.
    pub async fn prompt(
        &self,
        input: impl Into<PromptInput>,
    ) -> Result<AgentMessage, AgentHarnessError> {
        self.assert_not_shut_down()?;
        self.run_turn(Some(input.into())).await
    }

    /// Prompt with images.
    pub async fn prompt_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> Result<AgentMessage, AgentHarnessError> {
        self.prompt(PromptInput::Message(user_message_with_images(text, images)))
            .await
    }

    async fn run_turn(
        &self,
        input: Option<PromptInput>,
    ) -> Result<AgentMessage, AgentHarnessError> {
        {
            let mut phase = self.phase.lock();
            if *phase != AgentHarnessPhase::Idle {
                return Err(AgentHarnessError::Busy);
            }
            *phase = AgentHarnessPhase::Turn;
        }

        let token = CancellationToken::new();
        *self.cancel.lock() = Some(token.clone());

        let snapshot = self.create_turn_state().await?;

        let before_start = self
            .hooks
            .emit(HarnessHookEvent::BeforeAgentStart {
                messages: snapshot.messages.clone(),
            })
            .await;
        if before_start.cancel {
            *self.phase.lock() = AgentHarnessPhase::Idle;
            self.idle.notify_waiters();
            return Err(AgentHarnessError::Hook("turn cancelled".into()));
        }

        let mut prompts = Vec::new();
        {
            let mut nq = self.next_turn.lock();
            while let Some(m) = nq.pop_front() {
                prompts.push(m);
            }
        }
        if let Some(input) = input {
            prompts.extend(match input {
                PromptInput::Text(t) => vec![AgentMessage::user_text(t)],
                PromptInput::TextWithImages { text, images } => {
                    vec![user_message_with_images(text, images)]
                }
                PromptInput::Message(m) => vec![m],
                PromptInput::Messages(ms) => ms,
            });
        }

        let mut config = AgentLoopConfig::new(snapshot.model.clone());
        config.convert_to_llm = Arc::new(|msgs| Box::pin(async move { convert_to_llm(&msgs) }));
        config.tool_execution = *self.tool_execution.lock();
        config.stream_options = snapshot.stream_options.clone();
        config.stream_options.reasoning = snapshot.thinking_level.to_reasoning();
        config.before_tool_call = self.before_tool_call.lock().clone();
        config.after_tool_call = self.after_tool_call.lock().clone();

        let steer_q = Arc::clone(&self.steering);
        let follow_q = Arc::clone(&self.follow_up);
        let steer_mode = *self.steering_mode.lock();
        let follow_mode = *self.follow_up_mode.lock();
        config.get_steering_messages = Some(Arc::new(move || {
            let q = Arc::clone(&steer_q);
            Box::pin(async move {
                let mut g = q.lock();
                match steer_mode {
                    QueueMode::All => g.drain(..).collect(),
                    QueueMode::OneAtATime => g.pop_front().into_iter().collect(),
                }
            })
        }));
        config.get_follow_up_messages = Some(Arc::new(move || {
            let q = Arc::clone(&follow_q);
            Box::pin(async move {
                let mut g = q.lock();
                match follow_mode {
                    QueueMode::All => g.drain(..).collect(),
                    QueueMode::OneAtATime => g.pop_front().into_iter().collect(),
                }
            })
        }));

        let context = AgentContext {
            system_prompt: snapshot.system_prompt.clone(),
            messages: snapshot.messages.clone(),
            tools: Some(snapshot.tools.clone()),
        };

        let pending = Arc::clone(&self.pending_writes);
        let subscribers = self.subscribers.lock().clone();

        let emit: AgentEventSink = Arc::new(move |event| {
            let pending = Arc::clone(&pending);
            let subscribers = subscribers.clone();
            Box::pin(async move {
                if let AgentEvent::MessageEnd { message } = &event {
                    pending.lock().push(PendingSessionWrite::Message {
                        message: message.clone(),
                    });
                }
                for sub in &subscribers {
                    sub(event.clone()).await;
                }
            })
        });

        let result = run_agent_loop(
            prompts,
            context,
            config,
            emit,
            Some(token),
            Some(Arc::clone(&self.stream_fn)),
        )
        .await;

        let writes = std::mem::take(&mut *self.pending_writes.lock());
        {
            let session = self.session.lock().await;
            let store = session.store();
            let sid = session.metadata().id.clone();
            for w in writes {
                let _ = store.append_entry(&sid, w).await;
            }
        }

        *self.phase.lock() = AgentHarnessPhase::Idle;
        *self.cancel.lock() = None;
        self.idle.notify_waiters();
        if self.shutting_down.load(Ordering::Relaxed) {
            self.shutdown_notify.notify_waiters();
        }
        let _ = self.hooks.emit(HarnessHookEvent::Settled).await;

        match result {
            Ok(msgs) => Ok(msgs
                .into_iter()
                .rev()
                .find(|m| m.role() == "assistant")
                .unwrap_or_else(|| AgentMessage::user_text(""))),
            Err(e) => Err(AgentHarnessError::Other(e.to_string())),
        }
    }

    /// Tool env that the current sandbox/host would provide (for rebuilding tools).
    pub async fn tool_env(&self) -> Result<Arc<dyn ExecutionEnv>, AgentHarnessError> {
        Ok(self.create_turn_state().await?.tool_env)
    }
}

/// Map a cut index in the built context messages back to the branch entry that
/// produced that message. Mirrors the message-production order of
/// `default_context_from_entries`.
fn first_kept_entry_id_for_cut(entries: &[SessionTreeEntry], cut_index: usize) -> Option<String> {
    let mut producer_ids: Vec<&str> = Vec::new();
    for e in entries {
        match e {
            SessionTreeEntry::Message { .. } | SessionTreeEntry::BranchSummary { .. } => {
                producer_ids.push(e.id());
            }
            SessionTreeEntry::Compaction {
                first_kept_entry_id,
                ..
            } => {
                if first_kept_entry_id.is_none() {
                    producer_ids.clear();
                }
                producer_ids.insert(0, e.id());
            }
            _ => {}
        }
    }
    producer_ids.get(cut_index).map(|s| s.to_string())
}
