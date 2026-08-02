//! Interactive inline CLI (Pi-style): transcript in scrollback, footer redrawn in place.

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
    format_prompt_template_invocation, format_skill_invocation, LocalShellSandbox, Sandbox,
    SandboxConfig, SandboxMode,
};
use loop_agent::types::{AgentEvent, AgentMessage, AgentThinkingLevel};
use loop_ai::providers::{SOKET_BASE_URL, SOKET_PROVIDER_ID};
use loop_ai::{Credential, CredentialStore, ModelsRefreshOptions, ToolResultContent};

use crate::commands::{self, AutocompleteEntry, CommandEffect};
use crate::keybindings::{hotkey_help, Action};
use crate::runtime::Runtime;
use crate::theme::Theme;
use crate::tui::{
    find_tool_index, format_item_lines, item_is_committed, render_lines_to_buffer,
    tool_args_summary, welcome_lines, CardStatus, ChatItem, FOOTER_HEIGHT, FooterOpts,
    InputBuffer, PickerRow, PickerView,
};

enum UiEvent {
    Agent(AgentEvent),
    /// `/compact` finished (success message or error).
    CompactDone(Result<String, String>),
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
    let mut flushed = 0usize;
    let mut input = InputBuffer::new();
    let mut status: String = if runtime.needs_api_key_setup {
        "setup · paste your API key · enter save".into()
    } else {
        "ready · /help for commands".into()
    };
    let mut clear_presses = 0u8;
    let mut last_clear = Instant::now();
    let mut last_escape = Instant::now();
    let mut streaming_assistant: Option<usize> = None;
    let mut streaming_thinking: Option<usize> = None;
    // Pi-style: one global expand flag; toggling clears + reprints the transcript.
    let mut expand_details = false;
    let mut redraw_request = false;
    let mut last_width = terminal.size()?.width;
    let mut hide_thinking = runtime.settings.hide_thinking_block;
    let mut pending_login: Option<String> = if runtime.needs_api_key_setup {
        Some(SOKET_PROVIDER_ID.into())
    } else {
        None
    };
    let mut model_picker: Option<ModelPickerState> = None;
    let mut ac_selected: usize = 0;
    let mut last_ac_filter = String::new();
    let mut working = false;
    let mut spinner_frame: usize = 0;
    let mut last_spin = Instant::now();
    let version = env!("CARGO_PKG_VERSION");
    let path_line = path_status_line(&runtime.cwd);

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
        {
            let extra = dynamic_command_entries(runtime);
            commands::autocomplete_entries(input.as_str(), &extra)
        } else {
            Vec::new()
        };
        if input.as_str() != last_ac_filter {
            ac_selected = 0;
            last_ac_filter = input.as_str().to_string();
        }
        if !ac_entries.is_empty() {
            ac_selected = ac_selected.min(ac_entries.len() - 1);
        }

        let picker = if let Some(p) = &model_picker {
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
        } else {
            PickerView::None
        };

        let status_line = if model_picker.is_some() {
            "↑↓ select · enter confirm · esc cancel".into()
        } else if pending_login.is_some() {
            "setup · paste your API key · enter save".into()
        } else if !ac_entries.is_empty() {
            "↑↓ select · tab complete · enter run".into()
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

        let live: Vec<ChatItem> = chat[flushed..].to_vec();
        let setup_mode = pending_login.is_some();

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
                },
            );
        })?;
        let _ = terminal.hide_cursor();

        let timed_out = !event::poll(tick)?;
        if timed_out {
            drain_ui_events(
                &mut rx,
                &mut chat,
                &mut status,
                &mut streaming_assistant,
                &mut streaming_thinking,
                &mut working,
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
                        &mut hide_thinking,
                        &mut pending_login,
                        &mut model_picker,
                        &mut working,
                        &mut should_quit,
                        &ac_entries,
                        &mut ac_selected,
                        &tx,
                    )
                    .await?;
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
            &mut rx,
            &mut chat,
            &mut status,
            &mut streaming_assistant,
            &mut streaming_thinking,
            &mut working,
        );
    }

    runtime.harness.request_shutdown();
    Ok(())
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
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
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
                );
            }
            UiEvent::CompactDone(result) => {
                *working = false;
                match result {
                    Ok(msg) => {
                        chat.push(sys(msg));
                        *status = "ready".into();
                    }
                    Err(e) => {
                        chat.push(sys(format!("compact failed: {e}")));
                        *status = "ready".into();
                    }
                }
            }
        }
    }
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
    hide_thinking: &mut bool,
    pending_login: &mut Option<String>,
    model_picker: &mut Option<ModelPickerState>,
    working: &mut bool,
    should_quit: &mut bool,
    ac_entries: &[AutocompleteEntry],
    ac_selected: &mut usize,
    tx: &mpsc::UnboundedSender<UiEvent>,
) -> anyhow::Result<()> {
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
                if runtime.harness.phase() != loop_agent::harness::AgentHarnessPhase::Idle {
                    runtime.harness.abort();
                    *working = false;
                    *status = "interrupted".into();
                } else if *working {
                    // Clear a stuck spinner (e.g. fire-and-forget work with no AgentEnd).
                    *working = false;
                    *status = "ready".into();
                } else if last_escape.elapsed() < Duration::from_millis(500)
                    && runtime.settings.double_escape_action == "tree"
                {
                    chat.push(sys(
                        "session tree: use /tree (branch nav in session store)",
                    ));
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
                        let effect = commands::dispatch(&cmd, &skill_names, &template_names);
                        *should_quit = apply_effect(
                            effect,
                            runtime,
                            chat,
                            status,
                            pending_login,
                            model_picker,
                            hide_thinking,
                            working,
                            tx,
                        )
                        .await?;
                    }
                } else {
                    chat.push(ChatItem::User { text: line.clone() });
                    *working = true;
                    *streaming_assistant = None;
                    *streaming_thinking = None;
                    let harness = Arc::clone(&runtime.harness);
                    tokio::spawn(async move {
                        let _ = harness.prompt(line).await;
                    });
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
                *model_picker = Some(ModelPickerState::new(&runtime.models));
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
                    runtime
                        .harness
                        .follow_up(AgentMessage::user_text(line.clone()));
                    chat.push(sys(format!("queued follow-up: {line}")));
                    input.clear();
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
            // Assistant items are created lazily on the first text delta so that
            // thinking / tool items land in true stream order.
            if message.role() == "assistant" {
                *streaming_assistant = None;
            }
        }
        AgentEvent::MessageEnd { .. } => {
            *streaming_assistant = None;
            if let Some(idx) = streaming_thinking.take() {
                if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
                    *done = true;
                }
            }
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
                            chat.push(ChatItem::Assistant {
                                text: String::new(),
                            });
                            let idx = chat.len() - 1;
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
                    *streaming_assistant = None;
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
    working: &mut bool,
    tx: &mpsc::UnboundedSender<UiEvent>,
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
            *status = format!("setup · paste API key for {p}");
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
            let tx = tx.clone();
            *working = true;
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
                text.push_str("\n\nInvoke with /skill:<name> [args] or let the model pick them up automatically.");
                chat.push(sys(text));
            }
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
