//! Full-screen terminal dashboard over the usage summary.
//!
//! Runs on the terminal's alternate screen, so opening it from inside the chat (`/usage`)
//! leaves the transcript untouched, and `loop usage` uses the same view standalone.
//!
//! Views: **Now** (live status of the running loop, like Codex's `/status`: session,
//! context window, agents and subagents, skills, MCP servers, extensions), **Usage**
//! (tokens and activity over time), **Models**, **Tools**, **Agents** (main and subagent
//! runs, skills, MCP servers) and **Sessions**. History reloads every few seconds.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDate, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};

use crate::aggregate::{DayRow, LogCache, ModelRow, Summary, ToolRow, BUILT_IN};

/// How often the open dashboard re-reads the usage log.
const RELOAD_EVERY: Duration = Duration::from_secs(5);
/// Smallest terminal the layout is drawn for.
const MIN_SIZE: (u16, u16) = (60, 18);

/// Colors the dashboard draws with; the CLI maps its theme onto this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Primary text and values.
    pub text: Color,
    /// Labels and axis text.
    pub muted: Color,
    /// Hints and secondary notes.
    pub dim: Color,
    /// Panel borders.
    pub border: Color,
    /// Titles, the selected tab and highlights.
    pub accent: Color,
    /// First data series (input tokens, single-series charts).
    pub series_1: Color,
    /// Second data series (output tokens, subagents).
    pub series_2: Color,
    /// Things that are running or connected.
    pub good: Color,
    /// Background of the selected tab / row.
    pub selected_bg: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            text: Color::Reset,
            muted: Color::Gray,
            dim: Color::DarkGray,
            border: Color::DarkGray,
            accent: Color::Cyan,
            series_1: Color::Cyan,
            series_2: Color::Yellow,
            good: Color::Green,
            selected_bg: Color::Indexed(237),
        }
    }
}

/// State of the running loop, for the **Now** view; the chat refreshes it while open.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveStatus {
    /// When this snapshot was taken.
    pub taken_at: DateTime<Local>,
    /// Active model, e.g. `soket/sarvam-m`.
    pub model: String,
    /// Reasoning effort, if the model has one.
    pub effort: Option<String>,
    /// Tool permission mode.
    pub permission_mode: String,
    /// Sandbox state.
    pub sandbox: String,
    /// Web tools state.
    pub web: String,
    /// Working directory.
    pub cwd: String,
    /// Chat session id.
    pub session_id: String,
    /// Tokens in the current context, when known.
    pub context_tokens: Option<u64>,
    /// Context window of the model (0 when unknown).
    pub context_window: u64,
    /// Tokens used by this session so far.
    pub session_tokens: u64,
    /// Whether the chat's agent is working on a turn.
    pub main_agent_working: bool,
    /// What it is doing right now, e.g. `tool · bash` (empty when unknown).
    pub main_agent_step: String,
    /// Subscription agent serving the model (e.g. ChatGPT through `codex-acp`).
    pub external_agent: Option<String>,
    /// Workflow subagents of this session.
    pub sub_agents: Vec<SubAgent>,
    /// Skills available to the agent.
    pub skills_loaded: Vec<String>,
    /// Skills the user activated with `/skill`.
    pub skills_active: Vec<String>,
    /// Skills turned off in settings.
    pub skills_disabled: usize,
    /// Configured MCP servers.
    pub mcp_servers: Vec<McpServerStatus>,
    /// Loaded extensions.
    pub extensions: usize,
    /// Loaded hook files.
    pub hooks: usize,
    /// File of the anonymous usage log, while it is on.
    pub usage_log: Option<String>,
}

/// One workflow subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgent {
    /// Task id.
    pub id: String,
    /// What it was asked to do.
    pub description: String,
    /// Where it is.
    pub state: SubAgentState,
}

/// Progress of a subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentState {
    /// Still working.
    Running,
    /// Finished (successfully or not).
    Finished,
}

/// One configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    /// Server name.
    pub name: String,
    /// Tools it provides, while connected.
    pub tools: usize,
    /// Whether loop is connected to it.
    pub connected: bool,
}

/// What to show and how.
#[derive(Debug, Clone)]
pub struct Options {
    /// Folder holding the usage log (`loop-*.jsonl`).
    pub dir: PathBuf,
    /// Colors.
    pub palette: Palette,
    /// One-line notice under the header, e.g. that the usage log is off.
    pub notice: Option<String>,
    /// Live status, when opened from inside the chat.
    pub live: Option<LiveStatus>,
}

/// Run the dashboard standalone: takes over the terminal until the user quits.
pub fn run(options: Options) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let result = Dashboard::open(options).and_then(|mut dashboard| loop {
        dashboard.tick()?;
        if event::poll(Duration::from_millis(500))? && dashboard.handle_event(&event::read()?)? {
            return dashboard.close();
        }
    });
    let _ = execute!(io::stdout(), crossterm::cursor::Show);
    terminal::disable_raw_mode()?;
    result
}

/// The dashboard open on the alternate screen, driven by a host that keeps running its
/// own loop (the chat TUI): the host reads terminal events and passes them in, calls
/// [`tick`](Self::tick) to redraw, and refreshes the **Now** view with
/// [`set_live`](Self::set_live), so agents keep working and the live status stays current.
/// Leaves raw mode as it was; closing (or dropping) returns to the main screen.
pub struct Dashboard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    app: App,
    dirty: bool,
    closed: bool,
}

