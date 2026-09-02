//! Pi-style inline UI: transcript in terminal scrollback, footer redrawn in place.

pub mod editor;
pub mod file_mentions;
pub mod highlight;
pub mod history;
pub mod markdown;

pub use editor::InputBuffer;
pub use history::CommandHistory;
pub use file_mentions::{filter_files, find_at_mention, insert_text, list_files, AtMention, FileEntry};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Fixed inline footer height (live + input + picker + status).
pub const FOOTER_HEIGHT: u16 = 18;

/// Prompt prefix width (`❯ ` / `  `).
const INPUT_PREFIX_WIDTH: usize = 2;

/// Hard cap on visible input body rows (top/bottom rules are extra).
const MAX_INPUT_BODY_LINES: u16 = 10;

/// Max visible rows in a picker list.
pub const PICKER_PAGE: usize = 8;

/// Tool / thinking card status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    Pending,
    Success,
    Error,
}

/// A chat transcript item.
#[derive(Debug, Clone)]
pub enum ChatItem {
    User { text: String },
    /// User message waiting for the agent to become idle (not yet sent).
    Queued { text: String },
    Assistant { text: String },
    Thinking { text: String, done: bool },
    Tool {
        id: String,
        name: String,
        summary: String,
        detail: String,
        status: CardStatus,
    },
    /// Local `!command` output — always shown in full (never collapsed).
    Shell {
        command: String,
        output: String,
        exit_code: Option<i32>,
    },
    System { text: String },
}

/// One row in a navigable picker (commands / models).
#[derive(Debug, Clone)]
pub struct PickerRow {
    pub label: String,
    pub description: String,
    /// Optional trailing mark (e.g. ✓ for current model).
    pub mark: Option<String>,
}

/// What appears under the input line.
#[derive(Debug, Clone)]
pub enum PickerView {
    None,
    Commands {
        rows: Vec<PickerRow>,
        selected: usize,
    },
    Models {
        rows: Vec<PickerRow>,
        selected: usize,
        hint: String,
    },
    Setup {
        provider: String,
    },
    /// Accept / reject a pending tool (optional reject reason).
    FileReview {
        path: String,
        /// 0 = Accept, 1 = Accept all (session), 2 = Reject.
        selected: usize,
        accept_all_label: String,
        reason: String,
        reason_focused: bool,
    },
}

/// Braille spinner frames.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Options for drawing the inline footer.
pub struct FooterOpts<'a> {
    pub theme: &'a Theme,
    /// Unflushed / streaming items shown above the input.
    pub live: &'a [ChatItem],
    pub input: &'a str,
    pub cursor: usize,
    pub working: bool,
    pub spinner_frame: usize,
    pub status: &'a str,
    pub picker: &'a PickerView,
    /// Global tools/thinking expand state (pi-style single flag).
    pub expanded: bool,
    pub hide_thinking: bool,
    pub setup_mode: bool,
    pub mask_input: bool,
    /// Left status (e.g. `~/loop (main)`).
    pub path_line: &'a str,
    /// Right status (e.g. `soket/qwen3-30b · medium`).
    pub model_line: &'a str,
    /// Right of the bottom status row (e.g. `12,345 · 23% · 29,440/128,000`).
    pub usage_line: &'a str,
}

/// Whether a transcript item is finished and safe to flush into scrollback.
pub fn item_is_committed(
    item: &ChatItem,
    index: usize,
    streaming_assistant: Option<usize>,
    streaming_thinking: Option<usize>,
) -> bool {
    match item {
        ChatItem::User { .. } | ChatItem::System { .. } | ChatItem::Shell { .. } => true,
        // Keep queued messages in the live footer so Esc can still remove them.
        ChatItem::Queued { .. } => false,
        ChatItem::Assistant { .. } => streaming_assistant != Some(index),
        ChatItem::Thinking { done, .. } => *done && streaming_thinking != Some(index),
        ChatItem::Tool { status, .. } => !matches!(status, CardStatus::Pending),
    }
}

/// Block-letter banner rows (ANSI-shadow style).
const BANNER_ROWS: [&str; 6] = [
    "██╗      ██████╗  ██████╗ ██████╗ ",
    "██║     ██╔═══██╗██╔═══██╗██╔══██╗",
    "██║     ██║   ██║██║   ██║██████╔╝",
    "██║     ██║   ██║██║   ██║██╔═══╝ ",
    "███████╗╚██████╔╝╚██████╔╝██║     ",
    "╚══════╝ ╚═════╝  ╚═════╝ ╚═╝     ",
];

/// Top→bottom banner gradient (periwinkle blues).
const BANNER_GRADIENT: [(u8, u8, u8); 6] = [
    (165, 184, 255),
    (147, 167, 251),
    (130, 150, 247),
    (112, 133, 243),
    (95, 116, 239),
    (77, 99, 235),
];

