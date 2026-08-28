//! Bridges `AgentHarness` events to the GPUI UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_channel::{Receiver, Sender};
use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarnessPhase,
};
use loop_agent::types::{AgentEvent, AgentMessage, AgentThinkingLevel, AgentToolResult};
use loop_ai::AssistantMessageEvent;
use loop_app_core::tool_approval::{permissions_from_settings, ApprovalDecision, ToolApprovalBridge};
use loop_app_core::{bootstrap, BootstrapOpts, Runtime};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::approval::{ApprovalUiPrompt, PendingApproval};
use crate::state::{ChatRow, ComposerStats, PendingFileChange, SessionRow, ToolCardStatus};

/// UI-facing agent command.
#[derive(Debug)]
pub enum DesktopCommand {
    Prompt(String),
    SetModel { provider: String, model_id: String },
    CycleModel,
    SetThinking(AgentThinkingLevel),
    CycleThinking,
    SelectFileChange(String),
    AcceptFileChange(String),
    RejectFileChange(String),
    ResolveApproval {
        accept: bool,
        session: bool,
        reason: Option<String>,
    },
    OpenInEditor,
    RefreshSessions,
    SelectSession(String),
    NewSession,
}

/// Snapshot pushed to the GPUI entity after each controller update.
#[derive(Debug, Clone)]
pub struct DesktopSnapshot {
    pub chat_rows: Vec<ChatRow>,
    pub sessions: Vec<SessionRow>,
    pub pending_changes: Vec<PendingFileChange>,
    pub selected_change_id: Option<String>,
    pub stats: ComposerStats,
    pub streaming: bool,
    pub phase: AgentHarnessPhase,
    pub active_session_id: String,
    pub cwd: PathBuf,
    pub model_label: String,
    pub thinking_label: String,
    pub available_models: Vec<(String, String, String)>,
    pub approval_prompt: Option<ApprovalUiPrompt>,
    pub detected_editors: Vec<String>,
}

/// Background controller owning the shared runtime.
pub struct DesktopController {
    runtime: Arc<Runtime>,
    event_rx: Receiver<()>,
    ui_tick_tx: Sender<()>,
    snapshot: Arc<Mutex<DesktopSnapshot>>,
    pending_approval: Arc<Mutex<Option<PendingApproval>>>,
    file_snapshots: Arc<Mutex<HashMap<String, (PathBuf, Option<String>)>>>,
    model_index: Arc<Mutex<usize>>,
    thinking_levels: Vec<AgentThinkingLevel>,
    thinking_index: Arc<Mutex<usize>>,
}

