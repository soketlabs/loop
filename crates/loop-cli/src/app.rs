//! Interactive ratatui application loop.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use loop_agent::harness::{
    format_prompt_template_invocation, format_skill_invocation, LocalShellSandbox, Sandbox,
    SandboxConfig, SandboxMode,
};
use loop_agent::types::{AgentEvent, AgentMessage, AgentThinkingLevel};
use loop_ai::providers::{SOKET_API_KEY_ENVS, SOKET_PROVIDER_ID};
use loop_ai::{Credential, CredentialStore, ModelsRefreshOptions, ToolResultContent};

use crate::commands::{self, CommandEffect};
use crate::keybindings::{hotkey_help, Action};
use crate::runtime::Runtime;
use crate::theme::Theme;
use crate::tui::{
    find_tool_index, tool_args_summary, CardStatus, ChatItem, DrawOpts, ScrollState,
};

enum UiEvent {
    Agent(AgentEvent),
}

/// Run the interactive TUI.
pub async fn run(mut runtime: Runtime) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut runtime).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &mut Runtime,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let tx_agent = tx.clone();
    runtime.harness.subscribe(move |ev| {
        let tx = tx_agent.clone();
        async move {
            let _ = tx.send(UiEvent::Agent(ev));
        }
    });

    let mut chat: Vec<ChatItem> = Vec::new();
    if !runtime.settings.quiet_startup {
        chat.push(sys(format!(
            "Loop by Soket AI · {}/{} · theme {} · /help",
            runtime.settings.default_provider,
            runtime.settings.default_model,
            runtime.theme.name
        )));
    }

    let mut input = String::new();
    let mut status = String::from("ready");
    let mut scroll = ScrollState::default();
    let mut clear_presses = 0u8;
    let mut last_clear = Instant::now();
    let mut last_escape = Instant::now();
    let mut streaming_assistant: Option<usize> = None;
    let mut streaming_thinking: Option<usize> = None;
    let mut expand_details = false;
    let mut hide_thinking = runtime.settings.hide_thinking_block;
    let mut pending_login: Option<String> = None;
    let mut model_picker: Option<ModelPickerState> = None;
    let mut working = false;
    let mut spinner_frame: usize = 0;
    let mut last_spin = Instant::now();

    let tick = Duration::from_millis(33);
    let mut should_quit = false;

    while !should_quit {
        if working && last_spin.elapsed() >= Duration::from_millis(80) {
            spinner_frame = spinner_frame.wrapping_add(1);
            last_spin = Instant::now();
        }

        let header = format_header(runtime);
        let ac = if input.starts_with('/') && !input.contains(' ') {
            let mut extra: Vec<String> = runtime
                .resources
                .skills
                .iter()
                .map(|s| format!("skill:{}", s.name))
                .collect();
            extra.extend(runtime.resources.prompts.iter().map(|p| p.name.clone()));
            commands::autocomplete(&input, &extra)
        } else {
            Vec::new()
        };

        let status_line = if let Some(p) = &model_picker {
            format!(
                "model: {}  (enter select · esc cancel)",
                p.filtered
                    .get(p.selected)
                    .cloned()
                    .unwrap_or_default()
            )
        } else if pending_login.is_some() {
            "enter API key · esc cancel".into()
        } else if working {
            let pending_tools = chat
                .iter()
                .filter(|c| {
                    matches!(
                        c,
                        ChatItem::Tool {
                            status: CardStatus::Pending,
                            ..
                        }
                    )
                })
                .count();
            if pending_tools > 0 {
                format!("Working… · {pending_tools} tool(s)")
            } else {
                "Working…".into()
            }
        } else {
            status.clone()
        };

        let cursor_col = input.lines().last().unwrap_or("").chars().count();
        let thinking_level = runtime.settings.default_thinking_level.clone();

        terminal.draw(|f| {
            crate::tui::draw(
                f,
                DrawOpts {
                    theme: &runtime.theme,
                    header: &header,
                    chat: &chat,
                    input: &input,
                    cursor_col,
                    status: &status_line,
                    working,
                    spinner_frame,
                    autocomplete: &ac,
                    scroll: &mut scroll,
                    expanded: expand_details,
                    hide_thinking,
                    thinking_level: &thinking_level,
                },
            );
        })?;

        let timed_out = !event::poll(tick)?;
        if timed_out {
            while let Ok(UiEvent::Agent(ev)) = rx.try_recv() {
                handle_agent_event(
                    ev,
                    &mut chat,
                    &mut status,
                    &mut streaming_assistant,
                    &mut streaming_thinking,
                    &mut working,
                );
            }
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if let Some(picker) = model_picker.as_mut() {
                match key.code {
                    KeyCode::Esc => {
                        model_picker = None;
                        status = "cancelled".into();
                    }
                    KeyCode::Up => {
                        picker.selected = picker.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if picker.selected + 1 < picker.filtered.len() {
                            picker.selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(id) = picker.filtered.get(picker.selected).cloned() {
                            if let Some((provider, model)) = id.split_once('/') {
                                if let Some(m) = runtime.models.get_model(provider, model) {
                                    runtime.harness.set_model(m).await;
                                    runtime.settings.default_provider = provider.into();
                                    runtime.settings.default_model = model.into();
                                    let _ = runtime.settings.save_file(
                                        &crate::config::paths::settings_path(&runtime.agent_dir),
                                    );
                                    chat.push(sys(format!("model → {provider}/{model}")));
                                }
                            }
                        }
                        model_picker = None;
                    }
                    KeyCode::Char(c) => {
                        picker.query.push(c);
                        picker.refilter(&runtime.models);
                    }
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.refilter(&runtime.models);
                    }
                    _ => {}
                }
                continue;
            }

            if pending_login.is_some() {
                match key.code {
                    KeyCode::Esc => {
                        pending_login = None;
                        input.clear();
                        status = "login cancelled".into();
                    }
                    KeyCode::Enter => {
                        let provider =
                            pending_login.take().unwrap_or_else(|| SOKET_PROVIDER_ID.into());
                        let key_val = input.trim().to_string();
                        input.clear();
                        if key_val.is_empty() {
                            status = "empty key".into();
                        } else {
                            runtime
                                .credentials
                                .set(&provider, Credential::api_key(key_val));
                            let _ = runtime
                                .models
                                .refresh(ModelsRefreshOptions {
                                    allow_network: Some(true),
                                    force: true,
                                    provider_id: Some(provider.clone()),
                                })
                                .await;
                            chat.push(sys(format!("logged in: {provider}")));
                            status = "ready".into();
                        }
                    }
                    KeyCode::Char(c) => input.push(c),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(action) = runtime.keybindings.resolve(key) {
                match action {
                    Action::Exit if input.is_empty() => {
                        should_quit = true;
                        continue;
                    }
                    Action::Clear => {
                        if last_clear.elapsed() < Duration::from_secs(1) {
                            clear_presses += 1;
                        } else {
                            clear_presses = 1;
                        }
                        last_clear = Instant::now();
                        if clear_presses >= 2 {
                            should_quit = true;
                        } else if input.is_empty() {
                            status = "ctrl+c again to quit".into();
                        } else {
                            input.clear();
                        }
                        continue;
                    }
                    Action::Interrupt => {
                        if runtime.harness.phase()
                            != loop_agent::harness::AgentHarnessPhase::Idle
                        {
                            runtime.harness.abort();
                            working = false;
                            status = "interrupted".into();
                        } else if last_escape.elapsed() < Duration::from_millis(500)
                            && runtime.settings.double_escape_action == "tree"
                        {
                            chat.push(sys(
                                "session tree: use /tree (branch nav in session store)",
                            ));
                        }
                        last_escape = Instant::now();
                        continue;
                    }
                    Action::Submit => {
                        let line = input.trim().to_string();
                        input.clear();
                        if line.is_empty() {
                            continue;
                        }
                        if line.starts_with('/') {
                            let skill_names: Vec<_> = runtime
                                .resources
                                .skills
                                .iter()
                                .map(|s| s.name.clone())
                                .collect();
                            let template_names: Vec<_> = runtime
                                .resources
                                .prompts
                                .iter()
                                .map(|p| p.name.clone())
                                .collect();
                            if let Some(cmd) = commands::parse_command(&line) {
                                let effect =
                                    commands::dispatch(&cmd, &skill_names, &template_names);
                                should_quit = apply_effect(
                                    effect,
                                    runtime,
                                    &mut chat,
                                    &mut status,
                                    &mut pending_login,
                                    &mut model_picker,
                                    &mut hide_thinking,
                                    &mut expand_details,
                                    &mut working,
                                )
                                .await?;
                            }
                        } else {
                            chat.push(ChatItem::User { text: line.clone() });
                            working = true;
                            streaming_assistant = None;
                            streaming_thinking = None;
                            scroll.follow_end = true;
                            let harness = Arc::clone(&runtime.harness);
                            tokio::spawn(async move {
                                let _ = harness.prompt(line).await;
                            });
                        }
                        continue;
                    }
                    Action::NewLine => {
                        input.push('\n');
                        continue;
                    }
                    Action::ModelSelect => {
                        model_picker = Some(ModelPickerState::new(&runtime.models));
                        continue;
                    }
                    Action::ModelCycleForward | Action::ModelCycleBackward => {
                        cycle_model(
                            runtime,
                            action == Action::ModelCycleForward,
                            &mut chat,
                        )
                        .await;
                        continue;
                    }
                    Action::ThinkingCycle => {
                        cycle_thinking(runtime, &mut chat).await;
                        continue;
                    }
                    Action::ThinkingToggle => {
                        hide_thinking = !hide_thinking;
                        status = if hide_thinking {
                            "thinking hidden".into()
                        } else {
                            "thinking visible".into()
                        };
                        continue;
                    }
                    Action::ToolsExpand => {
                        expand_details = !expand_details;
                        status = if expand_details {
                            "details expanded".into()
                        } else {
                            "details collapsed".into()
                        };
                        continue;
                    }
                    Action::MessageCopy => {
                        copy_last_assistant(&chat, &mut status);
                        continue;
                    }
                    Action::ExternalEditor => {
                        if let Ok(edited) = external_edit(&input) {
                            input = edited;
                        }
                        continue;
                    }
                    Action::FollowUp => {
                        let line = input.trim().to_string();
                        if !line.is_empty() {
                            runtime
                                .harness
                                .follow_up(AgentMessage::user_text(line.clone()));
                            chat.push(sys(format!("queued follow-up: {line}")));
                            input.clear();
                        }
                        continue;
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Char(_c) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Up => {
                    let h = terminal.size()?.height.saturating_sub(8);
                    scroll.scroll_up(3, estimate_content_h(&chat), h);
                }
                KeyCode::Down => {
                    let h = terminal.size()?.height.saturating_sub(8);
                    scroll.scroll_down(3, estimate_content_h(&chat), h);
                }
                KeyCode::PageUp => {
                    let h = terminal.size()?.height.saturating_sub(8);
                    scroll.scroll_up(h.max(1), estimate_content_h(&chat), h);
                }
                KeyCode::PageDown => {
                    let h = terminal.size()?.height.saturating_sub(8);
                    scroll.scroll_down(h.max(1), estimate_content_h(&chat), h);
                }
                KeyCode::End => {
                    scroll.follow_end = true;
                }
                KeyCode::Home => {
                    scroll.follow_end = false;
                    scroll.scroll_top = 0;
                }
                _ => {}
            }
        }

        while let Ok(UiEvent::Agent(ev)) = rx.try_recv() {
            handle_agent_event(
                ev,
                &mut chat,
                &mut status,
                &mut streaming_assistant,
                &mut streaming_thinking,
                &mut working,
            );
        }
    }

    runtime.harness.request_shutdown();
    Ok(())
}

fn sys(text: impl Into<String>) -> ChatItem {
    ChatItem::System { text: text.into() }
}

fn estimate_content_h(chat: &[ChatItem]) -> u16 {
    let mut n = 0u16;
    for item in chat {
        n = n.saturating_add(match item {
            ChatItem::User { text } => text.lines().count() as u16 + 2,
            ChatItem::Assistant { text } => text.lines().count() as u16 + 2,
            ChatItem::Thinking { text, .. } => (text.lines().count() as u16).min(6) + 3,
            ChatItem::Tool { detail, .. } => (detail.lines().count() as u16).min(8) + 3,
            ChatItem::System { text } => text.lines().count() as u16 + 1,
        });
    }
    n
}

struct ModelPickerState {
    all: Vec<String>,
    filtered: Vec<String>,
    selected: usize,
    query: String,
}

impl ModelPickerState {
    fn new(models: &loop_ai::Models) -> Self {
        let all: Vec<String> = models
            .get_models(None)
            .into_iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        let filtered = all.clone();
        Self {
            all,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self, _models: &loop_ai::Models) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .all
            .iter()
            .filter(|m| m.to_lowercase().contains(&q))
            .cloned()
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }
}

fn format_header(runtime: &Runtime) -> String {
    let sandbox = match runtime.settings.sandbox.mode.as_str() {
        "local-shell" => "sandbox:local",
        _ => "sandbox:off",
    };
    format!(
        "{}/{} · {} · {sandbox}",
        runtime.settings.default_provider, runtime.settings.default_model, runtime.theme.name
    )
}

fn handle_agent_event(
    ev: AgentEvent,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
) {
    match ev {
        AgentEvent::AgentStart => {
            *working = true;
        }
        AgentEvent::AgentEnd { .. } => {
            *working = false;
            *status = "ready".into();
            *streaming_assistant = None;
            if let Some(idx) = streaming_thinking.take() {
                if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
                    *done = true;
                }
            }
        }
        AgentEvent::MessageStart { message } => {
            if message.role() == "assistant" {
                chat.push(ChatItem::Assistant {
                    text: String::new(),
                });
                *streaming_assistant = Some(chat.len() - 1);
            }
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            use loop_ai::AssistantMessageEvent;
            match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    if let Some(idx) = *streaming_assistant {
                        if let Some(ChatItem::Assistant { text }) = chat.get_mut(idx) {
                            text.push_str(&delta);
                        }
                    }
                }
                AssistantMessageEvent::ThinkingStart { .. } => {
                    chat.push(ChatItem::Thinking {
                        text: String::new(),
                        done: false,
                    });
                    *streaming_thinking = Some(chat.len() - 1);
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    if let Some(idx) = *streaming_thinking {
                        if let Some(ChatItem::Thinking { text, .. }) = chat.get_mut(idx) {
                            text.push_str(&delta);
                        }
                    }
                }
                AssistantMessageEvent::ThinkingEnd { .. } => {
                    if let Some(idx) = streaming_thinking.take() {
                        if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
                            *done = true;
                        }
                    }
                }
                AssistantMessageEvent::ToolcallEnd { tool_call, .. } => {
                    let detail =
                        serde_json::to_string_pretty(&tool_call.arguments).unwrap_or_default();
                    let summary = tool_args_summary(&tool_call.name, &tool_call.arguments);
                    upsert_tool(
                        chat,
                        &tool_call.id,
                        &tool_call.name,
                        summary,
                        detail,
                        CardStatus::Pending,
                    );
                }
                AssistantMessageEvent::Error { error, .. } => {
                    let msg = error
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "error".into());
                    chat.push(sys(format!("error: {msg}")));
                    *working = false;
                    *status = "error".into();
                }
                _ => {}
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
            ..
        } => {
            let summary = tool_args_summary(&tool_name, &args);
            let detail = serde_json::to_string_pretty(&args).unwrap_or_default();
            upsert_tool(
                chat,
                &tool_call_id,
                &tool_name,
                summary,
                detail,
                CardStatus::Pending,
            );
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => {
            if let Some(idx) = find_tool_index(chat, &tool_call_id) {
                if let Some(ChatItem::Tool { detail, status, .. }) = chat.get_mut(idx) {
                    *status = CardStatus::Pending;
                    if detail.is_empty() {
                        *detail = format!(
                            "(partial) {}",
                            serde_json::to_string(&partial_result.details).unwrap_or_default()
                        );
                    }
                }
            }
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
            ..
        } => {
            let result_text = result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let st = if is_error {
                CardStatus::Error
            } else {
                CardStatus::Success
            };
            if let Some(idx) = find_tool_index(chat, &tool_call_id) {
                if let Some(ChatItem::Tool {
                    name,
                    detail,
                    status,
                    summary,
                    ..
                }) = chat.get_mut(idx)
                {
                    *name = tool_name;
                    *status = st;
                    if !result_text.is_empty() {
                        if detail.is_empty() {
                            *detail = result_text;
                        } else {
                            detail.push_str("\n---\n");
                            detail.push_str(&result_text);
                        }
                    }
                    if summary.is_empty() {
                        *summary = if is_error {
                            "error".into()
                        } else {
                            "done".into()
                        };
                    }
                }
            } else {
                chat.push(ChatItem::Tool {
                    id: tool_call_id,
                    name: tool_name,
                    summary: if is_error {
                        "error".into()
                    } else {
                        "done".into()
                    },
                    detail: result_text,
                    status: st,
                });
            }
        }
        _ => {}
    }
}