/// Welcome screen for `insert_before`: banner, tagline, and an info card.
#[allow(clippy::too_many_arguments)]
pub fn welcome_lines(
    theme: &Theme,
    version: &str,
    provider: &str,
    model: &str,
    endpoint: &str,
    session_id: &str,
    skills: usize,
    prompts: usize,
    needs_setup: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let w = width.max(20) as usize;
    let mut out = Vec::new();
    out.push(Line::from(""));

    // Banner
    if w >= BANNER_ROWS[0].chars().count() + 2 {
        for (row, (r, g, b)) in BANNER_ROWS.iter().zip(BANNER_GRADIENT) {
            out.push(Line::from(Span::styled(
                (*row).to_string(),
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD),
            )));
        }
    } else {
        out.push(Line::from(Span::styled(
            "LOOP".to_string(),
            theme.accent_bold(),
        )));
    }
    out.push(Line::from(""));
    out.push(Line::from(vec![
        Span::styled("✦ ".to_string(), theme.accent()),
        Span::styled("Interactive Coding Agent Harness".to_string(), theme.style("text")),
        Span::styled(" ✦".to_string(), theme.accent()),
    ]));
    out.push(Line::from(""));

    // Info card
    let border = theme.style("border");
    let rows: Vec<(&str, String)> = vec![
        ("Provider", provider.to_string()),
        ("Model", model.to_string()),
        ("Endpoint", endpoint.to_string()),
        ("Session", session_id.to_string()),
    ];
    let (dot_style, status_text) = if needs_setup {
        (
            theme.style("warning"),
            "setup — paste your API key to begin".to_string(),
        )
    } else {
        (
            theme.style("success"),
            "ready — type /help to begin".to_string(),
        )
    };
    let max_inner = w.saturating_sub(2).max(24);
    let mut inner = rows
        .iter()
        .map(|(_, v)| 2 + 11 + UnicodeWidthStr::width(v.as_str()) + 2)
        .chain(std::iter::once(
            2 + 2 + UnicodeWidthStr::width(status_text.as_str()) + 2,
        ))
        .max()
        .unwrap_or(24);
    inner = inner.clamp(24, max_inner);

    let hline = |l: char, r: char| -> Line<'static> {
        Line::from(Span::styled(
            format!("{l}{}{r}", "─".repeat(inner)),
            border,
        ))
    };
    let pad_row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let used: usize = spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let mut all = vec![Span::styled("│".to_string(), border)];
        all.extend(spans);
        if used < inner {
            all.push(Span::raw(" ".repeat(inner - used)));
        }
        all.push(Span::styled("│".to_string(), border));
        Line::from(all)
    };

    out.push(hline('╭', '╮'));
    for (label, value) in &rows {
        let value = truncate_width(value, inner.saturating_sub(15));
        out.push(pad_row(vec![
            Span::styled(format!("  {label:<11}"), theme.muted()),
            Span::styled(value, theme.style("text").add_modifier(Modifier::BOLD)),
        ]));
    }
    out.push(hline('├', '┤'));
    out.push(pad_row(vec![
        Span::styled("  ● ".to_string(), dot_style),
        Span::styled(truncate_width(&status_text, inner.saturating_sub(6)), theme.style("text")),
    ]));
    out.push(hline('╰', '╯'));

    out.push(Line::from(Span::styled(
        format!("loop v{version} · {skills} skills · {prompts} prompts"),
        theme.muted(),
    )));
    out.push(Line::from(Span::styled(
        "enter send · queues while busy · esc interrupt · shift+enter newline · / commands · ctrl+o expand details · ctrl+c quit"
            .to_string(),
        theme.dim(),
    )));
    out.push(Line::from(""));
    out
}

/// Full-width user message band with a `❯` prompt marker.
/// When `queued`, render muted with a trailing queue hint.
fn format_user_band(
    text: &str,
    theme: &Theme,
    w: usize,
    queued: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let text_style = if queued {
        theme.muted()
    } else {
        theme.style("userMessageText")
    };
    let marker_style = if queued {
        theme.muted()
    } else {
        theme.accent_bold()
    };
    lines.push(bg_spans_line(
        theme,
        "userMessageBg",
        vec![Span::raw("\u{00a0}")],
        w,
    ));
    let mut first = true;
    let body_w = w.saturating_sub(5).max(1);
    for l in text.lines() {
        for part in soft_wrap(l, body_w) {
            let marker = if first { "❯ " } else { "  " };
            first = false;
            lines.push(bg_spans_line(
                theme,
                "userMessageBg",
                vec![
                    Span::raw(" "),
                    Span::styled(marker.to_string(), marker_style),
                    Span::styled(part, text_style),
                ],
                w,
            ));
        }
    }
    if first {
        lines.push(bg_spans_line(
            theme,
            "userMessageBg",
            vec![
                Span::raw(" "),
                Span::styled("❯".to_string(), marker_style),
            ],
            w,
        ));
    }
    if queued {
        lines.push(bg_spans_line(
            theme,
            "userMessageBg",
            vec![
                Span::raw(" "),
                Span::styled("  queued".to_string(), theme.dim()),
            ],
            w,
        ));
    }
    lines.push(bg_spans_line(
        theme,
        "userMessageBg",
        vec![Span::raw("\u{00a0}")],
        w,
    ));
    lines.push(Line::from(" "));
    lines
}

