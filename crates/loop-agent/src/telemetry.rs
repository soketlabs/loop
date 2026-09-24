//! Langfuse observations for the agent loop: run → turn → generation / tool.
//!
//! All span creation and attribute recording for `agent_loop.rs` lives here so the
//! loop itself only starts and finishes observers.

use chrono::Utc;
use loop_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Message, Model, SimpleStreamOptions,
    StopReason, ToolCall, ToolResultContent, Usage,
};
use loop_telemetry::obs;
use loop_telemetry::{ObservationExt, TraceAttrs};
use serde::Serialize;
use tracing::Span;

use crate::types::{AgentMessage, AgentToolResult};

/// Name of the agent observation for one run of the loop.
pub(crate) const RUN_SPAN_NAME: &str = "loop.run";
/// Name of each LLM call observation.
pub(crate) const GENERATION_SPAN_NAME: &str = "llm.generate";
/// Name of the tool preflight (validation + approval hook) observation.
pub(crate) const PREFLIGHT_SPAN_NAME: &str = "tool.preflight";

/// `loop.run`: one invocation of the agent loop.
pub(crate) struct RunObserver {
    span: Span,
}

impl RunObserver {
    /// `session_id` becomes the Langfuse session when this run is the trace root;
    /// an enclosing span (e.g. print mode's root) owns trace attributes otherwise.
    pub(crate) fn start(model: &Model, prompts: &[AgentMessage], session_id: Option<&str>) -> Self {
        let is_root = !Span::current().is_recording();
        let span = obs::agent(RUN_SPAN_NAME);
        if is_root {
            span.record_trace(&TraceAttrs {
                session_id: session_id.map(str::to_string),
                ..Default::default()
            });
        }
        span.record_metadata("model", &model.id);
        span.record_metadata("provider", &model.provider);
        span.record_input(prompts);
        Self { span }
    }

    pub(crate) fn span(&self) -> &Span {
        &self.span
    }

    /// Record the outcome from the messages this run produced.
    pub(crate) fn finish(&self, new_messages: &[AgentMessage]) {
        if !self.span.is_recording() {
            return;
        }
        let summary = RunSummary::from_messages(new_messages);
        if let Some(last) = &summary.last_assistant {
            self.span.record_output(last);
            self.span.record_metadata("stop_reason", &last.stop_reason);
            if matches!(last.stop_reason, StopReason::Error | StopReason::Aborted) {
                self.span.record_error(&stop_message(last));
            }
        }
        self.span.record_metadata("turns", &summary.turns);
        self.span.record_metadata("tool_calls", &summary.tool_calls);
        self.span
            .record_metadata("tool_errors", &summary.tool_errors);
        // Totals go in metadata, not usage/cost details, so Langfuse's trace-level
        // aggregation over generations does not count them twice.
        self.span
            .record_metadata("total_tokens", &summary.usage.total_tokens);
        self.span
            .record_metadata("total_cost", &summary.usage.cost.total);
        self.span.record_metadata("usage", &summary.usage);
    }
}

/// Aggregates for a finished run, derived from its transcript.
#[derive(Debug, Default)]
pub(crate) struct RunSummary<'a> {
    pub(crate) turns: u64,
    pub(crate) tool_calls: u64,
    pub(crate) tool_errors: u64,
    pub(crate) usage: Usage,
    pub(crate) last_assistant: Option<&'a AssistantMessage>,
}

impl<'a> RunSummary<'a> {
    pub(crate) fn from_messages(messages: &'a [AgentMessage]) -> Self {
        let mut summary = Self::default();
        for message in messages.iter().filter_map(AgentMessage::as_llm) {
            match message {
                Message::Assistant(a) => {
                    summary.turns += 1;
                    add_usage(&mut summary.usage, &a.usage);
                    summary.last_assistant = Some(a);
                }
                Message::ToolResult(t) => {
                    summary.tool_calls += 1;
                    summary.tool_errors += u64::from(t.is_error);
                }
                Message::User(_) => {}
            }
        }
        summary
    }
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.total_tokens += usage.total_tokens;
    if let Some(reasoning) = usage.reasoning {
        *total.reasoning.get_or_insert(0) += reasoning;
    }
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

/// `turn N`: one model call plus the tools it requested.
pub(crate) fn turn_span(index: u64) -> Span {
    let span = obs::span(&format!("turn {index}"));
    span.record_metadata("turn", &index);
    span
}

/// Request parameters worth comparing across runs (never credentials).
#[derive(Serialize)]
struct ModelParameters {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<loop_ai::ThinkingLevel>,
}

/// `llm.generate`: one streamed LLM call.
pub(crate) struct GenerationObserver {
    span: Span,
    first_token_seen: bool,
}

impl GenerationObserver {
    pub(crate) fn start(model: &Model, context: &Context, options: &SimpleStreamOptions) -> Self {
        let span = obs::generation(GENERATION_SPAN_NAME);
        span.record_model(
            &model.id,
            &model.provider,
            &ModelParameters {
                temperature: options.base.temperature,
                max_tokens: options.base.max_tokens,
                reasoning: options.reasoning,
            },
        );
        span.record_input(context);
        Self {
            span,
            first_token_seen: false,
        }
    }

