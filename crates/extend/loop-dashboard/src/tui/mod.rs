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

mod views;
mod widgets;

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

/// Run the usage view standalone: takes over the terminal until the user quits.
pub fn run(options: Options) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let _raw_mode = RawModeGuard;
    let mut dashboard = Dashboard::open(options)?;
    loop {
        dashboard.tick()?;
        if event::poll(Duration::from_millis(500))? && dashboard.handle_event(&event::read()?)? {
            return dashboard.close();
        }
    }
}

/// Leaves raw mode and shows the cursor when dropped, so an error or a panic in
/// [`run`] never leaves the shell unusable. (Leaving the alternate screen is
/// [`Dashboard`]'s job.)
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), crossterm::cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
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
}

#[cfg(test)]
mod tests;
