//! Context overflow heuristics.

use crate::types::{AssistantMessage, Model, StopReason};

/// Heuristic: did this assistant error indicate a context overflow?
pub fn is_context_overflow(model: &Model, message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        // Silent overflow: input beyond context window with length stop and tiny output.
        if message.stop_reason == StopReason::Length
            && message.usage.input > model.context_window
            && message.usage.output == 0
        {
            return true;
        }
        return false;
    }

    let Some(err) = message.error_message.as_deref() else {
        return false;
    };
    let lower = err.to_lowercase();
    OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p))
}

const OVERFLOW_PATTERNS: &[&str] = &[
    "context length",
    "context_length",
    "maximum context",
    "max context",
    "too many tokens",
    "token limit",
    "context window",
    "prompt is too long",
    "maximum.*tokens",
    "exceeds.*context",
    "context_length_exceeded",
    "string_above_max_length",
    "input is too long",
    "request too large",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputModality, ModelCost, Usage};

    fn model() -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            provider: "p".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1000,
            max_tokens: 100,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn detects_error_message() {
        let mut msg = AssistantMessage::pending(&model());
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("This model's maximum context length is 128000 tokens".into());
        assert!(is_context_overflow(&model(), &msg));
    }

    #[test]
    fn detects_silent_overflow() {
        let mut msg = AssistantMessage::pending(&model());
        msg.stop_reason = StopReason::Length;
        msg.usage = Usage {
            input: 2000,
            output: 0,
            total_tokens: 2000,
            ..Usage::empty()
        };
        assert!(is_context_overflow(&model(), &msg));
    }
}
