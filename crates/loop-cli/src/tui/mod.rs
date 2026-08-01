//! Pi-style inline UI: transcript in terminal scrollback, footer redrawn in place.

pub mod editor;
pub mod markdown;

pub use editor::InputBuffer;

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
    Assistant { text: String },
    Thinking { text: String, done: bool },
    Tool {
        id: String,
        name: String,
        summary: String,
        detail: String,
        status: CardStatus,
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
    pub expanded: bool,
    pub hide_thinking: bool,
    pub setup_mode: bool,
    pub mask_input: bool,
    /// Left status (e.g. `~/loop (main)`).
    pub path_line: &'a str,
    /// Right status (e.g. `soket/qwen3-30b · medium`).
    pub model_line: &'a str,
}

/// Whether a transcript item is finished and safe to flush into scrollback.
pub fn item_is_committed(
    item: &ChatItem,
    index: usize,
    streaming_assistant: Option<usize>,
    streaming_thinking: Option<usize>,
) -> bool {
    match item {
        ChatItem::User { .. } | ChatItem::System { .. } => true,
        ChatItem::Assistant { .. } => streaming_assistant != Some(index),
        ChatItem::Thinking { done, .. } => *done && streaming_thinking != Some(index),
        ChatItem::Tool { status, .. } => !matches!(status, CardStatus::Pending),
    }
}

/// Pi-style welcome lines for `insert_before`.
pub fn welcome_lines(theme: &Theme, version: &str, model_label: &str, endpoint: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            format!("loop v{version}"),
            theme.style("text").add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o details"
                .to_string(),
            theme.dim(),
        )),
        Line::from(Span::styled(
            format!("{model_label} · {endpoint}"),
            theme.muted(),
        )),
        Line::from(""),
    ]
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
            // Encapsulated band — full-width background like Pi.
            if text.is_empty() {
                lines.push(padded_bg_line(theme, "userMessageBg", "userMessageText", " ", false, w));
            } else {
                for l in text.lines() {
                    for part in soft_wrap(l, w.saturating_sub(2).max(1)) {
                        lines.push(padded_bg_line(
                            theme,
                            "userMessageBg",
                            "userMessageText",
                            &format!(" {part} "),
                            false,
                            w,
                        ));
                    }
                }
            }
            lines.push(Line::from(""));
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
            let label = if *done { "thinking" } else { "thinking…" };
            let bg = "toolPendingBg";
            if hide_thinking {
                lines.push(padded_bg_line(
                    theme,
                    bg,
                    "thinkingText",
                    &format!("  {label} "),
                    false,
                    w,
                ));
            } else {
                let summary = first_line_summary(text, 72);
                let head = if summary.is_empty() {
                    format!("  ✦ {label} ")
                } else {
                    format!("  ✦ {label} · {summary} ")
                };
                lines.push(padded_bg_line(theme, bg, "thinkingText", &head, true, w));
                if expanded {
                    for l in text.lines().take(20) {
                        lines.push(padded_bg_line(
                            theme,
                            bg,
                            "thinkingText",
                            &format!("    {l} "),
                            false,
                            w,
                        ));
                    }
                }
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
            let (bg, title_fg) = match status {
                CardStatus::Pending => ("toolPendingBg", "warning"),
                CardStatus::Success => ("toolSuccessBg", "toolTitle"),
                CardStatus::Error => ("toolErrorBg", "error"),
            };
            // Summary row only by default — `$ read · README.md`
            let head = if name == "bash" && !summary.is_empty() {
                format!(" $ {summary} ")
            } else if summary.is_empty() {
                format!(" $ {name} ")
            } else {
                format!(" $ {name} · {summary} ")
            };
            lines.push(padded_bg_line(theme, bg, title_fg, &head, true, w));

            if matches!(status, CardStatus::Pending) && detail.is_empty() {
                lines.push(padded_bg_line(theme, bg, "dim", " running… ", false, w));
            } else if expanded && !detail.is_empty() {
                for l in detail.lines().take(48) {
                    lines.push(padded_bg_line(
                        theme,
                        bg,
                        "toolOutput",
                        &format!(" {l} "),
                        false,
                        w,
                    ));
                }
                let n = detail.lines().count();
                let hint = if n > 48 {
                    " … truncated · ctrl+o to collapse "
                } else {
                    " ctrl+o to collapse "
                };
                lines.push(padded_bg_line(theme, bg, "dim", hint, false, w));
            } else if !detail.is_empty() {
                lines.push(padded_bg_line(
                    theme,
                    bg,
                    "dim",
                    " ctrl+o to expand ",
                    false,
                    w,
                ));
            }
            lines.push(Line::from(""));
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

/// Draw lines into a buffer (used by `insert_before`).
pub fn render_lines_to_buffer(lines: &[Line<'static>], buf: &mut Buffer) {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(buf.area, buf);
}

/// Draw the inline footer (live stream + input + picker + status).
pub fn draw_footer(frame: &mut Frame, opts: FooterOpts<'_>) {
    let area = frame.area();
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

    let input_body_lines = opts.input.split('\n').count().max(1) as u16;
    let input_h = input_body_lines + 2; // top + bottom rules
    let picker_h = picker_height(opts.picker);
    let status_h = 2u16;
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
        frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), chunks[0]);
    }

    draw_input(frame, chunks[1], &opts);
    draw_picker(frame, chunks[2], opts.theme, opts.picker);
    draw_status(frame, chunks[3], &opts);
}