fn upsert_tool(
    chat: &mut Vec<ChatItem>,
    id: &str,
    name: &str,
    summary: String,
    detail: String,
    status: CardStatus,
) {
    if let Some(idx) = find_tool_index(chat, id) {
        if let Some(ChatItem::Tool {
            name: n,
            summary: s,
            detail: d,
            status: st,
            ..
        }) = chat.get_mut(idx)
        {
            *n = name.to_string();
            *s = summary;
            if d.is_empty() || detail.len() >= d.len() {
                *d = detail;
            }
            *st = status;
        }
    } else {
        chat.push(ChatItem::Tool {
            id: id.to_string(),
            name: name.to_string(),
            summary,
            detail,
            status,
        });
    }
}

async fn apply_effect(
    effect: CommandEffect,
    runtime: &mut Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    pending_login: &mut Option<String>,
    model_picker: &mut Option<ModelPickerState>,
    hide_thinking: &mut bool,
    expand_details: &mut bool,
    working: &mut bool,
) -> anyhow::Result<bool> {
    match effect {
        CommandEffect::Quit => return Ok(true),
        CommandEffect::Status(s) => {
            if s.contains("Usage: /theme") {
                let dirs = crate::theme::theme_search_dirs(
                    &runtime.agent_dir,
                    runtime
                        .project_trusted
                        .then_some(crate::config::paths::get_project_dir(&runtime.cwd))
                        .as_ref()
                        .map(|p| p.as_path()),
                );
                let list = Theme::list(&dirs);
                chat.push(sys(format!("themes: {}\n{s}", list.join(", "))));
            } else {
                chat.push(sys(s));
            }
        }
        CommandEffect::SetTheme(name) => {
            let dirs = crate::theme::theme_search_dirs(
                &runtime.agent_dir,
                runtime
                    .project_trusted
                    .then_some(crate::config::paths::get_project_dir(&runtime.cwd))
                    .as_ref()
                    .map(|p| p.as_path()),
            );
            match Theme::load(&name, &dirs) {
                Ok(t) => {
                    runtime.theme = t;
                    runtime.settings.theme = name.clone();
                    let _ = runtime
                        .settings
                        .save_file(&crate::config::paths::settings_path(&runtime.agent_dir));
                    chat.push(sys(format!("theme → {name}")));
                }
                Err(e) => chat.push(sys(format!("theme error: {e}"))),
            }
        }
        CommandEffect::SelectModel(None) => {
            *model_picker = Some(ModelPickerState::new(&runtime.models));
        }
        CommandEffect::SelectModel(Some(spec)) => {
            let (provider, model) = spec
                .split_once('/')
                .map(|(p, m)| (p.to_string(), m.to_string()))
                .unwrap_or_else(|| (runtime.settings.default_provider.clone(), spec));
            if let Some(m) = runtime.models.get_model(&provider, &model) {
                runtime.harness.set_model(m).await;
                runtime.settings.default_provider = provider.clone();
                runtime.settings.default_model = model.clone();
                let _ = runtime
                    .settings
                    .save_file(&crate::config::paths::settings_path(&runtime.agent_dir));
                chat.push(sys(format!("model → {provider}/{model}")));
            } else {
                chat.push(sys(format!(
                    "model not found: {provider}/{model} — try /model or refresh"
                )));
            }
        }
        CommandEffect::SetSandbox(mode) => {
            if mode.is_empty() {
                chat.push(sys(format!(
                    "sandbox mode: {} (use /sandbox off|local-shell)",
                    runtime.settings.sandbox.mode
                )));
                return Ok(false);
            }
            match mode.as_str() {
                "off" | "disabled" => {
                    runtime.harness.clear_sandbox().await;
                    runtime.settings.sandbox.mode = "off".into();
                    chat.push(sys("sandbox → off"));
                }
                "local-shell" | "local" => {
                    let sb = LocalShellSandbox::new(SandboxConfig {
                        workdir: runtime.cwd.clone(),
                        ..Default::default()
                    });
                    sb.start()
                        .await
                        .map_err(|e| anyhow::anyhow!("sandbox: {e}"))?;
                    runtime
                        .harness
                        .set_sandbox(SandboxMode::Enabled {
                            sandbox: Arc::new(sb),
                        })
                        .await;
                    runtime.settings.sandbox.mode = "local-shell".into();
                    chat.push(sys("sandbox → local-shell"));
                }
                other => chat.push(sys(format!(
                    "unknown sandbox '{other}' (off|local-shell)"
                ))),
            }
            let _ = runtime
                .settings
                .save_file(&crate::config::paths::settings_path(&runtime.agent_dir));
        }
        CommandEffect::Login(provider) => {
            let p = provider.unwrap_or_else(|| SOKET_PROVIDER_ID.into());
            *pending_login = Some(p.clone());
            *status = format!("paste API key for {p}");
            chat.push(sys(format!(
                "login {p} — paste key and press enter (env: {})",
                SOKET_API_KEY_ENVS.join(", ")
            )));
        }
        CommandEffect::Logout(provider) => {
            let p = provider.unwrap_or_else(|| SOKET_PROVIDER_ID.into());
            runtime.credentials.remove(&p);
            chat.push(sys(format!("logged out: {p}")));
        }
        CommandEffect::NewSession => {
            chat.clear();
            chat.push(sys(
                "new session — UI cleared; continue chatting on the same store session",
            ));
            *status = "ready".into();
        }
        CommandEffect::Compact(instructions) => {
            let harness = Arc::clone(&runtime.harness);
            *working = true;
            tokio::spawn(async move {
                let _ = harness.compact(instructions.as_deref()).await;
            });
            chat.push(sys("compacting…"));
        }
        CommandEffect::CopyLast => copy_last_assistant(chat, status),
        CommandEffect::Hotkeys => {
            let text = hotkey_help()
                .into_iter()
                .map(|(k, d)| format!("{k:28} {d}"))
                .collect::<Vec<_>>()
                .join("\n");
            chat.push(sys(text));
        }
        CommandEffect::Help => {
            let mut extra: Vec<String> = runtime
                .resources
                .skills
                .iter()
                .map(|s| format!("skill:{}", s.name))
                .collect();
            extra.extend(runtime.resources.prompts.iter().map(|p| p.name.clone()));
            chat.push(sys(commands::help_text(&extra)));
        }
        CommandEffect::SessionInfo => {
            chat.push(sys(format!(
                "provider/model: {}/{}\nsessions db: {}\ntheme: {}\ntrusted: {}",
                runtime.settings.default_provider,
                runtime.settings.default_model,
                runtime.sessions_db.display(),
                runtime.theme.name,
                runtime.project_trusted
            )));
        }
        CommandEffect::Settings => {
            chat.push(sys(format!(
                "settings ({})\n  provider: {}\n  model: {}\n  theme: {}\n  thinking: {}\n  sandbox: {}\n  ui: {}",
                crate::config::paths::settings_path(&runtime.agent_dir).display(),
                runtime.settings.default_provider,
                runtime.settings.default_model,
                runtime.settings.theme,
                runtime.settings.default_thinking_level,
                runtime.settings.sandbox.mode,
                runtime.settings.ui_mode
            )));
        }
        CommandEffect::Trust(arg) => {
            let decision = match arg.as_deref() {
                Some("yes") | Some("y") => true,
                Some("no") | Some("n") => false,
                None => !runtime.project_trusted,
                Some(other) => {
                    chat.push(sys(format!("usage: /trust [yes|no] (got {other})")));
                    return Ok(false);
                }
            };
            runtime.trust.set(&runtime.cwd, decision)?;
            runtime.project_trusted = decision;
            chat.push(sys(format!("project trust → {decision}")));
        }
        CommandEffect::Reload => {
            let _ = runtime
                .models
                .refresh(ModelsRefreshOptions {
                    allow_network: Some(true),
                    force: true,
                    provider_id: None,
                })
                .await;
            runtime.resources = crate::resources::load_resources(
                &runtime.agent_dir,
                &runtime.cwd,
                runtime.project_trusted,
                &runtime.settings,
            );
            chat.push(sys("reloaded models, skills, prompts, themes"));
        }
        CommandEffect::Resume => {
            chat.push(sys(
                "resume: restart with `loop --resume <session-id>` (picker UI forthcoming)",
            ));
        }
        CommandEffect::Tree => {
            chat.push(sys(
                "session tree stored in SQLite — use /fork /clone; full tree UI forthcoming",
            ));
        }
        CommandEffect::SetName(name) => {
            chat.push(sys(format!("session name → {name}")));
        }
        CommandEffect::Export(path) => {
            chat.push(sys(format!("export: path {:?}", path)));
        }
        CommandEffect::Import(path) => {
            chat.push(sys(format!("import not yet wired: {path}")));
        }
        CommandEffect::Changelog => {
            chat.push(sys(
                "Loop 0.1.0 — interactive coding agent by Soket AI\n- Soket dynamic models\n- ratatui CLI\n- skills, themes, sandbox",
            ));
        }
        CommandEffect::ScopedModels => {
            chat.push(sys(format!(
                "enabledModels: {:?}\nEdit settings.json to change Ctrl+P cycle set.",
                runtime.settings.enabled_models
            )));
        }
        CommandEffect::Fork | CommandEffect::CloneSession => {
            chat.push(sys(
                "fork/clone: available via session store API — full UI forthcoming",
            ));
        }
        CommandEffect::Skill { name, args } => {
            if let Some(skill) = runtime.resources.skills.iter().find(|s| s.name == name) {
                let text = format_skill_invocation(skill, &args);
                chat.push(ChatItem::User {
                    text: format!("/skill:{name} {args}"),
                });
                *working = true;
                let harness = Arc::clone(&runtime.harness);
                tokio::spawn(async move {
                    let _ = harness.prompt(text).await;
                });
            } else {
                chat.push(sys(format!("skill not found: {name}")));
            }
        }
        CommandEffect::Template { name, args } => {
            if let Some(tmpl) = runtime.resources.prompts.iter().find(|p| p.name == name) {
                let text = format_prompt_template_invocation(tmpl, &args);
                chat.push(ChatItem::User {
                    text: format!("/{name} {args}"),
                });
                *working = true;
                let harness = Arc::clone(&runtime.harness);
                tokio::spawn(async move {
                    let _ = harness.prompt(text).await;
                });
            } else {
                chat.push(sys(format!("template not found: {name}")));
            }
        }
    }
    let _ = (hide_thinking, expand_details);
    Ok(false)
}