/// Format a single chat item as lines (for scrollback or live area).
pub fn format_item_lines(
    item: &ChatItem,
    theme: &Theme,
    expanded: bool,
    hide_thinking: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let w = width.max(8) as usize;
    let mut lines = Vec::new();
    match item {
        ChatItem::User { text } => {
            lines.extend(format_user_band(text, theme, w, false));
        }
        ChatItem::Queued { text } => {
            lines.extend(format_user_band(text, theme, w, true));
        }
        ChatItem::Assistant { text } => {
            if text.is_empty() {
                return lines;
            }
            let rendered = markdown::render_lines(text, theme);
            lines.extend(markdown::wrap_rendered_lines(rendered, w));
            lines.push(Line::from(""));
        }
        ChatItem::Thinking { text, done } => {
            let label = if *done { "Thinking" } else { "Thinking…" };
            let think = theme
                .style("thinkingText")
                .add_modifier(Modifier::ITALIC);
            if hide_thinking {
                lines.push(Line::from(vec![
                    Span::styled("✦ ".to_string(), theme.accent()),
                    Span::styled(label.to_string(), think),
                ]));
            } else if expanded && !text.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("✦ ".to_string(), theme.accent()),
                    Span::styled(label.to_string(), think.add_modifier(Modifier::BOLD)),
                    Span::styled("  ctrl+o to collapse".to_string(), theme.dim()),
                ]));
                let body_w = w.saturating_sub(4).max(1);
                let mut shown = 0usize;
                'outer: for l in text.lines() {
                    for part in soft_wrap(l, body_w) {
                        if shown >= 200 {
                            lines.push(Line::from(vec![
                                Span::styled("  │ ".to_string(), theme.style("borderMuted")),
                                Span::styled("…".to_string(), theme.dim()),
                            ]));
                            break 'outer;
                        }
                        lines.push(Line::from(vec![
                            Span::styled("  │ ".to_string(), theme.style("borderMuted")),
                            Span::styled(part, think),
                        ]));
                        shown += 1;
                    }
                }
            } else {
                // Collapsed: one-line trace. While streaming, show the latest line.
                let summary = if *done {
                    line_summary(text.lines().next().unwrap_or(""), 72)
                } else {
                    line_summary(
                        text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(""),
                        72,
                    )
                };
                let mut spans = vec![
                    Span::styled("✦ ".to_string(), theme.accent()),
                    Span::styled(label.to_string(), think),
                ];
                if !summary.is_empty() {
                    spans.push(Span::styled(format!(" · {summary}"), think));
                }
                if *done && !text.is_empty() {
                    spans.push(Span::styled("  ctrl+o".to_string(), theme.dim()));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
        }
        ChatItem::Tool {
            name,
            summary,
            detail,
            status,
            ..
        } => {
            let (dot_key, bg) = match status {
                CardStatus::Pending => ("warning", "toolPendingBg"),
                CardStatus::Success => ("success", "toolSuccessBg"),
                CardStatus::Error => ("error", "toolErrorBg"),
            };
            let title = if name == "bash" && !summary.is_empty() {
                format!("$ {summary}")
            } else if summary.is_empty() {
                name.clone()
            } else {
                format!("{name} {summary}")
            };
            let detail_lines = detail.lines().count();
            let mut head = vec![
                Span::styled(" ● ".to_string(), theme.style(dot_key)),
                Span::styled(
                    title,
                    theme.style("toolTitle").add_modifier(Modifier::BOLD),
                ),
            ];
            match status {
                CardStatus::Pending => {
                    head.push(Span::styled(" · running…".to_string(), theme.dim()));
                }
                _ if detail.is_empty() => {}
                _ if expanded => {
                    head.push(Span::styled(
                        " · ctrl+o to collapse".to_string(),
                        theme.dim(),
                    ));
                }
                _ => {
                    head.push(Span::styled(
                        format!(
                            " · {detail_lines} line{} · ctrl+o",
                            if detail_lines == 1 { "" } else { "s" }
                        ),
                        theme.dim(),
                    ));
                }
            }
            lines.push(bg_spans_line(theme, bg, head, w));

            // Body: full detail when expanded; error previews stay visible.
            let preview = if expanded {
                200
            } else if matches!(status, CardStatus::Error) {
                4
            } else {
                0
            };
            if preview > 0 && !detail.is_empty() {
                let body_w = w.saturating_sub(5).max(1);
                let fallback = theme.style("toolOutput");
                let highlighted = highlight_tool_detail(name, summary, detail, theme, fallback);
                for spans in highlighted.into_iter().take(preview) {
                    let mut row = vec![Span::styled(
                        "  │ ".to_string(),
                        theme.style("borderMuted"),
                    )];
                    row.extend(highlight::truncate_spans(spans, body_w));
                    lines.push(Line::from(row));
                }
                if detail_lines > preview {
                    let hint = if expanded {
                        format!("  … {} more lines truncated", detail_lines - preview)
                    } else {
                        format!("  … {} more lines · ctrl+o", detail_lines - preview)
                    };
                    lines.push(Line::from(Span::styled(hint, theme.dim())));
                }
            }
            lines.push(Line::from(""));
        }
        ChatItem::Shell {
            command,
            output,
            exit_code,
        } => {
            lines.extend(format_shell_box(command, output, *exit_code, theme, w));
        }
        ChatItem::System { text } => {
            for l in text.lines() {
                lines.extend(wrap_plain(l, theme.dim(), w));
            }
            lines.push(Line::from(""));
        }
    }
    lines
}

/// Always-visible boxed `!command` output.
fn format_shell_box(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    theme: &Theme,
    w: usize,
) -> Vec<Line<'static>> {
    let border = theme.style("border");
    let text = theme.style("text");
    let code_style = match exit_code {
        Some(0) | None => theme.style("success"),
        Some(_) => theme.style("error"),
    };

    // Inner width between the vertical borders (`│` + content + `│`).
    let inner = w.saturating_sub(2).max(8);
    let content_w = inner.saturating_sub(2).max(1); // side padding inside the box

    let hline = |l: char, r: char| -> Line<'static> {
        Line::from(Span::styled(
            format!("{l}{}{r}", "─".repeat(inner)),
            border,
        ))
    };
    let pad_row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let used: usize = spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let mut all = vec![Span::styled("│".to_string(), border)];
        all.extend(spans);
        if used < inner {
            all.push(Span::raw(" ".repeat(inner - used)));
        }
        all.push(Span::styled("│".to_string(), border));
        Line::from(all)
    };
    let content_row = |prefix: Vec<Span<'static>>, body: String, body_style: Style| -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(prefix);
        spans.push(Span::styled(body, body_style));
        pad_row(spans)
    };

    let mut out = Vec::new();
    out.push(hline('╭', '╮'));

    // Command header: `$ cmd` (wrap long commands).
    let mut cmd_first = true;
    for part in soft_wrap(command, content_w.saturating_sub(2).max(1)) {
        if cmd_first {
            out.push(content_row(
                vec![Span::styled("$ ".to_string(), theme.accent())],
                part,
                text,
            ));
            cmd_first = false;
        } else {
            out.push(content_row(
                vec![Span::raw("  ".to_string())],
                part,
                text,
            ));
        }
    }
    if cmd_first {
        out.push(content_row(
            vec![Span::styled("$".to_string(), theme.accent())],
            String::new(),
            text,
        ));
    }

    if !output.is_empty() {
        out.push(hline('├', '┤'));
        for l in output.lines() {
            if l.is_empty() {
                out.push(pad_row(Vec::new()));
                continue;
            }
            for part in soft_wrap(l, content_w) {
                out.push(content_row(Vec::new(), part, text));
            }
        }
    }

    if let Some(code) = exit_code {
        out.push(hline('├', '┤'));
        out.push(content_row(
            Vec::new(),
            format!("[exit {code}]"),
            code_style,
        ));
    }

    out.push(hline('╰', '╯'));
    out.push(Line::from(""));
    out
}

