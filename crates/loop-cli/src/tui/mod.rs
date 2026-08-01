//! Ratatui widgets: scrollable chat, tool/thinking cards, editor chrome, spinner.

pub mod markdown;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Theme;

/// Tool / thinking card status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    /// Still running / streaming.
    Pending,
    /// Finished ok.
    Success,
    /// Failed.
    Error,
}

/// A chat transcript item.
#[derive(Debug, Clone)]
pub enum ChatItem {
    /// User message.
    User {
        /// Message text.
        text: String,
    },
    /// Assistant markdown/text (streams).
    Assistant {
        /// Accumulated text.
        text: String,
    },
    /// Reasoning / thinking block.
    Thinking {
        /// Full thinking text.
        text: String,
        /// Whether the block finished streaming.
        done: bool,
    },
    /// Tool execution card (keyed by tool_call_id).
    Tool {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
        /// One-line summary (always shown).
        summary: String,
        /// Detail body (args / result) shown when expanded.
        detail: String,
        /// Status.
        status: CardStatus,
    },
    /// Dim system notice.
    System {
        /// Notice text.
        text: String,
    },
}

/// Scroll / follow-end state (pi ScrollView semantics).
#[derive(Debug, Clone)]
pub struct ScrollState {
    /// When true, viewport pins to bottom as content grows.
    pub follow_end: bool,
    /// Absolute scroll offset from top (used when not following).
    pub scroll_top: u16,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            follow_end: true,
            scroll_top: 0,
        }
    }
}

impl ScrollState {
    /// Scroll up (breaks follow).
    pub fn scroll_up(&mut self, n: u16, content_h: u16, view_h: u16) {
        self.sync(content_h, view_h);
        self.follow_end = false;
        self.scroll_top = self.scroll_top.saturating_sub(n);
    }

    /// Scroll down; re-enable follow at bottom.
    pub fn scroll_down(&mut self, n: u16, content_h: u16, view_h: u16) {
        self.sync(content_h, view_h);
        let max = content_h.saturating_sub(view_h);
        self.scroll_top = (self.scroll_top.saturating_add(n)).min(max);
        if self.scroll_top >= max {
            self.follow_end = true;
        }
    }

    /// Recompute offset for current sizes.
    pub fn sync(&mut self, content_h: u16, view_h: u16) {
        let max = content_h.saturating_sub(view_h);
        if self.follow_end {
            self.scroll_top = max;
        } else {
            self.scroll_top = self.scroll_top.min(max);
        }
    }

    /// Effective scroll for Paragraph.
    pub fn offset(&self, content_h: u16, view_h: u16) -> u16 {
        let max = content_h.saturating_sub(view_h);
        if self.follow_end {
            max
        } else {
            self.scroll_top.min(max)
        }
    }
}

/// Braille spinner frames (pi Loader).
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draw options for one frame.
pub struct DrawOpts<'a> {
    /// Theme.
    pub theme: &'a Theme,
    /// Header right of brand.
    pub header: &'a str,
    /// Chat items.
    pub chat: &'a [ChatItem],
    /// Editor buffer.
    pub input: &'a str,
    /// Cursor column in last line (byte-agnostic: char count in last line).
    pub cursor_col: usize,
    /// Status / working line (without spinner glyph).
    pub status: &'a str,
    /// Whether agent is working (shows spinner animation).
    pub working: bool,
    /// Spinner frame index.
    pub spinner_frame: usize,
    /// Slash autocomplete hints.
    pub autocomplete: &'a [String],
    /// Scroll state (updated for content height).
    pub scroll: &'a mut ScrollState,
    /// Global tool/thinking detail expand (ctrl+o).
    pub expanded: bool,
    /// Hide thinking bodies (ctrl+t) — show "Thinking…" only.
    pub hide_thinking: bool,
    /// Thinking level label for editor border color.
    pub thinking_level: &'a str,
}

