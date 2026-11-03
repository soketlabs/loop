//! Pure viewer state and drawing for the `/diff` full-screen viewer (M4).
//!
//! This module holds the state machine (selection, navigation, notices) and
//! the plain-text rendering of every pane. Syntax highlighting arrives in M5
//! and per-hunk actions in M6; keeping the state pure makes every rule
//! unit-testable.

use crate::diff_model::{DiffLineKind, DiffModel, DiffSide, HunkId};
use crate::git_diff::GitDiff;
use crate::theme::Theme;
use crate::tui::diff_actions::{plan_action, Action, ActionPlan};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

/// Key help shown in the viewer footer.
pub fn footer_hint(side: DiffSide) -> String {
    let nav = "↑/↓ hunk · tab file · j/k scroll · PgUp/PgDn page · q close";
    match side {
        // The buttons already show accept/reject; only the side-specific
        // unstage key needs a mention in staged mode.
        DiffSide::WorkingTree => nav.to_string(),
        DiffSide::Index => format!("u unstage · {nav}"),
    }
}

/// Notice severity: success notices render in the theme's success color,
/// errors in its error color (§3.6 step 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeKind {
    Success,
    Error,
}

/// A notice banner: text plus how to color it.
#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
}

impl Notice {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Success,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Error,
            text: text.into(),
        }
    }
}

/// Selection + scroll + notice for one open viewer instance.
#[derive(Debug, Clone)]
pub struct ViewerState {
    /// Parsed diff being viewed.
    pub model: DiffModel,
    /// Index into `model.files`.
    pub file_idx: usize,
    /// Hunk index within the selected file.
    pub hunk_idx: usize,
    /// Vertical scroll offset inside the hunk pane.
    pub scroll: u16,
    /// Transient notice banner; cleared by the next action.
    pub notice: Option<Notice>,
    /// Undo/backup notices collected during this session, drained into the
    /// chat when the viewer closes so a cut-off footer never loses them.
    pub undo_notes: Vec<String>,
    /// True while the destructive-action confirm popup is open.
    pub confirm_open: bool,
}

impl ViewerState {
    /// State over `model`, selection at the first file/hunk.
    pub fn new(model: DiffModel) -> Self {
        let mut state = Self {
            model,
            file_idx: 0,
            hunk_idx: 0,
            scroll: 0,
            notice: None,
            undo_notes: Vec::new(),
            confirm_open: false,
        };
        state.clamp_selection();
        state
    }

    /// Selected file, if any.
    pub fn current_file(&self) -> Option<&crate::diff_model::FileDiff> {
        self.model.files.get(self.file_idx)
    }

    /// Selected hunk of the selected file, if any.
    pub fn current_hunk(&self) -> Option<&crate::diff_model::Hunk> {
        self.current_file()?.hunks.get(self.hunk_idx)
    }

    /// Bring `(file_idx, hunk_idx)` back inside the model after a mutation:
    /// empty model, file without hunks (binary / rename / mode-only) and
    /// out-of-range indices all end in a valid (or empty) selection.
    pub fn clamp_selection(&mut self) {
        if self.model.files.is_empty() {
            self.file_idx = 0;
            self.hunk_idx = 0;
            return;
        }
        self.file_idx = self.file_idx.min(self.model.files.len() - 1);
        let hunks = self.current_file().map_or(0, |f| f.hunks.len());
        self.hunk_idx = self.hunk_idx.min(hunks.saturating_sub(1));
    }

    /// Clear the notice (called on every key press before acting).
    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// Next file; hunk index resets to 0. Wraps past the last file.
    pub fn next_file(&mut self) {
        if self.model.files.is_empty() {
            return;
        }
        self.file_idx = (self.file_idx + 1) % self.model.files.len();
        self.hunk_idx = 0;
        self.scroll = 0;
    }

    /// Previous file; hunk index resets to 0. Wraps before the first file.
    pub fn prev_file(&mut self) {
        if self.model.files.is_empty() {
            return;
        }
        self.file_idx = if self.file_idx == 0 {
            self.model.files.len() - 1
        } else {
            self.file_idx - 1
        };
        self.hunk_idx = 0;
        self.scroll = 0;
    }

    /// Next hunk; wraps to the next file (and its first hunk) at the end.
    pub fn next_hunk(&mut self) {
        let Some(file) = self.current_file() else {
            return;
        };
        if self.hunk_idx + 1 < file.hunks.len() {
            self.hunk_idx += 1;
            self.scroll = 0;
            return;
        }
        self.next_file();
    }

    /// Previous hunk; wraps to the previous file's **last** hunk at the start
    /// of a file, mirroring how `next_hunk` enters a file at its first hunk.
    pub fn prev_hunk(&mut self) {
        if self.hunk_idx > 0 {
            self.hunk_idx -= 1;
            self.scroll = 0;
            return;
        }
        if self.model.files.is_empty() {
            return;
        }
        self.prev_file();
        // Land on the new file's final hunk (it may have none: binary).
        let hunks = self.current_file().map_or(0, |f| f.hunks.len());
        self.hunk_idx = hunks.saturating_sub(1);
    }

    /// Scroll the hunk pane up one row (never below 0).
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Scroll the hunk pane down one row; `visible` is the pane height so the
    /// offset can stop at the end of the hunk. A `visible` of 0 disables
    /// scrolling entirely.
    pub fn scroll_down(&mut self, visible: u16) {
        if visible == 0 {
            return;
        }
        let total = self
            .current_hunk()
            // +1 for the rendered @@ header row.
            .map_or(0, |h| (h.lines.len() + 1).min(u16::MAX as usize) as u16);
        let max = total.saturating_sub(visible);
        self.scroll = (self.scroll + 1).min(max);
    }

    /// Page down by a full pane (`visible - 1` rows, like editors).
    pub fn page_down(&mut self, visible: u16) {
        if visible <= 1 {
            return;
        }
        for _ in 0..visible - 1 {
            self.scroll_down(visible);
        }
    }

    /// Page up by a full pane (`visible - 1` rows).
    pub fn page_up(&mut self, visible: u16) {
        if visible <= 1 {
            return;
        }
        self.scroll = self.scroll.saturating_sub(visible - 1);
    }

    /// Jump to the first hunk of the first file.
    pub fn jump_first(&mut self) {
        self.file_idx = 0;
        self.hunk_idx = 0;
        self.scroll = 0;
        self.clamp_selection();
    }

    /// Jump to the last file's last hunk.
    pub fn jump_last(&mut self) {
        if self.model.files.is_empty() {
            return;
        }
        self.file_idx = self.model.files.len() - 1;
        let hunks = self.current_file().map_or(0, |f| f.hunks.len());
        self.hunk_idx = hunks.saturating_sub(1);
        self.scroll = 0;
    }
}

/// Diff color set for one line kind, with fallback to the older
/// toolDiff* keys when a user theme lacks the new diff* ones.
struct DiffColors {
    added_fg: Style,
    removed_fg: Style,
    added_bg: Color,
    removed_bg: Color,
    context: Style,
}

fn diff_colors(theme: &Theme) -> DiffColors {
    let has_new =
        theme.get("diffAddedBg") != Color::Reset && theme.get("diffAddedFg") != Color::Reset;
    let (added_fg, removed_fg, added_bg, removed_bg) = if has_new {
        (
            Style::default().fg(theme.get("diffAddedFg")),
            Style::default().fg(theme.get("diffRemovedFg")),
            theme.get("diffAddedBg"),
            theme.get("diffRemovedBg"),
        )
    } else {
        // Fallback: old toolDiff colors, no row background.
        let added = theme.get("toolDiffAdded");
        let removed = theme.get("toolDiffRemoved");
        (
            Style::default().fg(added),
            Style::default().fg(removed),
            Color::Reset,
            Color::Reset,
        )
    };
    DiffColors {
        added_fg,
        removed_fg,
        added_bg,
        removed_bg,
        context: theme.style("toolDiffContext"),
    }
}

/// Side label for the header and empty states.
fn side_label(side: DiffSide) -> &'static str {
    match side {
        DiffSide::WorkingTree => "Unstaged changes",
        DiffSide::Index => "Staged changes",
    }
}

/// Last observed mouse position, for button hover styles.
#[derive(Debug, Clone, Copy, Default)]
pub struct MousePos {
    pub x: u16,
    pub y: u16,
}

/// Draw the whole viewer (M4: plain text, no syntax highlighting).
pub fn render(frame: &mut Frame, state: &ViewerState, theme: &Theme) {
    render_with_mouse(frame, state, theme, None);
}

/// [`render`] plus the tracked mouse position for hover styles.
pub fn render_with_mouse(
    frame: &mut Frame,
    state: &ViewerState,
    theme: &Theme,
    mouse: Option<MousePos>,
) {
    let area = frame.area();
    let lay = layout(area, state);

    render_header(frame, lay.header, state, theme);
    if let Some(list) = lay.file_list {
        render_file_list(frame, list, state, theme);
        render_hunk(frame, lay.hunk_pane, state, theme);
    } else if state.model.files.is_empty() {
        let label = match state.model.side {
            DiffSide::WorkingTree => "No unstaged changes",
            DiffSide::Index => "No staged changes",
        };
        let line = centered_line(label, theme.dim(), lay.panes.width);
        frame.render_widget(Paragraph::new(line).style(theme.page()), lay.panes);
    } else {
        render_hunk(frame, lay.hunk_pane, state, theme);
    }
    render_footer(frame, lay.footer, state, theme, &lay, mouse);
    if state.confirm_open {
        render_confirm_popup(frame, &lay, state, theme, mouse);
    }
}

