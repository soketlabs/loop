//! Terminal usage view for Loop (`/usage`, `loop usage`).
//!
//! Shows the running loop's live status (session, context window, agents and Subagents,
//! skills, MCP servers) next to the history in the anonymous usage log that Loop writes to
//! `~/.loop/agent/usage` (one JSON-lines file per process, see `loop-telemetry`):
//! tokens and calls over time, models, tools, agent runs, skills, MCP servers and sessions.
//! Nothing is sent anywhere.
//!
//! * [`aggregate`] turns trace files into a [`Summary`](aggregate::Summary).
//! * [`tui`] draws it; [`tui::run`] standalone, [`tui::Dashboard`] driven from inside the chat.

pub mod aggregate;
pub mod tui;

pub use aggregate::{summarize_dir, Summary};
pub use tui::{
    run, Dashboard, LiveStatus, McpServerStatus, Options, Palette, SubAgent, SubAgentState,
};