/// Draw lines into a buffer (used by `insert_before`).
pub fn render_lines_to_buffer(lines: &[Line<'static>], buf: &mut Buffer, theme: &Theme) {
    Paragraph::new(lines.to_vec())
        .style(theme.page())
        .wrap(Wrap { trim: false })
        .render(buf.area, buf);
}

/// Draw the inline footer (live stream + input + picker + status).
pub fn draw_footer(frame: &mut Frame, opts: FooterOpts<'_>) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, opts.theme.page());
    let width = area.width.max(1);

    let live_lines = {
        let mut out = Vec::new();
        for item in opts.live {
            out.extend(format_item_lines(
                item,
                opts.theme,
                opts.expanded,
                opts.hide_thinking,
                width,
            ));
        }
        if opts.working && out.is_empty() {
            let spin = SPINNER_FRAMES[opts.spinner_frame % SPINNER_FRAMES.len()];
            out.push(Line::from(Span::styled(
                format!(" {spin} {}", opts.status),
                opts.theme.muted(),
            )));
        }
        out
    };

    let picker_h = picker_height(opts.picker);
    let status_h = 2u16;
    let max_input_body = area
        .height
        .saturating_sub(picker_h + status_h + 2)
        .max(1)
        .min(MAX_INPUT_BODY_LINES);
    let input_body_lines =
        count_input_visual_lines(opts.input, width as usize).clamp(1, max_input_body as usize) as u16;
    let input_h = input_body_lines + 2; // top + bottom rules
    let used = input_h + picker_h + status_h;
    let live_h = area.height.saturating_sub(used);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(live_h),
            Constraint::Length(input_h),
            Constraint::Length(picker_h),
            Constraint::Length(status_h),
        ])
        .split(area);

    // Live / spacer
    if live_h > 0 {
        let visible = if live_lines.len() as u16 > live_h {
            let skip = live_lines.len() - live_h as usize;
            live_lines[skip..].to_vec()
        } else {
            live_lines
        };
        frame.render_widget(
            Paragraph::new(visible)
                .style(opts.theme.page())
                .wrap(Wrap { trim: false }),
            chunks[0],
        );
    }

    draw_input(frame, chunks[1], &opts);
    draw_picker(frame, chunks[2], opts.theme, opts.picker);
    draw_status(frame, chunks[3], &opts);
}

fn draw_input(frame: &mut Frame, area: Rect, opts: &FooterOpts<'_>) {
    let rule = Style::default().fg(opts.theme.get("borderAccent"));
    let text_style = opts.theme.style("text");
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let rule_line = "─".repeat(area.width as usize);
    let page = opts.theme.page();
    frame.render_widget(
        Paragraph::new(Span::styled(rule_line.clone(), rule)).style(page),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(rule_line, rule)).style(page),
        chunks[2],
    );

    let display = if opts.mask_input {
        "•".repeat(opts.input.chars().count())
    } else if opts.setup_mode && opts.input.is_empty() {
        String::new()
    } else {
        opts.input.to_string()
    };

    let placeholder = if !display.is_empty() {
        ""
    } else if opts.setup_mode {
        " paste your API key"
    } else {
        " Type a message · / for commands · @ for files"
    };
    let lines = render_input_lines(
        &display,
        opts.cursor,
        text_style,
        opts.theme,
        placeholder,
        area.width as usize,
        chunks[1].height as usize,
    );
    frame.render_widget(
        Paragraph::new(lines).style(opts.theme.page()),
        chunks[1],
    );
}

