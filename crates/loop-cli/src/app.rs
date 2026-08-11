//! Interactive inline CLI (Pi-style): transcript in scrollback, footer redrawn in place.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use loop_agent::harness::{
    format_prompt_template_invocation, AgentHarnessPhase, KrunSandbox, Sandbox, SandboxMode,
    SessionForkPoint, SessionForkSelection,
};
use loop_agent::types::{AgentEvent, AgentMessage, AgentThinkingLevel};
use loop_ai::providers::{SOKET_BASE_URL, SOKET_PROVIDER_ID};
use loop_ai::{
    calculate_context_tokens, Credential, CredentialStore, Message, ModelsRefreshOptions,
    ToolResultContent, Usage,
};

use crate::commands::{self, AutocompleteEntry, CommandEffect};
use crate::keybindings::{hotkey_help, Action};
use crate::runtime::Runtime;
use crate::theme::Theme;
use crate::tool_approval::{
    auto_approve_from_entries, permissions_from_settings, ApprovalDecision, ApprovalKind,
    ApprovalPolicy, ApprovalPrompt, ToolApprovalBridge, GROUP_BASH, GROUP_FILE,
};
use crate::tui::{
    chat_items_from_agent_messages, filter_files, find_at_mention, find_tool_index,
    format_item_lines, format_token_usage_line, insert_text, item_is_committed, list_files,
    render_lines_to_buffer, tool_args_summary, welcome_lines, CardStatus, ChatItem, FileEntry,
    FOOTER_HEIGHT, FooterOpts, InputBuffer, PickerRow, PickerView,
};

enum UiEvent {
    Agent(AgentEvent),
    /// `/compact` finished (success message or error).
    CompactDone(Result<String, String>),
    /// `/sandbox` enable/disable finished.
    SandboxDone(Result<SandboxDoneOk, String>),
}

/// Successful sandbox switch applied on the UI thread.
enum SandboxDoneOk {
    /// Sandbox disabled.
    Off,
    /// Local sandbox enabled.
    Local {
        isolation: String,
        runtime: String,
    },
}

/// Pending accept/reject prompt for a tool call.
struct ActiveApproval {
    kind: ApprovalKind,
    tool_name: String,
    summary: String,
    selected: usize,
    reason: String,
    reason_focused: bool,
    response_tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

impl ActiveApproval {
    fn from_prompt(prompt: ApprovalPrompt) -> Self {
        Self {
            kind: prompt.kind,
            tool_name: prompt.tool_name,
            summary: prompt.summary,
            selected: 0,
            reason: String::new(),
            reason_focused: false,
            response_tx: prompt.response_tx,
        }
    }

    fn into_picker(&self) -> PickerView {
        PickerView::FileReview {
            path: format!("{} · {}", self.tool_name, self.summary),
            selected: self.selected,
            accept_all_label: self.kind.accept_all_label().into(),
            reason: self.reason.clone(),
            reason_focused: self.reason_focused,
        }
    }
}

/// Outbound user message waiting for the agent to become idle.
#[derive(Debug, Clone)]
struct QueuedMessage {
    /// Shown in the transcript (and matched to `ChatItem::Queued`).
    display: String,
    /// Sent to `harness.prompt`.
    prompt: String,
}

/// Live token / context stats for the footer usage line.
#[derive(Debug, Clone)]
struct TokenBarState {
    total_tokens: u64,
    context_tokens: Option<u64>,
    context_window: u64,
}

impl TokenBarState {
    fn from_model(runtime: &Runtime) -> Self {
        let context_window = runtime
            .models
            .get_model(
                &runtime.settings.default_provider,
                &runtime.settings.default_model,
            )
            .map(|m| m.context_window)
            .unwrap_or(0);
        Self {
            total_tokens: 0,
            context_tokens: Some(0),
            context_window,
        }
    }

    async fn load(runtime: &Runtime) -> Self {
        let mut state = Self::from_model(runtime);
        state.refresh(runtime).await;
        state
    }

    async fn refresh(&mut self, runtime: &Runtime) {
        self.sync_window(runtime);
        if let Ok(stats) = runtime.harness.session_stats().await {
            self.total_tokens = stats.tokens.total_tokens();
            if let Some(ctx) = stats.context_usage {
                self.context_tokens = ctx.tokens;
                self.context_window = ctx.context_window;
            }
        }
    }

    fn sync_window(&mut self, runtime: &Runtime) {
        if let Some(m) = runtime.models.get_model(
            &runtime.settings.default_provider,
            &runtime.settings.default_model,
        ) {
            self.context_window = m.context_window;
        }
    }

    fn reset(&mut self, runtime: &Runtime) {
        *self = Self::from_model(runtime);
    }

    fn apply_usage(&mut self, usage: &Usage) {
        self.total_tokens = self
            .total_tokens
            .saturating_add(usage.input)
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        let ctx = calculate_context_tokens(usage);
        if ctx > 0 {
            self.context_tokens = Some(ctx);
        }
    }

    fn apply_message(&mut self, message: &AgentMessage) {
        match message {
            AgentMessage::Llm(Message::Assistant(a)) => self.apply_usage(&a.usage),
            AgentMessage::Llm(Message::ToolResult(t)) => {
                if let Some(usage) = &t.usage {
                    self.apply_usage(usage);
                }
            }
            _ => {}
        }
    }

