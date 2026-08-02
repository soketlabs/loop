//! Rough context token estimation.

use crate::types::{
    AssistantContent, Context, Message, ToolResultContent, Usage, UserContent, UserMessageContent,
};

/// Context size implied by a usage record.
///
/// Prefers `total_tokens` when set; otherwise sums input/output/cache components.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

/// Rough estimate of tokens in a context (~4 chars/token).
///
/// Prefer last successful assistant `usage.total_tokens` + estimate of trailing
/// messages when available.
pub fn estimate_context_tokens(context: &Context) -> u64 {
    let mut last_usage_idx: Option<usize> = None;
    let mut last_total = 0u64;

    for (i, msg) in context.messages.iter().enumerate() {
        if let Message::Assistant(a) = msg {
            if a.usage.total_tokens > 0
                && !matches!(
                    a.stop_reason,
                    crate::types::StopReason::Error | crate::types::StopReason::Aborted
                )
            {
                last_usage_idx = Some(i);
                last_total = a.usage.total_tokens;
            }
        }
    }

    if let Some(idx) = last_usage_idx {
        let trailing: u64 = context.messages[idx + 1..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        return last_total.saturating_add(trailing);
    }

    let mut total = estimate_text_tokens(context.system_prompt.as_deref().unwrap_or(""));
    if let Some(tools) = &context.tools {
        for tool in tools {
            total += estimate_text_tokens(&tool.name);
            total += estimate_text_tokens(&tool.description);
            total += estimate_text_tokens(&tool.parameters.to_string());
        }
    }
    for msg in &context.messages {
        total += estimate_message_tokens(msg);
    }
    total
}

/// Rough estimate of a single message's own token size (~4 chars/token).
pub fn estimate_message_tokens(msg: &Message) -> u64 {
    match msg {
        Message::User(u) => match &u.content {
            UserMessageContent::Text(t) => estimate_text_tokens(t),
            UserMessageContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    UserContent::Text(t) => estimate_text_tokens(&t.text),
                    UserContent::Image(_) => 1000, // rough vision token stub
                })
                .sum(),
        },
        Message::Assistant(a) => a
            .content
            .iter()
            .map(|b| match b {
                AssistantContent::Text(t) => estimate_text_tokens(&t.text),
                AssistantContent::Thinking(t) => estimate_text_tokens(&t.thinking),
                AssistantContent::ToolCall(tc) => {
                    estimate_text_tokens(&tc.name) + estimate_text_tokens(&tc.arguments.to_string())
                }
            })
            .sum(),
        Message::ToolResult(t) => t
            .content
            .iter()
            .map(|b| match b {
                ToolResultContent::Text(t) => estimate_text_tokens(&t.text),
                ToolResultContent::Image(_) => 1000,
            })
            .sum(),
    }
}

fn estimate_text_tokens(text: &str) -> u64 {
    ((text.chars().count() as f64) / 4.0).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_simple_context() {
        let ctx = Context {
            system_prompt: Some("abcd".into()), // 1 token
            messages: vec![Message::user_text("abcdefgh")], // 2 tokens
            tools: None,
        };
        assert_eq!(estimate_context_tokens(&ctx), 3);
    }

    #[test]
    fn calculates_context_tokens_prefers_total() {
        let usage = Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            total_tokens: 99,
            ..Usage::empty()
        };
        assert_eq!(calculate_context_tokens(&usage), 99);
    }

    #[test]
    fn calculates_context_tokens_sums_when_total_zero() {
        let usage = Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            ..Usage::empty()
        };
        assert_eq!(calculate_context_tokens(&usage), 10);
    }
}