fn draw_picker(frame: &mut Frame, area: Rect, theme: &Theme, picker: &PickerView) {
    if area.height == 0 {
        return;
    }
    let lines = match picker {
        PickerView::None => Vec::new(),
        PickerView::Setup { provider } => {
            let env_hint = if provider == "soket" {
                "SOKET_API_KEY / TENSORSTUDIO_API_KEY / LOOP_API_KEY".to_string()
            } else {
                format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"))
            };
            vec![
                Line::from(vec![
                    Span::styled("  ◆ ".to_string(), theme.accent()),
                    Span::styled(
                        format!("Connect to {provider}"),
                        theme.style("text").add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(
                    "    Paste your API key and press enter — input stays hidden".to_string(),
                    theme.muted(),
                )),
                Line::from(Span::styled(
                    format!("    Tip: you can also set {env_hint} and restart"),
                    theme.dim(),
                )),
                Line::from(Span::styled(
                    "    enter save · esc quit".to_string(),
                    theme.dim(),
                )),
            ]
        }
        PickerView::Commands { rows, selected } => picker_lines(rows, *selected, theme, false),
        PickerView::Models { rows, selected, hint } => {
            let mut out = vec![Line::from(Span::styled(hint.clone(), theme.style("warning")))];
            out.extend(picker_lines(rows, *selected, theme, true));
            out
        }
        PickerView::FileReview {
            path,
            selected,
            accept_all_label,
            reason,
            reason_focused,
        } => {
            let opt = |idx: usize, label: &str, style_key: &str| {
                let mark = if *selected == idx { "❯" } else { " " };
                let style = if *selected == idx {
                    theme.style(style_key).add_modifier(Modifier::BOLD)
                } else {
                    theme.style("text")
                };
                Line::from(vec![
                    Span::styled(format!(" {mark} "), theme.accent()),
                    Span::styled(label.to_string(), style),
                ])
            };
            let reason_line = if *reason_focused {
                format!("  reason › {reason}▌")
            } else if reason.is_empty() {
                "  tab · add reject reason for the model".to_string()
            } else {
                format!("  reason · {reason}")
            };
            vec![
                Line::from(vec![
                    Span::styled("  review ".to_string(), theme.accent_bold()),
                    Span::styled(path.clone(), theme.style("text").add_modifier(Modifier::BOLD)),
                ]),
                opt(0, "Accept", "success"),
                opt(1, accept_all_label, "success"),
                opt(2, "Reject", "error"),
                Line::from(Span::styled(reason_line, theme.muted())),
                Line::from(Span::styled(
                    "  ↑↓ choose · tab reason · enter confirm · esc reject".to_string(),
                    theme.dim(),
                )),
            ]
        }
    };
    frame.render_widget(Paragraph::new(lines).style(theme.page()), area);
}

fn picker_lines(
    rows: &[PickerRow],
    selected: usize,
    theme: &Theme,
    _show_mark: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if rows.is_empty() {
        out.push(Line::from(Span::styled("  no matches".to_string(), theme.dim())));
        return out;
    }
    let page = PICKER_PAGE;
    let start = if selected >= page {
        selected + 1 - page
    } else {
        0
    };
    let end = (start + page).min(rows.len());
    let label_w = rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.label.as_str()))
        .max()
        .unwrap_or(16)
        .clamp(16, 28);
    for (i, row) in rows.iter().enumerate().take(end).skip(start) {
        let active = i == selected;
        let arrow = if active { "❯" } else { " " };
        let row_bg = active.then(|| theme.get("selectedBg"));
        let with_bg = |style: Style| match row_bg {
            Some(bg) => style.bg(bg),
            None => style,
        };
        let label_style = if active {
            with_bg(theme.style("text").add_modifier(Modifier::BOLD))
        } else {
            theme.style("text")
        };
        let mut spans = vec![
            Span::styled(format!(" {arrow} "), with_bg(theme.accent_bold())),
            Span::styled(
                format!("{:<label_w$}", truncate_width(&row.label, label_w)),
                label_style,
            ),
            Span::styled(format!(" {}", row.description), with_bg(theme.muted())),
        ];
        if let Some(m) = &row.mark {
            spans.push(Span::styled(format!(" {m}"), with_bg(theme.style("success"))));
        }
        out.push(Line::from(spans));
    }
    let mut counter = format!("  {}/{}", selected + 1, rows.len());
    if rows.len() > page {
        counter.push_str(" · ↑↓ scroll");
    }
    out.push(Line::from(Span::styled(counter, theme.dim())));
    out
}

fn draw_status(frame: &mut Frame, area: Rect, opts: &FooterOpts<'_>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let top = Line::from(vec![
        Span::styled(opts.path_line.to_string(), opts.theme.muted()),
        Span::raw(" ".repeat(
            area.width.saturating_sub(
                (UnicodeWidthStr::width(opts.path_line) + UnicodeWidthStr::width(opts.model_line))
                    as u16,
            ) as usize,
        )),
        Span::styled(opts.model_line.to_string(), opts.theme.muted()),
    ]);

    let status_left = if opts.working {
        let spin = SPINNER_FRAMES[opts.spinner_frame % SPINNER_FRAMES.len()];
        format!(" {spin} {}", opts.status)
    } else {
        format!(" {}", opts.status)
    };
    let usage = opts.usage_line;
    let gap = area.width.saturating_sub(
        (UnicodeWidthStr::width(status_left.as_str()) + UnicodeWidthStr::width(usage)) as u16,
    ) as usize;
    let bottom = if opts.working {
        Line::from(vec![
            Span::styled(status_left, opts.theme.accent()),
            Span::raw(" ".repeat(gap)),
            Span::styled(usage.to_string(), opts.theme.muted()),
        ])
    } else {
        Line::from(vec![
            Span::styled(status_left, opts.theme.dim()),
            Span::raw(" ".repeat(gap)),
            Span::styled(usage.to_string(), opts.theme.muted()),
        ])
    };
    let page = opts.theme.page();
    frame.render_widget(Paragraph::new(top).style(page), chunks[0]);
    frame.render_widget(Paragraph::new(bottom).style(page), chunks[1]);
}

/// Format a token count for the footer: commas below 1M, then `M` / `B` with decimals.
pub fn format_compact_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format_token_unit(n as f64 / 1_000_000_000.0, 'B')
    } else if n >= 1_000_000 {
        format_token_unit(n as f64 / 1_000_000.0, 'M')
    } else {
        format_commas(n)
    }
}

fn format_token_unit(v: f64, unit: char) -> String {
    let rounded = (v * 10.0).round() / 10.0;
    if (rounded - rounded.trunc()).abs() < f64::EPSILON {
        format!("{:.0}{unit}", rounded)
    } else {
        format!("{rounded:.1}{unit}")
    }
}

fn format_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// Footer usage summary: `total · pct% · current/window`.
pub fn format_token_usage_line(
    total_tokens: u64,
    context_tokens: Option<u64>,
    context_window: u64,
) -> String {
    let total = format_compact_tokens(total_tokens);
    if context_window == 0 {
        return total;
    }
    let window = format_compact_tokens(context_window);
    match context_tokens {
        Some(tokens) => {
            let pct = ((tokens as f64 / context_window as f64) * 100.0).clamp(0.0, 999.0);
            let pct_label = if pct < 10.0 {
                format!("{pct:.1}%")
            } else {
                format!("{:.0}%", pct.round())
            };
            format!(
                "{total} · {pct_label} · {}/{window}",
                format_compact_tokens(tokens)
            )
        }
        None => format!("{total} · —/{window}"),
    }
}