    fn usage_line(&self) -> String {
        format_token_usage_line(self.total_tokens, self.context_tokens, self.context_window)
    }
}

/// Run the interactive CLI (inline viewport — native terminal scrollback).
pub async fn run(mut runtime: Runtime) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    // Start on a clean screen (pi-style): clear and home before anchoring the viewport.
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(FOOTER_HEIGHT),
        },
    )?;
    let _ = terminal.hide_cursor();

    let result = run_loop(&mut terminal, &mut runtime).await;

    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    // Clear the inline footer so the shell prompt lands cleanly.
    let _ = terminal.clear();
    terminal.show_cursor()?;

    match result {
        Ok(Some(session_id)) => {
            println!("You can resume this session with: loop --resume {session_id}");
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &mut Runtime,
) -> anyhow::Result<Option<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let tx_agent = tx.clone();
    runtime.harness.subscribe(move |ev| {
        let tx = tx_agent.clone();
        async move {
            let _ = tx.send(UiEvent::Agent(ev));
        }
    });

    let review_policy = ApprovalPolicy::parse(&runtime.settings.file_edit_review);
    let policy_active = review_policy.asks_for_session(!runtime.resumed);
    let tool_env = runtime.harness.tool_env().await?;
    let session_grants = match runtime.harness.read_session_entries().await {
        Ok(entries) => auto_approve_from_entries(&entries),
        Err(e) => {
            tracing::warn!("failed to load session tool approvals: {e}");
            Default::default()
        }
    };
    let (bridge, mut review_rx) = ToolApprovalBridge::new(
        tool_env,
        runtime.settings.diff_editor.clone(),
        policy_active,
        permissions_from_settings(&runtime.settings.tool_permissions),
        session_grants,
    );
    let bridge = Arc::new(bridge);
    {
        let harness = Arc::clone(&runtime.harness);
        bridge.set_persist(Arc::new(move |label| {
            let harness = Arc::clone(&harness);
            Box::pin(async move {
                if let Err(e) = harness.append_label(label).await {
                    tracing::warn!("persist tool approval label: {e}");
                }
            })
        }));
    }
    runtime
        .harness
        .set_before_tool_call(Some(bridge.before_tool_hook()));
    runtime
        .harness
        .set_after_tool_call(Some(bridge.after_tool_hook()));
    runtime.tool_approval = Some(Arc::clone(&bridge));

    let mut chat: Vec<ChatItem> = match runtime.harness.session_context().await {
        Ok(ctx) if !ctx.messages.is_empty() => chat_items_from_agent_messages(&ctx.messages),
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!("failed to restore session transcript: {e}");
            Vec::new()
        }
    };
    let mut flushed = 0usize;
    let mut input = InputBuffer::new();
    let mut status: String = if runtime.needs_api_key_setup {
        "setup · paste your API key · enter save".into()
    } else if runtime.resumed {
        if chat.is_empty() {
            "resumed · empty session · /help for commands".into()
        } else {
            format!(
                "resumed · {} messages · /help for commands",
                chat.len()
            )
        }
    } else {
        let review_hint = if policy_active {
            " · tool approval on (files + bash)"
        } else {
            ""
        };
        format!("ready · /help for commands{review_hint}")
    };
    let mut clear_presses = 0u8;
    let mut last_clear = Instant::now();
    let mut last_escape = Instant::now();
    let mut streaming_assistant: Option<usize> = None;
    let mut streaming_thinking: Option<usize> = None;
    // Pi-style: one global expand flag; toggling clears + reprints the transcript.
    let mut expand_details = false;
    let mut redraw_request = false;
    let mut purge_ui_events = false;
    let mut last_width = terminal.size()?.width;
    let mut hide_thinking = runtime.settings.hide_thinking_block;
    let mut pending_login: Option<String> = if runtime.needs_api_key_setup {
        Some(SOKET_PROVIDER_ID.into())
    } else {
        None
    };
    let mut model_picker: Option<ModelPickerState> = None;
    let mut fork_picker: Option<ForkPickerState> = None;
    let mut active_approval: Option<ActiveApproval> = None;
    let mut ac_selected: usize = 0;
    let mut last_ac_filter = String::new();
    let mut file_index: Option<Vec<FileEntry>> = None;
    let mut working = false;
    let mut message_queue: VecDeque<QueuedMessage> = VecDeque::new();
    let mut spinner_frame: usize = 0;
    let mut last_spin = Instant::now();
    let version = env!("CARGO_PKG_VERSION");
    let path_line = path_status_line(&runtime.cwd);
    let mut token_bar = TokenBarState::load(runtime).await;
    let mut refresh_token_bar = false;

    // Welcome banner into terminal scrollback (above the footer).
    print_welcome(terminal, runtime, version)?;

    let tick = Duration::from_millis(33);
    let mut should_quit = false;

    while !should_quit {
        if working && last_spin.elapsed() >= Duration::from_millis(80) {
            spinner_frame = spinner_frame.wrapping_add(1);
            last_spin = Instant::now();
        }

        // Flush finished transcript items into native scrollback.
        if flushed > chat.len() {
            flushed = chat.len();
        }
        flush_committed(
            terminal,
            &chat,
            &mut flushed,
            streaming_assistant,
            streaming_thinking,
            &runtime.theme,
            expand_details,
            hide_thinking,
        )?;

        let model_label = format!(
            "{}/{}",
            runtime.settings.default_provider, runtime.settings.default_model
        );
        let model_line = format!(
            "{model_label} · {}",
            runtime.settings.default_thinking_level
        );

        let ac_entries = if input.as_str().starts_with('/')
            && !input.as_str().contains(' ')
            && pending_login.is_none()
            && model_picker.is_none()
            && fork_picker.is_none()
            && active_approval.is_none()
        {
            let extra = dynamic_command_entries(runtime);
            commands::autocomplete_entries(input.as_str(), &extra)
        } else {
            Vec::new()
        };
        let at_mention = if ac_entries.is_empty()
            && pending_login.is_none()
            && model_picker.is_none()
            && fork_picker.is_none()
            && active_approval.is_none()
        {
            find_at_mention(input.as_str(), input.cursor())
        } else {
            None
        };
        let file_ac_entries = if let Some(ref mention) = at_mention {
            let index = file_index.get_or_insert_with(|| list_files(&runtime.cwd));
            filter_files(index, &mention.query, 100)
        } else {
            Vec::new()
        };
        let ac_filter_key = if let Some(ref m) = at_mention {
            format!("@{}", m.query)
        } else {
            input.as_str().to_string()
        };
        if ac_filter_key != last_ac_filter {
            ac_selected = 0;
            last_ac_filter = ac_filter_key;
        }
        let picker_len = if !ac_entries.is_empty() {
            ac_entries.len()
        } else {
            file_ac_entries.len()
        };
        if picker_len > 0 {
            ac_selected = ac_selected.min(picker_len - 1);
        }

        let picker = if let Some(review) = &active_approval {
            review.into_picker()
        } else if let Some(p) = &model_picker {
            let current = format!(
                "{}/{}",
                runtime.settings.default_provider, runtime.settings.default_model
            );
            PickerView::Models {
                rows: p
                    .filtered
                    .iter()
                    .map(|id| {
                        let (label, desc) = match id.split_once('/') {
                            Some((prov, model)) => (model.to_string(), format!("[{prov}]")),
                            None => (id.clone(), String::new()),
                        };
                        PickerRow {
                            label,
                            description: desc,
                            mark: if *id == current {
                                Some("✓".into())
                            } else {
                                None
                            },
                        }
                    })
                    .collect(),
                selected: p.selected,
                hint: "Only showing models from configured providers. Use /login to add providers."
                    .into(),
            }
        } else if let Some(p) = &fork_picker {
            PickerView::Models {
                rows: p
                    .filtered
                    .iter()
                    .filter_map(|&i| p.points.get(i))
                    .map(|pt| PickerRow {
                        label: format!("#{}", pt.index),
                        description: pt.preview.clone(),
                        mark: None,
                    })
                    .collect(),
                selected: p.selected,
                hint: "Fork: edit this user message and continue (prior history kept)."
                    .into(),
            }
        } else if let Some(provider) = &pending_login {
            PickerView::Setup {
                provider: provider.clone(),
            }
        } else if !ac_entries.is_empty() {
            PickerView::Commands {
                rows: ac_entries
                    .iter()
                    .map(|e| PickerRow {
                        label: e.name.clone(),
                        description: e.description.clone(),
                        mark: None,
                    })
                    .collect(),
                selected: ac_selected,
            }
        } else if !file_ac_entries.is_empty() {
            PickerView::Commands {
                rows: file_ac_entries
                    .iter()
                    .map(|e| PickerRow {
                        label: e.relative.clone(),
                        description: e.absolute.clone(),
                        mark: None,
                    })
                    .collect(),
                selected: ac_selected,
            }
        } else {
            PickerView::None
        };

        let status_line = if active_approval.is_some() {
            "tool approval · ↑↓ choose · tab reason · enter confirm".into()
        } else if model_picker.is_some() {
            "↑↓ select · enter confirm · esc cancel".into()
        } else if fork_picker.is_some() {
            "↑↓ select · enter edit · esc cancel".into()
        } else if pending_login.is_some() {
            "setup · paste your API key · enter save".into()
        } else if !ac_entries.is_empty() {
            "↑↓ select · tab complete · enter run".into()
        } else if !file_ac_entries.is_empty() {
            "↑↓ select · tab complete · enter insert".into()
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
            let queued = message_queue.len();
            let mut parts = vec!["Working…".to_string()];
            if pending_tools > 0 {
                parts.push(format!("{pending_tools} tool(s)"));
            }
            if queued > 0 {
                parts.push(format!("{queued} queued"));
            }
            parts.join(" · ")
        } else if !message_queue.is_empty() {
            format!("{} queued · sending…", message_queue.len())
        } else if !runtime.active_skills.is_empty() {
            let names = runtime.active_skills.join(", ");
            format!("skill(s) active: {names} · type your message")
        } else {
            status.clone()
        };

        let live: Vec<ChatItem> = chat[flushed..].to_vec();
        let setup_mode = pending_login.is_some();
        if refresh_token_bar {
            token_bar.refresh(runtime).await;
            refresh_token_bar = false;
        }
        let usage_line = token_bar.usage_line();

        terminal.draw(|f| {
            crate::tui::draw_footer(
                f,
                FooterOpts {
                    theme: &runtime.theme,
                    live: &live,
                    input: input.as_str(),
                    cursor: input.cursor(),
                    working,
                    spinner_frame,
                    status: &status_line,
                    picker: &picker,
                    expanded: expand_details,
                    hide_thinking,
                    setup_mode,
                    mask_input: setup_mode,
                    path_line: &path_line,
                    model_line: &model_line,
                    usage_line: &usage_line,
                },
            );
        })?;
        let _ = terminal.hide_cursor();

        let timed_out = !event::poll(tick)?;
        if timed_out {
            drain_ui_events(
                runtime,
                &mut rx,
                &mut review_rx,
                &mut active_approval,
                &mut chat,
                &mut status,
                &mut streaming_assistant,
                &mut streaming_thinking,
                &mut working,
                &mut message_queue,
                &mut token_bar,
                &mut refresh_token_bar,
            );
            continue;
        }

        loop {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(
                        key,
                        runtime,
                        &mut input,
                        &mut chat,
                        &mut status,
                        &mut clear_presses,
                        &mut last_clear,
                        &mut last_escape,
                        &mut streaming_assistant,
                        &mut streaming_thinking,
                        &mut expand_details,
                        &mut redraw_request,
                        &mut purge_ui_events,
                        &mut hide_thinking,
                        &mut pending_login,
                        &mut model_picker,
                        &mut fork_picker,
                        &mut active_approval,
                        &mut working,
                        &mut message_queue,
                        &mut should_quit,
                        &ac_entries,
                        &file_ac_entries,
                        at_mention.as_ref(),
                        &mut ac_selected,
                        &tx,
                        &mut token_bar,
                        &mut refresh_token_bar,
                    )
                    .await?;
                    token_bar.sync_window(runtime);
                }
                Event::Resize(w, _) => {
                    let _ = terminal.autoresize();
                    // Re-wrapping invalidates the whole transcript (pi does the same).
                    if w != last_width {
                        last_width = w;
                        redraw_request = true;
                    }
                }
                _ => {}
            }
            if should_quit || !event::poll(Duration::from_millis(0))? {
                break;
            }
        }

        // Drop events from an aborted turn so they can't repopulate a fresh session.
        if purge_ui_events {
            while rx.try_recv().is_ok() {}
            while review_rx.try_recv().is_ok() {}
            if let Some(r) = active_approval.take() {
                let _ = r.response_tx.send(ApprovalDecision::Reject {
                    reason: Some("session reset".into()),
                });
            }
            purge_ui_events = false;
            working = false;
            streaming_assistant = None;
            streaming_thinking = None;
        }

        // Pi-style hard reset: clear screen + scrollback, then reprint the whole
        // transcript with the current expand state. Native scrollback handles
        // anything taller than the viewport.
        if redraw_request {
            redraw_request = false;
            reset_and_redraw(
                terminal,
                runtime,
                version,
                &chat,
                &mut flushed,
                streaming_assistant,
                streaming_thinking,
                expand_details,
                hide_thinking,
            )?;
        }

        drain_ui_events(
            runtime,
            &mut rx,
            &mut review_rx,
            &mut active_approval,
            &mut chat,
            &mut status,
            &mut streaming_assistant,
            &mut streaming_thinking,
            &mut working,
            &mut message_queue,
            &mut token_bar,
            &mut refresh_token_bar,
        );
    }

    runtime.harness.request_shutdown();
    let has_user_message = chat
        .iter()
        .any(|item| matches!(item, ChatItem::User { .. }));
    Ok(has_user_message.then(|| runtime.session_id.clone()))
}