impl DesktopController {
    pub async fn new(cwd: PathBuf) -> anyhow::Result<Self> {
        let runtime = Arc::new(
            bootstrap(BootstrapOpts {
                cwd: cwd.clone(),
                provider: None,
                model: None,
                theme: None,
                system_prompt: None,
                append_system_prompt: None,
                no_context_files: false,
                interactive: false,
                session_id: None,
            })
            .await?,
        );

        let (ui_tick_tx, event_rx) = async_channel::unbounded();
        let stats = ComposerStats::from_runtime(&runtime);
        let model_label = model_display(&runtime);
        let thinking_label = runtime.settings.default_thinking_level.clone();
        let available_models = list_models(&runtime);
        let detected_editors = crate::editor_launcher::detect_editors()
            .into_iter()
            .map(|e| e.label().to_string())
            .collect();

        let snapshot = Arc::new(Mutex::new(DesktopSnapshot {
            chat_rows: Vec::new(),
            sessions: Vec::new(),
            pending_changes: Vec::new(),
            selected_change_id: None,
            stats,
            streaming: false,
            phase: AgentHarnessPhase::Idle,
            active_session_id: runtime.session_id.clone(),
            cwd: cwd.clone(),
            model_label,
            thinking_label: thinking_label.clone(),
            available_models: available_models.clone(),
            approval_prompt: None,
            detected_editors,
        }));

        let tool_env = runtime.harness.tool_env().await?;
        let policy_active = loop_app_core::tool_approval::ApprovalPolicy::parse(
            &runtime.settings.file_edit_review,
        )
        .asks_for_session(!runtime.resumed);
        let (bridge, approval_rx) = ToolApprovalBridge::new(
            tool_env,
            runtime.settings.diff_editor.clone(),
            policy_active,
            permissions_from_settings(&runtime.settings.tool_permissions),
            Default::default(),
        );
        let bridge = Arc::new(bridge);
        runtime
            .harness
            .set_before_tool_call(Some(bridge.before_tool_hook()));
        runtime
            .harness
            .set_after_tool_call(Some(bridge.after_tool_hook()));

        let thinking_levels = vec![
            AgentThinkingLevel::Off,
            AgentThinkingLevel::Minimal,
            AgentThinkingLevel::Low,
            AgentThinkingLevel::Medium,
            AgentThinkingLevel::High,
            AgentThinkingLevel::XHigh,
            AgentThinkingLevel::Max,
        ];
        let thinking_index = thinking_levels
            .iter()
            .position(|l| thinking_label == thinking_label_str(*l))
            .unwrap_or(0);

        let controller = Self {
            runtime: Arc::clone(&runtime),
            event_rx,
            ui_tick_tx,
            snapshot,
            pending_approval: Arc::new(Mutex::new(None)),
            file_snapshots: Arc::new(Mutex::new(HashMap::new())),
            model_index: Arc::new(Mutex::new(0)),
            thinking_levels,
            thinking_index: Arc::new(Mutex::new(thinking_index)),
        };

        controller.subscribe_agent_events();
        controller.spawn_approval_handler(approval_rx);
        controller.spawn_model_refresh();
        let _ = crate::session_title::backfill_untitled_sessions(&controller.runtime).await;
        controller.refresh_sessions().await?;
        controller.hydrate_transcript().await?;
        controller.refresh_stats().await?;

        Ok(controller)
    }

    pub fn snapshot(&self) -> DesktopSnapshot {
        self.snapshot.lock().clone()
    }

    pub fn ui_receiver(&self) -> Receiver<()> {
        self.event_rx.clone()
    }

    pub fn runtime(&self) -> Arc<Runtime> {
        Arc::clone(&self.runtime)
    }

    fn notify_ui(&self) {
        let _ = self.ui_tick_tx.try_send(());
    }

    fn subscribe_agent_events(&self) {
        let snap = Arc::clone(&self.snapshot);
        let file_snapshots = Arc::clone(&self.file_snapshots);
        let ui_tick = self.ui_tick_tx.clone();

        self.runtime.harness.subscribe(move |event| {
            let snap = Arc::clone(&snap);
            let file_snapshots = Arc::clone(&file_snapshots);
            let ui_tick = ui_tick.clone();
            async move {
                apply_agent_event(&snap, &file_snapshots, event);
                let _ = ui_tick.try_send(());
            }
        });
    }

    fn spawn_approval_handler(&self, mut approval_rx: mpsc::UnboundedReceiver<loop_app_core::tool_approval::ApprovalPrompt>) {
        let snap = Arc::clone(&self.snapshot);
        let pending = Arc::clone(&self.pending_approval);
        let ui_tick = self.ui_tick_tx.clone();
        tokio::spawn(async move {
            while let Some(prompt) = approval_rx.recv().await {
                {
                    let mut s = snap.lock();
                    s.approval_prompt = Some(ApprovalUiPrompt::from_prompt(&prompt));
                }
                *pending.lock() = Some(PendingApproval { prompt });
                let _ = ui_tick.send(()).await;
            }
        });
    }

    fn spawn_model_refresh(&self) {
        let runtime = Arc::clone(&self.runtime);
        let snapshot = Arc::clone(&self.snapshot);
        let ui_tick = self.ui_tick_tx.clone();
        tokio::spawn(async move {
            use loop_ai::providers::SOKET_PROVIDER_ID;
            use loop_ai::ModelsRefreshOptions;
            let refresh = runtime
                .models
                .refresh(ModelsRefreshOptions {
                    allow_network: Some(true),
                    force: false,
                    provider_id: Some(SOKET_PROVIDER_ID.into()),
                })
                .await;
            for (pid, err) in &refresh.errors {
                tracing::warn!("model refresh {pid}: {err}");
            }
            snapshot.lock().available_models = list_models(&runtime);
            let _ = ui_tick.try_send(());
        });
    }