async fn cycle_model(runtime: &mut Runtime, forward: bool, chat: &mut Vec<ChatItem>) {
    let models = runtime.models.get_models(None);
    if models.is_empty() {
        return;
    }
    let current = format!(
        "{}/{}",
        runtime.settings.default_provider, runtime.settings.default_model
    );
    let idx = models
        .iter()
        .position(|m| format!("{}/{}", m.provider, m.id) == current)
        .unwrap_or(0);
    let next = if forward {
        (idx + 1) % models.len()
    } else {
        (idx + models.len() - 1) % models.len()
    };
    let m = &models[next];
    runtime.harness.set_model(m.clone()).await;
    runtime.settings.default_provider = m.provider.clone();
    runtime.settings.default_model = m.id.clone();
    chat.push(sys(format!("model → {}/{}", m.provider, m.id)));
}

async fn cycle_thinking(runtime: &mut Runtime, chat: &mut Vec<ChatItem>) {
    let order = [
        AgentThinkingLevel::Off,
        AgentThinkingLevel::Low,
        AgentThinkingLevel::Medium,
        AgentThinkingLevel::High,
    ];
    let cur = runtime.settings.default_thinking_level.to_lowercase();
    let idx = match cur.as_str() {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        _ => 0,
    };
    let next = order[(idx + 1) % order.len()];
    let label = match next {
        AgentThinkingLevel::Off => "off",
        AgentThinkingLevel::Low => "low",
        AgentThinkingLevel::Medium => "medium",
        AgentThinkingLevel::High => "high",
        _ => "off",
    };
    runtime.harness.set_thinking_level(next).await;
    runtime.settings.default_thinking_level = label.into();
    chat.push(sys(format!("thinking → {label}")));
}

fn copy_last_assistant(chat: &[ChatItem], status: &mut String) {
    let text = chat.iter().rev().find_map(|l| match l {
        ChatItem::Assistant { text: t } if !t.is_empty() => Some(t.clone()),
        _ => None,
    });
    match text {
        Some(t) => match arboard::Clipboard::new() {
            Ok(mut cb) => {
                let _ = cb.set_text(t);
                *status = "copied".into();
            }
            Err(e) => *status = format!("clipboard: {e}"),
        },
        None => *status = "nothing to copy".into(),
    }
}

fn external_edit(current: &str) -> anyhow::Result<String> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut tmp = tempfile::NamedTempFile::new()?;
    use std::io::Write;
    tmp.write_all(current.as_bytes())?;
    let path = tmp.path().to_path_buf();
    let _ = disable_raw_mode();
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    let _ = enable_raw_mode();
    if !status.success() {
        anyhow::bail!("editor exited with error");
    }
    Ok(std::fs::read_to_string(path)?)
}