fn picker_height(picker: &PickerView) -> u16 {
    match picker {
        PickerView::None => 0,
        PickerView::Setup { .. } => 4,
        PickerView::FileReview { .. } => 6,
        PickerView::Commands { rows, .. } => {
            let n = rows.len().min(PICKER_PAGE) as u16;
            n + 1 // page indicator
        }
        PickerView::Models { rows, .. } => {
            let n = rows.len().min(PICKER_PAGE) as u16;
            n + 2 // hint + page
        }
    }
}

fn input_content_width(term_width: usize) -> usize {
    term_width.saturating_sub(INPUT_PREFIX_WIDTH).max(1)
}

fn count_input_visual_lines(input: &str, term_width: usize) -> usize {
    if input.is_empty() {
        return 1;
    }
    let w = input_content_width(term_width);
    input
        .split('\n')
        .map(|line| soft_wrap(line, w).len().max(1))
        .sum()
}

fn input_scroll_top(total: usize, cursor_row: usize, visible: usize) -> usize {
    if total <= visible {
        0
    } else if cursor_row < visible {
        0
    } else {
        (cursor_row + 1)
            .saturating_sub(visible)
            .min(total.saturating_sub(visible))
    }
}

fn render_input_lines(
    input: &str,
    cursor: usize,
    text_style: Style,
    theme: &Theme,
    placeholder: &str,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    // Block caret in the theme cursor color, sitting on the page background.
    let caret_style = Style::default()
        .fg(theme.get("cursor"))
        .bg(theme.get("bg"));
    let content_width = input_content_width(width);
    let logical_lines: Vec<&str> = if input.is_empty() {
        vec![""]
    } else {
        input.split('\n').collect()
    };

    let mut all_lines = Vec::new();
    let mut cursor_visual_row = 0usize;
    let mut char_at = 0usize;

    for (row, line) in logical_lines.iter().enumerate() {
        let line_char_len = line.chars().count();
        let caret_on_line = cursor >= char_at && cursor <= char_at + line_char_len;
        let cursor_col = caret_on_line.then_some(cursor - char_at);
        let wrapped = if line.is_empty() {
            vec![String::new()]
        } else {
            soft_wrap(line, content_width)
        };

        let mut caret_wrap = None;
        if caret_on_line {
            let col = cursor_col.unwrap_or(0);
            let mut offset = 0usize;
            for (i, chunk) in wrapped.iter().enumerate() {
                let chunk_len = chunk.chars().count();
                if col >= offset && col <= offset + chunk_len {
                    caret_wrap = Some(i);
                    break;
                }
                offset += chunk_len;
            }
            if caret_wrap.is_none() && !wrapped.is_empty() {
                caret_wrap = Some(wrapped.len() - 1);
            }
        }

        for (wrap_i, chunk) in wrapped.iter().enumerate() {
            if caret_wrap == Some(wrap_i) {
                cursor_visual_row = all_lines.len();
            }

            let prefix = if row == 0 && wrap_i == 0 {
                "❯ "
            } else {
                "  "
            };
            let mut spans = vec![Span::styled(prefix.to_string(), theme.accent_bold())];

            let caret_here = caret_wrap == Some(wrap_i);
            if caret_here {
                let chunk_start: usize = wrapped[..wrap_i]
                    .iter()
                    .map(|s| s.chars().count())
                    .sum();
                let col = cursor_col.unwrap_or(0).saturating_sub(chunk_start);
                let chars: Vec<char> = chunk.chars().collect();
                let col = col.min(chars.len());
                if col > 0 {
                    spans.push(Span::styled(
                        chars[..col].iter().collect::<String>(),
                        text_style,
                    ));
                }
                spans.push(Span::styled("█".to_string(), caret_style));
                if col < chars.len() {
                    spans.push(Span::styled(
                        chars[col + 1..].iter().collect::<String>(),
                        text_style,
                    ));
                }
            } else if !chunk.is_empty() {
                spans.push(Span::styled(chunk.clone(), text_style));
            }

            if row == 0 && wrap_i == 0 && input.is_empty() && !placeholder.is_empty() {
                spans.push(Span::styled(placeholder.to_string(), theme.dim()));
            }
            all_lines.push(Line::from(spans));
        }
        char_at += line_char_len + 1;
    }

    let visible = visible_rows.max(1);
    let scroll_top = input_scroll_top(all_lines.len(), cursor_visual_row, visible);
    let end = (scroll_top + visible).min(all_lines.len());
    all_lines[scroll_top..end].to_vec()
}

fn line_summary(line: &str, max: usize) -> String {
    let line = line.trim();
    if line.is_empty() {
        return String::new();
    }
    let mut s: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        s.push('…');
    }
    s
}

