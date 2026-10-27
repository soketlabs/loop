//! `/dashboard` inside the chat. While open, the dashboard owns the screen and the keys;
//! the chat loop keeps draining agent events and hands it a fresh live status, so turns
//! and sub-agents carry on and the **Now** view stays current. Nothing runs while closed.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event;
use loop_dashboard::{
    Dashboard, LiveStatus, McpServerStatus, Options, Palette, SubAgent, SubAgentState,
};

use super::{CardStatus, ChatItem, CliRuntime, Theme, TokenBarState};

/// How often the Now view is rebuilt from the chat's in-memory state.
const LIVE_EVERY: Duration = Duration::from_millis(500);
/// How often sandbox and MCP state are queried again and this session's spans flushed.
const SLOW_EVERY: Duration = Duration::from_secs(5);

/// Usage dashboard settings in the colors of the chat theme.
pub fn options(theme: &Theme, usage_log_on: bool) -> Options {
    Options {
        dir: loop_telemetry::default_trace_dir(),
        palette: Palette {
            text: theme.get("text"),
            muted: theme.get("muted"),
            dim: theme.get("dim"),
            border: theme.get("borderMuted"),
            accent: theme.get("borderAccent"),
            series_1: theme.get("borderAccent"),
            series_2: theme.get("warning"),
            good: theme.get("success"),
            selected_bg: theme.get("selectedBg"),
        },
        notice: (!usage_log_on).then(|| {
            "Usage log is off, so new activity is not recorded · /tracing local on".into()
        }),
        live: None,
    }
}

/// What the chat is doing right now.
pub(super) struct ChatState<'a> {
    pub items: &'a [ChatItem],
    pub working: bool,
    pub turn_step: &'a str,
    pub token_bar: &'a TokenBarState,
}

/// The open dashboard.
pub(super) struct Screen {
    board: Dashboard,
    size: Option<(u16, u16)>,
    slow: Slow,
    live_at: Instant,
    slow_at: Instant,
}

impl Screen {
    pub async fn open(runtime: &CliRuntime, chat: ChatState<'_>) -> io::Result<Self> {
        let slow = Slow::query(runtime).await;
        let usage_log_on = runtime
            .tracing_status()
            .is_some_and(|status| status.local_log.is_some());
        let mut options = options(&runtime.theme, usage_log_on);
        options.live = Some(live_status(runtime, &chat, &slow));
        // Include this session's latest spans.
        tokio::task::block_in_place(|| runtime.flush_tracing());
        Ok(Self {
            board: Dashboard::open(options)?,
            size: crossterm::terminal::size().ok(),
            slow,
            live_at: Instant::now(),
            slow_at: Instant::now(),
        })
    }

    /// Wait up to `wait` for keys and pass them on; returns `true` once the user closed it.
    pub fn handle_input(&mut self, wait: Duration) -> io::Result<bool> {
        let mut ready = event::poll(wait)?;
        while ready {
            if self.board.handle_event(&event::read()?)? {
                return Ok(true);
            }
            ready = event::poll(Duration::ZERO)?;
        }
        Ok(false)
    }

    /// Refresh the live status when due and redraw.
    pub async fn refresh(&mut self, runtime: &CliRuntime, chat: ChatState<'_>) -> io::Result<()> {
        if self.slow_at.elapsed() >= SLOW_EVERY {
            self.slow = Slow::query(runtime).await;
            tokio::task::block_in_place(|| runtime.flush_tracing());
            self.slow_at = Instant::now();
        }
        if self.live_at.elapsed() >= LIVE_EVERY {
            self.board.set_live(live_status(runtime, &chat, &self.slow));
            self.live_at = Instant::now();
        }
        self.board.tick()
    }

    /// Back to the chat; returns the terminal width if it was resized meanwhile.
    pub fn close(self) -> io::Result<Option<u16>> {
        self.board.close()?;
        Ok(crossterm::terminal::size()
            .ok()
            .filter(|size| Some(*size) != self.size)
            .map(|(width, _)| width))
    }
}