    /// Record time-to-first-token on the first content delta.
    pub(crate) fn on_event(&mut self, event: &AssistantMessageEvent) {
        let is_delta = matches!(
            event,
            AssistantMessageEvent::TextDelta { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
                | AssistantMessageEvent::ToolcallDelta { .. }
        );
        if is_delta && !self.first_token_seen {
            self.first_token_seen = true;
            self.span.record_completion_start(Utc::now());
        }
    }

    pub(crate) fn finish(&self, message: &AssistantMessage) {
        self.span.record_output(message);
        self.span.record_usage(&message.usage);
        self.span
            .record_metadata("stop_reason", &message.stop_reason);
        if let Some(response_id) = &message.response_id {
            self.span.record_metadata("response_id", response_id);
        }
        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            self.span.record_error(&stop_message(message));
        }
    }
}

fn stop_message(message: &AssistantMessage) -> String {
    message
        .error_message
        .clone()
        .unwrap_or_else(|| format!("stopped with {:?}", message.stop_reason))
}

/// `tool.preflight`: argument validation and the before-tool-call hook.
pub(crate) fn preflight_span(tool_call: &ToolCall) -> Span {
    let span = obs::span(PREFLIGHT_SPAN_NAME);
    span.record_metadata("tool", &tool_call.name);
    span.record_metadata("tool_call_id", &tool_call.id);
    span
}

/// `tool <name>`: one tool execution.
pub(crate) fn tool_span(tool_call: &ToolCall, args: &serde_json::Value) -> Span {
    let span = obs::tool(&tool_call.name, &tool_call.id);
    span.record_input(args);
    span
}

/// Record a tool (or preflight) outcome on its span.
pub(crate) fn record_tool_result(span: &Span, result: &AgentToolResult, is_error: bool) {
    span.record_output(&result.content);
    if is_error {
        span.record_error(&tool_result_text(result));
    }
}

fn tool_result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use loop_ai::{Cost, ToolResultMessage};

    use super::*;

    fn assistant(input: u64, cost: f64, stop_reason: StopReason) -> AgentMessage {
        AgentMessage::assistant(AssistantMessage {
            content: vec![],
            api: "faux".into(),
            provider: "faux".into(),
            model: "faux-model".into(),
            response_model: None,
            response_id: None,
            usage: Usage {
                input,
                total_tokens: input,
                reasoning: Some(1),
                cost: Cost {
                    total: cost,
                    ..Default::default()
                },
                ..Default::default()
            },
            stop_reason,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        })
    }

    fn tool_result(is_error: bool) -> AgentMessage {
        AgentMessage::tool_result(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "t".into(),
            content: vec![],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error,
            timestamp: 0,
        })
    }

    #[test]
    fn summary_counts_turns_tools_and_sums_usage() {
        let messages = vec![
            AgentMessage::user_text("go"),
            assistant(10, 0.25, StopReason::ToolUse),
            tool_result(false),
            tool_result(true),
            assistant(5, 0.5, StopReason::Stop),
        ];
        let summary = RunSummary::from_messages(&messages);
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.tool_calls, 2);
        assert_eq!(summary.tool_errors, 1);
        assert_eq!(summary.usage.input, 15);
        assert_eq!(summary.usage.reasoning, Some(2));
        assert!((summary.usage.cost.total - 0.75).abs() < f64::EPSILON);
        assert_eq!(
            summary.last_assistant.unwrap().stop_reason,
            StopReason::Stop
        );
    }

    #[test]
    fn summary_of_empty_run_has_no_assistant() {
        let summary = RunSummary::from_messages(&[]);
        assert_eq!(summary.turns, 0);
        assert!(summary.last_assistant.is_none());
    }
}