/// A full-width line with a background color, built from styled spans.
fn bg_spans_line(
    theme: &Theme,
    bg_key: &str,
    spans: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let bg = theme.get(bg_key);
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = spans
        .into_iter()
        .map(|s| {
            used += UnicodeWidthStr::width(s.content.as_ref());
            Span::styled(s.content.into_owned(), s.style.bg(bg))
        })
        .collect();
    if used < width {
        out.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(out)
}


fn wrap_plain(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    soft_wrap(text, width.max(1))
        .into_iter()
        .map(|s| Line::from(Span::styled(s, style)))
        .collect()
}

/// Highlight tool card body: file tools by path, JSON args + body after `---`, else plain.
fn highlight_tool_detail(
    name: &str,
    summary: &str,
    detail: &str,
    theme: &Theme,
    fallback: Style,
) -> Vec<Vec<Span<'static>>> {
    let path = match name {
        "read" | "write" | "edit" if !summary.is_empty() && summary != "…" => Some(summary),
        _ => None,
    };

    if let Some((args, body)) = detail.split_once("\n---\n") {
        let mut lines = highlight::highlight_lines(args, Some("json"), None, theme, fallback);
        lines.push(vec![Span::styled("---".to_string(), theme.dim())]);
        let body_lang = if path.is_some() { None } else { Some("json") };
        lines.extend(highlight::highlight_lines(
            body,
            body_lang,
            path,
            theme,
            fallback,
        ));
        return lines;
    }

    match name {
        "read" | "write" | "edit" => {
            highlight::highlight_lines(detail, None, path, theme, fallback)
        }
        // Bash stdout stays plain — coloring ls/etc would fight the existing look.
        "bash" => highlight::highlight_lines(detail, None, None, theme, fallback),
        _ => highlight::highlight_lines(detail, Some("json"), None, theme, fallback),
    }
}