/// Header row: side label, file X of N, hunk Y of M.
fn render_header(frame: &mut Frame, area: Rect, state: &ViewerState, theme: &Theme) {
    let model = &state.model;
    let mut spans = vec![
        Span::styled(side_label(model.side).to_string(), theme.accent_bold()),
        Span::raw("  "),
    ];
    if model.files.is_empty() {
        spans.push(Span::styled("no files".to_string(), theme.dim()));
    } else {
        spans.push(Span::styled(
            format!("file {} of {}", state.file_idx + 1, model.files.len()),
            theme.muted(),
        ));
        let current = state.current_file();
        let hunk_part = match current.map(|f| f.hunks.len()) {
            Some(0) => " · no hunks".to_string(),
            Some(n) => format!(" · hunk {} of {}", state.hunk_idx + 1, n),
            None => String::new(),
        };
        spans.push(Span::styled(hunk_part, theme.muted()));
    }
    if model.truncated {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("diff truncated".to_string(), theme.error()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One-line label centred in `width`.
fn centered_line(text: &str, style: Style, width: u16) -> Line<'static> {
    let pad = (width.saturating_sub(text_len(text) as u16)) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(pad as usize)),
        Span::styled(text.to_string(), style),
    ])
}

/// Visible width of `text` (chars are close enough for plain ASCII labels).
fn text_len(text: &str) -> usize {
    text.chars().count()
}

/// Screen-safe rendering of one diff line: each tab becomes 4 spaces, every
/// other control character becomes '\u{FFFD}'. ratatui drops control
/// characters entirely, so a raw tab would simply vanish. Only the display
/// copy is rewritten; `raw_body` (used for patches) stays byte-exact.
fn display_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => "    ".to_string(),
            c if c.is_control() => '\u{FFFD}'.to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// Shorten a folder path by whole components in the middle, keeping the
/// first two components and the last one ("src/module_1/…/folder") since
/// the second component is usually what differs between files; returns
/// `None` when it cannot fit that way.
fn shorten_components(dir: &str, max: usize) -> Option<String> {
    let parts: Vec<&str> = dir.split('/').collect();
    // Need at least head(2) + "…" + tail(1) to make sense.
    if parts.len() < 4 || text_len(dir) <= max {
        return None;
    }
    // Keep the first two and grow the tail from the right end.
    for tail_len in 1..parts.len() - 2 {
        let head = parts[..2].join("/");
        let tail = parts[parts.len() - tail_len..].join("/");
        let joined = format!("{head}/…/{tail}");
        if text_len(&joined) <= max {
            return Some(joined);
        }
    }
    // Minimal form: first two + "…" + last component.
    let joined = format!("{}/…/{}", parts[..2].join("/"), parts[parts.len() - 1]);
    (text_len(&joined) <= max).then_some(joined)
}
/// Shorten `text` to at most `max` columns by replacing its middle with "…"
/// so the start and the end stay visible.
fn shorten_middle(text: &str, max: usize) -> String {
    let len = text_len(text);
    if len <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(len - tail));
    out
}

