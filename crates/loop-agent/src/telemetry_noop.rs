//! No-op stand-ins for [`telemetry`](self) when the `telemetry` feature is off.
//!
//! Same API as the real module so `agent_loop.rs` needs no `cfg`. Every span is
//! [`Span::none`], so `.instrument()` costs nothing and no observation data is built.

use loop_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Model, SimpleStreamOptions, ToolCall,
};
use tracing::Span;

use crate::types::{AgentMessage, AgentToolResult};

/// `loop.run` (disabled).
pub(crate) struct RunObserver {
    span: Span,
}

impl RunObserver {
    pub(crate) fn start(
        _model: &Model,
        _prompts: &[AgentMessage],
        _session_id: Option<&str>,
    ) -> Self {
        Self { span: Span::none() }
    }

    pub(crate) fn span(&self) -> &Span {
        &self.span
    }

    pub(crate) fn finish(&self, _new_messages: &[AgentMessage]) {}
}

/// `turn N` (disabled).
pub(crate) fn turn_span(_index: u64) -> Span {
    Span::none()
}

/// `llm.generate` (disabled).
pub(crate) struct GenerationObserver;

impl GenerationObserver {
    pub(crate) fn start(
        _model: &Model,
        _context: &Context,
        _options: &SimpleStreamOptions,
    ) -> Self {
        Self
    }

    pub(crate) fn on_event(&mut self, _event: &AssistantMessageEvent) {}

    pub(crate) fn finish(&self, _message: &AssistantMessage) {}
}

/// `tool.preflight` (disabled).
pub(crate) fn preflight_span(_tool_call: &ToolCall) -> Span {
    Span::none()
}

/// `tool <name>` (disabled).
pub(crate) fn tool_span(_tool_call: &ToolCall, _args: &serde_json::Value) -> Span {
    Span::none()
}

/// Record a tool outcome (disabled).
pub(crate) fn record_tool_result(_span: &Span, _result: &AgentToolResult, _is_error: bool) {}