#[allow(clippy::too_many_arguments)]
fn flush_committed(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    chat: &[ChatItem],
    flushed: &mut usize,
    streaming_assistant: Option<usize>,
    streaming_thinking: Option<usize>,
    theme: &Theme,
    expanded: bool,
    hide_thinking: bool,
) -> io::Result<()> {
    let width = terminal.size()?.width;
    while *flushed < chat.len() {
        if !item_is_committed(
            &chat[*flushed],
            *flushed,
            streaming_assistant,
            streaming_thinking,
        ) {
            break;
        }
        // Items land in scrollback with the current global expand state; toggling
        // ctrl+o clears and reprints everything (see `reset_and_redraw`).
        let lines = format_item_lines(&chat[*flushed], theme, expanded, hide_thinking, width);
        if !lines.is_empty() {
            let h = lines.len() as u16;
            terminal.insert_before(h, |buf| {
                render_lines_to_buffer(&lines, buf);
            })?;
        }
        *flushed += 1;
    }
    Ok(())
}

/// Pi-style hard reset: clear screen + home + clear scrollback, then reprint
/// the welcome banner and the entire committed transcript with the current
/// expand state. Content taller than the viewport spills into fresh native
/// scrollback, so the user scrolls with their terminal as usual.
#[allow(clippy::too_many_arguments)]
fn reset_and_redraw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &Runtime,
    version: &str,
    chat: &[ChatItem],
    flushed: &mut usize,
    streaming_assistant: Option<usize>,
    streaming_thinking: Option<usize>,
    expanded: bool,
    hide_thinking: bool,
) -> anyhow::Result<()> {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::{
        BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
    };

    let mut out = io::stdout();
    // Equivalent of pi's `\x1b[2J\x1b[H\x1b[3J`: without purging scrollback the
    // reprinted transcript would duplicate below the old copy.
    execute!(
        out,
        BeginSynchronizedUpdate,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Clear(ClearType::Purge)
    )?;

    // Re-anchor a fresh inline viewport at the top of the now-empty screen.
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(FOOTER_HEIGHT),
        },
    )?;
    let _ = terminal.hide_cursor();

    print_welcome(terminal, runtime, version)?;
    *flushed = 0;
    flush_committed(
        terminal,
        chat,
        flushed,
        streaming_assistant,
        streaming_thinking,
        &runtime.theme,
        expanded,
        hide_thinking,
    )?;
    let _ = execute!(io::stdout(), EndSynchronizedUpdate);
    Ok(())
}

/// Print the welcome banner + info card into terminal scrollback.
fn print_welcome(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &Runtime,
    version: &str,
) -> anyhow::Result<()> {
    let endpoint = endpoint_for(runtime);
    let width = terminal.size()?.width;
    let lines = welcome_lines(
        &runtime.theme,
        version,
        &runtime.settings.default_provider,
        &runtime.settings.default_model,
        &endpoint,
        &runtime.session_id,
        runtime.resources.skills.len(),
        runtime.resources.prompts.len(),
        runtime.needs_api_key_setup,
        width,
    );
    terminal.insert_before(lines.len() as u16, |buf| {
        render_lines_to_buffer(&lines, buf);
    })?;
    Ok(())
}

/// Dynamic `(name, description)` command entries for skills and prompt templates.
fn dynamic_command_entries(runtime: &Runtime) -> Vec<(String, String)> {
    let mut extra: Vec<(String, String)> = runtime
        .resources
        .skills
        .iter()
        .map(|s| {
            let desc = if s.description.is_empty() {
                "skill".to_string()
            } else {
                format!("skill — {}", s.description)
            };
            (format!("skill:{}", s.name), desc)
        })
        .collect();
    extra.extend(runtime.resources.prompts.iter().map(|p| {
        let desc = match &p.argument_hint {
            Some(h) => format!("{h} — prompt template"),
            None => "prompt template".to_string(),
        };
        (p.name.clone(), desc)
    }));
    extra
}

fn path_status_line(cwd: &Path) -> String {
    let home = dirs::home_dir();
    let display = if let Some(home) = home {
        if let Ok(rest) = cwd.strip_prefix(&home) {
            format!("~/{}", rest.display())
        } else {
            cwd.display().to_string()
        }
    } else {
        cwd.display().to_string()
    };
    let branch = git_branch(cwd);
    match branch {
        Some(b) => format!("{display} ({b})"),
        None => display,
    }
}

fn git_branch(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s)
    }
}

fn sys(text: impl Into<String>) -> ChatItem {
    ChatItem::System { text: text.into() }
}

/// Index of the first queued bubble, or `chat.len()` if none.
/// Agent stream items must be inserted here so they stay above the queue.
fn first_queued_index(chat: &[ChatItem]) -> usize {
    chat.iter()
        .position(|c| matches!(c, ChatItem::Queued { .. }))
        .unwrap_or(chat.len())
}

fn bump_stream_index(slot: &mut Option<usize>, inserted_at: usize) {
    if let Some(i) = slot {
        if *i >= inserted_at {
            *i += 1;
        }
    }
}

/// Push an agent/transcript item above any pending queued user messages.
fn push_before_queued(chat: &mut Vec<ChatItem>, item: ChatItem) -> usize {
    let idx = first_queued_index(chat);
    chat.insert(idx, item);
    idx
}

/// Move any queued bubbles to the end (repairs mid-stream enqueue order) and
/// remap live streaming indices.
fn settle_queue_at_end(
    chat: &mut Vec<ChatItem>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
) {
    if !chat
        .iter()
        .any(|c| matches!(c, ChatItem::Queued { .. }))
    {
        return;
    }
    // Already a clean trailing run of Queued items?
    if let Some(first) = chat
        .iter()
        .position(|c| matches!(c, ChatItem::Queued { .. }))
    {
        if chat[first..]
            .iter()
            .all(|c| matches!(c, ChatItem::Queued { .. }))
        {
            return;
        }
    }

    let mut queued = Vec::new();
    let mut settled = Vec::with_capacity(chat.len());
    let mut map = vec![None; chat.len()];
    for (i, item) in chat.drain(..).enumerate() {
        if matches!(item, ChatItem::Queued { .. }) {
            queued.push(item);
        } else {
            map[i] = Some(settled.len());
            settled.push(item);
        }
    }
    settled.extend(queued);
    *chat = settled;

    if let Some(idx) = *streaming_assistant {
        *streaming_assistant = map.get(idx).copied().flatten();
    }
    if let Some(idx) = *streaming_thinking {
        *streaming_thinking = map.get(idx).copied().flatten();
    }
}