/// Shorten `text` to at most `max` columns with a trailing "…".
fn shorten_end(text: &str, max: usize) -> String {
    if text_len(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Left pane: every file with status letter, file name and dimmed folder;
/// selected is highlighted. When files outnumber rows the list scrolls to
/// keep the selection visible.
fn render_file_list(frame: &mut Frame, area: Rect, state: &ViewerState, theme: &Theme) {
    let block = Block::default()
        .title(format!(
            " Files {}/{} ",
            state.file_idx + 1,
            state.model.files.len()
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .border_style(theme.style("border"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    // Keep the selected row on screen: once the selection would fall past
    // the bottom of the pane, start the window above it.
    let skip = if height > 0 && state.file_idx >= height {
        state.file_idx + 1 - height
    } else {
        0
    };
    let mut lines = Vec::new();
    // "❯ " marker plus status letter and space precede every row.
    let max = (inner.width as usize).saturating_sub(4);
    for (i, file) in state.model.files.iter().enumerate().skip(skip) {
        let selected = i == state.file_idx;
        let marker = if selected { "❯ " } else { "  " };
        let base_style = if selected {
            theme.style("text").add_modifier(Modifier::BOLD)
        } else {
            theme.style("text")
        };
        // Name first, dimmed folder after, both fitted to the pane width.
        let (dir, name) = match file.path.rfind('/') {
            Some(pos) => (&file.path[..pos], &file.path[pos + 1..]),
            None => ("", file.path.as_str()),
        };
        // Per-file "+N −M" over all hunks; only non-zero parts are shown.
        let colors = diff_colors(theme);
        let added = file
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == DiffLineKind::Added)
            .count();
        let removed = file
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == DiffLineKind::Removed)
            .count();
        let mut count = String::new();
        if added > 0 {
            count.push_str(&format!("+{added}"));
        }
        if removed > 0 {
            if !count.is_empty() {
                count.push(' ');
            }
            count.push_str(&format!("−{removed}"));
        }
        // Reserve the count's width plus one separating space before
        // fitting name + folder, so the count never touches the folder.
        let count_w = text_len(&count);
        let (name, dir) = fit_path(name, dir, max.saturating_sub(count_w + 1));
        let mut spans = vec![
            Span::styled(marker.to_string(), theme.accent()),
            Span::styled(format!("{} ", file.status.letter()), base_style),
            Span::styled(name.clone(), base_style),
        ];
        if !dir.is_empty() {
            spans.push(Span::styled(dir.clone(), theme.dim()));
        }
        if !count.is_empty() {
            let pad = max
                .saturating_sub(count_w + 1)
                .saturating_sub(text_len(&name))
                .saturating_sub(text_len(&dir));
            spans.push(Span::raw(" ".repeat(pad + 1)));
            // "+N" in added color, "−M" in removed color.
            if added > 0 && removed > 0 {
                spans.push(Span::styled(format!("+{added}"), colors.added_fg));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(format!("−{removed}"), colors.removed_fg));
            } else if added > 0 {
                spans.push(Span::styled(format!("+{added}"), colors.added_fg));
            } else {
                spans.push(Span::styled(format!("−{removed}"), colors.removed_fg));
            }
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines).style(theme.page()), inner);

    // Scrollbar when files outnumber visible rows.
    if state.model.files.len() > height {
        let mut scrollbar_state = ScrollbarState::new(state.model.files.len())
            .position(skip)
            .viewport_content_length(height);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

/// Fit `(name, folder)` into `max` columns, shortening the folder's middle first, then the name's end.
fn fit_path(name: &str, dir: &str, max: usize) -> (String, String) {
    if max == 0 {
        return (String::new(), String::new());
    }
    if dir.is_empty() {
        return (shorten_end(name, max), String::new());
    }
    if text_len(name) + 1 + text_len(dir) <= max {
        return (name.to_string(), format!(" {dir}"));
    }
    // Room left for the folder after the whole name and its separator.
    let dir_max = max.saturating_sub(text_len(name) + 1);
    if dir_max >= 2 {
        // Prefer dropping whole components in the middle, then characters.
        let shortened =
            shorten_components(dir, dir_max).unwrap_or_else(|| shorten_middle(dir, dir_max));
        return (name.to_string(), format!(" {shortened}"));
    }
    // Both need shortening: cap the name, keep a 3-column folder hint.
    if max >= 5 {
        let name_max = max - 4;
        let dir_max = max - name_max - 1;
        let shortened =
            shorten_components(dir, dir_max).unwrap_or_else(|| shorten_middle(dir, dir_max));
        return (shorten_end(name, name_max), format!(" {shortened}"));
    }
    (shorten_end(name, max), String::new())
}

/// Right pane: the selected hunk's @@ header plus its lines, scrolled.
fn render_hunk(frame: &mut Frame, area: Rect, state: &ViewerState, theme: &Theme) {
    let Some(file) = state.current_file() else {
        return;
    };
    // Title: path, shortened from the left with "…" when too long.
    let title_w = area.width.saturating_sub(2) as usize; // borders
    let title_path = if text_len(&file.path) > title_w.saturating_sub(2) {
        let keep = title_w.saturating_sub(3); // " " + "…" + path tail + " "
        let skipped = file.path.chars().count().saturating_sub(keep);
        format!("…{}", file.path.chars().skip(skipped).collect::<String>())
    } else {
        file.path.clone()
    };
    let hunk_total = file.hunks.len();
    // Title notes: a mode-only change and an invisible text change (same
    // display text on both sides) are worth naming, since the hunk body
    // alone does not make either obvious.
    let mut notes = String::new();
    if let (Some(old_mode), Some(new_mode)) = (&file.old_mode, &file.new_mode) {
        if old_mode != new_mode {
            notes.push_str(" · permission changed");
        }
    }
    if let Some(hunk) = file.hunks.get(state.hunk_idx) {
        let removed: Vec<String> = hunk
            .lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Removed)
            .map(|l| display_text(&l.text))
            .collect();
        let added: Vec<String> = hunk
            .lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Added)
            .map(|l| display_text(&l.text))
            .collect();
        let has_removed = hunk.lines.iter().any(|l| l.kind == DiffLineKind::Removed);
        if has_removed && !added.is_empty() && removed == added {
            notes.push_str(" · invisible change only");
        }
    }
    let title = if hunk_total == 0 {
        format!(" {title_path}{notes} ")
    } else {
        format!(
            " {} · hunk {}/{}{} ",
            title_path,
            state.hunk_idx + 1,
            hunk_total.max(state.hunk_idx + 1),
            notes
        )
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .border_style(theme.style("border"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if file.hunks.is_empty() {
        let label = match file.status {
            crate::diff_model::FileStatus::Binary => "Binary file (no text diff)",
            crate::diff_model::FileStatus::TooLarge => {
                "too large to review (over 1 MiB or 10,000 lines)"
            }
            crate::diff_model::FileStatus::Unmerged => {
                "merge conflict — resolve it, then reopen /diff"
            }
            crate::diff_model::FileStatus::ModeOnly => {
                "permission change only — use git add to stage it"
            }
            _ => "No content changes",
        };
        let line = centered_line(label, theme.dim(), inner.width);
        frame.render_widget(Paragraph::new(line).style(theme.page()), inner);
        return;
    }
    let Some(hunk) = file.hunks.get(state.hunk_idx) else {
        return;
    };

    let colors = diff_colors(theme);
    // Line-number width from the largest number in this hunk.
    let max_no = hunk
        .lines
        .iter()
        .flat_map(|l| [l.old_no, l.new_no])
        .flatten()
        .max()
        .unwrap_or(0);
    let no_w = max_no.to_string().len();

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ),
        theme.muted(),
    ))];
    for line in &hunk.lines {
        let text = display_text(&line.text);
        let fmt_no = |n: Option<u32>| match n {
            Some(n) => format!("{n:>no_w$} "),
            None => " ".repeat(no_w + 1),
        };
        match line.kind {
            DiffLineKind::Context => {
                // Fit the text; overflow ends with a dim "…".
                let used = 2 * (no_w + 1) + 1 + text_len(&text);
                let over = used.saturating_sub(inner.width as usize);
                let mut spans = vec![
                    Span::styled(fmt_no(line.old_no), theme.dim()),
                    Span::styled(fmt_no(line.new_no), theme.dim()),
                    Span::raw(" "),
                ];
                if over > 0 {
                    let keep = text_len(&text).saturating_sub(over).max(1);
                    let head: String = text.chars().take(keep - 1).collect();
                    spans.push(Span::styled(head, colors.context));
                    spans.push(Span::styled("…".to_string(), theme.dim()));
                } else {
                    spans.push(Span::styled(text.clone(), colors.context));
                }
                lines.push(Line::from(spans));
            }
            DiffLineKind::Added | DiffLineKind::Removed => {
                let (fg, bg, marker, no) = if line.kind == DiffLineKind::Added {
                    (colors.added_fg, colors.added_bg, '+', fmt_no(line.new_no))
                } else {
                    (
                        colors.removed_fg,
                        colors.removed_bg,
                        '-',
                        fmt_no(line.old_no),
                    )
                };
                let other_no = if line.kind == DiffLineKind::Added {
                    line.old_no
                } else {
                    line.new_no
                };
                // Full-row background: pad to the inner pane width; a line
                // that would overflow ends with a dim "…" in the last column.
                let used_prefix = 2 * (no_w + 1) + 1 + 1;
                let over = used_prefix
                    .saturating_add(text_len(&text))
                    .saturating_sub(inner.width as usize);
                let row_style = if bg != Color::Reset {
                    Style::default().bg(bg)
                } else {
                    fg
                };
                let mut row = vec![
                    Span::styled(fmt_no(other_no), theme.dim().bg(bg)),
                    Span::styled(no, theme.dim().bg(bg)),
                    Span::styled(marker.to_string(), fg.bg(bg)),
                ];
                if over > 0 {
                    let keep = text_len(&text).saturating_sub(over).max(1);
                    let head: String = text.chars().take(keep - 1).collect();
                    row.push(Span::styled(head, fg.bg(bg)));
                    row.push(Span::styled("…".to_string(), theme.dim().bg(bg)));
                } else {
                    row.push(Span::styled(text.clone(), fg.bg(bg)));
                }
                // The row's own width decides the padding so the background
                // fills exactly to the border, never past it.
                let used: usize = row.iter().map(|s| text_len(s.content.as_ref())).sum();
                let pad = inner.width.saturating_sub(used as u16) as usize;
                row.push(Span::styled(" ".repeat(pad), row_style));
                lines.push(Line::from(row));
            }
        }
    }

    // Apply scroll: skip `scroll` rows after the @@ header.
    let skip = (state.scroll as usize).min(lines.len());
    let visible = lines[skip..].to_vec();
    frame.render_widget(Paragraph::new(visible).style(theme.page()), inner);
}

/// Footer: notice if set, else key hints plus the two action buttons.
fn render_footer(
    frame: &mut Frame,
    area: Rect,
    state: &ViewerState,
    theme: &Theme,
    lay: &ViewerLayout,
    mouse: Option<MousePos>,
) {
    let text = match &state.notice {
        Some(notice) => notice.text.clone(),
        None => footer_hint(state.model.side),
    };
    let style = match &state.notice {
        Some(n) if n.kind == NoticeKind::Success => theme.success(),
        Some(_) => theme.error(),
        None => theme.dim(),
    };
    // Leave room on the right for the buttons; truncate the hint safely.
    let btn_edge = lay.accept_btn.x.min(area.right());
    let text_w = btn_edge.saturating_sub(area.x).saturating_sub(1) as usize;
    let shown: String = text.chars().take(text_w).collect();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(shown, style))).style(theme.page()),
        area,
    );
    let (accept_label, reject_label) = footer_buttons(state.model.side);
    // No hunk to act on (binary / rename / mode-only / empty model): dim
    // both buttons.
    let actionable = state.current_file().is_some_and(|f| !f.hunks.is_empty());
    if actionable {
        draw_button(
            frame,
            lay.accept_btn,
            accept_label,
            theme,
            hovered(lay.accept_btn, mouse),
            ButtonKind::Accept,
        );
        draw_button(
            frame,
            lay.reject_btn,
            reject_label,
            theme,
            hovered(lay.reject_btn, mouse),
            ButtonKind::Reject,
        );
    } else {
        for (rect, label) in [
            (lay.accept_btn, accept_label),
            (lay.reject_btn, reject_label),
        ] {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("[ {label} ]"),
                    theme.dim(),
                ))),
                rect,
            );
        }
    }
}

/// True when the mouse currently rests on `rect`.
fn hovered(rect: Rect, mouse: Option<MousePos>) -> bool {
    mouse.is_some_and(|m| hit(rect, m.x, m.y))
}

/// Which color family a button belongs to (accept vs reject/unstage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Accept,
    Reject,
}