/// Build display lines for the transcript.
pub fn build_chat_lines(
    chat: &[ChatItem],
    theme: &Theme,
    expanded: bool,
    hide_thinking: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let w = width.max(8) as usize;

    for item in chat {
        match item {
            ChatItem::User { text } => {
                lines.push(padded_bg_line(
                    theme,
                    "userMessageBg",
                    "userMessageText",
                    " you ",
                    true,
                    w,
                ));
                for l in text.lines() {
                    lines.push(padded_bg_line(
                        theme,
                        "userMessageBg",
                        "userMessageText",
                        &format!(" {l} "),
                        false,
                        w,
                    ));
                }
                lines.push(Line::from(""));
            }
            ChatItem::Assistant { text } => {
                if text.is_empty() {
                    continue;
                }
                lines.push(Line::from(Span::styled(
                    " assistant ",
                    theme.accent().add_modifier(Modifier::BOLD),
                )));
                lines.extend(markdown::render_lines(text, theme));
                lines.push(Line::from(""));
            }
            ChatItem::Thinking { text, done } => {
                let label = if *done { "thinking" } else { "thinking…" };
                let summary = first_line_summary(text, 72);
                if hide_thinking {
                    lines.push(Line::from(Span::styled(
                        format!("  {label}"),
                        theme
                            .style("thinkingText")
                            .add_modifier(Modifier::ITALIC),
                    )));
                    lines.push(Line::from(""));
                    continue;
                }
                let bg = "toolPendingBg";
                let header = if summary.is_empty() {
                    format!(" ✦ {label} ")
                } else {
                    format!(" ✦ {label} · {summary} ")
                };
                lines.push(padded_bg_line(
                    theme,
                    bg,
                    "thinkingText",
                    &header,
                    true,
                    w,
                ));
                if expanded && !text.is_empty() {
                    for l in text.lines() {
                        lines.push(padded_bg_line(
                            theme,
                            bg,
                            "thinkingText",
                            &format!("   {l} "),
                            false,
                            w,
                        ));
                    }
                    lines.push(padded_bg_line(
                        theme,
                        bg,
                        "dim",
                        "   ctrl+o collapse ",
                        false,
                        w,
                    ));
                } else if !text.is_empty() {
                    lines.push(padded_bg_line(
                        theme,
                        bg,
                        "dim",
                        "   ctrl+o expand ",
                        false,
                        w,
                    ));
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
                let (bg, fg, mark) = match status {
                    CardStatus::Pending => ("toolPendingBg", "warning", "◉"),
                    CardStatus::Success => ("toolSuccessBg", "success", "✓"),
                    CardStatus::Error => ("toolErrorBg", "error", "✗"),
                };
                let status_word = match status {
                    CardStatus::Pending => "running",
                    CardStatus::Success => "ok",
                    CardStatus::Error => "error",
                };
                let head = if summary.is_empty() {
                    format!(" {mark} {name} · {status_word} ")
                } else {
                    format!(" {mark} {name} · {summary} ")
                };
                lines.push(padded_bg_line(theme, bg, fg, &head, true, w));
                if expanded && !detail.is_empty() {
                    for l in detail.lines().take(40) {
                        lines.push(padded_bg_line(
                            theme,
                            bg,
                            "toolOutput",
                            &format!("   {l} "),
                            false,
                            w,
                        ));
                    }
                    if detail.lines().count() > 40 {
                        lines.push(padded_bg_line(
                            theme,
                            bg,
                            "dim",
                            "   … truncated · ctrl+o ",
                            false,
                            w,
                        ));
                    } else {
                        lines.push(padded_bg_line(
                            theme,
                            bg,
                            "dim",
                            "   ctrl+o collapse ",
                            false,
                            w,
                        ));
                    }
                } else {
                    lines.push(padded_bg_line(
                        theme,
                        bg,
                        "dim",
                        "   ctrl+o expand details ",
                        false,
                        w,
                    ));
                }
                lines.push(Line::from(""));
            }
            ChatItem::System { text } => {
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        theme.dim(),
                    )));
                }
                lines.push(Line::from(""));
            }
        }
    }
    lines
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
    // Pad to width so background fills the row.
    let visual = unicode_width::UnicodeWidthStr::width(content.as_str());
    if visual < width {
        content.push_str(&" ".repeat(width - visual));
    }
    Line::from(Span::styled(content, style))
}

