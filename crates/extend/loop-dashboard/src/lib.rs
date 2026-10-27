//! Terminal usage dashboard for Loop.
//!
//! Shows the running loop's live status (session, context window, agents and sub-agents,
//! skills, MCP servers) next to the history in the anonymous usage log that Loop writes to
//! `<OS temp dir>/loop-traces` (one JSON-lines file per process, see `loop-telemetry`):
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