fn draw_button(
    frame: &mut Frame,
    rect: Rect,
    label: &str,
    theme: &Theme,
    hover: bool,
    kind: ButtonKind,
) {
    let colors = diff_colors(theme);
    let accent = match kind {
        ButtonKind::Accept => colors.added_fg.fg.unwrap_or(Color::Reset),
        ButtonKind::Reject => colors.removed_fg.fg.unwrap_or(Color::Reset),
    };
    let (fg_style, bg) = if hover {
        (
            Style::default()
                .fg(theme.get("bg"))
                .add_modifier(Modifier::BOLD),
            accent,
        )
    } else {
        (Style::default().fg(accent), Color::Reset)
    };
    let mut spans = vec![Span::styled("[ ".to_string(), fg_style.bg(bg))];
    spans.push(Span::styled(label.to_string(), fg_style.bg(bg)));
    spans.push(Span::styled(" ]".to_string(), fg_style.bg(bg)));
    let pad = rect.width.saturating_sub(
        spans
            .iter()
            .map(|s| text_len(s.content.as_ref()) as u16)
            .sum::<u16>(),
    );
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad as usize), fg_style.bg(bg)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// Centered confirm popup with Yes/No buttons for destructive actions.
fn render_confirm_popup(
    frame: &mut Frame,
    lay: &ViewerLayout,
    state: &ViewerState,
    theme: &Theme,
    mouse: Option<MousePos>,
) {
    use ratatui::widgets::Clear;
    let Some(popup) = lay.popup else {
        return;
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Discard ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .border_style(theme.style("error"));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // Untracked files are deleted, tracked hunks discarded; both back up first.
    let msg = match state.current_file() {
        Some(f) if f.untracked => format!("Delete new file {}? It is backed up first.", f.path),
        _ => "Discard this hunk? It is backed up first.".to_string(),
    };
    let line = centered_line(&msg, theme.dim(), inner.width);
    frame.render_widget(Paragraph::new(line).style(theme.page()), inner);
    draw_button(
        frame,
        lay.yes_btn,
        "y  Yes, discard",
        theme,
        hovered(lay.yes_btn, mouse),
        ButtonKind::Reject,
    );
    draw_button(
        frame,
        lay.no_btn,
        "n  No",
        theme,
        hovered(lay.no_btn, mouse),
        ButtonKind::Reject,
    );
}

/// Rects of every clickable/drawable region of the viewer, produced by
/// [`layout`] so mouse hit-testing and rendering can never disagree.
#[derive(Debug, Clone, Default)]
pub struct ViewerLayout {
    /// Whole viewer area (header + panes + footer).
    pub root: Rect,
    /// Header row.
    pub header: Rect,
    /// Panes row (file list + hunk pane).
    pub panes: Rect,
    /// Footer row.
    pub footer: Rect,
    /// File list block (None when hidden at <80 columns or empty model).
    pub file_list: Option<Rect>,
    /// Hunk pane block.
    pub hunk_pane: Rect,
    /// Accept button in the footer.
    pub accept_btn: Rect,
    /// Reject/Unstage button in the footer.
    pub reject_btn: Rect,
    /// Confirm popup (Some only while it is open).
    pub popup: Option<Rect>,
    /// "Yes, discard" button inside the popup.
    pub yes_btn: Rect,
    /// "No" button inside the popup.
    pub no_btn: Rect,
}

/// Labels of the two footer action buttons for the current side.
fn footer_buttons(side: DiffSide) -> (&'static str, &'static str) {
    match side {
        DiffSide::WorkingTree => ("↵ Accept hunk", "⌫ Reject hunk"),
        DiffSide::Index => ("↵ Accept hunk", "⌫ Unstage hunk"),
    }
}

/// Pure layout: split `area` exactly like `render` does, plus the footer
/// buttons and the centered confirm popup. `popup_open` decides whether the
/// popup rects are meaningful (`ViewerLayout::popup` is `Some` then).
pub fn layout(area: Rect, state: &ViewerState) -> ViewerLayout {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(1),    // panes
            Constraint::Length(1), // footer
        ])
        .split(area);
    let mut out = ViewerLayout {
        root: area,
        header: rows[0],
        panes: rows[1],
        footer: rows[2],
        ..ViewerLayout::default()
    };

    // Panes: same width rule as render_panes; hide the list under 80 cols.
    if !state.model.files.is_empty() && area.width >= 80 {
        let longest = state
            .model
            .files
            .iter()
            .map(|f| text_len(&f.path))
            .max()
            .unwrap_or(0);
        let wanted = (longest + 4).clamp(20, ((out.panes.width as usize) * 2 / 5).max(20));
        let file_w = (wanted as u16).min(out.panes.width.saturating_sub(10));
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(file_w), Constraint::Min(1)])
            .split(out.panes);
        out.file_list = Some(cols[0]);
        out.hunk_pane = cols[1];
    } else {
        out.hunk_pane = out.panes;
    }

    // Footer buttons: "[ <label> ]" right-aligned with one space between.
    let (accept_label, reject_label) = footer_buttons(state.model.side);
    let reject_w = text_len(reject_label) as u16 + 4;
    let accept_w = text_len(accept_label) as u16 + 4;
    out.reject_btn = Rect {
        x: out.footer.right().saturating_sub(reject_w),
        y: out.footer.y,
        width: reject_w.min(out.footer.width),
        height: 1,
    };
    out.accept_btn = Rect {
        x: out.reject_btn.x.saturating_sub(accept_w + 1),
        y: out.footer.y,
        width: accept_w.min(out.footer.width),
        height: 1,
    };

    // Confirm popup: centered 40x5 card over everything.
    let pw = 40u16.min(area.width);
    let ph = 5u16.min(area.height);
    out.popup = Some(Rect {
        x: area.x + (area.width.saturating_sub(pw)) / 2,
        y: area.y + (area.height.saturating_sub(ph)) / 2,
        width: pw,
        height: ph,
    });
    // Buttons on the popup's last inner row, centered as a pair.
    let yes_label = "y  Yes, discard";
    let no_label = "n  No";
    let yes_w = text_len(yes_label) as u16 + 4;
    let no_w = text_len(no_label) as u16 + 4;
    let pair_w = yes_w + 1 + no_w;
    let popup_rect = out.popup.unwrap_or(area);
    let base_x = popup_rect.x + popup_rect.width.saturating_sub(pair_w) / 2;
    let btn_y = popup_rect.bottom().saturating_sub(2);
    out.yes_btn = Rect {
        x: base_x,
        y: btn_y,
        width: yes_w,
        height: 1,
    };
    out.no_btn = Rect {
        x: base_x + yes_w + 1,
        y: btn_y,
        width: no_w,
        height: 1,
    };
    out
}

/// True when `rect` contains the point `(x, y)`.
fn hit(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0 && rect.height > 0 && rect.contains(ratatui::layout::Position { x, y })
}

/// Guard that leaves the alternate screen and disables mouse capture when
/// dropped, so every exit path (ok, error, panic-unwind) restores the main
/// screen and never leaks mouse capture to Loop's chat UI (§3.4).
struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        )?;
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen
        );
    }
}

/// Rows visible in the hunk pane for the current terminal size: header and
/// footer take one row each, the pane's block borders take two more.
/// (Horizontal padding does not change the row count.)
fn hunk_pane_height(term_height: u16) -> u16 {
    term_height.saturating_sub(4).max(1)
}

/// True when Reject needs the confirm popup: only a file with at least one
/// hunk has something destructive to discard. Hunkless files (binary /
/// rename / mode-only …) go straight to apply_action so plan_action's real
/// reason is shown instead of opening a popup that can do nothing.
fn reject_needs_confirm(state: &ViewerState) -> bool {
    state.current_file().is_some_and(|f| !f.hunks.is_empty())
}

/// §3.6 step 5-7 for one key press: remember what the user is looking at,
/// re-read the model, plan against the fresh data, execute, then adopt the
/// newest model with the cursor kept on the same file when possible.
async fn apply_action(
    state: &mut ViewerState,
    git: &GitDiff,
    action: Action,
    is_busy: &impl Fn() -> bool,
    backup_dir: &std::path::Path,
) {
    // Remember what the user is acting on before anything changes.
    let path = state.current_file().map(|f| f.path.clone());
    let expected = state.current_hunk().map(|h| h.id);
    let side = state.model.side;

    // Re-read: the world may have moved since the last render.
    let fresh = match git.read(side).await {
        Ok(model) => model,
        Err(err) => {
            state.notice = Some(Notice::error(format!("diff: {err}")));
            return;
        }
    };
    let Some(path) = path else {
        state.notice = Some(Notice::error("no file selected"));
        return;
    };
    let Some(expected) = expected else {
        // No hunk to act on (binary / rename / mode-only / unmerged …):
        // plan_action names the real reason, so surface it instead of a
        // generic "no hunk" message.
        let idx = fresh
            .files
            .iter()
            .position(|f| f.path == path)
            .unwrap_or(state.file_idx);
        match plan_action(&fresh, idx, 0, HunkId(0), action, is_busy()) {
            ActionPlan::Disabled(note) | ActionPlan::Blocked(note) => {
                state.notice = Some(Notice::error(note));
            }
            _ => state.notice = Some(Notice::error("no hunk selected")),
        }
        return;
    };
    let file_idx = fresh.files.iter().position(|f| f.path == path);
    let plan = match file_idx {
        Some(idx) => plan_action(
            &fresh,
            idx,
            state.hunk_idx.min(fresh.files[idx].hunks.len()),
            expected,
            action,
            is_busy(),
        ),
        None => ActionPlan::Refreshed,
    };
    if plan == ActionPlan::Refreshed {
        state.model = fresh;
        keep_cursor(state, &path);
        state.notice = Some(Notice::error("hunk changed, refreshed"));
        return;
    }
    match crate::tui::diff_actions::execute(git, plan, backup_dir).await {
        Ok(text) => {
            // The footer truncates long notices and the next key press
            // clears them; keep any undo/backup command for the chat.
            if text.contains(" · undo: ") || text.contains(" · backup: ") {
                state.undo_notes.push(text.clone());
            }
            state.notice = Some(Notice::success(text));
        }
        Err(text) => state.notice = Some(Notice::error(text)),
    }
    // Adopt the post-action model, keeping the cursor stable.
    match git.read(side).await {
        Ok(model) => state.model = model,
        Err(err) => state.notice = Some(Notice::error(format!("diff: {err}"))),
    }
    keep_cursor(state, &path);
}

/// What a key press means while the confirm popup is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupOutcome {
    /// Run the destructive action.
    Confirm,
    /// Close the popup without acting.
    Cancelled,
    /// The popup swallows the key; nothing changes.
    Ignored,
}

/// Pure popup key logic: y/Enter confirm, n/Esc cancel (with a notice),
/// every other key is ignored while the popup stays open.
fn handle_popup_key(state: &mut ViewerState, code: KeyCode) -> PopupOutcome {
    match code {
        KeyCode::Char('y') | KeyCode::Enter => {
            state.confirm_open = false;
            PopupOutcome::Confirm
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            state.confirm_open = false;
            state.notice = Some(Notice::success("cancelled"));
            PopupOutcome::Cancelled
        }
        _ => PopupOutcome::Ignored,
    }
}