/// Draw the full interactive frame.
pub fn draw(frame: &mut Frame, opts: DrawOpts<'_>) {
    let area = frame.area();
    let input_lines = opts.input.lines().count().max(1) as u16;
    let editor_h = (input_lines + 2).clamp(3, (area.height / 3).max(3));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // brand header
            Constraint::Min(4),             // chat
            Constraint::Length(1),          // working / status
            Constraint::Length(editor_h),   // editor
            Constraint::Length(1),          // footer
        ])
        .split(area);

    // Header
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" loop ", opts.theme.accent_bold()),
            Span::styled(opts.header.to_string(), opts.theme.muted()),
        ])),
        chunks[0],
    );

    // Chat (measure then scroll)
    let chat_area = chunks[1];
    let view_h = chat_area.height.saturating_sub(1); // top border
    let lines = build_chat_lines(
        opts.chat,
        opts.theme,
        opts.expanded,
        opts.hide_thinking,
        chat_area.width,
    );
    let content_h = lines.len() as u16;
    opts.scroll.sync(content_h, view_h);
    let offset = opts.scroll.offset(content_h, view_h);

    let chat_widget = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(opts.theme.style("borderMuted"))
                .title(if opts.scroll.follow_end {
                    Span::styled(" chat ", opts.theme.dim())
                } else {
                    Span::styled(" chat ↑ scroll ", opts.theme.style("warning"))
                }),
        );
    frame.render_widget(chat_widget, chat_area);

    // Working / status dock
    let status_spans = if opts.working {
        let frame_ch = SPINNER_FRAMES[opts.spinner_frame % SPINNER_FRAMES.len()];
        vec![
            Span::styled(format!(" {frame_ch} "), opts.theme.accent()),
            Span::styled(opts.status.to_string(), opts.theme.muted()),
        ]
    } else {
        vec![Span::styled(
            format!("  {}", opts.status),
            opts.theme.dim(),
        )]
    };
    frame.render_widget(Paragraph::new(Line::from(status_spans)), chunks[2]);

    // Editor — pi-style top/bottom rules, border tinted by thinking level
    let border_key = thinking_border_key(opts.thinking_level);
    let mut title = if opts.autocomplete.is_empty() {
        " message · / commands · enter send · shift+enter newline ".to_string()
    } else {
        format!(" {} ", opts.autocomplete.iter().take(6).cloned().collect::<Vec<_>>().join("  "))
    };
    if opts.input.starts_with('!') {
        title = " bash mode ".into();
    }
    let editor_block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(opts.theme.style(border_key))
        .title(Span::styled(title, opts.theme.dim()));
    let inner = editor_block.inner(chunks[3]);
    frame.render_widget(editor_block, chunks[3]);

    // Show input with a reverse-video cursor on the last line
    let display = render_input_with_cursor(opts.input, opts.cursor_col, opts.theme);
    frame.render_widget(
        Paragraph::new(display)
            .style(opts.theme.style("text"))
            .wrap(Wrap { trim: false }),
        inner,
    );

    // Footer
    frame.render_widget(
        Paragraph::new(Span::styled(
            " ctrl+o expand · ctrl+t thinking · ctrl+l model · shift+tab effort · pgup/pgdn scroll ",
            opts.theme.dim(),
        )),
        chunks[4],
    );
}

fn thinking_border_key(level: &str) -> &'static str {
    match level.to_lowercase().as_str() {
        "minimal" => "thinkingMinimal",
        "low" => "thinkingLow",
        "medium" => "thinkingMedium",
        "high" => "thinkingHigh",
        "xhigh" => "thinkingXhigh",
        "max" => "thinkingXhigh",
        _ => "borderMuted",
    }
}

fn render_input_with_cursor(input: &str, cursor_col: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let lines: Vec<&str> = if input.is_empty() {
        vec![""]
    } else {
        input.split('\n').collect()
    };
    let last = lines.len() - 1;
    for (i, line) in lines.iter().enumerate() {
        if i != last {
            out.push(Line::from(Span::styled(
                (*line).to_string(),
                theme.style("text"),
            )));
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let col = cursor_col.min(chars.len());
        let mut spans = Vec::new();
        if col > 0 {
            let before: String = chars[..col].iter().collect();
            spans.push(Span::styled(before, theme.style("text")));
        }
        let cursor_ch = chars.get(col).copied().unwrap_or(' ');
        spans.push(Span::styled(
            cursor_ch.to_string(),
            Style::default()
                .fg(theme.get("text"))
                .bg(theme.get("accent"))
                .add_modifier(Modifier::BOLD),
        ));
        if col < chars.len() {
            let after: String = chars[col + 1..].iter().collect();
            if !after.is_empty() {
                spans.push(Span::styled(after, theme.style("text")));
            }
        } else if cursor_ch == ' ' {
            // already showed space cursor at EOL
        }
        out.push(Line::from(spans));
    }
    out
}

/// Find chat index of a tool by id.
pub fn find_tool_index(chat: &[ChatItem], id: &str) -> Option<usize> {
    chat.iter().position(|c| matches!(c, ChatItem::Tool { id: tid, .. } if tid == id))
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

/// Helper: centered rect.
#[allow(dead_code)]
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}