fn draw_input(frame: &mut Frame, area: Rect, opts: &FooterOpts<'_>) {
    let rule = Style::default().fg(opts.theme.get("borderAccent"));
    // No fill behind typed text — only the top/bottom rules frame the editor.
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
    frame.render_widget(
        Paragraph::new(Span::styled(rule_line.clone(), rule)),
        chunks[0],
    );
    frame.render_widget(Paragraph::new(Span::styled(rule_line, rule)), chunks[2]);

    let display = if opts.mask_input {
        "•".repeat(opts.input.chars().count())
    } else if opts.setup_mode && opts.input.is_empty() {
        String::new()
    } else {
        opts.input.to_string()
    };

    let lines = render_input_lines(&display, opts.cursor, text_style, opts.theme, area.width as usize);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);
}

fn draw_picker(frame: &mut Frame, area: Rect, theme: &Theme, picker: &PickerView) {
    if area.height == 0 {
        return;
    }
    let lines = match picker {
        PickerView::None => Vec::new(),
        PickerView::Setup { provider } => vec![
            Line::from(Span::styled(
                format!("  paste API key for {provider} · enter save · esc quit"),
                theme.muted(),
            )),
        ],
        PickerView::Commands { rows, selected } => picker_lines(rows, *selected, theme, false),
        PickerView::Models { rows, selected, hint } => {
            let mut out = vec![Line::from(Span::styled(hint.clone(), theme.style("warning")))];
            out.extend(picker_lines(rows, *selected, theme, true));
            out
        }
    };
    frame.render_widget(Paragraph::new(lines), area);
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
    for (i, row) in rows.iter().enumerate().take(end).skip(start) {
        let active = i == selected;
        let arrow = if active { "→" } else { " " };
        let label_style = if active {
            theme.style("text").add_modifier(Modifier::BOLD)
        } else {
            theme.style("text")
        };
        let mut spans = vec![
            Span::styled(format!(" {arrow} "), theme.accent()),
            Span::styled(format!("{:<16}", truncate_width(&row.label, 16)), label_style),
            Span::styled(format!(" {}", row.description), theme.muted()),
        ];
        if let Some(m) = &row.mark {
            spans.push(Span::styled(format!(" {m}"), theme.style("success")));
        }
        out.push(Line::from(spans));
    }
    if rows.len() > page || start > 0 {
        let page_num = selected / page + 1;
        let pages = rows.len().div_ceil(page).max(1);
        out.push(Line::from(Span::styled(
            format!("  ({page_num}/{pages})"),
            theme.dim(),
        )));
    } else {
        out.push(Line::from(Span::styled(
            format!("  ({}/{})", selected + 1, rows.len()),
            theme.dim(),
        )));
    }
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
    let bottom = if opts.working {
        let spin = SPINNER_FRAMES[opts.spinner_frame % SPINNER_FRAMES.len()];
        Line::from(Span::styled(
            format!(" {spin} {}", opts.status),
            opts.theme.accent(),
        ))
    } else {
        Line::from(Span::styled(
            format!(" {}", opts.status),
            opts.theme.dim(),
        ))
    };
    frame.render_widget(Paragraph::new(top), chunks[0]);
    frame.render_widget(Paragraph::new(bottom), chunks[1]);
}

fn picker_height(picker: &PickerView) -> u16 {
    match picker {
        PickerView::None => 0,
        PickerView::Setup { .. } => 1,
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

fn render_input_lines(
    input: &str,
    cursor: usize,
    text_style: Style,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    // Absolute black block (theme `cursor`, default #000000). White cell bg keeps it
    // visible on dark terminals where a bare black glyph would disappear.
    let caret_style = Style::default()
        .fg(theme.get("cursor"))
        .bg(Color::Rgb(255, 255, 255));
    let lines: Vec<&str> = if input.is_empty() {
        vec![""]
    } else {
        input.split('\n').collect()
    };
    let mut char_at = 0usize;
    let mut out = Vec::new();
    for line in &lines {
        let chars: Vec<char> = line.chars().collect();
        let line_len = chars.len();
        let caret_here = cursor >= char_at && cursor <= char_at + line_len;
        let mut spans = Vec::new();
        if caret_here {
            let col = cursor - char_at;
            if col > 0 {
                spans.push(Span::styled(chars[..col].iter().collect::<String>(), text_style));
            }
            // Block glyph + bg — reliable when the buffer is empty.
            spans.push(Span::styled("█".to_string(), caret_style));
            if col < line_len {
                let after: String = chars[col + 1..].iter().collect();
                if !after.is_empty() {
                    spans.push(Span::styled(after, text_style));
                }
            }
        } else if !line.is_empty() {
            spans.push(Span::styled((*line).to_string(), text_style));
        }
        if spans.is_empty() {
            spans.push(Span::styled("█".to_string(), caret_style));
        }
        out.push(Line::from(spans));
        let _ = width;
        char_at += line_len + 1;
    }
    out
}

fn padded_bg_line(
    theme: &Theme,
    bg_key: &str,
    fg_key: &str,
    text: &str,
    bold: bool,
    width: usize,
) -> Line<'static> {
    let mut style = Style::default()
        .fg(theme.get(fg_key))
        .bg(theme.get(bg_key));
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    let mut content = text.to_string();
    let visual = UnicodeWidthStr::width(content.as_str());
    if visual < width {
        content.push_str(&" ".repeat(width - visual));
    } else if visual > width {
        content = truncate_width(&content, width);
    }
    Line::from(Span::styled(content, style))
}

fn first_line_summary(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return String::new();
    }
    let mut s: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        s.push('…');
    }
    s
}

fn wrap_plain(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    soft_wrap(text, width.max(1))
        .into_iter()
        .map(|s| Line::from(Span::styled(s, style)))
        .collect()
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