fn truncate_status(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or(s).trim();
    let mut out: String = one_line.chars().take(max).collect();
    if one_line.chars().count() > max {
        out.push('…');
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn drain_ui_events(
    runtime: &mut Runtime,
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    review_rx: &mut mpsc::UnboundedReceiver<ApprovalPrompt>,
    active_approval: &mut Option<ActiveApproval>,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) {
    while let Ok(ev) = rx.try_recv() {
        match ev {
            UiEvent::Agent(ev) => {
                handle_agent_event(
                    ev,
                    chat,
                    status,
                    streaming_assistant,
                    streaming_thinking,
                    working,
                    token_bar,
                    refresh_token_bar,
                );
            }
            UiEvent::CompactDone(result) => {
                *working = false;
                match result {
                    Ok(msg) => {
                        chat.push(sys(msg));
                        *status = "ready".into();
                        // Compaction invalidates prior context estimates until the next turn.
                        token_bar.context_tokens = None;
                        *refresh_token_bar = true;
                    }
                    Err(e) => {
                        chat.push(sys(format!("compact failed: {e}")));
                        *status = "ready".into();
                    }
                }
            }
            UiEvent::SandboxDone(result) => {
                *working = false;
                match result {
                    Ok(SandboxDoneOk::Off) => {
                        runtime.settings.sandbox.mode = "off".into();
                        let _ = runtime.settings.save_file(
                            &crate::config::paths::settings_path(&runtime.agent_dir),
                        );
                        chat.push(sys("sandbox → off"));
                        *status = "ready".into();
                    }
                    Ok(SandboxDoneOk::Local {
                        isolation,
                        runtime: oci,
                    }) => {
                        runtime.settings.sandbox.mode = "local".into();
                        runtime.settings.sandbox.isolation = isolation.clone();
                        runtime.settings.sandbox.runtime = oci.clone();
                        let _ = runtime.settings.save_file(
                            &crate::config::paths::settings_path(&runtime.agent_dir),
                        );
                        chat.push(sys(format!("sandbox → local --{isolation} --{oci}")));
                        *status = "ready".into();
                    }
                    Err(e) => {
                        chat.push(sys(e));
                        *status = "ready".into();
                    }
                }
            }
        }
    }
    while let Ok(prompt) = review_rx.try_recv() {
        if let Some(prev) = active_approval.take() {
            let _ = prev.response_tx.send(ApprovalDecision::Reject {
                reason: Some("superseded by another review".into()),
            });
        }
        *status = format!(
            "review · {} · {}",
            prompt.kind.label(),
            prompt.summary
        );
        *active_approval = Some(ActiveApproval::from_prompt(prompt));
    }
    try_drain_message_queue(
        runtime,
        chat,
        status,
        streaming_assistant,
        streaming_thinking,
        working,
        message_queue,
    );
}

fn agent_is_busy(runtime: &Runtime, working: bool) -> bool {
    working || runtime.harness.phase() != AgentHarnessPhase::Idle
}

fn enqueue_user_message(
    chat: &mut Vec<ChatItem>,
    message_queue: &mut VecDeque<QueuedMessage>,
    display: String,
    prompt: String,
) {
    message_queue.push_back(QueuedMessage {
        display: display.clone(),
        prompt,
    });
    chat.push(ChatItem::Queued { text: display });
}

fn flush_message_queue(chat: &mut Vec<ChatItem>, message_queue: &mut VecDeque<QueuedMessage>) {
    message_queue.clear();
    chat.retain(|item| !matches!(item, ChatItem::Queued { .. }));
}

fn dequeue_last_message(
    chat: &mut Vec<ChatItem>,
    message_queue: &mut VecDeque<QueuedMessage>,
) -> Option<String> {
    let item = message_queue.pop_back()?;
    if let Some(idx) = chat.iter().rposition(|c| {
        matches!(c, ChatItem::Queued { text: t } if *t == item.display)
    }) {
        chat.remove(idx);
    }
    Some(item.display)
}

fn start_user_turn(
    runtime: &Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    display: String,
    prompt: String,
) {
    // Keep prior turn output above this user message.
    settle_queue_at_end(chat, streaming_assistant, streaming_thinking);

    // Promote a matching queued bubble if this came from the outbound queue.
    if let Some(idx) = chat.iter().position(|item| {
        matches!(item, ChatItem::Queued { text: t } if *t == display)
    }) {
        chat[idx] = ChatItem::User {
            text: display,
        };
    } else {
        chat.push(ChatItem::User { text: display });
    }
    *working = true;
    *streaming_assistant = None;
    if let Some(idx) = streaming_thinking.take() {
        if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
            *done = true;
        }
    }
    *status = "Working…".into();
    let harness = Arc::clone(&runtime.harness);
    tokio::spawn(async move {
        let _ = harness.prompt(prompt).await;
    });
}

fn try_drain_message_queue(
    runtime: &Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
) {
    // Wait until the harness is fully idle. AgentEnd clears `working` slightly
    // before phase flips, and Esc must be able to flush the queue in between.
    if *working
        || runtime.harness.phase() != AgentHarnessPhase::Idle
        || message_queue.is_empty()
    {
        return;
    }
    let Some(item) = message_queue.pop_front() else {
        return;
    };
    start_user_turn(
        runtime,
        chat,
        status,
        streaming_assistant,
        streaming_thinking,
        working,
        item.display,
        item.prompt,
    );
}

fn submit_user_text(
    runtime: &Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    text: String,
) {
    if agent_is_busy(runtime, *working) {
        enqueue_user_message(chat, message_queue, text.clone(), text);
        let n = message_queue.len();
        *status = if n == 1 {
            "queued · will send when ready".into()
        } else {
            format!("{n} messages queued")
        };
    } else {
        start_user_turn(
            runtime,
            chat,
            status,
            streaming_assistant,
            streaming_thinking,
            working,
            text.clone(),
            text,
        );
    }
}

/// Run `!command` locally: show output in the transcript, never send to the LLM
/// or append to the session message list.
async fn run_bang_command(
    runtime: &Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    command: &str,
) {
    let command = command.trim();
    if command.is_empty() {
        let idx = push_before_queued(chat, sys("usage: !command"));
        bump_stream_index(streaming_assistant, idx);
        bump_stream_index(streaming_thinking, idx);
        *status = "ready".into();
        return;
    }

    *status = format!("running · !{command}");
    let env = match runtime.harness.tool_env().await {
        Ok(env) => env,
        Err(e) => {
            let idx = push_before_queued(chat, sys(format!("shell error: {e}")));
            bump_stream_index(streaming_assistant, idx);
            bump_stream_index(streaming_thinking, idx);
            *status = "shell failed".into();
            return;
        }
    };

    let options = loop_agent::harness::types::ShellExecOptions::default();
    let item = match env.exec(command, options).await {
        Ok(out) => {
            let combined = if out.stderr.is_empty() {
                out.stdout
            } else if out.stdout.is_empty() {
                out.stderr
            } else {
                format!("{}\n{}", out.stdout, out.stderr)
            };
            *status = if out.exit_code == 0 {
                "done".into()
            } else {
                format!("exit {}", out.exit_code)
            };
            ChatItem::Shell {
                command: command.to_string(),
                output: combined,
                exit_code: Some(out.exit_code),
            }
        }
        Err(e) => {
            *status = "shell failed".into();
            ChatItem::Shell {
                command: command.to_string(),
                output: format!("error: {e}"),
                exit_code: None,
            }
        }
    };
    let idx = push_before_queued(chat, item);
    bump_stream_index(streaming_assistant, idx);
    bump_stream_index(streaming_thinking, idx);
}

async fn handle_key(
    key: crossterm::event::KeyEvent,
    runtime: &mut Runtime,
    input: &mut InputBuffer,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    clear_presses: &mut u8,
    last_clear: &mut Instant,
    last_escape: &mut Instant,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    expand_details: &mut bool,
    redraw_request: &mut bool,
    purge_ui_events: &mut bool,
    hide_thinking: &mut bool,
    pending_login: &mut Option<String>,
    model_picker: &mut Option<ModelPickerState>,
    fork_picker: &mut Option<ForkPickerState>,
    active_approval: &mut Option<ActiveApproval>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    should_quit: &mut bool,
    ac_entries: &[AutocompleteEntry],
    file_ac_entries: &[FileEntry],
    at_mention: Option<&crate::tui::AtMention>,
    ac_selected: &mut usize,
    tx: &mpsc::UnboundedSender<UiEvent>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) -> anyhow::Result<()> {
    if let Some(review) = active_approval.as_mut() {
        match key.code {
            KeyCode::Esc => {
                if review.reason_focused {
                    review.reason_focused = false;
                } else if let Some(r) = active_approval.take() {
                    let _ = r.response_tx.send(ApprovalDecision::Reject { reason: None });
                    chat.push(sys(format!("rejected {} · {}", r.kind.label(), r.summary)));
                    *status = "rejected · continuing".into();
                }
            }
            KeyCode::Up | KeyCode::Left => {
                review.selected = review.selected.saturating_sub(1);
                if review.selected != 2 {
                    review.reason_focused = false;
                }
            }
            KeyCode::Down | KeyCode::Right => {
                if review.selected < 2 {
                    review.selected += 1;
                }
            }
            KeyCode::Tab => {
                review.selected = 2;
                review.reason_focused = true;
            }
            KeyCode::Enter => {
                if let Some(r) = active_approval.take() {
                    let decision = if r.reason_focused || r.selected == 2 {
                        let reason = r.reason.trim().to_string();
                        ApprovalDecision::Reject {
                            reason: (!reason.is_empty()).then_some(reason),
                        }
                    } else if r.selected == 1 {
                        ApprovalDecision::AcceptSession
                    } else {
                        ApprovalDecision::Accept
                    };
                    match &decision {
                        ApprovalDecision::Accept => {
                            chat.push(sys(format!(
                                "accepted {} · {}",
                                r.kind.label(),
                                r.summary
                            )));
                            *status = "accepted · continuing".into();
                        }
                        ApprovalDecision::AcceptSession => {
                            chat.push(sys(format!(
                                "accepted all {} for this session",
                                r.kind.label()
                            )));
                            *status = "session auto-approve on · continuing".into();
                        }
                        ApprovalDecision::Reject { reason } => {
                            if let Some(why) = reason {
                                chat.push(sys(format!(
                                    "rejected {} · {} · {why}",
                                    r.kind.label(),
                                    r.summary
                                )));
                            } else {
                                chat.push(sys(format!(
                                    "rejected {} · {}",
                                    r.kind.label(),
                                    r.summary
                                )));
                            }
                            *status = "rejected · continuing".into();
                        }
                    }
                    let _ = r.response_tx.send(decision);
                }
            }
            KeyCode::Backspace if review.reason_focused => {
                review.reason.pop();
            }
            KeyCode::Char(c) if review.reason_focused => {
                if !c.is_control() {
                    review.reason.push(c);
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') if !review.reason_focused => {
                review.selected = 0;
            }
            KeyCode::Char('s') | KeyCode::Char('S') if !review.reason_focused => {
                review.selected = 1;
            }
            KeyCode::Char('r') | KeyCode::Char('R') if !review.reason_focused => {
                review.selected = 2;
            }
            _ => {}
        }
        return Ok(());
    }

    if let Some(picker) = model_picker.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *model_picker = None;
                *status = "cancelled".into();
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
                *model_picker = None;
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
        return Ok(());
    }

    if let Some(picker) = fork_picker.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *fork_picker = None;
                *status = "cancelled".into();
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
                let selected = picker.selected_point().cloned();
                *fork_picker = None;
                if let Some(point) = selected {
                    adopt_forked_session(
                        runtime,
                        chat,
                        status,
                        working,
                        message_queue,
                        streaming_assistant,
                        streaming_thinking,
                        redraw_request,
                        purge_ui_events,
                        input,
                        SessionForkSelection::BeforeEntry,
                        Some(point.entry_id.as_str()),
                        None,
                        Some(point.text),
                        token_bar,
                        refresh_token_bar,
                    )
                    .await;
                }
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter();
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            _ => {}
        }
        return Ok(());
    }

    // Slash-command picker navigation (before general keybindings).
    if !ac_entries.is_empty() {
        match key.code {
            KeyCode::Up => {
                *ac_selected = ac_selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                if *ac_selected + 1 < ac_entries.len() {
                    *ac_selected += 1;
                }
                return Ok(());
            }
            KeyCode::Tab => {
                if let Some(sel) = ac_entries.get(*ac_selected) {
                    input.set(format!("{} ", sel.name));
                }
                return Ok(());
            }
            KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(sel) = ac_entries.get(*ac_selected) {
                    // Prefer the highlighted command when input is still a partial `/…`.
                    if input.as_str().starts_with('/') && !input.as_str().contains(' ') {
                        input.set(sel.name.clone());
                    }
                }
                // Fall through to Submit via keybindings below.
            }
            _ => {}
        }
    }

    // `@file` mention picker — Tab/Enter insert absolute path in place of `@…`.
    if !file_ac_entries.is_empty() {
        match key.code {
            KeyCode::Up => {
                *ac_selected = ac_selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                if *ac_selected + 1 < file_ac_entries.len() {
                    *ac_selected += 1;
                }
                return Ok(());
            }
            KeyCode::Tab => {
                if let (Some(mention), Some(sel)) =
                    (at_mention, file_ac_entries.get(*ac_selected))
                {
                    let text = format!("{} ", insert_text(&sel.absolute));
                    input.replace_char_range(mention.start, mention.end, &text);
                }
                return Ok(());
            }
            KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let (Some(mention), Some(sel)) =
                    (at_mention, file_ac_entries.get(*ac_selected))
                {
                    let text = insert_text(&sel.absolute);
                    input.replace_char_range(mention.start, mention.end, &text);
                }
                // Insert only — stay in the editor so the user can keep typing.
                return Ok(());
            }
            _ => {}
        }
    }

    if pending_login.is_some() {
        if key.code == KeyCode::Esc
            || matches!(
                runtime.keybindings.resolve(key),
                Some(Action::Interrupt | Action::Clear)
            )
        {
            if runtime.needs_api_key_setup {
                *should_quit = true;
            } else {
                *pending_login = None;
                input.clear();
                *status = "login cancelled".into();
            }
            return Ok(());
        }
        let plain_enter = key.code == KeyCode::Enter
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !key.modifiers.contains(KeyModifiers::CONTROL);
        if plain_enter {
            let provider = pending_login
                .take()
                .unwrap_or_else(|| SOKET_PROVIDER_ID.into());
            let key_val = input.as_str().trim().to_string();
            input.clear();
            if key_val.is_empty() {
                *pending_login = Some(provider);
                *status = "empty key".into();
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
                runtime.needs_api_key_setup = false;
                chat.clear();
                chat.push(sys(format!(
                    "✓ API key saved for {provider} — you're all set. Type /help for commands."
                )));
                *status = "ready · /help for commands".into();
            }
            return Ok(());
        }
        match runtime.keybindings.resolve(key) {
            Some(Action::DeleteBackward) => input.backspace(),
            Some(Action::DeleteForward) => input.delete(),
            Some(Action::MoveLeft) => input.move_left(),
            Some(Action::MoveRight) => input.move_right(),
            Some(Action::MoveWordLeft) => input.move_word_left(),
            Some(Action::MoveWordRight) => input.move_word_right(),
            Some(Action::MoveLineStart) => input.move_line_start(),
            Some(Action::MoveLineEnd) => input.move_line_end(),
            Some(Action::DeleteWordBackward) => input.delete_word_backward(),
            Some(Action::DeleteToLineStart | Action::DeleteLine) => input.clear(),
            Some(Action::NewLine) => {} // ignore newlines while pasting a key
            _ => {
                if let KeyCode::Char(c) = key.code {
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::SUPER)
                        && c != '\n'
                    {
                        input.insert_char(c);
                    }
                }
            }
        }
        return Ok(());
    }

    if let Some(action) = runtime.keybindings.resolve(key) {
        match action {
            Action::Exit if input.is_empty() => {
                *should_quit = true;
                return Ok(());
            }
            Action::Clear => {
                if last_clear.elapsed() < Duration::from_secs(1) {
                    *clear_presses += 1;
                } else {
                    *clear_presses = 1;
                }
                *last_clear = Instant::now();
                if *clear_presses >= 2 {
                    *should_quit = true;
                } else if input.is_empty() {
                    *status = "ctrl+c again to quit".into();
                } else {
                    input.clear();
                }
                return Ok(());
            }
            Action::Interrupt => {
                let had_work = agent_is_busy(runtime, *working);
                let had_queue = !message_queue.is_empty();
                if had_work {
                    runtime.harness.abort();
                }
                if had_queue {
                    flush_message_queue(chat, message_queue);
                }
                if had_work || had_queue {
                    *working = false;
                    *streaming_assistant = None;
                    if let Some(idx) = streaming_thinking.take() {
                        if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
                            *done = true;
                        }
                    }
                    *status = if had_queue {
                        "interrupted · queue cleared".into()
                    } else {
                        "interrupted".into()
                    };
                } else if last_escape.elapsed() < Duration::from_millis(500) {
                    match runtime.settings.double_escape_action.as_str() {
                        "tree" => {
                            chat.push(sys(
                                "session tree: use /tree (branch nav in session store)",
                            ));
                        }
                        "fork" => {
                            open_fork_picker(
                                runtime,
                                fork_picker,
                                model_picker,
                                chat,
                                status,
                            )
                            .await;
                        }
                        _ => {}
                    }
                }
                *last_escape = Instant::now();
                return Ok(());
            }
            Action::Submit => {
                let line = input.as_str().trim().to_string();
                input.clear();
                if line.is_empty() {
                    return Ok(());
                }
                if let Some(command) = line.strip_prefix('!') {
                    run_bang_command(
                        runtime,
                        chat,
                        status,
                        streaming_assistant,
                        streaming_thinking,
                        command,
                    )
                    .await;
                } else if line.starts_with('/') {
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
                        let effect = commands::dispatch(&cmd, &skill_names, &template_names);
                        *should_quit = apply_effect(
                            effect,
                            runtime,
                            chat,
                            status,
                            pending_login,
                            model_picker,
                            fork_picker,
                            hide_thinking,
                            working,
                            message_queue,
                            streaming_assistant,
                            streaming_thinking,
                            redraw_request,
                            purge_ui_events,
                            input,
                            tx,
                            token_bar,
                            refresh_token_bar,
                        )
                        .await?;
                    }
                } else {
                    submit_user_text(
                        runtime,
                        chat,
                        status,
                        streaming_assistant,
                        streaming_thinking,
                        working,
                        message_queue,
                        line,
                    );
                }
                return Ok(());
            }
            Action::NewLine => {
                input.insert_newline();
                return Ok(());
            }
            Action::MoveLeft => {
                input.move_left();
                return Ok(());
            }
            Action::MoveRight => {
                input.move_right();
                return Ok(());
            }
            Action::MoveUp => {
                let _ = input.move_up();
                return Ok(());
            }
            Action::MoveDown => {
                let _ = input.move_down();
                return Ok(());
            }
            Action::MoveWordLeft => {
                input.move_word_left();
                return Ok(());
            }
            Action::MoveWordRight => {
                input.move_word_right();
                return Ok(());
            }
            Action::MoveLineStart => {
                input.move_line_start();
                return Ok(());
            }
            Action::MoveLineEnd => {
                input.move_line_end();
                return Ok(());
            }
            Action::DeleteBackward => {
                input.backspace();
                return Ok(());
            }
            Action::DeleteForward => {
                input.delete();
                return Ok(());
            }
            Action::DeleteWordBackward => {
                input.delete_word_backward();
                return Ok(());
            }
            Action::DeleteWordForward => {
                input.delete_word_forward();
                return Ok(());
            }
            Action::DeleteToLineStart => {
                input.delete_to_line_start();
                return Ok(());
            }
            Action::DeleteToLineEnd => {
                input.delete_to_line_end();
                return Ok(());
            }
            Action::DeleteLine => {
                input.delete_line();
                return Ok(());
            }
            Action::ModelSelect => {
                *fork_picker = None;
                *model_picker = Some(ModelPickerState::new(&runtime.models));
                return Ok(());
            }
            Action::SessionFork => {
                open_fork_picker(runtime, fork_picker, model_picker, chat, status).await;
                return Ok(());
            }
            Action::ModelCycleForward | Action::ModelCycleBackward => {
                cycle_model(
                    runtime,
                    action == Action::ModelCycleForward,
                    chat,
                )
                .await;
                return Ok(());
            }
            Action::ThinkingCycle => {
                cycle_thinking(runtime, chat).await;
                return Ok(());
            }
            Action::ThinkingToggle => {
                *hide_thinking = !*hide_thinking;
                *redraw_request = true;
                *status = if *hide_thinking {
                    "thinking hidden".into()
                } else {
                    "thinking visible".into()
                };
                return Ok(());
            }
            Action::ToolsExpand => {
                *expand_details = !*expand_details;
                *redraw_request = true;
                *status = if *expand_details {
                    "tool output: expanded".into()
                } else {
                    "tool output: collapsed".into()
                };
                return Ok(());
            }
            Action::MessageCopy => {
                copy_last_assistant(chat, status);
                return Ok(());
            }
            Action::ExternalEditor => {
                if let Ok(edited) = external_edit(input.as_str()) {
                    input.set(edited);
                }
                return Ok(());
            }
            Action::FollowUp => {
                let line = input.as_str().trim().to_string();
                if !line.is_empty() {
                    input.clear();
                    submit_user_text(
                        runtime,
                        chat,
                        status,
                        streaming_assistant,
                        streaming_thinking,
                        working,
                        message_queue,
                        line,
                    );
                }
                return Ok(());
            }
            Action::Dequeue => {
                if let Some(text) = dequeue_last_message(chat, message_queue) {
                    let preview = truncate_status(&text, 40);
                    *status = if message_queue.is_empty() {
                        format!("dequeued: {preview}")
                    } else {
                        format!(
                            "dequeued: {preview} · {} still queued",
                            message_queue.len()
                        )
                    };
                } else {
                    *status = "queue empty".into();
                }
                return Ok(());
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char(_c) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(_c) if key.modifiers.contains(KeyModifiers::SUPER) => {}
        KeyCode::Tab => {
            if let Some(sel) = ac_entries.get(*ac_selected).or_else(|| ac_entries.first()) {
                input.set(format!("{} ", sel.name));
            } else if let (Some(mention), Some(sel)) = (
                at_mention,
                file_ac_entries
                    .get(*ac_selected)
                    .or_else(|| file_ac_entries.first()),
            ) {
                let text = format!("{} ", insert_text(&sel.absolute));
                input.replace_char_range(mention.start, mention.end, &text);
            }
        }
        KeyCode::Char(c) => input.insert_char(c),
        _ => {}
    }
    Ok(())
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