impl Dashboard {
    /// Switch to the alternate screen and load the usage log.
    pub fn open(options: Options) -> io::Result<Self> {
        execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide)?;
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                return Err(err);
            }
        };
        let mut app = App::new(options);
        app.reload();
        Ok(Self {
            terminal,
            app,
            dirty: true,
            closed: false,
        })
    }

    /// Handle one terminal event; returns `true` when the user closed the dashboard.
    pub fn handle_event(&mut self, event: &Event) -> io::Result<bool> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if self.app.handle_key(*key) == Action::Quit {
                    return Ok(true);
                }
                self.dirty = true;
            }
            Event::Resize(..) => {
                self.terminal.autoresize()?;
                self.dirty = true;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Replace the live status shown in the **Now** view.
    pub fn set_live(&mut self, live: LiveStatus) {
        if self.app.options.live.as_ref() != Some(&live) {
            self.app.options.live = Some(live);
            self.dirty = true;
        }
    }

    /// Re-read the usage log when due, and redraw if anything changed.
    pub fn tick(&mut self) -> io::Result<()> {
        if self.app.loaded_at.elapsed() >= RELOAD_EVERY {
            self.dirty |= self.app.reload();
        }
        if self.dirty {
            self.terminal.draw(|frame| self.app.render(frame))?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Return to the main screen.
    pub fn close(mut self) -> io::Result<()> {
        self.closed = true;
        execute!(io::stdout(), LeaveAlternateScreen)
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        if !self.closed {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
    }
}

/// Dashboard views, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Now,
    Usage,
    Models,
    Tools,
    Agents,
    Sessions,
}

impl View {
    const ALL: [Self; 6] = [
        Self::Now,
        Self::Usage,
        Self::Models,
        Self::Tools,
        Self::Agents,
        Self::Sessions,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Now => "Now",
            Self::Usage => "Usage",
            Self::Models => "Models",
            Self::Tools => "Tools",
            Self::Agents => "Agents",
            Self::Sessions => "Sessions",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }

    /// Whether the view shows history (and so the period selector applies).
    fn is_history(self) -> bool {
        self != Self::Now
    }
}

/// Period shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Day,
    Week,
    Month,
    All,
}

impl Range {
    const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::All];

    fn label(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "all",
        }
    }

    /// Days passed to [`aggregate::summarize_dir`]; `None` for everything retained.
    pub fn days(self) -> Option<u32> {
        match self {
            Self::Day => Some(1),
            Self::Week => Some(7),
            Self::Month => Some(30),
            Self::All => None,
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Continue,
    Quit,
}

struct App {
    options: Options,
    view: View,
    range: Range,
    summary: Summary,
    loaded_at: Instant,
    /// Parsed log files, re-read only when they change.
    cache: LogCache,
    table: TableState,
}

impl App {
    fn new(options: Options) -> Self {
        let view = if options.live.is_some() {
            View::Now
        } else {
            View::Usage
        };
        Self {
            options,
            view,
            range: Range::Month,
            summary: Summary::default(),
            loaded_at: Instant::now(),
            cache: LogCache::default(),
            table: TableState::default().with_selected(Some(0)),
        }
    }

    /// Re-read the log files that changed and summarize again, so rolling periods move
    /// forward even when nothing new was written. Returns whether anything changed.
    fn reload(&mut self) -> bool {
        self.cache.refresh(&self.options.dir);
        self.resummarize()
    }

    /// Summarize the cached log for the current period; returns whether anything changed.
    fn resummarize(&mut self) -> bool {
        let mut summary = self.cache.summarize(self.range.days(), Utc::now());
        self.loaded_at = Instant::now();
        // `generated_at` always moves; compare the data only.
        summary.generated_at = self.summary.generated_at;
        let changed = summary != self.summary;
        summary.generated_at = Utc::now();
        self.summary = summary;
        changed
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let views = View::ALL.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('c') if ctrl => return Action::Quit,
            KeyCode::Tab => self.set_view(View::ALL[(self.view.index() + 1) % views]),
            KeyCode::BackTab => self.set_view(View::ALL[(self.view.index() + views - 1) % views]),
            KeyCode::Char(c @ '1'..='6') => self.set_view(View::ALL[c as usize - '1' as usize]),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(']') => {
                self.set_range(Range::ALL[(self.range.index() + 1).min(3)]);
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => {
                self.set_range(Range::ALL[self.range.index().saturating_sub(1)]);
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::PageDown => self.scroll(10),
            KeyCode::PageUp => self.scroll(-10),
            KeyCode::Home | KeyCode::Char('g') => self.table.select(Some(0)),
            KeyCode::Char('r') => {
                self.reload();
            }
            _ => {}
        }
        Action::Continue
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.table.select(Some(0));
    }

    fn set_range(&mut self, range: Range) {
        if range != self.range {
            self.range = range;
            self.table.select(Some(0));
            self.resummarize();
        }
    }

    fn rows_in_view(&self) -> usize {
        match self.view {
            View::Models => self.summary.models.len(),
            View::Tools => self.summary.tools.len(),
            View::Sessions => self.summary.sessions.len(),
            View::Now | View::Usage | View::Agents => 0,
        }
    }

    fn scroll(&mut self, delta: isize) {
        let rows = self.rows_in_view();
        if rows == 0 {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, rows as isize - 1);
        self.table.select(Some(next as usize));
    }

    // ---------------------------------------------------------------- frame

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < MIN_SIZE.0 || area.height < MIN_SIZE.1 {
            let msg = Paragraph::new(vec![
                Line::from("Terminal too small for the usage view".bold()),
                Line::from(format!(
                    "Resize to at least {}×{} · q to close",
                    MIN_SIZE.0, MIN_SIZE.1
                ))
                .fg(self.p().muted),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
            frame.render_widget(msg, centered_rows(area, 2));
            return;
        }
        let notice_h = u16::from(self.options.notice.is_some());
        let [header, notice, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(notice_h),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        self.render_header(frame, header);
        if let Some(text) = &self.options.notice {
            frame.render_widget(
                Paragraph::new(format!(" {text}")).fg(self.p().series_2),
                notice,
            );
        }
        if self.view.is_history() && self.is_empty() {
            self.render_empty(frame, body);
        } else {
            match self.view {
                View::Now => self.render_now(frame, body),
                View::Usage => self.render_usage(frame, body),
                View::Models => self.render_models(frame, body),
                View::Tools => self.render_tools(frame, body),
                View::Agents => self.render_agents(frame, body),
                View::Sessions => self.render_sessions(frame, body),
            }
        }
        self.render_footer(frame, footer);
    }

    fn p(&self) -> Palette {
        self.options.palette
    }

    fn is_empty(&self) -> bool {
        let t = &self.summary.totals;
        t.runs + t.sub_agent_runs + t.llm_calls + t.tool_calls == 0
    }

    fn panel(&self, title: &str) -> Block<'static> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.p().border))
            .title(Line::from(format!(" {title} ")).fg(self.p().text).bold())
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let range_w = 26;
        // Narrow terminals: no "usage" title, and numbers only except the selected view.
        let full_width =
            16 + View::ALL.iter().map(|v| v.label().len() + 4).sum::<usize>() + range_w;
        let compact = (area.width as usize) < full_width;
        let mut left = vec![Span::from(" ◆ loop ").fg(p.accent).bold()];
        if !compact {
            left.push(Span::from("usage").fg(p.text).bold());
            left.push(Span::from("   "));
        }
        for (i, view) in View::ALL.iter().enumerate() {
            let label = if compact && *view != self.view {
                format!(" {} ", i + 1)
            } else {
                format!(" {} {} ", i + 1, view.label())
            };
            left.push(if *view == self.view {
                Span::from(label).fg(p.accent).bg(p.selected_bg).bold()
            } else {
                Span::from(label).fg(p.muted)
            });
        }
        let right: Vec<Span> = if self.view.is_history() {
            let mut spans = vec![Span::from("◀ ").fg(p.dim)];
            for range in Range::ALL {
                let label = format!(" {} ", range.label());
                spans.push(if range == self.range {
                    Span::from(label).fg(p.accent).bg(p.selected_bg).bold()
                } else {
                    Span::from(label).fg(p.muted)
                });
            }
            spans.push(Span::from(" ▶ ").fg(p.dim));
            spans
        } else {
            let at = self
                .options
                .live
                .as_ref()
                .map(|l| l.taken_at.format("%H:%M:%S").to_string());
            match at {
                Some(at) => vec![
                    Span::from("● ").fg(p.good),
                    Span::from(format!("live · {at} ")).fg(p.dim),
                ],
                None => vec![],
            }
        };
        let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(range_w as u16)])
            .areas(Rect { height: 1, ..area });
        frame.render_widget(Line::from(left), l);
        frame.render_widget(Line::from(right).alignment(Alignment::Right), r);
        let rule = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).fg(p.border),
            rule,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let hints = [
            ("1-6", "view"),
            ("←→", "period"),
            ("↑↓", "scroll"),
            ("r", "refresh"),
            ("q", "close"),
        ];
        let mut spans = vec![Span::from(" ")];
        for (key, what) in hints {
            spans.push(Span::from(key).fg(p.accent).bold());
            spans.push(Span::from(format!(" {what}   ")).fg(p.dim));
        }
        let s = &self.summary;
        let updated = s.generated_at.with_timezone(&Local).format("%H:%M:%S");
        let hints_w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let room = (area.width as usize).saturating_sub(hints_w);
        let status = [
            format!(
                "anonymous · local only · {} file{} · updated {updated} ",
                s.files,
                if s.files == 1 { "" } else { "s" },
            ),
            format!("updated {updated} "),
        ]
        .into_iter()
        .find(|text| text.chars().count() <= room)
        .unwrap_or_default();
        let [l, r] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(status.chars().count() as u16),
        ])
        .areas(area);
        frame.render_widget(Line::from(spans), l);
        frame.render_widget(Line::from(status).fg(p.dim), r);
    }

    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let lines = vec![
            Line::from("No usage recorded in this period".bold().fg(p.text)),
            Line::from(""),
            Line::from("Loop keeps an anonymous usage log on this machine while you chat.")
                .fg(p.muted),
            Line::from(format!("Reading {}", self.options.dir.display())).fg(p.dim),
            Line::from(""),
            Line::from(vec![
                Span::from("←→").fg(p.accent).bold(),
                Span::from(" try a longer period").fg(p.dim),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            centered_rows(area, 6),
        );
    }

    // ---------------------------------------------------------------- now

    fn render_now(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let Some(live) = &self.options.live else {
            let lines = vec![
                Line::from("Live status needs a running loop".bold().fg(p.text)),
                Line::from(""),
                Line::from(vec![
                    Span::from("Open it with ").fg(p.muted),
                    Span::from("/usage").fg(p.accent),
                    Span::from(" inside loop to see the session, context window,").fg(p.muted),
                ]),
                Line::from("agents and subagents, skills and MCP servers.").fg(p.muted),
                Line::from(""),
                Line::from(vec![
                    Span::from("2").fg(p.accent).bold(),
                    Span::from(" Usage shows the history of every session.").fg(p.dim),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                centered_rows(area, 6),
            );
            return;
        };
        let [top, bottom] =
            Layout::vertical([Constraint::Min(10), Constraint::Length(4)]).areas(area);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
        let [session, context] =
            Layout::vertical([Constraint::Min(8), Constraint::Length(5)]).areas(left);
        let agents_h = (live.sub_agents.len().min(6) as u16 + 5).min(right.height / 2 + 2);
        let [agents, capabilities] =
            Layout::vertical([Constraint::Length(agents_h), Constraint::Min(6)]).areas(right);
        self.render_session(frame, session, live);
        self.render_context(frame, context, live);
        self.render_live_agents(frame, agents, live);
        self.render_capabilities(frame, capabilities, live);
        self.render_today(frame, bottom);
    }

    fn render_session(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let model = match &live.effort {
            Some(effort) => format!("{} · effort {effort}", live.model),
            None => live.model.clone(),
        };
        let rows = [
            ("Model", model),
            ("Directory", live.cwd.clone()),
            ("Permissions", live.permission_mode.clone()),
            ("Sandbox", live.sandbox.clone()),
            ("Web tools", live.web.clone()),
            ("Session", live.session_id.clone()),
            (
                "Usage log",
                live.usage_log
                    .as_ref()
                    .map_or("not available in this build".into(), |path| {
                        format!("anonymous, local only · {path}")
                    }),
            ),
        ];
        self.key_values(frame, area, "Session", &rows);
    }

    fn key_values(&self, frame: &mut Frame, area: Rect, title: &str, rows: &[(&str, String)]) {
        let p = self.p();
        let block = self.panel(title);
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 2;
        let value_w = (inner.width as usize).saturating_sub(key_w);
        let lines: Vec<Line> = rows
            .iter()
            .map(|(key, value)| {
                Line::from(vec![
                    Span::from(format!("{key:<key_w$}")).fg(p.muted),
                    Span::from(truncate(value, value_w)).fg(p.text),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_context(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let p = self.p();
        let block = self.panel("Context window");
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let mut lines = Vec::new();
        match (live.context_tokens, live.context_window) {
            (Some(used), window) if window > 0 => {
                let fraction = used as f64 / window as f64;
                let label = format!(
                    " {:.0}% used · {} of {}",
                    100.0 * fraction,
                    compact(used),
                    compact(window)
                );
                let bar_w = (inner.width as usize).saturating_sub(label.chars().count());
                let color = if fraction >= 0.8 {
                    p.series_2
                } else {
                    p.series_1
                };
                lines.push(Line::from(vec![
                    Span::from(gauge(fraction, bar_w)).fg(color),
                    Span::from(label).fg(p.text),
                ]));
                lines.push(
                    Line::from(format!(
                        "{} left before the conversation is compacted",
                        compact(window.saturating_sub(used))
                    ))
                    .fg(p.dim),
                );
            }
            (_, window) if window > 0 => lines.push(
                Line::from(format!(
                    "{} window · fills as the conversation grows",
                    compact(window)
                ))
                .fg(p.muted),
            ),
            _ => lines.push(Line::from("Context window unknown for this model").fg(p.muted)),
        }
        lines.push(Line::from(vec![
            Span::from("Session tokens ").fg(p.muted),
            Span::from(thousands(live.session_tokens)).fg(p.text).bold(),
        ]));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_live_agents(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let p = self.p();
        let running = live
            .sub_agents
            .iter()
            .filter(|a| a.state == SubAgentState::Running)
            .count();
        let finished = live.sub_agents.len() - running;
        let active = usize::from(live.main_agent_working) + running;
        let block = self.panel(&format!("Agents · {active} running"));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let status = |on: bool, label: &str| {
            if on {
                Span::from(format!("● {label}")).fg(p.good).bold()
            } else {
                Span::from(format!("○ {label}")).fg(p.dim)
            }
        };
        let mut lines = vec![Line::from(vec![
            Span::from(format!("{:<13}", "Main agent")).fg(p.muted),
            if live.main_agent_working {
                let step = live.main_agent_step.trim();
                let label = if step.is_empty() {
                    "working".to_string()
                } else {
                    let room = (inner.width as usize).saturating_sub(13 + 12);
                    format!("working · {}", truncate(step, room))
                };
                status(true, &label)
            } else {
                status(false, "idle")
            },
        ])];
        if let Some(external) = &live.external_agent {
            lines.push(Line::from(vec![
                Span::from(format!("{:<13}", "Subscription")).fg(p.muted),
                Span::from(external.clone()).fg(p.text),
            ]));
        }
        lines.push(Line::from(vec![
            Span::from(format!("{:<13}", "Subagents")).fg(p.muted),
            Span::from(format!("{running} running")).fg(if running > 0 { p.good } else { p.text }),
            Span::from(format!(" · {finished} finished")).fg(p.muted),
        ]));
        let width = inner.width as usize;
        let mut agents: Vec<&SubAgent> = live.sub_agents.iter().collect();
        agents.sort_by_key(|a| a.state != SubAgentState::Running);
        for agent in agents {
            let (mark, color) = match agent.state {
                SubAgentState::Running => ("▶", p.good),
                SubAgentState::Finished => ("✓", p.dim),
            };
            let id = truncate(&agent.id, 10);
            let desc_w = width.saturating_sub(id.chars().count() + 5);
            lines.push(Line::from(vec![
                Span::from(format!("  {mark} ")).fg(color),
                Span::from(format!("{id} ")).fg(p.muted),
                Span::from(truncate(&agent.description, desc_w)).fg(p.text),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_capabilities(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let p = self.p();
        let block = self.panel("Skills · MCP · extensions");
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let width = inner.width as usize;
        let label = |text: &str| Span::from(format!("{text:<13}")).fg(p.muted);
        let mut skills = format!("{} loaded", live.skills_loaded.len());
        if !live.skills_active.is_empty() {
            skills.push_str(&format!(" · active: {}", live.skills_active.join(", ")));
        }
        if live.skills_disabled > 0 {
            skills.push_str(&format!(" · {} off", live.skills_disabled));
        }
        let connected = live.mcp_servers.iter().filter(|s| s.connected).count();
        let tools: usize = live.mcp_servers.iter().map(|s| s.tools).sum();
        let mut lines = vec![
            Line::from(vec![
                label("Skills"),
                Span::from(truncate(&skills, width.saturating_sub(13))).fg(p.text),
            ]),
            Line::from(vec![
                label("MCP servers"),
                Span::from(format!(
                    "{connected} of {} connected · {tools} tools",
                    live.mcp_servers.len()
                ))
                .fg(p.text),
            ]),
        ];
        let name_w = live
            .mcp_servers
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(20);
        for server in &live.mcp_servers {
            let (mark, color, detail) = if server.connected {
                ("●", p.good, format!("{} tools", server.tools))
            } else {
                ("○", p.dim, "not connected".to_string())
            };
            lines.push(Line::from(vec![
                Span::from(format!("  {mark} ")).fg(color),
                Span::from(format!("{:<name_w$} ", truncate(&server.name, name_w))).fg(p.text),
                Span::from(detail).fg(p.muted),
            ]));
        }
        lines.push(Line::from(vec![
            label("Extensions"),
            Span::from(live.extensions.to_string()).fg(p.text),
            Span::from("   hooks ").fg(p.muted),
            Span::from(live.hooks.to_string()).fg(p.text),
        ]));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// Today's totals from the usage log, under the live panels.
    fn render_today(&self, frame: &mut Frame, area: Rect) {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let day = self
            .summary
            .days
            .iter()
            .find(|d| d.date == today)
            .cloned()
            .unwrap_or_default();
        let tiles = [
            Tile::new(
                "Tokens today",
                compact(day.input_tokens + day.output_tokens),
                String::new(),
            ),
            Tile::new("Model calls", thousands(day.llm_calls), String::new()),
            Tile::new("Tool calls", thousands(day.tool_calls), String::new()),
            Tile::new(
                "Agent runs",
                format!(
                    "{} · {} sub",
                    thousands(day.runs),
                    thousands(day.sub_agent_runs)
                ),
                String::new(),
            ),
        ];
        self.tile_row(frame, area, &tiles);
    }

    // ---------------------------------------------------------------- usage

    fn render_usage(&self, frame: &mut Frame, area: Rect) {
        let tiles_h = if area.width >= 108 { 4 } else { 7 };
        let bottom_h = if area.height >= 30 { 10 } else { 8 };
        let [tiles, charts, bottom] = Layout::vertical([
            Constraint::Length(tiles_h),
            Constraint::Min(8),
            Constraint::Length(bottom_h),
        ])
        .areas(area);
        self.render_usage_tiles(frame, tiles);

        let [tokens, activity] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(charts);
        let days = fill_days(&self.summary.days, self.range, Local::now().date_naive());
        let peak = days
            .iter()
            .max_by_key(|d| d.input + d.output)
            .filter(|d| d.input + d.output > 0);
        let title = match peak {
            Some(d) => format!(
                "Tokens per day · peak {} on {}",
                compact(d.input + d.output),
                d.label
            ),
            None => "Tokens per day".into(),
        };
        self.daily_histogram(
            frame,
            tokens,
            &title,
            "Tokens",
            &days,
            &[
                Series {
                    name: "input",
                    value: |d| d.input as f64,
                },
                Series {
                    name: "output",
                    value: |d| d.output as f64,
                },
            ],
        );
        self.daily_histogram(
            frame,
            activity,
            "Calls per day",
            "Calls",
            &days,
            &[
                Series {
                    name: "model",
                    value: |d| d.llm_calls as f64,
                },
                Series {
                    name: "tool",
                    value: |d| d.tool_calls as f64,
                },
            ],
        );

        let [hours, tools, models] = Layout::horizontal([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .areas(bottom);
        self.render_hours(frame, hours);
        self.render_top_tools(frame, tools);
        self.render_top_models(frame, models);
    }

    fn render_usage_tiles(&self, frame: &mut Frame, area: Rect) {
        let t = &self.summary.totals;
        let cached_share = if t.input_tokens > 0 {
            format!(
                "{:.0}% of input",
                100.0 * t.cache_read_tokens as f64 / t.input_tokens as f64
            )
        } else {
            String::new()
        };
        let tiles = [
            Tile::new(
                "Tokens",
                compact(t.input_tokens + t.output_tokens),
                format!(
                    "{} in · {} out",
                    compact(t.input_tokens),
                    compact(t.output_tokens)
                ),
            ),
            Tile::new("Cached", compact(t.cache_read_tokens), cached_share),
            Tile::new(
                "Model calls",
                thousands(t.llm_calls),
                format!("avg {}", duration(weighted_latency(&self.summary))),
            ),
            Tile::new(
                "Tool calls",
                thousands(t.tool_calls),
                format!("{} via MCP", thousands(t.mcp_calls)),
            ),
            Tile::new(
                "Agent runs",
                thousands(t.runs + t.sub_agent_runs),
                format!("{} subagents", thousands(t.sub_agent_runs)),
            ),
            Tile::new(
                "Sessions",
                thousands(t.sessions),
                format!("{} skill uses", thousands(t.skill_uses)),
            ),
        ];
        self.tile_row(frame, area, &tiles);
    }

    /// Bordered stat tiles: one row, or two rows when the area is tall enough.
    fn tile_row(&self, frame: &mut Frame, area: Rect, tiles: &[Tile]) {
        let p = self.p();
        let rows: Vec<Rect> = if area.height >= 7 && tiles.len() > 3 {
            Layout::vertical([Constraint::Length(4), Constraint::Length(3)])
                .split(area)
                .to_vec()
        } else {
            vec![area]
        };
        let per_row = tiles.len().div_ceil(rows.len());
        for (r, row) in rows.iter().enumerate() {
            let cells =
                Layout::horizontal(vec![Constraint::Ratio(1, per_row as u32); per_row]).split(*row);
            for (i, cell) in cells.iter().enumerate() {
                let Some(Tile { label, value, note }) = tiles.get(r * per_row + i) else {
                    continue;
                };
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(p.border))
                    .title(Line::from(format!(" {label} ")).fg(p.muted));
                let mut lines = vec![Line::from(Span::from(value.clone()).fg(p.text).bold())];
                if !note.is_empty() {
                    if cell.height >= 4 {
                        lines.push(Line::from(note.clone()).fg(p.dim));
                    } else {
                        lines[0].push_span(Span::from(format!("  {note}")).fg(p.dim));
                    }
                }
                frame.render_widget(Paragraph::new(lines).block(block), *cell);
            }
        }
    }

    /// Stacked histogram of daily series: one bar per day, named axes.
    fn daily_histogram(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        y_name: &str,
        days: &[DayPoint],
        series: &[Series],
    ) {
        let p = self.p();
        let colors = [p.series_1, p.series_2];
        let last = days.len().saturating_sub(1);
        self.histogram(
            frame,
            area,
            &Histogram {
                title,
                y_name,
                x_name: "Day",
                series: &series
                    .iter()
                    .zip(colors)
                    .map(|(series, color)| Legend {
                        name: series.name,
                        color,
                    })
                    .collect::<Vec<_>>(),
                bars: &days
                    .iter()
                    .map(|d| {
                        series
                            .iter()
                            .map(|series| (series.value)(d) as u64)
                            .collect()
                    })
                    .collect::<Vec<_>>(),
                labels: &days.iter().map(|d| d.label.clone()).collect::<Vec<_>>(),
                ticks: &[0, last / 2, last],
            },
        );
    }

    /// Bar chart drawn cell by cell: series stacked bottom to top in eighth-block steps,
    /// y axis with its name and 0 / half / max ticks, x axis with its name and tick labels.
    fn histogram(&self, frame: &mut Frame, area: Rect, h: &Histogram) {
        const LOWER: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        let p = self.p();
        let block = self.panel(h.title);
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        if inner.height < 4 || inner.width < 16 || h.bars.is_empty() {
            return;
        }
        let max_total = h
            .bars
            .iter()
            .map(|b| b.iter().sum::<u64>())
            .max()
            .unwrap_or(0);
        let max = nice_max(max_total as f64);
        let ticks = [compact_f(max), compact_f(max / 2.0), compact_f(0.0)];
        let gutter = ticks
            .iter()
            .map(|t| t.chars().count())
            .chain([h.y_name.chars().count()])
            .max()
            .unwrap_or(0) as u16;
        // Rows: names/legend, plot, x axis line, x tick labels.
        let legend_y = inner.y;
        let axis_y = inner.bottom() - 2;
        let label_y = inner.bottom() - 1;
        let plot_top = inner.y + 1;
        let plot_h = axis_y - plot_top;
        let plot_x = inner.x + gutter + 1;
        let plot_w = inner.right().saturating_sub(plot_x);
        if plot_h == 0 || plot_w == 0 {
            return;
        }
        let buf = frame.buffer_mut();
        let muted = Style::new().fg(p.muted);
        let axis = Style::new().fg(p.border);

        // Y axis name, then the legend after it.
        buf.set_string(
            inner.x,
            legend_y,
            h.y_name,
            muted.add_modifier(Modifier::BOLD),
        );
        if h.series.len() > 1 {
            let mut x = plot_x + 1;
            for Legend { name, color } in h.series {
                if x + 3 + name.chars().count() as u16 > inner.right() {
                    break;
                }
                buf.set_string(x, legend_y, "■", Style::new().fg(*color));
                buf.set_string(x + 2, legend_y, name, muted);
                x += 2 + name.chars().count() as u16 + 3;
            }
        }

        // X axis line (its name goes at the right end, below).
        buf.set_string(
            plot_x - 1,
            axis_y,
            format!("└{}", "─".repeat(plot_w as usize)),
            axis,
        );

        // Y axis: line with max, half and zero ticks.
        for y in plot_top..axis_y {
            buf.set_string(plot_x - 1, y, "│", axis);
        }
        // Max at the top edge, half at the middle row, zero on the x axis itself.
        let tick_rows = [plot_top, plot_top + plot_h / 2, axis_y];
        for (i, (tick, y)) in ticks.iter().zip(tick_rows).enumerate() {
            // Too short for a separate half-way row.
            if i == 1 && plot_h < 3 {
                continue;
            }
            let x = inner.x + gutter - tick.chars().count() as u16;
            buf.set_string(x, y, tick, muted);
            if y != axis_y {
                buf.set_string(plot_x - 1, y, "┤", axis);
            }
        }

        // X axis name at the right end of the axis line.
        let x_name = format!(" {} ", h.x_name);
        let x_name_w = x_name.chars().count() as u16;
        if x_name_w + 4 < plot_w {
            buf.set_string(
                inner.right() - x_name_w,
                axis_y,
                &x_name,
                muted.add_modifier(Modifier::BOLD),
            );
        }

        // Bars.
        let n = h.bars.len() as u16;
        let slot = |i: u16| plot_x + (u32::from(i) * u32::from(plot_w) / u32::from(n)) as u16;
        let mut last_label_end = 0u16;
        for (i, values) in h.bars.iter().enumerate() {
            let i = i as u16;
            let (x0, x1) = (slot(i), slot(i + 1).max(slot(i) + 1));
            let gap = u16::from(x1 - x0 >= 3);
            let width = (x1 - x0 - gap).max(1);
            // Cumulative heights in eighths of a row.
            let scale = f64::from(plot_h) * 8.0 / max;
            let mut bounds = Vec::with_capacity(values.len());
            let mut sum = 0u64;
            for v in values {
                sum += v;
                let mut eighths = (sum as f64 * scale).round() as u32;
                if sum > 0 && eighths == 0 {
                    eighths = 1;
                }
                bounds.push(eighths);
            }
            let top = bounds.last().copied().unwrap_or(0);
            for row in 0..u32::from(plot_h) {
                let (lo, hi) = (row * 8, row * 8 + 8);
                if top <= lo {
                    break;
                }
                // Series covering the bottom of this cell, and where it ends.
                let s = bounds.iter().position(|b| *b > lo).unwrap_or(0);
                let color = h.series.get(s).map_or(p.series_1, |l| l.color);
                let (symbol, style) = if bounds[s] >= hi {
                    ("█", Style::new().fg(color))
                } else if top <= hi || s + 1 >= bounds.len() {
                    (LOWER[(top.min(hi) - lo) as usize], Style::new().fg(color))
                } else {
                    // Two series meet inside this cell: lower one in front, upper behind.
                    let upper = h.series.get(s + 1).map_or(color, |l| l.color);
                    (
                        LOWER[(bounds[s] - lo) as usize],
                        Style::new().fg(color).bg(upper),
                    )
                };
                let y = axis_y - 1 - row as u16;
                for x in x0..(x0 + width).min(inner.right()) {
                    buf[(x, y)].set_symbol(symbol).set_style(style);
                }
            }
            // X tick label centred under its bar, when it does not collide.
            if h.ticks.contains(&usize::from(i)) {
                let label = &h.labels[usize::from(i)];
                let len = label.chars().count() as u16;
                let start = (x0 + width / 2)
                    .saturating_sub(len / 2)
                    .clamp(plot_x, inner.right().saturating_sub(len));
                if start >= last_label_end {
                    buf.set_string(start, label_y, label, muted);
                    last_label_end = start + len + 1;
                }
            }
        }
    }

    fn render_hours(&self, frame: &mut Frame, area: Rect) {
        let hours = &self.summary.hours;
        let busiest = (0..24).max_by_key(|h| hours[*h]).unwrap_or(0);
        let title = if area.width >= 42 {
            format!("Model calls by hour · busiest {busiest:02}:00")
        } else {
            format!("By hour · peak {busiest:02}:00")
        };
        self.histogram(
            frame,
            area,
            &Histogram {
                title: &title,
                y_name: "Calls",
                x_name: "Hour",
                series: &[Legend {
                    name: "model calls",
                    color: self.p().series_1,
                }],
                bars: &hours.iter().map(|n| vec![*n]).collect::<Vec<_>>(),
                labels: &(0..24).map(|h| format!("{h:02}")).collect::<Vec<_>>(),
                ticks: &[0, 6, 12, 18],
            },
        );
    }

    /// Ranked horizontal bars in aligned columns: label, bar, value, note.
    fn bar_list(&self, frame: &mut Frame, area: Rect, title: &str, items: &[BarItem]) {
        let p = self.p();
        let block = self.panel(title);
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        if items.is_empty() {
            frame.render_widget(Paragraph::new("Nothing in this period").fg(p.dim), inner);
            return;
        }
        let width = inner.width as usize;
        let widest = |f: fn(&BarItem) -> &str| {
            items
                .iter()
                .map(|i| f(i).chars().count())
                .max()
                .unwrap_or(0)
        };
        let label_w = widest(|i| &i.label).clamp(4, 24).min(width / 2);
        let value_w = widest(|i| &i.value_text);
        let note_w = match widest(|i| &i.note) {
            0 => 0,
            w => w + 1,
        };
        let mut bar_w = width.saturating_sub(label_w + 2 + value_w + note_w);
        let note_w = if bar_w < 4 { 0 } else { note_w };
        if note_w == 0 {
            bar_w = width.saturating_sub(label_w + 2 + value_w);
        }
        let max = items
            .iter()
            .map(|i| i.value)
            .fold(0.0, f64::max)
            .max(f64::EPSILON);
        let lines: Vec<Line> = items
            .iter()
            .take(inner.height as usize)
            .map(|i| {
                Line::from(vec![
                    Span::from(format!("{:<label_w$} ", truncate(&i.label, label_w))).fg(p.muted),
                    Span::from(format!("{:<bar_w$}", bar(i.value / max, bar_w))).fg(p.series_1),
                    Span::from(format!(" {:>value_w$}", i.value_text)).fg(p.text),
                    Span::from(if note_w > 0 {
                        format!("{:>note_w$}", i.note)
                    } else {
                        String::new()
                    })
                    .fg(p.dim),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_top_tools(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<BarItem> = self
            .summary
            .tools
            .iter()
            .map(|t| BarItem::new(&t.tool, t.calls as f64, thousands(t.calls)))
            .collect();
        self.bar_list(frame, area, "Top tools", &items);
    }

    fn render_top_models(&self, frame: &mut Frame, area: Rect) {
        let total =
            (self.summary.totals.input_tokens + self.summary.totals.output_tokens).max(1) as f64;
        let mut models: Vec<&ModelRow> = self.summary.models.iter().collect();
        models.sort_by_key(|m| std::cmp::Reverse(m.input_tokens + m.output_tokens));
        let items: Vec<BarItem> = models
            .iter()
            .map(|m| {
                let tokens = m.input_tokens + m.output_tokens;
                BarItem::new(&m.model, tokens as f64, compact(tokens))
                    .note(format!("{:.0}%", 100.0 * tokens as f64 / total))
            })
            .collect();
        self.bar_list(frame, area, "Models by tokens", &items);
    }

    // ---------------------------------------------------------------- tables

    /// A table that keeps the columns that fit `width`, dropping the lowest `keep` first.
    fn table(
        &self,
        title: String,
        columns: &[Column],
        rows: Vec<Vec<Cell<'static>>>,
        width: u16,
    ) -> Table<'static> {
        let p = self.p();
        let kept = fit_columns(columns, width);
        let widths: Vec<Constraint> = kept
            .iter()
            .map(|&i| match columns[i].fill {
                true => Constraint::Min(columns[i].width),
                false => Constraint::Length(columns[i].width),
            })
            .collect();
        let header = Row::new(kept.iter().map(|&i| {
            let h = columns[i].header;
            match h.strip_prefix('>') {
                Some(text) => Cell::from(Line::from(text).alignment(Alignment::Right)),
                None => Cell::from(h),
            }
        }))
        .style(Style::new().fg(p.muted).add_modifier(Modifier::BOLD))
        .bottom_margin(1);
        let rows: Vec<Row> = rows
            .into_iter()
            .map(|cells| {
                Row::new(
                    cells
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| kept.contains(i))
                        .map(|(_, cell)| cell),
                )
            })
            .collect();
        Table::new(rows, widths)
            .header(header)
            .block(self.panel(&title))
            .column_spacing(2)
            .flex(Flex::Start)
            .row_highlight_style(Style::new().bg(p.selected_bg))
            .highlight_symbol("▌ ")
    }

    /// Table on top, sized to its rows; comparison panels (one row per item) below when
    /// there is room.
    fn split_table_area(area: Rect, rows: usize, panels: usize) -> (Rect, Vec<Rect>) {
        let table_h = (rows as u16 + 4).max(6);
        if area.height < table_h + 5 {
            return (area, Vec::new());
        }
        let table_h = table_h.min(area.height / 2).max(6);
        let panel_h = (rows as u16 + 2).max(4).min(area.height - table_h);
        let [table, rest, _] = Layout::vertical([
            Constraint::Length(table_h),
            Constraint::Length(panel_h),
            Constraint::Min(0),
        ])
        .areas(area);
        let cells = Layout::horizontal(vec![Constraint::Ratio(1, panels as u32); panels])
            .split(rest)
            .to_vec();
        (table, cells)
    }

    fn render_models(&mut self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let models = &self.summary.models;
        let total_tokens =
            (self.summary.totals.input_tokens + self.summary.totals.output_tokens).max(1) as f64;
        let columns = [
            Column::text(
                "Model",
                column_width(models.iter().map(|m| m.model.as_str()), "Model", 32),
            ),
            Column::new("Provider", 10, 2),
            Column::new(">Calls", 6, 9),
            Column::new(">Input", 7, 8),
            Column::new(">Output", 7, 7),
            Column::new(">Cached", 7, 4),
            Column::new(">Avg", 7, 6),
            Column::new(">p95", 7, 3),
            Column::new(">1st token", 9, 5),
        ];
        let rows: Vec<Vec<Cell>> = models
            .iter()
            .map(|m| {
                vec![
                    Cell::from(m.model.clone()).fg(p.text),
                    Cell::from(if m.provider.is_empty() {
                        "–".into()
                    } else {
                        m.provider.clone()
                    })
                    .fg(p.muted),
                    right(thousands(m.calls)),
                    right(compact(m.input_tokens)),
                    right(compact(m.output_tokens)),
                    right(compact(m.cache_read_tokens)),
                    right(duration(m.avg_latency_ms)),
                    right(duration(m.p95_latency_ms)),
                    right(m.avg_ttft_ms.map_or("–".into(), duration)),
                ]
            })
            .collect();
        let count = rows.len();
        let (table_area, panels) = Self::split_table_area(area, count, 3);
        let table = self.table(format!("Models · {count}"), &columns, rows, area.width);
        frame.render_stateful_widget(table, table_area, &mut self.table);
        if let [latency, ttft, tokens] = panels.as_slice() {
            let models = &self.summary.models;
            let by = |value: fn(&ModelRow) -> f64| {
                let mut sorted: Vec<&ModelRow> = models.iter().collect();
                sorted.sort_by(|a, b| value(b).total_cmp(&value(a)));
                sorted
            };
            let items: Vec<BarItem> = by(|m| m.p95_latency_ms)
                .iter()
                .map(|m| {
                    BarItem::new(&m.model, m.p95_latency_ms, duration(m.p95_latency_ms))
                        .note(format!("avg {}", duration(m.avg_latency_ms)))
                })
                .collect();
            self.bar_list(frame, *latency, "Latency · p95", &items);
            let items: Vec<BarItem> = by(|m| m.avg_ttft_ms.unwrap_or(0.0))
                .iter()
                .filter_map(|m| {
                    let ttft = m.avg_ttft_ms?;
                    Some(BarItem::new(&m.model, ttft, duration(ttft)))
                })
                .collect();
            self.bar_list(frame, *ttft, "First token · avg", &items);
            let items: Vec<BarItem> = by(|m| (m.input_tokens + m.output_tokens) as f64)
                .iter()
                .map(|m| {
                    let tokens = m.input_tokens + m.output_tokens;
                    BarItem::new(&m.model, tokens as f64, compact(tokens))
                        .note(format!("{:.0}%", 100.0 * tokens as f64 / total_tokens))
                })
                .collect();
            self.bar_list(frame, *tokens, "Tokens", &items);
        }
    }

    fn render_tools(&mut self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let tools = &self.summary.tools;
        let max = tools.first().map_or(1, |t| t.calls).max(1);
        let columns = [
            Column::text(
                "Tool",
                column_width(tools.iter().map(|t| t.tool.as_str()), "Tool", 32),
            ),
            Column::new(
                "Source",
                column_width(tools.iter().map(|t| t.source.as_str()), "Source", 16),
                6,
            ),
            Column::new(">Calls", 7, 9),
            Column::new("", 16, 1),
            Column::new(">Avg", 8, 5),
            Column::new(">p95", 8, 4),
        ];
        let rows: Vec<Vec<Cell>> = tools
            .iter()
            .map(|t| {
                vec![
                    Cell::from(t.tool.clone()).fg(p.text),
                    Cell::from(t.source.clone()).fg(if t.source == BUILT_IN {
                        p.dim
                    } else {
                        p.muted
                    }),
                    right(thousands(t.calls)),
                    Cell::from(bar(t.calls as f64 / max as f64, 16)).fg(p.series_1),
                    right(duration(t.avg_ms)),
                    right(duration(t.p95_ms)),
                ]
            })
            .collect();
        let count = rows.len();
        let (table_area, panels) = Self::split_table_area(area, count, 2);
        let table = self.table(format!("Tools · {count}"), &columns, rows, area.width);
        frame.render_stateful_widget(table, table_area, &mut self.table);
        if let [slowest, sources] = panels.as_slice() {
            let tools = &self.summary.tools;
            let mut by_p95: Vec<&ToolRow> = tools.iter().collect();
            by_p95.sort_by(|a, b| b.p95_ms.total_cmp(&a.p95_ms));
            let items: Vec<BarItem> = by_p95
                .iter()
                .map(|t| {
                    BarItem::new(&t.tool, t.p95_ms, duration(t.p95_ms))
                        .note(format!("avg {}", duration(t.avg_ms)))
                })
                .collect();
            self.bar_list(frame, *slowest, "Slowest · p95", &items);
            let mut by_source: Vec<(String, u64)> = Vec::new();
            for t in tools {
                match by_source.iter_mut().find(|(s, _)| *s == t.source) {
                    Some((_, calls)) => *calls += t.calls,
                    None => by_source.push((t.source.clone(), t.calls)),
                }
            }
            by_source.sort_by_key(|(_, calls)| std::cmp::Reverse(*calls));
            let total = self.summary.totals.tool_calls.max(1) as f64;
            let items: Vec<BarItem> = by_source
                .iter()
                .map(|(source, calls)| {
                    let label = if source == BUILT_IN {
                        BUILT_IN.to_string()
                    } else {
                        format!("mcp · {source}")
                    };
                    BarItem::new(&label, *calls as f64, thousands(*calls))
                        .note(format!("{:.0}%", 100.0 * *calls as f64 / total))
                })
                .collect();
            self.bar_list(frame, *sources, "Calls by source", &items);
        }
    }

    // ---------------------------------------------------------------- agents

    fn render_agents(&self, frame: &mut Frame, area: Rect) {
        let t = &self.summary.totals;
        let tiles = [
            Tile::new(
                "Main agent",
                thousands(t.runs),
                format!("runs in {} sessions", thousands(t.sessions)),
            ),
            Tile::new(
                "Subagents",
                thousands(t.sub_agent_runs),
                format!("{} spawned", thousands(t.sub_agents_spawned)),
            ),
            Tile::new(
                "Skills",
                thousands(t.skill_uses),
                format!("uses of {} skills", self.summary.skills.len()),
            ),
            Tile::new(
                "MCP",
                thousands(t.mcp_calls),
                format!("calls to {} servers", self.summary.mcp_servers.len()),
            ),
        ];
        let lists_h = (self
            .summary
            .skills
            .len()
            .max(self.summary.mcp_servers.len())
            .clamp(1, 10)
            + 2) as u16;
        let [tiles_area, chart, lists] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(lists_h.max(4)),
        ])
        .areas(area);
        self.tile_row(frame, tiles_area, &tiles);
        let days = fill_days(&self.summary.days, self.range, Local::now().date_naive());
        self.daily_histogram(
            frame,
            chart,
            "Agent runs per day",
            "Runs",
            &days,
            &[
                Series {
                    name: "main",
                    value: |d| d.runs as f64,
                },
                Series {
                    name: "subagents",
                    value: |d| d.sub_agent_runs as f64,
                },
            ],
        );
        let [skills, servers] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(lists);
        let items: Vec<BarItem> = self
            .summary
            .skills
            .iter()
            .map(|s| {
                BarItem::new(&s.skill, s.uses as f64, thousands(s.uses)).note(format!(
                    "{} session{}",
                    s.sessions,
                    if s.sessions == 1 { "" } else { "s" }
                ))
            })
            .collect();
        self.bar_list(frame, skills, "Skills used", &items);
        let items: Vec<BarItem> = self
            .summary
            .mcp_servers
            .iter()
            .map(|s| {
                BarItem::new(&s.server, s.calls as f64, thousands(s.calls)).note(format!(
                    "{} tool{} · avg {}",
                    s.tools,
                    if s.tools == 1 { "" } else { "s" },
                    duration(s.avg_ms)
                ))
            })
            .collect();
        self.bar_list(frame, servers, "MCP servers", &items);
    }

    // ---------------------------------------------------------------- sessions

    fn render_sessions(&mut self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let now = Utc::now();
        let sessions = &self.summary.sessions;
        let columns = [
            Column::text(
                "Session",
                column_width(
                    sessions.iter().map(|s| s.session_id.as_str()),
                    "Session",
                    36,
                ),
            ),
            Column::new("Last active", 11, 9),
            Column::new(">Runs", 4, 5),
            Column::new(">Subagents", 10, 4),
            Column::new(">Calls", 5, 7),
            Column::new(">Tools", 5, 6),
            Column::new(">Tokens", 7, 8),
            Column {
                fill: true,
                ..Column::new("Models", 10, 3)
            },
        ];
        let rows: Vec<Vec<Cell>> = sessions
            .iter()
            .map(|s| {
                vec![
                    Cell::from(s.session_id.clone()).fg(p.text),
                    Cell::from(ago(s.last_active, now)).fg(p.muted),
                    right(thousands(s.runs)),
                    right(thousands(s.sub_agents_spawned)),
                    right(thousands(s.llm_calls)),
                    right(thousands(s.tool_calls)),
                    right(compact(s.tokens)),
                    Cell::from(s.models.join(", ")).fg(p.muted),
                ]
            })
            .collect();
        let shown = rows.len();
        let total = self.summary.totals.sessions;
        let title = if total as usize > shown {
            format!("Recent sessions · {shown} of {total}")
        } else {
            format!("Sessions · {total}")
        };
        let table = self.table(title, &columns, rows, area.width);
        frame.render_stateful_widget(table, area, &mut self.table);
    }
}

/// A table column. A leading `>` in `header` right-aligns it; on narrow terminals the
/// columns with the lowest `keep` are dropped first.
#[derive(Debug, Clone, Copy)]
struct Column {
    header: &'static str,
    width: u16,
    keep: u8,
    /// Takes the remaining width.
    fill: bool,
}

impl Column {
    const fn new(header: &'static str, width: u16, keep: u8) -> Self {
        Self {
            header,
            width,
            keep,
            fill: false,
        }
    }

    /// The identifying first column; never dropped.
    const fn text(header: &'static str, width: u16) -> Self {
        Self::new(header, width, u8::MAX)
    }
}

/// Indices of the columns to show in a table `width` cells wide.
fn fit_columns(columns: &[Column], width: u16) -> Vec<usize> {
    // Borders and the selection marker.
    let budget = width.saturating_sub(4);
    let needed = |kept: &[usize]| {
        kept.iter().map(|&i| columns[i].width).sum::<u16>()
            + 2 * kept.len().saturating_sub(1) as u16
    };
    let mut kept: Vec<usize> = (0..columns.len()).collect();
    while needed(&kept) > budget {
        let droppable = kept
            .iter()
            .enumerate()
            .filter(|(_, &i)| columns[i].keep < u8::MAX)
            .min_by_key(|(_, &i)| columns[i].keep);
        match droppable {
            Some((pos, _)) => {
                kept.remove(pos);
            }
            None => break,
        }
    }
    kept
}

/// One row of a ranked bar list.
struct BarItem {
    label: String,
    value: f64,
    value_text: String,
    note: String,
}

impl BarItem {
    fn new(label: &str, value: f64, value_text: String) -> Self {
        Self {
            label: label.to_string(),
            value,
            value_text,
            note: String::new(),
        }
    }

    fn note(mut self, note: String) -> Self {
        self.note = note;
        self
    }
}

/// Width of a text column: its widest cell or header, capped.
fn column_width<'a>(cells: impl Iterator<Item = &'a str>, header: &str, cap: usize) -> u16 {
    cells
        .map(|c| c.chars().count())
        .chain([header.len()])
        .max()
        .unwrap_or(0)
        .min(cap) as u16
}

/// A named daily series of a histogram.
struct Series {
    name: &'static str,
    /// The series' value on a day.
    value: fn(&DayPoint) -> f64,
}

/// One series of a histogram's legend.
#[derive(Debug, Clone, Copy)]
struct Legend<'a> {
    name: &'a str,
    color: Color,
}

/// One stat tile: a label, a big value and an optional note under it.
struct Tile {
    label: &'static str,
    value: String,
    note: String,
}

impl Tile {
    fn new(label: &'static str, value: String, note: String) -> Self {
        Self { label, value, note }
    }
}

/// What [`App::histogram`] draws.
struct Histogram<'a> {
    title: &'a str,
    /// Y axis name, e.g. `Tokens`.
    y_name: &'a str,
    /// X axis name, e.g. `Day`.
    x_name: &'a str,
    /// Series names and colors, stacked bottom to top.
    series: &'a [Legend<'a>],
    /// Per bar, one value per series.
    bars: &'a [Vec<u64>],
    /// Per bar, its x label.
    labels: &'a [String],
    /// Bars whose label is shown.
    ticks: &'a [usize],
}

/// One day of the daily charts, including idle days.
#[derive(Debug, Clone, PartialEq)]
struct DayPoint {
    label: String,
    input: u64,
    output: u64,
    llm_calls: u64,
    tool_calls: u64,
    runs: u64,
    sub_agent_runs: u64,
}

/// Every day of the period, ending today, with zeros for idle days. `All` starts at the
/// first day with data.
fn fill_days(days: &[DayRow], range: Range, today: NaiveDate) -> Vec<DayPoint> {
    let span = match range.days() {
        Some(d) => i64::from(d),
        None => days
            .first()
            .and_then(|d| NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok())
            .map_or(1, |first| (today - first).num_days() + 1)
            .max(1),
    };
    (0..span)
        .rev()
        .map(|back| {
            let date = today - chrono::Duration::days(back);
            let key = date.format("%Y-%m-%d").to_string();
            let row = days
                .iter()
                .find(|d| d.date == key)
                .cloned()
                .unwrap_or_default();
            DayPoint {
                label: date.format("%b %-d").to_string(),
                input: row.input_tokens,
                output: row.output_tokens,
                llm_calls: row.llm_calls,
                tool_calls: row.tool_calls,
                runs: row.runs,
                sub_agent_runs: row.sub_agent_runs,
            }
        })
        .collect()
}

fn weighted_latency(summary: &Summary) -> f64 {
    let calls: u64 = summary.models.iter().map(|m| m.calls).sum();
    if calls == 0 {
        return 0.0;
    }
    summary
        .models
        .iter()
        .map(|m| m.avg_latency_ms * m.calls as f64)
        .sum::<f64>()
        / calls as f64
}

fn centered_rows(area: Rect, rows: u16) -> Rect {
    let h = rows.min(area.height);
    Rect {
        y: area.y + (area.height - h) / 2,
        height: h,
        ..area
    }
}

fn right(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).alignment(Alignment::Right))
}

/// Horizontal bar of `fraction` (0–1) of `width` cells, in eighths.
fn bar(fraction: f64, width: usize) -> String {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let mut out = "█".repeat(eighths / 8);
    out.push_str(PARTS[eighths % 8]);
    if out.is_empty() && fraction > 0.0 && width > 0 {
        out.push('▏');
    }
    out
}

/// Full-width gauge: filled part, then a light track.
fn gauge(fraction: f64, width: usize) -> String {
    let filled = bar(fraction, width);
    let used = filled.chars().count();
    format!("{filled}{}", "░".repeat(width.saturating_sub(used)))
}

fn nice_max(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * p)
        .find(|c| *c >= v)
        .unwrap_or(10.0 * p)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `950`, `12.3K`, `4.5M`, `1.2B`.
pub fn compact(n: u64) -> String {
    compact_f(n as f64)
}

fn compact_f(n: f64) -> String {
    let (value, unit) = match n {
        n if n >= 1e9 => (n / 1e9, "B"),
        n if n >= 1e6 => (n / 1e6, "M"),
        n if n >= 1e3 => (n / 1e3, "K"),
        n => return format!("{}", n.round() as u64),
    };
    let text = if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    format!("{}{unit}", text.trim_end_matches(".0"))
}

/// `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `850ms`, `4.2s`, `3m 05s`.
pub fn duration(ms: f64) -> String {
    match ms {
        ms if ms < 1000.0 => format!("{}ms", ms.round() as u64),
        ms if ms < 60_000.0 => format!("{:.1}s", ms / 1000.0),
        ms => {
            let s = (ms / 1000.0).round() as u64;
            format!("{}m {:02}s", s / 60, s % 60)
        }
    }
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
fn ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - at).num_seconds().max(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::aggregate::{McpRow, SessionRow, SkillRow, Totals};

    fn summary() -> Summary {
        let today = Local::now().date_naive();
        Summary {
            files: 2,
            totals: Totals {
                sessions: 3,
                runs: 4,
                sub_agent_runs: 2,
                sub_agents_spawned: 2,
                llm_calls: 20,
                tool_calls: 30,
                mcp_calls: 5,
                skill_uses: 3,
                input_tokens: 1_234_567,
                output_tokens: 45_678,
                cache_read_tokens: 400_000,
                reasoning_tokens: 0,
            },
            days: vec![DayRow {
                date: today.format("%Y-%m-%d").to_string(),
                llm_calls: 20,
                tool_calls: 30,
                input_tokens: 1_234_567,
                output_tokens: 45_678,
                runs: 4,
                sub_agent_runs: 2,
            }],
            hours: std::array::from_fn(|h| (h as u64 * 7) % 11),
            models: vec![ModelRow {
                model: "sarvam-m".into(),
                provider: "soket".into(),
                calls: 20,
                input_tokens: 1_234_567,
                output_tokens: 45_678,
                cache_read_tokens: 400_000,
                avg_latency_ms: 2400.0,
                p95_latency_ms: 6100.0,
                avg_ttft_ms: Some(640.0),
            }],
            tools: vec![
                ToolRow {
                    tool: "read".into(),
                    source: BUILT_IN.into(),
                    calls: 25,
                    avg_ms: 12.0,
                    p95_ms: 40.0,
                },
                ToolRow {
                    tool: "mcp__github__list_issues".into(),
                    source: "github".into(),
                    calls: 5,
                    avg_ms: 1500.0,
                    p95_ms: 4000.0,
                },
            ],
            mcp_servers: vec![McpRow {
                server: "github".into(),
                calls: 5,
                tools: 1,
                avg_ms: 1500.0,
            }],
            skills: vec![SkillRow {
                skill: "pdf-tools".into(),
                uses: 3,
                sessions: 2,
            }],
            sessions: vec![SessionRow {
                session_id: "0192aaaa-bbbb".into(),
                started: Utc::now(),
                last_active: Utc::now(),
                runs: 4,
                llm_calls: 20,
                tool_calls: 30,
                sub_agents_spawned: 2,
                tokens: 1_280_245,
                models: vec!["sarvam-m".into()],
            }],
            ..Default::default()
        }
    }

    fn live() -> LiveStatus {
        LiveStatus {
            taken_at: Local::now(),
            model: "soket/sarvam-m".into(),
            effort: Some("high".into()),
            permission_mode: "ask".into(),
            sandbox: "off".into(),
            web: "on".into(),
            cwd: "F:/soket labs/loop".into(),
            session_id: "0192aaaa-bbbb".into(),
            context_tokens: Some(54_000),
            context_window: 128_000,
            session_tokens: 1_200_000,
            main_agent_working: true,
            main_agent_step: "tool · bash".into(),
            external_agent: None,
            sub_agents: vec![
                SubAgent {
                    id: "t1".into(),
                    description: "Survey the repo".into(),
                    state: SubAgentState::Running,
                },
                SubAgent {
                    id: "t2".into(),
                    description: "Write the report".into(),
                    state: SubAgentState::Finished,
                },
            ],
            skills_loaded: vec!["pdf-tools".into(), "review".into()],
            skills_active: vec!["review".into()],
            skills_disabled: 1,
            mcp_servers: vec![
                McpServerStatus {
                    name: "github".into(),
                    tools: 28,
                    connected: true,
                },
                McpServerStatus {
                    name: "slack".into(),
                    tools: 0,
                    connected: false,
                },
            ],
            extensions: 1,
            hooks: 2,
            usage_log: Some("/tmp/loop-traces/loop-1.jsonl".into()),
        }
    }

    fn app(summary: Summary, live: Option<LiveStatus>) -> App {
        let mut app = App::new(Options {
            dir: PathBuf::from("/nonexistent/loop-traces"),
            palette: Palette::default(),
            notice: None,
            live,
        });
        app.summary = summary;
        app
    }

    fn draw(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_shows(screen: &str, texts: &[&str]) {
        for text in texts {
            assert!(screen.contains(text), "missing {text:?}\n{screen}");
        }
    }

    #[test]
    fn now_shows_session_context_agents_and_capabilities() {
        let mut app = app(summary(), Some(live()));
        assert_eq!(app.view, View::Now, "live status opens on Now");
        let screen = draw(&mut app, 140, 40);
        assert_shows(
            &screen,
            &[
                "soket/sarvam-m · effort high",
                "Permissions",
                "42% used · 54K of 128K",
                "Agents · 2 running",
                "● working · tool · bash",
                "1 running · 1 finished",
                "Survey the repo",
                "2 loaded · active: review · 1 off",
                "1 of 2 connected · 28 tools",
                "not connected",
                "Tokens today",
                "● live · ",
            ],
        );
    }

    #[test]
    fn now_follows_live_updates() {
        let mut app = app(summary(), Some(live()));
        assert!(draw(&mut app, 140, 40).contains("Agents · 2 running"));
        let mut later = live();
        later.main_agent_working = false;
        later.sub_agents[0].state = SubAgentState::Finished;
        app.options.live = Some(later);
        let screen = draw(&mut app, 140, 40);
        assert_shows(
            &screen,
            &["Agents · 0 running", "○ idle", "0 running · 2 finished"],
        );
    }

    #[test]
    fn now_without_live_status_explains_where_to_get_it() {
        let mut app = app(summary(), None);
        assert_eq!(app.view, View::Usage, "standalone opens on Usage");
        app.set_view(View::Now);
        assert!(draw(&mut app, 100, 30).contains("Live status needs a running loop"));
    }

    #[test]
    fn usage_shows_tokens_activity_and_no_cost_or_errors() {
        let mut app = app(summary(), None);
        let screen = draw(&mut app, 140, 40);
        assert_shows(
            &screen,
            &[
                "Tokens",
                "1.3M",
                "Cached",
                "Agent runs",
                "2 subagents",
                "Tokens per day",
                "Calls per day",
                "Model calls by hour",
                "Top tools",
                "Models by tokens",
                "q close",
                // Histograms name both axes and their stacked series.
                "Tokens  ■ input   ■ output",
                "Calls  ■ model   ■ tool",
                " Day ",
                " Hour ",
            ],
        );
        for gone in ["Cost", "$", "Error"] {
            assert!(!screen.contains(gone), "{gone:?} should be gone\n{screen}");
        }
    }

    #[test]
    fn agents_view_shows_runs_skills_and_mcp_servers() {
        let mut app = app(summary(), None);
        app.set_view(View::Agents);
        let screen = draw(&mut app, 140, 40);
        assert_shows(
            &screen,
            &[
                "Main agent",
                "Subagents",
                "2 spawned",
                "Agent runs per day",
                "Runs  ■ main   ■ subagents",
                "Skills used",
                "pdf-tools",
                "2 sessions",
                "MCP servers",
                "github",
            ],
        );
    }

    #[test]
    fn every_view_draws_at_common_sizes_without_panicking() {
        for (w, h) in [(60, 18), (80, 24), (100, 30), (200, 60)] {
            for view in View::ALL {
                let mut app = app(summary(), Some(live()));
                app.view = view;
                let screen = draw(&mut app, w, h);
                assert!(
                    screen.contains(view.label()),
                    "{view:?} at {w}x{h}\n{screen}"
                );
            }
        }
    }

    #[test]
    fn tables_list_models_tools_and_sessions() {
        let mut app = app(summary(), None);
        app.set_view(View::Models);
        let screen = draw(&mut app, 140, 30);
        assert_shows(&screen, &["sarvam-m", "Cached", "400K", "640ms", "6.1s"]);
        app.set_view(View::Tools);
        let screen = draw(&mut app, 140, 30);
        assert_shows(
            &screen,
            &[
                "mcp__github__list_issues",
                "Source",
                "Calls by source",
                "mcp · github",
            ],
        );
        app.set_view(View::Sessions);
        let screen = draw(&mut app, 140, 30);
        assert_shows(&screen, &["0192aaaa-bbbb", "just now", "Subagents"]);
    }

    #[test]
    fn empty_and_tiny_terminals_explain_themselves() {
        let mut app = app(Summary::default(), None);
        assert!(draw(&mut app, 100, 30).contains("No usage recorded"));
        assert!(draw(&mut app, 40, 10).contains("too small"));
        // Now still works with no history.
        let mut app = self::app(Summary::default(), Some(live()));
        assert!(draw(&mut app, 100, 30).contains("Context window"));
    }

    #[test]
    fn keys_switch_views_periods_scroll_and_quit() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = app(summary(), None);
        app.handle_key(key(KeyCode::Char('5')));
        assert_eq!(app.view, View::Agents);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Sessions);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Now);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.view, View::Sessions);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.range, Range::Week);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Right));
        }
        assert_eq!(app.range, Range::All);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Quit);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn scrolling_is_clamped_to_rows() {
        let mut app = app(summary(), None);
        app.set_view(View::Tools);
        app.scroll(10);
        assert_eq!(app.table.selected(), Some(1));
        app.scroll(-10);
        assert_eq!(app.table.selected(), Some(0));
    }

    #[test]
    fn fill_days_covers_the_period_with_idle_days() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 26).unwrap();
        let rows = vec![DayRow {
            date: "2026-10-24".into(),
            input_tokens: 5,
            runs: 2,
            ..Default::default()
        }];
        let week = fill_days(&rows, Range::Week, today);
        assert_eq!(week.len(), 7);
        assert_eq!(week[6].label, "Oct 26");
        assert_eq!((week[4].input, week[4].runs), (5, 2));
        assert_eq!(fill_days(&rows, Range::All, today).len(), 3);
        assert_eq!(fill_days(&[], Range::All, today).len(), 1);
    }

    #[test]
    fn histogram_stacks_series_and_labels_axes() {
        let app = app(Summary::default(), None);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|f| {
                app.histogram(
                    f,
                    f.area(),
                    &Histogram {
                        title: "Test",
                        y_name: "Tokens",
                        x_name: "Day",
                        series: &[
                            Legend {
                                name: "a",
                                color: Color::Blue,
                            },
                            Legend {
                                name: "b",
                                color: Color::Yellow,
                            },
                        ],
                        bars: &[vec![4, 4], vec![0, 0], vec![8, 0]],
                        labels: &["d1".into(), "d2".into(), "d3".into()],
                        ticks: &[0, 2],
                    },
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
        let screen = (0..12).map(row).collect::<Vec<_>>().join(
            "
",
        );
        for text in ["Tokens", "■ a", "■ b", "10┤", "0└", " Day ", "d1", "d3"] {
            assert!(
                screen.contains(text),
                "missing {text:?}
{screen}"
            );
        }
        // The first bar is half blue (bottom) and half yellow (top); the second is empty.
        let colors_in_column = |x: u16| {
            (0..12)
                .map(|y| &buf[(x, y)])
                .filter(|c| c.symbol() == "█")
                .map(|c| c.fg)
                .collect::<Vec<_>>()
        };
        let first_bar_x = (0..40).find(|&x| !colors_in_column(x).is_empty()).unwrap();
        let colors = colors_in_column(first_bar_x);
        assert!(
            colors.contains(&Color::Blue) && colors.contains(&Color::Yellow),
            "{colors:?}"
        );
        assert_eq!(colors.first(), Some(&Color::Yellow), "yellow sits on top");
    }

    #[test]
    fn narrow_tables_drop_low_priority_columns_first() {
        let columns = [
            Column::text("Name", 20),
            Column::new("Low", 10, 1),
            Column::new("High", 10, 9),
            Column::new("Mid", 10, 5),
        ];
        assert_eq!(fit_columns(&columns, 200), [0, 1, 2, 3]);
        assert_eq!(fit_columns(&columns, 60), [0, 1, 2, 3], "exactly fits");
        assert_eq!(fit_columns(&columns, 55), [0, 2, 3]);
        assert_eq!(fit_columns(&columns, 40), [0, 2]);
        assert_eq!(
            fit_columns(&columns, 10),
            [0],
            "the first column always stays"
        );
    }

    #[test]
    fn formatting() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(1_000), "1K");
        assert_eq!(compact(12_345), "12.3K");
        assert_eq!(compact(4_500_000), "4.5M");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(duration(850.0), "850ms");
        assert_eq!(duration(4200.0), "4.2s");
        assert_eq!(duration(185_000.0), "3m 05s");
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.5, 3), "█▌");
        assert_eq!(bar(0.001, 4), "▏");
        assert_eq!(gauge(0.5, 4), "██░░");
        assert_eq!(nice_max(7.3), 10.0);
        assert_eq!(nice_max(0.0), 1.0);
    }
}