fn soft_wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_inclusive(' ') {
        let ww = UnicodeWidthStr::width(word);
        if current_w > 0 && current_w + ww > width {
            rows.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if ww > width {
            for ch in word.chars() {
                let cw = UnicodeWidthStr::width(ch.to_string().as_str());
                if current_w + cw > width && current_w > 0 {
                    rows.push(std::mem::take(&mut current));
                    current_w = 0;
                }
                current.push(ch);
                current_w += cw;
            }
        } else {
            current.push_str(word);
            current_w += ww;
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

fn truncate_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Find chat index of a tool by id.
pub fn find_tool_index(chat: &[ChatItem], id: &str) -> Option<usize> {
    chat.iter()
        .position(|c| matches!(c, ChatItem::Tool { id: tid, .. } if tid == id))
}

/// Truncate tool args to a short summary.
pub fn tool_args_summary(name: &str, args: &serde_json::Value) -> String {
    let pick = |keys: &[&str]| {
        for k in keys {
            if let Some(s) = args.get(*k).and_then(|v| v.as_str()) {
                let mut t: String = s.chars().take(48).collect();
                if s.chars().count() > 48 {
                    t.push('…');
                }
                return Some(t);
            }
        }
        None
    };
    match name {
        "read" | "write" | "edit" => pick(&["path"]).unwrap_or_else(|| "…".into()),
        "bash" => pick(&["command"]).unwrap_or_else(|| "…".into()),
        _ => {
            let s = args.to_string();
            let mut t: String = s.chars().take(40).collect();
            if s.chars().count() > 40 {
                t.push('…');
            }
            t
        }
    }
}

fn user_message_text(u: &loop_ai::UserMessage) -> String {
    use loop_ai::{UserContent, UserMessageContent};
    match &u.content {
        UserMessageContent::Text(s) => s.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn tool_result_text(tr: &loop_ai::ToolResultMessage) -> String {
    use loop_ai::ToolResultContent;
    tr.content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rebuild the TUI transcript from persisted session messages (for `--resume`).
pub fn chat_items_from_agent_messages(
    messages: &[loop_agent::types::AgentMessage],
) -> Vec<ChatItem> {
    use loop_ai::{AssistantContent, Message};
    use loop_agent::types::{AgentMessage, CustomAgentMessage};

    let mut chat = Vec::new();
    for msg in messages {
        match msg {
            AgentMessage::Llm(Message::User(u)) => {
                let text = user_message_text(u);
                if !text.is_empty() {
                    chat.push(ChatItem::User { text });
                }
            }
            AgentMessage::Llm(Message::Assistant(a)) => {
                for block in &a.content {
                    match block {
                        AssistantContent::Thinking(t) => {
                            if !t.thinking.is_empty() {
                                chat.push(ChatItem::Thinking {
                                    text: t.thinking.clone(),
                                    done: true,
                                });
                            }
                        }
                        AssistantContent::Text(t) => {
                            if !t.text.is_empty() {
                                chat.push(ChatItem::Assistant {
                                    text: t.text.clone(),
                                });
                            }
                        }
                        AssistantContent::ToolCall(tc) => {
                            let detail =
                                serde_json::to_string_pretty(&tc.arguments).unwrap_or_default();
                            let summary = tool_args_summary(&tc.name, &tc.arguments);
                            chat.push(ChatItem::Tool {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                summary,
                                detail,
                                status: CardStatus::Pending,
                            });
                        }
                    }
                }
                if let Some(err) = &a.error_message {
                    if !err.is_empty() {
                        chat.push(ChatItem::System {
                            text: format!("error: {err}"),
                        });
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult(tr)) => {
                let result_text = tool_result_text(tr);
                let st = if tr.is_error {
                    CardStatus::Error
                } else {
                    CardStatus::Success
                };
                if let Some(idx) = find_tool_index(&chat, &tr.tool_call_id) {
                    if let Some(ChatItem::Tool {
                        name,
                        detail,
                        status,
                        summary,
                        ..
                    }) = chat.get_mut(idx)
                    {
                        *name = tr.tool_name.clone();
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
                            *summary = if tr.is_error {
                                "error".into()
                            } else {
                                "done".into()
                            };
                        }
                    }
                } else {
                    chat.push(ChatItem::Tool {
                        id: tr.tool_call_id.clone(),
                        name: tr.tool_name.clone(),
                        summary: if tr.is_error {
                            "error".into()
                        } else {
                            "done".into()
                        },
                        detail: result_text,
                        status: st,
                    });
                }
            }
            AgentMessage::Custom(CustomAgentMessage::CompactionSummary { summary, .. }) => {
                chat.push(ChatItem::System {
                    text: format!("compaction summary\n{summary}"),
                });
            }
            AgentMessage::Custom(CustomAgentMessage::BranchSummary { summary, .. }) => {
                chat.push(ChatItem::System {
                    text: format!("branch summary\n{summary}"),
                });
            }
            AgentMessage::Custom(CustomAgentMessage::BashExecution {
                command,
                output,
                exit_code,
                ..
            }) => {
                let mut summary: String = command.chars().take(48).collect();
                if command.chars().count() > 48 {
                    summary.push('…');
                }
                let status = match exit_code {
                    Some(0) | None => CardStatus::Success,
                    Some(_) => CardStatus::Error,
                };
                chat.push(ChatItem::Tool {
                    id: format!("bash-exec-{}", chat.len()),
                    name: "bash".into(),
                    summary,
                    detail: format!("$ {command}\n{output}"),
                    status,
                });
            }
            AgentMessage::Custom(CustomAgentMessage::Custom {
                custom_type,
                content,
                ..
            }) => {
                chat.push(ChatItem::System {
                    text: format!("{custom_type}: {content}"),
                });
            }
        }
    }
    chat
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_agent::types::AgentMessage;
    use loop_ai::{
        AssistantContent, AssistantMessage, Message, StopReason, TextContent, ThinkingContent,
        ToolCall, ToolResultContent, ToolResultMessage, Usage, UserMessage, UserMessageContent,
    };

    #[test]
    fn resume_transcript_reconstructs_user_assistant_tools() {
        let messages = vec![
            AgentMessage::user_text("list files"),
            AgentMessage::assistant(AssistantMessage {
                content: vec![
                    AssistantContent::Thinking(ThinkingContent {
                        thinking: "I should use bash".into(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                    AssistantContent::Text(TextContent {
                        text: "Running ls".into(),
                        text_signature: None,
                    }),
                    AssistantContent::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                        thought_signature: None,
                    }),
                ],
                api: "openai-completions".into(),
                provider: "test".into(),
                model: "m".into(),
                response_model: None,
                response_id: None,
                usage: Usage::empty(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: None,
                timestamp: 1,
            }),
            AgentMessage::tool_result(ToolResultMessage {
                tool_call_id: "call_1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "a.rs\nb.rs".into(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 2,
            }),
            AgentMessage::Llm(Message::User(UserMessage {
                content: UserMessageContent::Text("thanks".into()),
                timestamp: 3,
            })),
        ];

        let items = chat_items_from_agent_messages(&messages);
        assert!(matches!(&items[0], ChatItem::User { text } if text == "list files"));
        assert!(matches!(&items[1], ChatItem::Thinking { text, done: true } if text == "I should use bash"));
        assert!(matches!(&items[2], ChatItem::Assistant { text } if text == "Running ls"));
        assert!(matches!(
            &items[3],
            ChatItem::Tool {
                id,
                name,
                status: CardStatus::Success,
                detail,
                ..
            } if id == "call_1" && name == "bash" && detail.contains("a.rs")
        ));
        assert!(matches!(&items[4], ChatItem::User { text } if text == "thanks"));
    }

    #[test]
    fn compact_tokens_uses_commas_then_m_b() {
        assert_eq!(format_compact_tokens(0), "0");
        assert_eq!(format_compact_tokens(999), "999");
        assert_eq!(format_compact_tokens(1_000), "1,000");
        assert_eq!(format_compact_tokens(12_345), "12,345");
        assert_eq!(format_compact_tokens(999_999), "999,999");
        assert_eq!(format_compact_tokens(1_000_000), "1M");
        assert_eq!(format_compact_tokens(1_250_000), "1.3M");
        assert_eq!(format_compact_tokens(1_000_000_000), "1B");
        assert_eq!(format_compact_tokens(2_500_000_000), "2.5B");
    }

    #[test]
    fn usage_line_shows_total_percent_and_window() {
        assert_eq!(
            format_token_usage_line(12_345, Some(29_440), 128_000),
            "12,345 · 23% · 29,440/128,000"
        );
        assert_eq!(
            format_token_usage_line(500, Some(500), 128_000),
            "500 · 0.4% · 500/128,000"
        );
        assert_eq!(
            format_token_usage_line(1_000, None, 128_000),
            "1,000 · —/128,000"
        );
        assert_eq!(format_token_usage_line(42, None, 0), "42");
    }

    #[test]
    fn input_visual_lines_wrap_and_count() {
        let long = "word ".repeat(20);
        assert_eq!(count_input_visual_lines("", 80), 1);
        assert_eq!(count_input_visual_lines("one\ntwo", 80), 2);
        assert!(count_input_visual_lines(&long, 20) > 1);
    }

    #[test]
    fn input_scroll_keeps_caret_visible() {
        assert_eq!(input_scroll_top(10, 0, 4), 0);
        assert_eq!(input_scroll_top(10, 3, 4), 0);
        assert_eq!(input_scroll_top(10, 5, 4), 2);
        assert_eq!(input_scroll_top(10, 9, 4), 6);
    }

    #[test]
    fn input_render_scrolls_long_multiline() {
        let theme = Theme::dark();
        let text = (0..12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cursor = text.chars().count();
        let lines = render_input_lines(
            &text,
            cursor,
            theme.style("text"),
            &theme,
            "",
            80,
            4,
        );
        assert_eq!(lines.len(), 4);
        assert!(lines.last().unwrap().to_string().contains("line 11"));
    }

    #[test]
    fn input_render_caret_at_end_of_wrapped_line() {
        let theme = Theme::dark();
        let text = "word ".repeat(30);
        let cursor = text.chars().count();
        let lines = render_input_lines(
            &text,
            cursor,
            theme.style("text"),
            &theme,
            "",
            20,
            4,
        );
        assert!(!lines.is_empty());
    }
}