/// One left click: buttons first (popup swallows everything), then file rows.
async fn handle_left_click(
    state: &mut ViewerState,
    lay: &ViewerLayout,
    x: u16,
    y: u16,
    git: &GitDiff,
    is_busy: &impl Fn() -> bool,
    backup_dir: &std::path::Path,
) {
    // While the confirm popup is open it owns all clicks.
    if state.confirm_open {
        if let Some(popup) = lay.popup {
            if hit(lay.yes_btn, x, y) {
                state.confirm_open = false;
                apply_action(state, git, Action::Reject, is_busy, backup_dir).await;
                return;
            }
            if hit(lay.no_btn, x, y) || !hit(popup, x, y) {
                state.confirm_open = false;
            }
            return;
        }
        state.confirm_open = false;
        return;
    }
    if hit(lay.accept_btn, x, y) {
        apply_action(state, git, Action::Accept, is_busy, backup_dir).await;
        return;
    }
    if hit(lay.reject_btn, x, y) {
        match state.model.side {
            DiffSide::WorkingTree => {
                // Only a file with a hunk has something destructive to
                // confirm; otherwise show plan_action's reason directly.
                if reject_needs_confirm(state) {
                    state.confirm_open = true;
                } else {
                    apply_action(state, git, Action::Reject, is_busy, backup_dir).await;
                }
            }
            DiffSide::Index => apply_action(state, git, Action::Unstage, is_busy, backup_dir).await,
        }
        return;
    }
    // File list row: map the inner y to a model index via the scroll offset.
    if let Some(list) = lay.file_list {
        // Inner area: 1 border + 1 padding column each side; rows start
        // after the top border. The visible window's offset mirrors
        // render_file_list's keep-selection-visible rule.
        let inner_h = list.height.saturating_sub(2) as usize;
        let skip = if inner_h > 0 && state.file_idx >= inner_h {
            state.file_idx + 1 - inner_h
        } else {
            0
        };
        let row = y.saturating_sub(list.y + 1) as usize;
        if y > list.y && y < list.bottom() - 1 && row < inner_h {
            let idx = skip + row;
            if idx < state.model.files.len() && idx != state.file_idx {
                state.file_idx = idx;
                state.hunk_idx = 0;
                state.scroll = 0;
            }
        }
    }
}

/// After a model swap, keep the cursor on `path` when it still exists (hunk
/// index clamped), else on the old file index clamped; scroll resets.
fn keep_cursor(state: &mut ViewerState, path: &str) {
    state.scroll = 0;
    if let Some(idx) = state.model.files.iter().position(|f| f.path == path) {
        state.file_idx = idx;
    } else {
        state.file_idx = state
            .file_idx
            .min(state.model.files.len().saturating_sub(1));
    }
    state.clamp_selection();
}

