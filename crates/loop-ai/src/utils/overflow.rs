//! Context overflow heuristics.

use crate::types::{AssistantMessage, Model, StopReason};

/// Heuristic: did this assistant error indicate a context overflow?
///
/// A rate limit counted in tokens ("tokens per min", TPM) is not an
/// overflow, although its text can name a request as too large. With an
/// unknown context window (0), only the error text tells.
pub fn is_context_overflow(model: &Model, message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        // Silent overflow: input beyond context window with length stop and tiny output.
        let u = &message.usage;
        let prompt = u.input + u.cache_read + u.cache_write;
        if message.stop_reason == StopReason::Length
            && model.context_window > 0
            && prompt > model.context_window
            && message.usage.output == 0
        {
            return true;
        }
        return false;
    }

    let Some(err) = message.error_message.as_deref() else {
        return false;
    };
    is_overflow_text(err)
}

/// Whether a provider error text says the request did not fit the
/// context window.
pub fn is_overflow_text(error: &str) -> bool {
    let lower = error.to_lowercase();
    if RATE_LIMIT_PATTERNS.iter().any(|p| lower.contains(p)) {
        return false;
    }
    OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p))
        || ORDERED_OVERFLOW_PATTERNS.iter().any(|(first, then)| {
            lower
                .find(first)
                .is_some_and(|at| lower[at + first.len()..].contains(then))
        })
}

/// Texts of rate limits, which name tokens but are not overflows.
const RATE_LIMIT_PATTERNS: &[&str] = &[
    "tokens per min",
    "tokens per minute",
    "tokens per day",
    "(tpm)",
    "tpm:",
    "rate limit",
    "rate_limit",
];

const OVERFLOW_PATTERNS: &[&str] = &[
    "context length",
    "context_length",
    "maximum context",
    "max context",
    "too many tokens",
    "token limit",
    "context window",
    "context size",
    "context limit",
    "prompt is too long",
    "maximum prompt length",
    "prompt length",
    "context_length_exceeded",
    "string_above_max_length",
    "input is too long",
    "request too large",
    "exceeds the maximum number of tokens",
];

/// Two words in this order, anything between: "exceeds … context".
const ORDERED_OVERFLOW_PATTERNS: &[(&str, &str)] = &[("exceed", "context")];

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

    #[test]
    fn detects_gemini_and_responses_wording() {
        let mut msg = AssistantMessage::pending(&model());
        msg.stop_reason = StopReason::Error;
        for text in [
            "HTTP 400 Bad Request: The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).",
            "HTTP 400 Bad Request: Your input exceeds the context window of this model.",
        ] {
            msg.error_message = Some(text.into());
            assert!(is_context_overflow(&model(), &msg), "{text}");
        }
    }

    #[test]
    fn silent_overflow_counts_cached_input() {
        let mut msg = AssistantMessage::pending(&model());
        msg.stop_reason = StopReason::Length;
        msg.usage = Usage {
            input: 100,
            cache_read: 1500,
            output: 0,
            ..Usage::empty()
        };
        assert!(is_context_overflow(&model(), &msg));
    }

    #[test]
    fn provider_overflow_texts() {
        for text in [
            // llama.cpp
            "the request exceeds the available context size, try increasing it",
            // Anthropic
            "input length and `max_tokens` exceed context limit: 190000 + 21333 > 200000",
            // xAI
            "This model's maximum prompt length is 131072 but the request contains 140000 tokens.",
            // OpenAI
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
            // Anthropic
            "prompt is too long: 210000 tokens > 200000 maximum",
            // Gemini
            "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).",
            // Mistral / others
            "Prompt contains 40000 tokens, too large for model with 32768 maximum context length",
            "Input exceeds the context window",
        ] {
            assert!(is_overflow_text(text), "{text}");
        }
    }

    #[test]
    fn rate_limits_are_not_overflows() {
        for text in [
            "Request too large for gpt-4o in organization org-x on tokens per min (TPM): Limit 30000, Requested 50000.",
            "Rate limit reached for model `llama-3.3-70b` in organization `org` service tier `on_demand` on tokens per minute (TPM): Limit 6000, Used 5000, Requested 2000.",
            "429 rate_limit_error: This request would exceed the rate limit for your organization of 40,000 input tokens per minute.",
            "max_tokens: 9000 > 8192, which is the maximum allowed number of output tokens",
        ] {
            assert!(!is_overflow_text(text), "{text}");
        }
    }

    #[test]
    fn an_unknown_window_has_no_silent_overflow() {
        let mut m = model();
        m.context_window = 0;
        let mut msg = AssistantMessage::pending(&m);
        msg.stop_reason = StopReason::Length;
        msg.usage.input = 10;
        assert!(!is_context_overflow(&m, &msg));
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("context_length_exceeded".into());
        assert!(is_context_overflow(&m, &msg));
    }
}