/// Status that needs a query and seldom changes.
struct Slow {
    sandbox: String,
    mcp_servers: Vec<McpServerStatus>,
}

impl Slow {
    async fn query(runtime: &CliRuntime) -> Self {
        let sandbox = runtime.harness.sandbox_info().await;
        let sandbox = sandbox
            .fields
            .iter()
            .map(|(_, value)| value.as_str())
            .take(2)
            .collect::<Vec<_>>()
            .join(" · ");
        let connected: HashMap<String, usize> = runtime
            .mcp_client
            .list_connections()
            .await
            .into_iter()
            .collect();
        let mut mcp_servers: Vec<McpServerStatus> = runtime
            .settings
            .mcp_servers
            .keys()
            .filter(|name| !runtime.settings.disabled_mcp_servers.contains(name))
            .map(|name| McpServerStatus {
                name: name.clone(),
                tools: connected.get(name).copied().unwrap_or(0),
                connected: connected.contains_key(name),
            })
            .collect();
        for (name, tools) in &connected {
            if !mcp_servers.iter().any(|s| &s.name == name) {
                mcp_servers.push(McpServerStatus {
                    name: name.clone(),
                    tools: *tools,
                    connected: true,
                });
            }
        }
        Self {
            sandbox,
            mcp_servers,
        }
    }
}

/// The Now view's status, from in-memory state only.
fn live_status(runtime: &CliRuntime, chat: &ChatState<'_>, slow: &Slow) -> LiveStatus {
    let subscription = runtime
        .active_agent
        .as_deref()
        .map(|id| match runtime.agents.entry(id) {
            Some(entry) => entry.display_name().to_string(),
            None => id.to_string(),
        });
    let (model, effort) = match &subscription {
        Some(agent) => (
            runtime
                .agent_model
                .as_ref()
                .map_or(agent.clone(), |m| format!("{agent}/{m}")),
            runtime.agent_effort.clone(),
        ),
        None => (
            runtime
                .selected_model_spec()
                .unwrap_or_else(|| "none selected".into()),
            Some(runtime.native_effort()).filter(|e| e != "off"),
        ),
    };
    let sub_agents = chat
        .items
        .iter()
        .filter_map(|item| match item {
            ChatItem::WorkflowTask {
                task_id,
                description,
                status,
                ..
            } => Some(SubAgent {
                id: task_id.clone(),
                description: description.clone(),
                state: match status {
                    CardStatus::Pending => SubAgentState::Running,
                    CardStatus::Success | CardStatus::Error => SubAgentState::Finished,
                },
            }),
            _ => None,
        })
        .collect();
    LiveStatus {
        taken_at: chrono::Local::now(),
        model,
        effort,
        permission_mode: runtime.permission_mode().label().to_string(),
        sandbox: slow.sandbox.clone(),
        web: runtime.web_tools().status(),
        cwd: runtime.cwd.display().to_string(),
        session_id: runtime.session_id.clone(),
        context_tokens: chat.token_bar.context_tokens,
        context_window: chat.token_bar.context_window,
        session_tokens: chat.token_bar.total_tokens,
        main_agent_working: chat.working,
        main_agent_step: if chat.working {
            chat.turn_step.to_string()
        } else {
            String::new()
        },
        external_agent: subscription.map(|agent| format!("{agent} · subscription agent")),
        sub_agents,
        skills_loaded: runtime
            .resources
            .skills
            .iter()
            .map(|s| s.name.clone())
            .collect(),
        skills_active: runtime.active_skills.clone(),
        skills_disabled: runtime.resources.disabled_skills.len(),
        mcp_servers: slow.mcp_servers.clone(),
        extensions: runtime.resources.extension_paths.len(),
        hooks: runtime.resources.hook_paths.len(),
        usage_log: runtime.tracing_status().and_then(|status| status.local_log),
    }
}