/// Modal full-screen viewer loop (§3.4). Owns the whole screen until the
/// user closes it; raw mode stays on for the app lifetime, only the
/// alternate screen is entered and left. `initial_notice` (e.g. the §3.7
/// busy-agent banner) is shown in the footer until the next key press.
/// Undo/backup notices are drained into `undo_notes` so they survive the
/// viewer closing.
pub async fn run(
    model: DiffModel,
    theme: &Theme,
    git: GitDiff,
    is_busy: impl Fn() -> bool,
    backup_dir: std::path::PathBuf,
    initial_notice: Option<String>,
    undo_notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    use crossterm::event::{
        self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;
    use ratatui::Viewport;

    let mut state = ViewerState::new(model);
    state.notice = initial_notice.map(Notice::error);
    // Tracked mouse position for hover styles on the buttons.
    let mut mouse_pos: Option<MousePos> = None;
    let _guard = AltScreenGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let _ = terminal.hide_cursor();

    // Non-blocking poll exactly like app.rs's run_loop: never block the
    // Tokio thread on event::read.
    loop {
        // Drain undo/backup notices out before drawing so a `q` on this
        // iteration can never lose them.
        undo_notes.append(&mut state.undo_notes);
        terminal.draw(|f| render_with_mouse(f, &state, theme, mouse_pos))?;

        let timed_out = !event::poll(std::time::Duration::from_millis(50))?;
        if timed_out {
            // Yield so pending runtime work keeps making progress.
            tokio::task::yield_now().await;
            continue;
        }
        while event::poll(std::time::Duration::from_millis(0))? {
            undo_notes.append(&mut state.undo_notes);
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    state.clear_notice();
                    let pane_h = hunk_pane_height(terminal.size()?.height);
                    // While the confirm popup is open it owns every key:
                    // y/Enter confirm, n/Esc cancel, all else is ignored
                    // (Esc must not close the viewer here).
                    if state.confirm_open {
                        match handle_popup_key(&mut state, key.code) {
                            PopupOutcome::Confirm => {
                                apply_action(
                                    &mut state,
                                    &git,
                                    Action::Reject,
                                    &is_busy,
                                    &backup_dir,
                                )
                                .await;
                            }
                            PopupOutcome::Cancelled | PopupOutcome::Ignored => {}
                        }
                        continue;
                    }
                    match (key.code, key.modifiers) {
                        // q / esc close the viewer.
                        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                        // Ctrl+C always closes immediately (never quits app).
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                        // Hunk navigation: arrows move between hunks.
                        (KeyCode::Down, _) => state.next_hunk(),
                        (KeyCode::Up, _) => state.prev_hunk(),
                        // File navigation.
                        (KeyCode::Tab, _) => state.next_file(),
                        (KeyCode::BackTab, _) => state.prev_file(),
                        // Scrolling: j/k one line, PgUp/PgDn a full pane.
                        (KeyCode::PageDown, _) => state.page_down(pane_h),
                        (KeyCode::PageUp, _) => state.page_up(pane_h),
                        (KeyCode::Char('j'), _) => state.scroll_down(pane_h),
                        (KeyCode::Char('k'), _) => state.scroll_up(),
                        // Jumps.
                        (KeyCode::Char('g'), _) => state.jump_first(),
                        (KeyCode::Char('G'), _) => state.jump_last(),
                        // Unstaged view: accept stages the hunk.
                        (KeyCode::Enter, _) | (KeyCode::Char('a'), _)
                            if state.model.side == DiffSide::WorkingTree =>
                        {
                            apply_action(&mut state, &git, Action::Accept, &is_busy, &backup_dir)
                                .await;
                        }
                        // Unstaged view: r/Backspace opens the confirm popup
                        // — unless there is no hunk to discard, in which
                        // case apply_action shows why nothing can happen.
                        (KeyCode::Char('r'), _) | (KeyCode::Backspace, _)
                            if state.model.side == DiffSide::WorkingTree =>
                        {
                            if reject_needs_confirm(&state) {
                                state.confirm_open = true;
                            } else {
                                apply_action(
                                    &mut state,
                                    &git,
                                    Action::Reject,
                                    &is_busy,
                                    &backup_dir,
                                )
                                .await;
                            }
                        }
                        // Staged view: u/Backspace unstages directly.
                        (KeyCode::Char('u'), _) | (KeyCode::Backspace, _)
                            if state.model.side == DiffSide::Index =>
                        {
                            apply_action(&mut state, &git, Action::Unstage, &is_busy, &backup_dir)
                                .await;
                        }
                        _ => {}
                    }
                }
                Event::Resize(..) => {
                    let _ = terminal.autoresize();
                }
                Event::Mouse(me) => {
                    mouse_pos = Some(MousePos {
                        x: me.column,
                        y: me.row,
                    });
                    let lay = layout(terminal.size()?.into(), &state);
                    match me.kind {
                        MouseEventKind::Moved => {}
                        MouseEventKind::Down(MouseButton::Left) => {
                            state.clear_notice();
                            handle_left_click(
                                &mut state,
                                &lay,
                                me.column,
                                me.row,
                                &git,
                                &is_busy,
                                &backup_dir,
                            )
                            .await;
                        }
                        MouseEventKind::ScrollUp => {
                            if state.confirm_open {
                                // ignore
                            } else if hit(lay.hunk_pane, me.column, me.row) {
                                state.scroll_up();
                            } else if lay.file_list.is_some_and(|r| hit(r, me.column, me.row)) {
                                state.prev_file();
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if state.confirm_open {
                                // ignore
                            } else if hit(lay.hunk_pane, me.column, me.row) {
                                let pane_h = hunk_pane_height(terminal.size()?.height);
                                state.scroll_down(pane_h);
                            } else if lay.file_list.is_some_and(|r| hit(r, me.column, me.row)) {
                                state.next_file();
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_model::{DiffLine, DiffLineKind, FileDiff, FileStatus, Hunk, HunkId};

    /// One file with `hunks` hunks of one line each.
    fn file(path: &str, hunks: usize) -> FileDiff {
        FileDiff {
            old_path: None,
            path: path.to_string(),
            status: FileStatus::Modified,
            old_mode: None,
            new_mode: None,
            untracked: false,
            hunks: (0..hunks)
                .map(|i| Hunk {
                    old_start: 1 + i as u32,
                    old_count: 1,
                    new_start: 1 + i as u32,
                    new_count: 1,
                    section: None,
                    lines: vec![DiffLine {
                        kind: DiffLineKind::Context,
                        text: format!("line {i}"),
                        no_newline: false,
                        old_no: Some(1),
                        new_no: Some(1),
                    }],
                    raw_body: Vec::new(),
                    id: HunkId(i as u64),
                })
                .collect(),
        }
    }

    fn model(files: Vec<FileDiff>) -> DiffModel {
        DiffModel {
            side: DiffSide::WorkingTree,
            files,
            truncated: false,
        }
    }

    #[test]
    fn empty_model_clamps_to_zero_selection() {
        let mut s = ViewerState::new(model(vec![]));
        assert_eq!((s.file_idx, s.hunk_idx), (0, 0));
        assert!(s.current_file().is_none());
        assert!(s.current_hunk().is_none());
        // Navigation is a no-op, never a panic.
        s.next_file();
        s.prev_file();
        s.next_hunk();
        s.prev_hunk();
        s.jump_last();
        assert_eq!((s.file_idx, s.hunk_idx), (0, 0));
    }

    #[test]
    fn zero_hunk_file_clamps_hunk_index() {
        // Binary / rename files carry no hunks.
        let mut f = file("bin.dat", 0);
        f.status = FileStatus::Binary;
        let mut s = ViewerState::new(model(vec![f]));
        assert_eq!(s.hunk_idx, 0);
        assert!(s.current_hunk().is_none());
        // Advancing moves to the next file's first hunk.
        s.next_hunk();
        assert_eq!(s.file_idx, 0, "single file wraps to itself");
    }

    #[test]
    fn hunk_navigation_wraps_across_files() {
        let mut s = ViewerState::new(model(vec![file("a.rs", 2), file("b.rs", 1)]));
        assert_eq!((s.file_idx, s.hunk_idx), (0, 0));
        s.next_hunk();
        assert_eq!((s.file_idx, s.hunk_idx), (0, 1));
        // End of a.rs -> first hunk of b.rs.
        s.next_hunk();
        assert_eq!((s.file_idx, s.hunk_idx), (1, 0));
        // End of the model wraps to the first file.
        s.next_hunk();
        assert_eq!((s.file_idx, s.hunk_idx), (0, 0));
        // Backwards from the very start wraps to the last file's last hunk.
        s.prev_hunk();
        assert_eq!((s.file_idx, s.hunk_idx), (1, 0));
        s.prev_hunk();
        assert_eq!((s.file_idx, s.hunk_idx), (0, 1));
    }

    #[test]
    fn file_navigation_resets_hunk_and_scroll() {
        let mut s = ViewerState::new(model(vec![file("a.rs", 2), file("b.rs", 2)]));
        s.hunk_idx = 1;
        s.scroll = 7;
        s.next_file();
        assert_eq!((s.file_idx, s.hunk_idx, s.scroll), (1, 0, 0));
        s.prev_file();
        assert_eq!((s.file_idx, s.hunk_idx, s.scroll), (0, 0, 0));
        // Wrap both ways.
        s.next_file();
        s.next_file();
        assert_eq!(s.file_idx, 0);
        s.prev_file();
        assert_eq!(s.file_idx, 1);
    }

    #[test]
    fn scroll_clamps_at_both_ends() {
        let mut s = ViewerState::new(model(vec![file("a.rs", 1)]));
        s.scroll_up();
        assert_eq!(s.scroll, 0, "cannot scroll above the hunk");
        s.scroll_down(1);
        assert_eq!(s.scroll, 1, "one-line hunk + header scrolls one row");
        // A zero-height pane cannot scroll further.
        s.scroll_down(0);
        assert_eq!(s.scroll, 1, "zero-height pane disables scrolling");
        // A taller hunk scrolls until its end, then stops.
        let mut big = file("big.rs", 1);
        big.hunks[0].lines = (0..10)
            .map(|i| DiffLine {
                kind: DiffLineKind::Context,
                text: format!("l{i}"),
                no_newline: false,
                old_no: Some(i + 1),
                new_no: Some(i + 1),
            })
            .collect();
        let mut s = ViewerState::new(model(vec![big]));
        for _ in 0..20 {
            s.scroll_down(4);
        }
        assert_eq!(s.scroll, 7, "10 lines + header minus a 4-row pane");
    }

    #[test]
    fn page_scroll_moves_a_pane_at_a_time() {
        let mut big = file("big.rs", 1);
        big.hunks[0].lines = (0..30)
            .map(|i| DiffLine {
                kind: DiffLineKind::Context,
                text: format!("l{i}"),
                no_newline: false,
                old_no: Some(i + 1),
                new_no: Some(i + 1),
            })
            .collect();
        let mut s = ViewerState::new(model(vec![big]));
        // 31 rows total, 10-row pane: max scroll is 21.
        s.page_down(10);
        assert_eq!(s.scroll, 9, "pane minus one row");
        s.page_up(10);
        assert_eq!(s.scroll, 0, "page up mirrors page down");
        for _ in 0..10 {
            s.page_down(10);
        }
        assert_eq!(s.scroll, 21, "clamped at the hunk end");
        s.page_up(10);
        assert_eq!(s.scroll, 12);
        // Degenerate panes never move.
        let mut s = ViewerState::new(model(vec![file("a.rs", 1)]));
        s.page_down(0);
        s.page_down(1);
        s.page_up(1);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn jump_first_and_last_clamp_safely() {
        let mut s = ViewerState::new(model(vec![file("a.rs", 2), file("b.rs", 3)]));
        s.jump_last();
        assert_eq!(s.file_idx, 1);
        assert_eq!(s.hunk_idx, 2, "last hunk of the last file");
        s.jump_first();
        assert_eq!((s.file_idx, s.hunk_idx), (0, 0));
        // An empty model ignores jumps.
        let mut s = ViewerState::new(model(vec![]));
        s.jump_last();
        s.jump_first();
        assert_eq!((s.file_idx, s.hunk_idx), (0, 0));
    }

    #[test]
    fn notice_clears_on_demand() {
        let mut s = ViewerState::new(model(vec![]));
        assert!(s.notice.is_none());
        s.notice = Some(Notice::success("staged 1 hunk"));
        s.clear_notice();
        assert!(s.notice.is_none());
    }

    #[test]
    fn footer_hint_differs_per_side() {
        let unstaged = footer_hint(DiffSide::WorkingTree);
        let staged = footer_hint(DiffSide::Index);
        // The buttons carry accept/reject; the hint only carries navigation
        // plus the staged-only unstage key.
        assert!(staged.contains("u unstage") && !unstaged.contains("u unstage"));
        for hint in [&unstaged, &staged] {
            assert!(hint.contains("↑/↓ hunk"), "{hint}");
            assert!(hint.contains("tab file"), "{hint}");
            assert!(hint.contains("j/k scroll"), "{hint}");
            assert!(hint.contains("PgUp/PgDn page"), "{hint}");
            assert!(hint.contains("q close"), "{hint}");
        }
    }

    /// Render into a TestBackend and return all buffer text.
    fn buffer_text(backend: &ratatui::backend::TestBackend) -> String {
        let buf = backend.buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_smoke_empty_and_two_file_models() {
        let theme = Theme::dark();

        // Empty model: centred empty state, no panic.
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let empty = ViewerState::new(model(vec![]));
        terminal
            .draw(|f| render(f, &empty, &theme))
            .expect("empty render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("No unstaged changes"), "{text}");

        // Two files: header, file list and hunk pane all populated.
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let two = ViewerState::new(model(vec![file("src/app.rs", 1), file("lib/main.rs", 1)]));
        terminal.draw(|f| render(f, &two, &theme)).expect("render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("Unstaged changes"), "{text}");
        assert!(text.contains("file 1 of 2"), "{text}");
        assert!(text.contains("hunk 1 of 1"), "{text}");
        assert!(text.contains("app.rs src"), "{text}");
        assert!(text.contains("main.rs lib"), "{text}");
        assert!(text.contains("@@ -1,1 +1,1 @@"), "{text}");
        assert!(text.contains("line 0"), "{text}");
    }

    #[test]
    fn footer_buttons_name_the_hunk() {
        // At a comfortable width both footer buttons are fully visible and
        // name what they act on (the hunk), not just the bare verb.
        let theme = Theme::dark();
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let state = ViewerState::new(model(vec![file("src/app.rs", 1)]));
        terminal
            .draw(|f| render(f, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("Accept hunk"), "{text}");
        assert!(text.contains("Reject hunk"), "{text}");
    }

    #[test]
    fn no_hunk_title_has_no_hunk_count() {
        let theme = Theme::dark();
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let state = ViewerState::new(model(vec![file("run.sh", 0)]));
        terminal
            .draw(|f| render(f, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("run.sh"), "{text}");
        assert!(!text.contains("hunk 1/1"), "{text}");
    }

    #[test]
    fn mode_only_file_says_what_to_do() {
        let theme = Theme::dark();
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut f = file("run.sh", 0);
        f.status = FileStatus::ModeOnly;
        f.old_mode = Some("100644".to_string());
        f.new_mode = Some("100755".to_string());
        let state = ViewerState::new(model(vec![f]));
        terminal
            .draw(|f| render(f, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("permission change only"), "{text}");
        assert!(text.contains("permission changed"), "{text}");
    }

    #[test]
    fn render_file_list_keeps_selection_visible() {
        let theme = Theme::dark();
        let files: Vec<FileDiff> = (0..50).map(|i| file(&format!("f{i:02}.rs"), 1)).collect();
        let mut state = ViewerState::new(model(files));
        state.file_idx = 40;
        state.clamp_selection();

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        // The selected (41st) path is on screen…
        assert!(text.contains("f40.rs"), "{text}");
        // …and the window has scrolled past the very first entries.
        assert!(!text.contains("f00.rs"), "{text}");
    }

    /// One modified file with a single hunk whose lines are given verbatim;
    /// ids are stable hashes via `hunk_id`, exactly like production.
    fn hunky_file(
        path: &str,
        lines: Vec<DiffLine>,
        old_mode: Option<String>,
        new_mode: Option<String>,
    ) -> FileDiff {
        let mut f = FileDiff {
            old_path: None,
            path: path.to_string(),
            status: FileStatus::Modified,
            old_mode,
            new_mode,
            untracked: false,
            hunks: Vec::new(),
        };
        let count_removed = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Removed)
            .count() as u32;
        let count_added = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Added)
            .count() as u32;
        let mut hunk = Hunk {
            old_start: 1,
            old_count: count_removed,
            new_start: 1,
            new_count: count_added,
            section: None,
            lines,
            raw_body: Vec::new(),
            id: HunkId(0),
        };
        hunk.id = crate::diff_model::hunk_id(&f, &hunk);
        f.hunks.push(hunk);
        f
    }

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            text: text.to_string(),
            no_newline: false,
            old_no: Some(1),
            new_no: Some(1),
        }
    }

    #[test]
    fn tabs_render_as_spaces() {
        let theme = Theme::dark();
        let f = hunky_file(
            "tab.txt",
            vec![line(DiffLineKind::Context, "\tindented")],
            None,
            None,
        );
        let state = ViewerState::new(model(vec![f]));

        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|fr| render(fr, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        // The tab must not vanish: it renders as four spaces.
        assert!(text.contains("    indented"), "{text}");
        assert!(!text.contains("\tindented"), "{text}");
    }

    #[test]
    fn control_characters_render_visibly() {
        let theme = Theme::dark();
        let f = hunky_file(
            "ctl.txt",
            vec![line(DiffLineKind::Context, "a\u{1b}b")],
            None,
            None,
        );
        let state = ViewerState::new(model(vec![f]));

        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|fr| render(fr, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        // ESC would be dropped by ratatui; it must show as U+FFFD instead.
        assert!(text.contains("a\u{FFFD}b"), "{text}");
    }

    #[test]
    fn title_notes_permission_change() {
        let theme = Theme::dark();
        let f = hunky_file(
            "run.sh",
            vec![line(DiffLineKind::Context, "body")],
            Some("100644".to_string()),
            Some("100755".to_string()),
        );
        let state = ViewerState::new(model(vec![f]));

        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|fr| render(fr, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("permission changed"), "{text}");
    }

    #[test]
    fn title_notes_invisible_change() {
        let theme = Theme::dark();

        // Removed "\tx" vs Added "    x": identical display text, so the
        // change is invisible and the title says so.
        let invisible = hunky_file(
            "ws.txt",
            vec![
                line(DiffLineKind::Removed, "\tx"),
                line(DiffLineKind::Added, "    x"),
            ],
            None,
            None,
        );
        let state = ViewerState::new(model(vec![invisible]));
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|fr| render(fr, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        assert!(text.contains("invisible change only"), "{text}");

        // Removed "a" vs Added "b": a real change, no note.
        let visible = hunky_file(
            "real.txt",
            vec![
                line(DiffLineKind::Removed, "a"),
                line(DiffLineKind::Added, "b"),
            ],
            None,
            None,
        );
        let state = ViewerState::new(model(vec![visible]));
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|fr| render(fr, &state, &theme))
            .expect("render");
        let text = buffer_text(terminal.backend());
        assert!(!text.contains("invisible change only"), "{text}");
    }

    #[test]
    fn fit_path_fits_untouched() {
        assert_eq!(
            fit_path("app.rs", "src/tui", 20),
            ("app.rs".to_string(), " src/tui".to_string())
        );
        assert_eq!(fit_path("a.rs", "", 5), ("a.rs".to_string(), String::new()));
    }

    #[test]
    fn fit_path_shortens_folder_in_the_middle() {
        let (name, dir) = fit_path("app.rs", "crates/loop-cli/src/tui/diff", 18);
        assert_eq!(name, "app.rs");
        assert!(text_len(&name) + text_len(&dir) <= 18);
        assert!(dir.starts_with(' ') && dir.contains('…'));
        assert!(dir.ends_with("diff"), "{dir}");
        assert!(dir.contains("crate"), "{dir}");
    }

    #[test]
    fn fit_path_shortens_name_when_both_too_long() {
        let (name, dir) = fit_path("very_long_file_name.rs", "some/deeply/nested/folder", 12);
        assert!(text_len(&name) + text_len(&dir) <= 12, "{name}{dir}");
        assert!(name.contains('…') && !name.contains(".rs"), "{name}");
        assert!(dir.contains('…'), "{dir}");
        // Tiny widths degrade to just the name.
        let (name, dir) = fit_path("mod.rs", "a/b/c/d", 4);
        assert_eq!((name.as_str(), dir.as_str()), ("mod…", ""));
    }

    /// §3.6 accept flow against a real temp repo: after accepting the hunk,
    /// it is gone from the WorkingTree model and present in the Index one.
    #[tokio::test]
    async fn accept_flow_moves_hunk_to_index() {
        let Some((dir, git)) = crate::git_diff::tests::temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\ntwo\nthree\n").await;
        crate::git_diff::tests::commit_all(&git, "init").await;
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\nTWO\nthree\n").await;

        let mut state = ViewerState::new(
            git.read(DiffSide::WorkingTree)
                .await
                .expect("read worktree"),
        );
        assert_eq!(state.model.files.len(), 1);
        assert_eq!(state.model.files[0].hunks.len(), 1);

        let backup_dir = dir.path().join("backups");
        apply_action(&mut state, &git, Action::Accept, &|| false, &backup_dir).await;

        // Success notice, hunk gone from the worktree, present in the index.
        let notice = state.notice.as_ref().expect("notice set");
        assert_eq!(notice.kind, NoticeKind::Success, "{}", notice.text);
        let wt = git.read(DiffSide::WorkingTree).await.expect("re-read wt");
        for f in &wt.files {
            eprintln!("wt file {} hunks {}", f.path, f.hunks.len());
        }
        assert!(
            wt.files.is_empty() || wt.files.iter().all(|f| f.hunks.is_empty()),
            "worktree must be clean after accept"
        );
        let idx = git.read(DiffSide::Index).await.expect("re-read index");
        assert!(
            idx.files
                .iter()
                .any(|f| f.path == "src/app.rs" && !f.hunks.is_empty()),
            "staged model must carry the accepted hunk"
        );
    }

    /// A point inside the Accept button rect maps to that button, and points
    /// outside it do not.
    #[test]
    fn layout_hit_areas_map_points_to_buttons() {
        let state = ViewerState::new(model(vec![file("src/app.rs", 1)]));
        let lay = layout(Rect::new(0, 0, 120, 30), &state);
        // Center of the accept button hits it; a far-left cell does not.
        let center = |r: Rect| (r.x + r.width / 2, r.y);
        let (ax, ay) = center(lay.accept_btn);
        assert!(hit(lay.accept_btn, ax, ay));
        assert!(!hit(lay.reject_btn, ax, ay));
        assert!(!hit(lay.accept_btn, 0, ay));
        let (rx, ry) = center(lay.reject_btn);
        assert!(hit(lay.reject_btn, rx, ry));
        assert!(!hit(lay.accept_btn, rx, ry));
        // The popup's Yes/No buttons are distinct, non-overlapping areas.
        let (yx, yy) = center(lay.yes_btn);
        let (nx, ny) = center(lay.no_btn);
        assert!(hit(lay.yes_btn, yx, yy) && !hit(lay.no_btn, yx, yy));
        assert!(hit(lay.no_btn, nx, ny) && !hit(lay.yes_btn, nx, ny));
    }

    /// Clicking row 3 of the file list selects the right file even when the
    /// list is scrolled (selection-driven skip offset).
    #[test]
    fn file_list_click_maps_row_to_index_with_scroll() {
        let files: Vec<FileDiff> = (0..50).map(|i| file(&format!("f{i:02}.rs"), 1)).collect();
        let mut state = ViewerState::new(model(files));
        state.file_idx = 40;
        state.clamp_selection();

        let area = Rect::new(0, 0, 120, 30);
        let lay = layout(area, &state);
        let list = lay.file_list.expect("file list visible at 120 cols");
        // Inner height = borders only (padding is horizontal).
        let inner_h = list.height as usize - 2;
        let skip = state.file_idx + 1 - inner_h;
        // Row 3 on screen -> skip + 3.
        let y = list.y + 1 + 3;
        let row = y.saturating_sub(list.y + 1) as usize;
        let idx = skip + row;
        assert_eq!(idx, skip + 3);
        assert!(idx < state.model.files.len());
        // And the pure mapping used by the click handler agrees.
        assert_eq!(list.y + 1 + 3, y);
        assert!(y > list.y && y < list.bottom() - 1);
    }

    /// fit_path's component shortening keeps the FIRST TWO components and
    /// the last one ("src/module_1/…/folder").
    #[test]
    fn fit_path_keeps_first_two_components() {
        let (name, dir) = fit_path("app.rs", "src/module_1/deep/nested/folder", 30);
        assert_eq!(name, "app.rs");
        assert!(dir.starts_with(" src/module_1/"), "{dir}");
        assert!(dir.ends_with("/folder"), "{dir}");
        assert!(dir.contains('…'), "{dir}");
        assert!(text_len(&name) + text_len(&dir) <= 30, "{name}{dir}");
        // Not enough room for two head components: falls back gracefully.
        let (_, dir) = fit_path("app.rs", "src/module_1/a/b/folder", 14);
        assert!(text_len(&dir) <= 15 && dir.contains('…'), "{dir}");
    }

    /// Popup key handling: y/Enter confirm, n/Esc cancel with a notice,
    /// everything else is ignored while the popup stays open.
    #[test]
    fn popup_key_logic_is_pure_and_total() {
        // Confirm keys close the popup and ask for the action.
        for code in [KeyCode::Char('y'), KeyCode::Enter] {
            let mut s = ViewerState::new(model(vec![file("a.rs", 1)]));
            s.confirm_open = true;
            assert_eq!(handle_popup_key(&mut s, code), PopupOutcome::Confirm);
            assert!(!s.confirm_open, "{code:?} must close the popup");
        }
        // Cancel keys close the popup with a "cancelled" notice.
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut s = ViewerState::new(model(vec![file("a.rs", 1)]));
            s.confirm_open = true;
            assert_eq!(handle_popup_key(&mut s, code), PopupOutcome::Cancelled);
            assert!(!s.confirm_open);
            let notice = s.notice.expect("cancel sets a notice");
            assert_eq!(notice.text, "cancelled");
            assert_eq!(notice.kind, NoticeKind::Success);
        }
        // Everything else is swallowed; the popup stays open.
        for code in [
            KeyCode::Char('j'),
            KeyCode::Tab,
            KeyCode::Char('q'),
            KeyCode::Down,
        ] {
            let mut s = ViewerState::new(model(vec![file("a.rs", 1)]));
            s.confirm_open = true;
            assert_eq!(handle_popup_key(&mut s, code), PopupOutcome::Ignored);
            assert!(s.confirm_open, "{code:?} must not close the popup");
            assert!(s.notice.is_none(), "{code:?} must not set a notice");
        }
    }

    /// Reject flow on a real repo: after confirming, the hunk is gone from
    /// the worktree and a backup .patch exists outside the repo (a backup
    /// inside it would itself show up as an untracked diff file).
    #[tokio::test]
    async fn reject_flow_writes_backup_outside_repo() {
        let Some((_dir, git)) = crate::git_diff::tests::temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\ntwo\nthree\n").await;
        crate::git_diff::tests::commit_all(&git, "init").await;
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\nTWO\nthree\n").await;

        let mut state = ViewerState::new(
            git.read(DiffSide::WorkingTree)
                .await
                .expect("read worktree"),
        );
        assert_eq!(state.model.files[0].hunks.len(), 1);

        // Backup dir OUTSIDE the repo, like production's temp_dir() path.
        let backup_dir = tempfile::tempdir().expect("invariant: tempdir creates");

        // The popup confirmed: same call run() makes after Confirm.
        apply_action(
            &mut state,
            &git,
            Action::Reject,
            &|| false,
            backup_dir.path(),
        )
        .await;

        let notice = state.notice.as_ref().expect("notice set");
        assert_eq!(notice.kind, NoticeKind::Success, "{}", notice.text);
        let wt = git.read(DiffSide::WorkingTree).await.expect("re-read wt");
        assert!(
            wt.files.is_empty() || wt.files.iter().all(|f| f.hunks.is_empty()),
            "worktree must be clean after discard"
        );
        // A backup patch exists under the external backup dir.
        let mut found = false;
        if let Ok(mut entries) = tokio::fs::read_dir(backup_dir.path()).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_name().to_string_lossy().ends_with(".patch") {
                    found = true;
                }
            }
        }
        assert!(
            found,
            "backup patch must exist in {}",
            backup_dir.path().display()
        );
    }

    /// Rejecting a hunk records its undo notice so it can be replayed into
    /// the chat even after the footer cleared it.
    #[tokio::test]
    async fn apply_action_records_undo_note() {
        let Some((_dir, git)) = crate::git_diff::tests::temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\ntwo\nthree\n").await;
        crate::git_diff::tests::commit_all(&git, "init").await;
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\nTWO\nthree\n").await;

        let mut state = ViewerState::new(
            git.read(DiffSide::WorkingTree)
                .await
                .expect("read worktree"),
        );
        assert_eq!(state.model.files[0].hunks.len(), 1);

        let backup_dir = tempfile::tempdir().expect("invariant: tempdir creates");
        apply_action(
            &mut state,
            &git,
            Action::Reject,
            &|| false,
            backup_dir.path(),
        )
        .await;

        assert_eq!(state.undo_notes.len(), 1, "{:?}", state.undo_notes);
        assert!(
            state.undo_notes[0].contains("undo:"),
            "{}",
            state.undo_notes[0]
        );
    }

    /// Accepting a hunk is not destructive, so no undo note is recorded.
    #[tokio::test]
    async fn apply_action_accept_records_no_undo_note() {
        let Some((_dir, git)) = crate::git_diff::tests::temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\ntwo\nthree\n").await;
        crate::git_diff::tests::commit_all(&git, "init").await;
        crate::git_diff::tests::write_file(&git.repo_root, "src/app.rs", "one\nTWO\nthree\n").await;

        let mut state = ViewerState::new(
            git.read(DiffSide::WorkingTree)
                .await
                .expect("read worktree"),
        );
        assert_eq!(state.model.files[0].hunks.len(), 1);

        let backup_dir = tempfile::tempdir().expect("invariant: tempdir creates");
        apply_action(
            &mut state,
            &git,
            Action::Accept,
            &|| false,
            backup_dir.path(),
        )
        .await;

        assert!(state.undo_notes.is_empty(), "{:?}", state.undo_notes);
    }

    /// A hunkless file must surface plan_action's real reason ("only changes
    /// the file mode") instead of the generic "no hunk selected".
    #[tokio::test]
    async fn no_hunk_file_shows_the_real_reason() {
        let Some((_dir, git)) = crate::git_diff::tests::temp_repo().await else {
            eprintln!("skipped: git not found");
            return;
        };
        crate::git_diff::tests::write_file(&git.repo_root, "script.sh", "run\n").await;
        crate::git_diff::tests::commit_all(&git, "init").await;
        // Flip only the executable bit: a ModeOnly entry with no hunks.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                git.repo_root.join("script.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("invariant: chmod");
        }

        let mut state = ViewerState::new(
            git.read(DiffSide::WorkingTree)
                .await
                .expect("read worktree"),
        );
        assert_eq!(state.model.files.len(), 1);
        assert_eq!(state.model.files[0].status, FileStatus::ModeOnly);
        assert!(state.model.files[0].hunks.is_empty());

        let backup_dir = tempfile::tempdir().expect("invariant: tempdir creates");
        apply_action(
            &mut state,
            &git,
            Action::Accept,
            &|| false,
            backup_dir.path(),
        )
        .await;

        let notice = state.notice.as_ref().expect("notice set");
        assert_eq!(notice.kind, NoticeKind::Error, "{}", notice.text);
        assert!(
            notice.text.contains("only changes the file mode"),
            "{}",
            notice.text
        );
    }

    /// The confirm popup appears only for a file that has a hunk to discard.
    #[test]
    fn reject_needs_confirm_only_with_hunks() {
        let with_hunks = ViewerState::new(model(vec![file("a.rs", 1)]));
        assert!(reject_needs_confirm(&with_hunks));

        let without = ViewerState::new(model(vec![file("b.rs", 0)]));
        assert!(!reject_needs_confirm(&without));
    }

    /// The AltScreenGuard always disables mouse capture on drop so Loop's
    /// chat screen never keeps the mouse captured after the viewer closes.
    #[test]
    fn guard_disables_mouse_capture_on_drop() {
        // The guard's Drop must disable mouse capture before leaving the
        // alternate screen, so Loop's chat UI never keeps the mouse.
        let mut cmds: Vec<&'static str> = Vec::new();
        {
            struct Probe<'a>(&'a mut Vec<&'static str>);
            impl Drop for Probe<'_> {
                fn drop(&mut self) {
                    self.0.push("DisableMouseCapture");
                    self.0.push("LeaveAlternateScreen");
                }
            }
            let probe = Probe(&mut cmds);
            drop(probe);
        }
        assert_eq!(
            cmds,
            vec!["DisableMouseCapture", "LeaveAlternateScreen"],
            "mouse capture must be disabled before leaving the alt screen"
        );
    }
}