struct ForkPickerState {
    /// Newest-first for display.
    points: Vec<SessionForkPoint>,
    /// Indices into `points` after filter.
    filtered: Vec<usize>,
    selected: usize,
    query: String,
}

impl ForkPickerState {
    fn new(mut points: Vec<SessionForkPoint>) -> Self {
        points.reverse();
        let filtered: Vec<_> = (0..points.len()).collect();
        Self {
            points,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.preview.to_lowercase().contains(&q)
                    || format!("#{}", p.index).contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn selected_point(&self) -> Option<&SessionForkPoint> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.points.get(i))
    }
}

async fn open_fork_picker(
    runtime: &Runtime,
    fork_picker: &mut Option<ForkPickerState>,
    model_picker: &mut Option<ModelPickerState>,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
) {
    *model_picker = None;
    match runtime.harness.fork_points().await {
        Ok(points) if points.is_empty() => {
            *fork_picker = None;
            chat.push(sys("no user messages to fork from"));
            *status = "ready".into();
        }
        Ok(points) => {
            *fork_picker = Some(ForkPickerState::new(points));
            *status = "fork · pick a message to edit".into();
        }
        Err(e) => {
            *fork_picker = None;
            chat.push(sys(format!("fork failed: {e}")));
            *status = "ready".into();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn adopt_forked_session(
    runtime: &mut Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    redraw_request: &mut bool,
    purge_ui_events: &mut bool,
    input: &mut InputBuffer,
    selection: SessionForkSelection,
    through_entry_id: Option<&str>,
    name: Option<String>,
    draft: Option<String>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) {
    message_queue.clear();
    *streaming_assistant = None;
    *streaming_thinking = None;
    *working = false;
    let parent_id = runtime.session_id.clone();
    match runtime
        .harness
        .fork_session(selection, through_entry_id, name)
        .await
    {
        Ok(id) => {
            runtime.session_id = id.clone();
            runtime.active_skills.clear();
            *chat = match runtime.harness.session_context().await {
                Ok(ctx) => chat_items_from_agent_messages(&ctx.messages),
                Err(e) => {
                    tracing::warn!("failed to load forked session transcript: {e}");
                    Vec::new()
                }
            };
            if let Some(text) = draft {
                input.set(text);
                chat.push(sys(format!(
                    "forked from {parent_id} → {id} · edit the message below and submit"
                )));
                *status = "ready · edit forked message".into();
            } else {
                input.clear();
                chat.push(sys(format!("cloned from {parent_id} → {id}")));
                *status = "ready · cloned session".into();
            }
            *redraw_request = true;
            *purge_ui_events = true;
            *refresh_token_bar = true;
            token_bar.sync_window(runtime);
        }
        Err(e) => {
            chat.push(sys(format!("fork failed: {e}")));
            *status = "ready".into();
        }
    }
}

fn endpoint_for(runtime: &Runtime) -> String {
    runtime
        .models
        .get_model(
            &runtime.settings.default_provider,
            &runtime.settings.default_model,
        )
        .map(|m| m.base_url)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| SOKET_BASE_URL.to_string())
}

fn handle_agent_event(
    ev: AgentEvent,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
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
            // Ensure any content that landed after queued bubbles can flush.
            settle_queue_at_end(chat, streaming_assistant, streaming_thinking);
            *refresh_token_bar = true;
        }
        AgentEvent::MessageStart { message } => {
            // Assistant items are created lazily on the first text delta so that
            // thinking / tool items land in true stream order.
            if message.role() == "assistant" {
                *streaming_assistant = None;
            }
        }
        AgentEvent::MessageEnd { message } => {
            token_bar.apply_message(&message);
            *streaming_assistant = None;
            if let Some(idx) = streaming_thinking.take() {
                if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
                    *done = true;
                }
            }
            settle_queue_at_end(chat, streaming_assistant, streaming_thinking);
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            use loop_ai::AssistantMessageEvent;
            match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    let idx = match *streaming_assistant {
                        Some(idx) => idx,
                        None => {
                            let idx = push_before_queued(
                                chat,
                                ChatItem::Assistant {
                                    text: String::new(),
                                },
                            );
                            bump_stream_index(streaming_thinking, idx);
                            *streaming_assistant = Some(idx);
                            idx
                        }
                    };
                    if let Some(ChatItem::Assistant { text }) = chat.get_mut(idx) {
                        text.push_str(&delta);
                    }
                }
                AssistantMessageEvent::TextEnd { .. } => {
                    // Close the segment; any later text starts a new item after
                    // whatever thinking/tool items streamed in between.
                    *streaming_assistant = None;
                }
                AssistantMessageEvent::ThinkingStart { .. } => {
                    *streaming_assistant = None;
                    let idx = push_before_queued(
                        chat,
                        ChatItem::Thinking {
                            text: String::new(),
                            done: false,
                        },
                    );
                    bump_stream_index(streaming_thinking, idx);
                    *streaming_thinking = Some(idx);
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
                    *streaming_assistant = None;
                    let detail =
                        serde_json::to_string_pretty(&tool_call.arguments).unwrap_or_default();
                    let summary = tool_args_summary(&tool_call.name, &tool_call.arguments);
                    upsert_tool(
                        chat,
                        streaming_assistant,
                        streaming_thinking,
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
                    push_before_queued(chat, sys(format!("error: {msg}")));
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
                streaming_assistant,
                streaming_thinking,
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
                push_before_queued(
                    chat,
                    ChatItem::Tool {
                        id: tool_call_id,
                        name: tool_name,
                        summary: if is_error {
                            "error".into()
                        } else {
                            "done".into()
                        },
                        detail: result_text,
                        status: st,
                    },
                );
            }
        }
        _ => {}
    }
}