    pub async fn handle_command(&self, cmd: DesktopCommand) -> anyhow::Result<()> {
        match cmd {
            DesktopCommand::Prompt(text) => {
                if text.trim().is_empty() {
                    return Ok(());
                }
                let needs_title = self.session_needs_title().await?;
                let fallback = crate::session_title::fallback_title(&text);
                let session_id = self.snapshot.lock().active_session_id.clone();
                {
                    let mut snap = self.snapshot.lock();
                    snap.streaming = true;
                    snap.chat_rows.push(ChatRow::User {
                        id: uuid::Uuid::now_v7().to_string(),
                        text: text.clone(),
                    });
                    if needs_title {
                        if let Some(row) = snap.sessions.iter_mut().find(|s| s.id == session_id) {
                            row.name = Some(fallback.clone());
                        }
                    }
                }
                self.notify_ui();
                if needs_title {
                    let _ = crate::session_title::persist_session_name(
                        &self.runtime,
                        &session_id,
                        &fallback,
                    )
                    .await;
                }
                let result = self.runtime.harness.prompt(text.clone()).await;
                {
                    let mut snap = self.snapshot.lock();
                    snap.streaming = false;
                    snap.phase = self.runtime.harness.phase();
                }
                self.notify_ui();
                result.map_err(|e| anyhow::anyhow!(e))?;
                if needs_title {
                    let runtime = Arc::clone(&self.runtime);
                    let snapshot = Arc::clone(&self.snapshot);
                    let ui_tick = self.ui_tick_tx.clone();
                    let message = text.clone();
                    let (provider, model_id) = {
                        let label = snapshot.lock().model_label.clone();
                        split_model_label(&label)
                    };
                    tokio::spawn(async move {
                        let title = crate::session_title::generate_session_title(
                            &runtime,
                            &message,
                            provider.as_deref(),
                            model_id.as_deref(),
                        )
                        .await
                        .unwrap_or(fallback);
                        if crate::session_title::persist_session_name(
                            &runtime,
                            &session_id,
                            &title,
                        )
                        .await
                        .is_ok()
                        {
                            if let Ok(rows) = load_session_rows(&runtime, &snapshot).await {
                                snapshot.lock().sessions = rows;
                                let _ = ui_tick.try_send(());
                            }
                        }
                    });
                }
                self.refresh_stats().await?;
                self.refresh_sessions().await?;
            }
            DesktopCommand::SetModel { provider, model_id } => {
                if let Some(model) = self.runtime.models.get_model(&provider, &model_id) {
                    self.runtime.harness.set_model(model).await;
                    let mut snap = self.snapshot.lock();
                    snap.model_label = format!("{provider}/{model_id}");
                    snap.available_models = list_models(&self.runtime);
                }
                self.refresh_stats().await?;
            }
            DesktopCommand::CycleModel => {
                let models = list_models(&self.runtime);
                if models.is_empty() {
                    return Ok(());
                }
                let idx = {
                    let mut i = self.model_index.lock();
                    *i = (*i + 1) % models.len();
                    *i
                };
                let (provider, model_id, _) = &models[idx];
                Box::pin(self.handle_command(DesktopCommand::SetModel {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                }))
                .await?;
            }
            DesktopCommand::SetThinking(level) => {
                self.runtime.harness.set_thinking_level(level).await;
                let mut snap = self.snapshot.lock();
                snap.thinking_label = thinking_label_str(level);
                if let Some(ix) = self
                    .thinking_levels
                    .iter()
                    .position(|l| *l == level)
                {
                    *self.thinking_index.lock() = ix;
                }
            }
            DesktopCommand::CycleThinking => {
                let idx = {
                    let mut i = self.thinking_index.lock();
                    *i = (*i + 1) % self.thinking_levels.len();
                    *i
                };
                let level = self.thinking_levels[idx];
                Box::pin(self.handle_command(DesktopCommand::SetThinking(level))).await?;
            }
            DesktopCommand::SelectFileChange(id) => {
                self.snapshot.lock().selected_change_id = Some(id);
                self.notify_ui();
            }
            DesktopCommand::AcceptFileChange(id) => {
                let mut snap = self.snapshot.lock();
                if let Some(c) = snap.pending_changes.iter_mut().find(|c| c.id == id) {
                    c.reviewed = true;
                }
                self.notify_ui();
            }
            DesktopCommand::RejectFileChange(id) => {
                let change = {
                    let snap = self.snapshot.lock();
                    snap.pending_changes.iter().find(|c| c.id == id).cloned()
                };
                if let Some(change) = change {
                    crate::state::reject_change(&change)?;
                    let mut snap = self.snapshot.lock();
                    snap.pending_changes.retain(|c| c.id != id);
                    if snap.selected_change_id.as_deref() == Some(&id) {
                        snap.selected_change_id = None;
                    }
                }
                self.notify_ui();
            }
            DesktopCommand::ResolveApproval {
                accept,
                session,
                reason,
            } => {
                if let Some(pending) = self.pending_approval.lock().take() {
                    let decision = if accept {
                        if session {
                            ApprovalDecision::AcceptSession
                        } else {
                            ApprovalDecision::Accept
                        }
                    } else {
                        ApprovalDecision::Reject { reason }
                    };
                    pending.respond(decision);
                }
                self.snapshot.lock().approval_prompt = None;
                self.notify_ui();
            }
            DesktopCommand::OpenInEditor => {
                let change = {
                    let snap = self.snapshot.lock();
                    snap.pending_changes
                        .iter()
                        .find(|c| {
                            Some(c.id.as_str()) == snap.selected_change_id.as_deref()
                                || snap.selected_change_id.is_none()
                        })
                        .or_else(|| snap.pending_changes.last())
                        .cloned()
                };
                if let Some(change) = change {
                    if let Some(editor) = crate::editor_launcher::detect_editors().into_iter().next() {
                        crate::editor_launcher::open_in_editor(editor, &change.path, 1)?;
                    }
                }
            }
            DesktopCommand::RefreshSessions => {
                self.refresh_sessions().await?;
            }
            DesktopCommand::SelectSession(id) => {
                if id == self.snapshot.lock().active_session_id {
                    return Ok(());
                }
                self.runtime
                    .harness
                    .switch_session(&id)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                {
                    let mut snap = self.snapshot.lock();
                    snap.active_session_id = id;
                    snap.chat_rows.clear();
                    snap.pending_changes.clear();
                    snap.selected_change_id = None;
                    snap.streaming = false;
                    snap.approval_prompt = None;
                }
                *self.pending_approval.lock() = None;
                self.file_snapshots.lock().clear();
                self.hydrate_transcript().await?;
                self.refresh_sessions().await?;
                self.refresh_stats().await?;
            }
            DesktopCommand::NewSession => {
                let id = self
                    .runtime
                    .harness
                    .start_new_session(
                        Some(self.runtime.cwd.to_string_lossy().into_owned()),
                        None,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                {
                    let mut snap = self.snapshot.lock();
                    snap.active_session_id = id;
                    snap.chat_rows.clear();
                    snap.pending_changes.clear();
                    snap.selected_change_id = None;
                    snap.streaming = false;
                    snap.approval_prompt = None;
                }
                *self.pending_approval.lock() = None;
                self.file_snapshots.lock().clear();
                self.refresh_sessions().await?;
                self.refresh_stats().await?;
            }
        }
        Ok(())
    }

    async fn refresh_stats(&self) -> anyhow::Result<()> {
        let mut stats = self.snapshot.lock().stats.clone();
        stats.refresh(&self.runtime).await;
        self.snapshot.lock().stats = stats;
        self.notify_ui();
        Ok(())
    }

    async fn session_needs_title(&self) -> anyhow::Result<bool> {
        let store = create_sqlite_session_store(&self.runtime.sessions_db)
            .map_err(|e| anyhow::anyhow!(e))?;
        let repo = create_session_repository(store, None);
        let active = self.snapshot.lock().active_session_id.clone();
        let list = repo.list(None).await.map_err(|e| anyhow::anyhow!(e))?;
        Ok(crate::session_title::is_untitled(
            list.iter()
                .find(|m| m.id == active)
                .and_then(|m| m.name.as_deref()),
        ))
    }

    async fn refresh_sessions(&self) -> anyhow::Result<()> {
        let rows = load_session_rows(&self.runtime, &self.snapshot).await?;
        self.snapshot.lock().sessions = rows;
        self.notify_ui();
        Ok(())
    }

    async fn hydrate_transcript(&self) -> anyhow::Result<()> {
        let ctx = self
            .runtime
            .harness
            .session_context()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut rows = Vec::new();
        for msg in ctx.messages {
            rows.extend(ChatRow::from_agent_message(&msg));
        }
        self.snapshot.lock().chat_rows = rows;
        self.maybe_name_from_transcript().await?;
        self.notify_ui();
        Ok(())
    }

    async fn maybe_name_from_transcript(&self) -> anyhow::Result<()> {
        if !self.session_needs_title().await? {
            return Ok(());
        }
        let first = {
            let snap = self.snapshot.lock();
            snap.chat_rows.iter().find_map(|row| match row {
                ChatRow::User { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
        };
        let Some(text) = first else {
            return Ok(());
        };
        let title = crate::session_title::fallback_title(&text);
        let session_id = self.snapshot.lock().active_session_id.clone();
        crate::session_title::persist_session_name(&self.runtime, &session_id, &title).await?;
        if let Some(row) = self
            .snapshot
            .lock()
            .sessions
            .iter_mut()
            .find(|s| s.id == session_id)
        {
            row.name = Some(title);
        }
        Ok(())
    }
}

async fn load_session_rows(
    runtime: &Runtime,
    snapshot: &Arc<Mutex<DesktopSnapshot>>,
) -> anyhow::Result<Vec<SessionRow>> {
    let store = create_sqlite_session_store(&runtime.sessions_db)
        .map_err(|e| anyhow::anyhow!(e))?;
    let repo = create_session_repository(store, None);
    let list = repo.list(None).await.map_err(|e| anyhow::anyhow!(e))?;
    let active = snapshot.lock().active_session_id.clone();
    let running = runtime.harness.phase() != AgentHarnessPhase::Idle;
    let mut rows: Vec<SessionRow> = list
        .into_iter()
        .map(|m| SessionRow {
            id: m.id.clone(),
            name: m.name.clone(),
            cwd: m.cwd.clone().unwrap_or_default(),
            updated_at: m.created_at,
            active: m.id == active,
            running: m.id == active && running,
        })
        .collect();
    rows.reverse();
    Ok(rows)
}

fn split_model_label(label: &str) -> (Option<String>, Option<String>) {
    label
        .split_once('/')
        .map(|(p, m)| (Some(p.to_string()), Some(m.to_string())))
        .unwrap_or((None, None))
}

fn apply_agent_event(
    snap: &Arc<Mutex<DesktopSnapshot>>,
    file_snapshots: &Arc<Mutex<HashMap<String, (PathBuf, Option<String>)>>>,
    event: AgentEvent,
) {
    let mut s = snap.lock();
    match &event {
        AgentEvent::AgentStart => {
            s.streaming = true;
            s.phase = AgentHarnessPhase::Turn;
        }
        AgentEvent::AgentEnd { messages } => {
            s.streaming = false;
            s.phase = AgentHarnessPhase::Idle;
            for msg in messages {
                s.stats.apply_message(msg);
            }
        }
        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            let thinking_ended = matches!(
                assistant_message_event,
                AssistantMessageEvent::ThinkingEnd { .. }
                    | AssistantMessageEvent::TextStart { .. }
                    | AssistantMessageEvent::TextDelta { .. }
            );
            merge_streaming_message(&mut s.chat_rows, message, thinking_ended);
            if matches!(
                assistant_message_event,
                AssistantMessageEvent::TextStart { .. }
                    | AssistantMessageEvent::TextDelta { .. }
            ) {
                ensure_streaming_assistant(&mut s.chat_rows);
            }
            s.stats.apply_message(message);
        }
        AgentEvent::MessageEnd { message } => {
            finalize_streaming_message(&mut s.chat_rows, message);
            s.stats.apply_message(message);
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            if tool_name == "write" || tool_name == "edit" {
                if let Some(path) = path_from_tool_args(args) {
                    let before = std::fs::read_to_string(&path).ok();
                    file_snapshots
                        .lock()
                        .insert(tool_call_id.clone(), (path, before));
                }
            }
            s.chat_rows.push(ChatRow::Tool {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                summary: crate::chat_ui::tool_args_summary(tool_name, args),
                status: ToolCardStatus::Running,
            });
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
            ..
        } => {
            if let Some(row) = s
                .chat_rows
                .iter_mut()
                .rev()
                .find(|r| matches!(r, ChatRow::Tool { id, .. } if id == tool_call_id))
            {
                if let ChatRow::Tool { status, .. } = row {
                    *status = if *is_error {
                        ToolCardStatus::Error
                    } else {
                        ToolCardStatus::Success
                    };
                }
            }
            if (tool_name == "write" || tool_name == "edit") && !*is_error {
                let snapshot = file_snapshots.lock().remove(tool_call_id);
                if let Some((path, before)) = snapshot.or_else(|| {
                    extract_file_path(result).map(|p| {
                        let before = std::fs::read_to_string(&p).ok();
                        (p, before)
                    })
                }) {
                    let after = std::fs::read_to_string(&path).unwrap_or_default();
                    let change = PendingFileChange::from_paths(path.clone(), before, after);
                    s.chat_rows.push(ChatRow::FileChange {
                        id: change.id.clone(),
                        path: path.display().to_string(),
                        added: change.added,
                        removed: change.removed,
                    });
                    s.pending_changes.push(change);
                    if s.selected_change_id.is_none() {
                        s.selected_change_id = Some(
                            s.pending_changes
                                .last()
                                .map(|c| c.id.clone())
                                .unwrap_or_default(),
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn path_from_tool_args(args: &Value) -> Option<PathBuf> {
    for key in ["path", "file_path", "file"] {
        if let Some(path) = args.get(key).and_then(|v| v.as_str()) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn merge_streaming_message(rows: &mut Vec<ChatRow>, message: &AgentMessage, thinking_ended: bool) {
    let AgentMessage::Llm(loop_ai::Message::Assistant(a)) = message else {
        return;
    };

    let mut text = String::new();
    let mut thinking = None;
    for block in &a.content {
        match block {
            loop_ai::AssistantContent::Text(t) => text.push_str(&t.text),
            loop_ai::AssistantContent::Thinking(t) => thinking = Some(t.thinking.clone()),
            _ => {}
        }
    }

    let finish_thinking = thinking_ended || !text.is_empty();
    if let Some(think) = thinking {
        if let Some(row) = rows
            .iter_mut()
            .rev()
            .find(|r| matches!(r, ChatRow::Thinking { .. }))
        {
            if let ChatRow::Thinking {
                text: existing,
                done,
                ..
            } = row
            {
                *existing = think;
                if finish_thinking {
                    *done = true;
                }
            }
        } else {
            rows.push(ChatRow::Thinking {
                id: uuid::Uuid::now_v7().to_string(),
                text: think,
                done: finish_thinking,
            });
        }
    } else if finish_thinking {
        if let Some(ChatRow::Thinking { done, .. }) = rows
            .iter_mut()
            .rev()
            .find(|r| matches!(r, ChatRow::Thinking { .. }))
        {
            *done = true;
        }
    }

    if text.is_empty() {
        return;
    }
    if let Some(row) = rows
        .iter_mut()
        .rev()
        .find(|r| matches!(r, ChatRow::Assistant { streaming: true, .. }))
    {
        if let ChatRow::Assistant {
            text: existing, ..
        } = row
        {
            *existing = text;
        }
        return;
    }
    rows.push(ChatRow::Assistant {
        id: uuid::Uuid::now_v7().to_string(),
        text,
        streaming: true,
    });
}

fn ensure_streaming_assistant(rows: &mut Vec<ChatRow>) {
    if rows
        .iter()
        .rev()
        .any(|r| matches!(r, ChatRow::Assistant { streaming: true, .. }))
    {
        return;
    }
    rows.push(ChatRow::Assistant {
        id: uuid::Uuid::now_v7().to_string(),
        text: String::new(),
        streaming: true,
    });
}

fn finalize_streaming_message(rows: &mut Vec<ChatRow>, message: &AgentMessage) {
    if let Some(ChatRow::Thinking { done, .. }) = rows
        .iter_mut()
        .rev()
        .find(|r| matches!(r, ChatRow::Thinking { .. }))
    {
        *done = true;
    }
    if let AgentMessage::Llm(loop_ai::Message::Assistant(a)) = message {
        let mut text = String::new();
        for block in &a.content {
            if let loop_ai::AssistantContent::Text(t) = block {
                text.push_str(&t.text);
            }
        }
        if let Some(ChatRow::Assistant {
            text: existing,
            streaming,
            ..
        }) = rows
            .iter_mut()
            .rev()
            .find(|r| matches!(r, ChatRow::Assistant { .. }))
        {
            if !text.is_empty() {
                *existing = text;
            }
            *streaming = false;
        } else if !text.is_empty() {
            rows.push(ChatRow::Assistant {
                id: uuid::Uuid::now_v7().to_string(),
                text,
                streaming: false,
            });
        }
    }
}

fn extract_file_path(result: &AgentToolResult) -> Option<PathBuf> {
    let text = tool_result_text(result);
    for line in text.lines() {
        let trimmed = line.trim().trim_matches('"');
        if looks_like_path(trimmed) {
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

fn looks_like_path(s: &str) -> bool {
    Path::new(s).extension().is_some() || s.contains('/') || s.contains('\\')
}

fn model_display(runtime: &Runtime) -> String {
    format!(
        "{}/{}",
        runtime.settings.default_provider, runtime.settings.default_model
    )
}

fn tool_result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn list_models(runtime: &Runtime) -> Vec<(String, String, String)> {
    let all = runtime.models.get_models(None);
    let enabled = &runtime.settings.enabled_models;
    let models: Vec<_> = if enabled.is_empty() {
        all
    } else {
        all.into_iter()
            .filter(|m| {
                let key = format!("{}/{}", m.provider, m.id);
                enabled.iter().any(|e| e == &key || e == &m.id)
            })
            .collect()
    };
    models
        .into_iter()
        .map(|m| (m.provider.clone(), m.id.clone(), m.name.clone()))
        .collect()
}

fn thinking_label_str(level: AgentThinkingLevel) -> String {
    match level {
        AgentThinkingLevel::Off => "off".into(),
        AgentThinkingLevel::Minimal => "minimal".into(),
        AgentThinkingLevel::Low => "low".into(),
        AgentThinkingLevel::Medium => "medium".into(),
        AgentThinkingLevel::High => "high".into(),
        AgentThinkingLevel::XHigh => "xhigh".into(),
        AgentThinkingLevel::Max => "max".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, Usage};

    fn assistant_partial(thinking: Option<&str>, text: &str) -> AgentMessage {
        let mut content = Vec::new();
        if let Some(think) = thinking {
            content.push(AssistantContent::Thinking(ThinkingContent {
                thinking: think.to_string(),
                thinking_signature: None,
                redacted: None,
            }));
        }
        if !text.is_empty() {
            content.push(AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            }));
        }
        AgentMessage::assistant(AssistantMessage {
            content,
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: StopReason::Pending,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        })
    }

    #[test]
    fn thinking_then_text_streams_assistant_instead_of_waiting_for_end() {
        let mut rows = Vec::new();
        merge_streaming_message(&mut rows, &assistant_partial(Some("plan"), ""), false);
        assert!(matches!(
            rows.as_slice(),
            [ChatRow::Thinking {
                text,
                done: false,
                ..
            }] if text == "plan"
        ));

        merge_streaming_message(
            &mut rows,
            &assistant_partial(Some("plan"), "Hello"),
            true,
        );
        assert!(matches!(
            rows.as_slice(),
            [
                ChatRow::Thinking { done: true, .. },
                ChatRow::Assistant {
                    text,
                    streaming: true,
                    ..
                }
            ] if text == "Hello"
        ));

        merge_streaming_message(
            &mut rows,
            &assistant_partial(Some("plan"), "Hello world"),
            true,
        );
        assert!(matches!(
            rows.as_slice(),
            [
                ChatRow::Thinking { done: true, .. },
                ChatRow::Assistant {
                    text,
                    streaming: true,
                    ..
                }
            ] if text == "Hello world"
        ));
    }
}