fn upsert_tool(
    chat: &mut Vec<ChatItem>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
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
        let idx = push_before_queued(
            chat,
            ChatItem::Tool {
                id: id.to_string(),
                name: name.to_string(),
                summary,
                detail,
                status,
            },
        );
        bump_stream_index(streaming_assistant, idx);
        bump_stream_index(streaming_thinking, idx);
    }
}

async fn apply_effect(
    effect: CommandEffect,
    runtime: &mut Runtime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    pending_login: &mut Option<String>,
    model_picker: &mut Option<ModelPickerState>,
    fork_picker: &mut Option<ForkPickerState>,
    hide_thinking: &mut bool,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    redraw_request: &mut bool,
    purge_ui_events: &mut bool,
    input: &mut InputBuffer,
    tx: &mpsc::UnboundedSender<UiEvent>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
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
            *fork_picker = None;
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
                token_bar.sync_window(runtime);
            } else {
                chat.push(sys(format!(
                    "model not found: {provider}/{model} — try /model or refresh"
                )));
            }
        }
        CommandEffect::SetSandbox(mode) => {
            if mode.is_empty() {
                chat.push(sys(format!(
                    "sandbox mode: {} (use /sandbox off|local [--full|--partial] [--crun|--runc|--runsc|--krun])",
                    runtime.settings.sandbox.display()
                )));
                return Ok(false);
            }
            let parts: Vec<&str> = mode.split_whitespace().collect();
            match parts.as_slice() {
                ["off"] | ["disabled"] => {
                    let harness = Arc::clone(&runtime.harness);
                    let tx = tx.clone();
                    *working = true;
                    *status = "disabling sandbox…".into();
                    chat.push(sys(" "));
                    chat.push(sys("disabling sandbox…"));
                    tokio::spawn(async move {
                        let result = async {
                            harness.clear_sandbox().await;
                            let env = harness
                                .tool_env()
                                .await
                                .map_err(|e| format!("sandbox: {e}"))?;
                            harness
                                .set_tools(crate::runtime::build_tools(env))
                                .await
                                .map_err(|e| format!("sandbox: {e}"))?;
                            Ok(SandboxDoneOk::Off)
                        }
                        .await;
                        let _ = tx.send(UiEvent::SandboxDone(result));
                    });
                    return Ok(false);
                }
                ["local", rest @ ..] => {
                    let (isolation, oci_runtime) = match commands::parse_local_sandbox_flags(rest) {
                        Ok(v) => v,
                        Err(e) => {
                            chat.push(sys(e));
                            return Ok(false);
                        }
                    };
                    let harness = Arc::clone(&runtime.harness);
                    let cwd = runtime.cwd.clone();
                    let tx = tx.clone();
                    let iso_s = isolation.as_str().to_string();
                    let rt_s = oci_runtime.as_str().to_string();
                    *working = true;
                    *status = format!("enabling sandbox (--{iso_s} --{rt_s})…");
                    chat.push(sys(" "));
                    chat.push(sys(format!(
                        "enabling sandbox (--{iso_s} --{rt_s})…"
                    )));
                    tokio::spawn(async move {
                        let result = async {
                            let sb = KrunSandbox::new(KrunSandbox::config_for(
                                cwd,
                                isolation,
                                oci_runtime,
                            ));
                            sb.start().await.map_err(|e| e.to_string())?;
                            let env = sb.env();
                            if let Err(e) = harness
                                .set_tools(crate::runtime::build_tools(Arc::clone(&env)))
                                .await
                            {
                                let _ = sb.destroy().await;
                                return Err(format!("sandbox: {e}"));
                            }
                            harness
                                .set_sandbox(SandboxMode::Enabled {
                                    sandbox: Arc::new(sb),
                                })
                                .await;
                            Ok(SandboxDoneOk::Local {
                                isolation: iso_s,
                                runtime: rt_s,
                            })
                        }
                        .await;
                        let _ = tx.send(UiEvent::SandboxDone(result));
                    });
                    return Ok(false);
                }
                ["remote", ..] => {
                    chat.push(sys(
                        "remote sandbox is not implemented yet (use /sandbox local …)",
                    ));
                }
                other => {
                    let joined = other.join(" ");
                    chat.push(sys(format!(
                        "unknown sandbox '{joined}' (off|local [--full|--partial] [--crun|--runc|--runsc|--krun])"
                    )));
                }
            }
        }
        CommandEffect::Login(provider) => {
            let p = provider.unwrap_or_else(|| SOKET_PROVIDER_ID.into());
            *pending_login = Some(p.clone());
            *status = format!("setup · paste API key for {p}");
        }
        CommandEffect::Logout(provider) => {
            let p = provider.unwrap_or_else(|| SOKET_PROVIDER_ID.into());
            runtime.credentials.remove(&p);
            chat.push(sys(format!("logged out: {p}")));
        }
        CommandEffect::NewSession => {
            // Drop queued prompts and live stream markers before swapping sessions.
            message_queue.clear();
            *streaming_assistant = None;
            *streaming_thinking = None;
            *working = false;
            match runtime
                .harness
                .start_new_session(
                    Some(runtime.cwd.to_string_lossy().into_owned()),
                    None,
                )
                .await
            {
                Ok(id) => {
                    runtime.session_id = id;
                    runtime.resumed = false;
                    runtime.active_skills.clear();
                    chat.clear();
                    let policy =
                        ApprovalPolicy::parse(&runtime.settings.file_edit_review);
                    let enabled = policy.asks_for_session(true);
                    if let Some(bridge) = &runtime.tool_approval {
                        bridge.clear_session_grants();
                        bridge.set_policy_active(enabled);
                    }
                    *status = if enabled {
                        "ready · new session · tool approval on (files + bash)".into()
                    } else {
                        "ready · new session".into()
                    };
                    // Clear screen + scrollback and reprint welcome with the new id.
                    *redraw_request = true;
                    *purge_ui_events = true;
                    token_bar.reset(runtime);
                }
                Err(e) => {
                    chat.push(sys(format!("new session failed: {e}")));
                    *status = "ready".into();
                }
            }
        }
        CommandEffect::SetFileReview(arg) => {
            match arg {
                None => {
                    let active = runtime
                        .tool_approval
                        .as_ref()
                        .map(|b| b.policy_active())
                        .unwrap_or(false);
                    let grants = runtime
                        .tool_approval
                        .as_ref()
                        .map(|b| {
                            let g = b.auto_approve_groups();
                            let mut parts = Vec::new();
                            if g.contains(GROUP_FILE) {
                                parts.push("file");
                            }
                            if g.contains(GROUP_BASH) {
                                parts.push("bash");
                            }
                            if parts.is_empty() {
                                "(none)".into()
                            } else {
                                parts.join(", ")
                            }
                        })
                        .unwrap_or_else(|| "(none)".into());
                    let mut perms = String::new();
                    for (k, v) in &runtime.settings.tool_permissions {
                        perms.push_str(&format!("\n    {k}: {v}"));
                    }
                    chat.push(sys(format!(
                        "tool approval policy: {} (session asking: {active})\n  session auto-approve: {grants}\n  /review newSession|always|never\n  settings.toolPermissions:{perms}\n  settings.diffEditor: {}",
                        runtime.settings.file_edit_review,
                        runtime
                            .settings
                            .diff_editor
                            .as_deref()
                            .unwrap_or("(auto: cursor|code)"),
                    )));
                }
                Some(raw) => {
                    let policy = ApprovalPolicy::parse(&raw);
                    runtime.settings.file_edit_review = policy.as_str().into();
                    let enabled = policy.asks_for_session(!runtime.resumed);
                    if let Some(bridge) = &runtime.tool_approval {
                        bridge.set_policy_active(enabled);
                        bridge.set_permissions(permissions_from_settings(
                            &runtime.settings.tool_permissions,
                        ));
                    }
                    let _ = runtime
                        .settings
                        .save_file(&crate::config::paths::settings_path(&runtime.agent_dir));
                    chat.push(sys(format!(
                        "tool approval → {} (this session: {})",
                        policy.as_str(),
                        if enabled { "on" } else { "off" }
                    )));
                }
            }
        }
        CommandEffect::Compact(instructions) => {
            let harness = Arc::clone(&runtime.harness);
            let tx = tx.clone();
            *working = true;
            chat.push(sys(" "));
            chat.push(sys("compacting…"));
            tokio::spawn(async move {
                let result = match harness.compact(instructions.as_deref()).await {
                    Ok(r) => Ok(format!(
                        "compacted · ~{} tokens before · {}",
                        r.tokens_before,
                        truncate_status(&r.summary, 80)
                    )),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(UiEvent::CompactDone(result));
            });
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
            let extra = dynamic_command_entries(runtime);
            chat.push(sys(commands::help_text(&extra)));
        }
        CommandEffect::SessionInfo => {
            match runtime.harness.session_stats().await {
                Ok(stats) => {
                    let mut report =
                        loop_agent::harness::format_session_stats(&stats);
                    report.push_str(&format!(
                        "\nEnvironment\n  Sessions DB: {}\n  Theme: {}\n  Trusted: {}\n  Settings model: {}/{}\n",
                        runtime.sessions_db.display(),
                        runtime.theme.name,
                        runtime.project_trusted,
                        runtime.settings.default_provider,
                        runtime.settings.default_model,
                    ));
                    chat.push(sys(report));
                }
                Err(e) => {
                    chat.push(sys(format!(
                        "provider/model: {}/{}\nsessions db: {}\ntheme: {}\ntrusted: {}\n(stats error: {e})",
                        runtime.settings.default_provider,
                        runtime.settings.default_model,
                        runtime.sessions_db.display(),
                        runtime.theme.name,
                        runtime.project_trusted
                    )));
                }
            }
        }
        CommandEffect::Settings => {
            let mut perms = String::new();
            for (k, v) in &runtime.settings.tool_permissions {
                perms.push_str(&format!("\n    {k}: {v}"));
            }
            chat.push(sys(format!(
                "settings ({})\n  provider: {}\n  model: {}\n  theme: {}\n  thinking: {}\n  sandbox: {}\n  toolApproval: {}\n  toolPermissions:{perms}\n  diffEditor: {}\n  ui: {}",
                crate::config::paths::settings_path(&runtime.agent_dir).display(),
                runtime.settings.default_provider,
                runtime.settings.default_model,
                runtime.settings.theme,
                runtime.settings.default_thinking_level,
                runtime.settings.sandbox.display(),
                runtime.settings.file_edit_review,
                runtime
                    .settings
                    .diff_editor
                    .as_deref()
                    .unwrap_or("(auto)"),
                runtime.settings.ui_mode,
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
        CommandEffect::ListSkills => {
            if runtime.resources.skills.is_empty() {
                chat.push(sys(format!(
                    "No skills loaded.\n\nAdd folders containing a SKILL.md (with YAML frontmatter: name, description) under:\n  {}/skills\n  ~/.agents/skills\n  .agents/skills or .loop/skills in trusted projects\nor list extra paths (e.g. ~/.claude/skills) under \"skills\" in settings.json,\nthen run /reload.",
                    runtime.agent_dir.display()
                )));
            } else {
                let mut text = format!("Skills ({} loaded)\n", runtime.resources.skills.len());
                for s in &runtime.resources.skills {
                    let desc = if s.description.is_empty() {
                        "(no description)".to_string()
                    } else {
                        s.description.clone()
                    };
                    text.push_str(&format!("\n  /skill:{} — {}\n      {}", s.name, desc, s.path.display()));
                }
                text.push_str("\n\nActivate with /skill:<name> [optional args for the input]. Skills stay active until /new; the model sees them under <available_skills> and can read SKILL.md when relevant.");
                chat.push(sys(text));
            }
        }
        CommandEffect::Resume => {
            chat.push(sys(
                "resume: restart with `loop --resume <session-id>` (picker UI forthcoming)",
            ));
            if !runtime.session_id.is_empty() {
                chat.push(sys(format!(
                    "current session id: {}",
                    runtime.session_id
                )));
            }
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
        CommandEffect::Fork => {
            open_fork_picker(runtime, fork_picker, model_picker, chat, status).await;
        }
        CommandEffect::CloneSession => {
            *fork_picker = None;
            *model_picker = None;
            adopt_forked_session(
                runtime,
                chat,
                status,
                working,
                message_queue,
                streaming_assistant,
                streaming_thinking,
                redraw_request,
                purge_ui_events,
                input,
                SessionForkSelection::All,
                None,
                None,
                None,
                token_bar,
                refresh_token_bar,
            )
            .await;
        }
        CommandEffect::Skill { name, args } => {
            if runtime.harness.activate_skill(&name).await {
                if !runtime.active_skills.iter().any(|n| n == &name) {
                    runtime.active_skills.push(name.clone());
                }
                let list = runtime.active_skills.join(", ");
                chat.push(sys(format!(
                    "skill `{name}` is active ({list}). Type your message when ready."
                )));
                *status = format!("skill(s) active: {list} · type your message");
                let args = args.trim();
                if !args.is_empty() {
                    let current = input.as_str();
                    if !current.is_empty()
                        && !current.ends_with(|c: char| c.is_whitespace())
                    {
                        input.insert_str(" ");
                    }
                    input.insert_str(args);
                }
            } else {
                chat.push(sys(format!("skill not found: {name}")));
            }
        }
        CommandEffect::Template { name, args } => {
            if let Some(tmpl) = runtime.resources.prompts.iter().find(|p| p.name == name) {
                let prompt = format_prompt_template_invocation(tmpl, &args);
                let display = format!("/{name} {args}");
                if agent_is_busy(runtime, *working) {
                    enqueue_user_message(chat, message_queue, display, prompt);
                    *status = format!("{} messages queued", message_queue.len());
                } else {
                    start_user_turn(
                        runtime,
                        chat,
                        status,
                        streaming_assistant,
                        streaming_thinking,
                        working,
                        display,
                        prompt,
                    );
                }
            } else {
                chat.push(sys(format!("template not found: {name}")));
            }
        }
    }
    let _ = hide_thinking;
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
        Some(t) => match crate::clipboard::copy_text(&t) {
            Ok(()) => *status = "copied".into(),
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
