//! The summary request: the Session's model summarizes the part of the
//! context that a compaction removes.

use futures::StreamExt;
use loop_ai::{
    AssistantContent, Context, Message, Model, SimpleStreamOptions, StopReason, ToolResultContent,
    Usage, UserContent, UserMessageContent,
};
use tokio_util::sync::CancellationToken;

use super::AutoCompactionError;
use crate::stream_fn::StreamFn;
use crate::types::{AgentMessage, CustomAgentMessage};

/// System prompt of the summary request.
pub const SUMMARY_SYSTEM_PROMPT: &str = "You summarize the older part of a conversation \
between a user and a coding agent. Your summary replaces that part, and the agent continues \
the work from it. Keep the user's goals and requests, decisions and constraints, file paths, \
commands, errors and their fixes, what is done and what remains to do. Be short and exact. \
Write only the summary.";

/// Longest text kept of one tool result or message.
const MAX_ITEM_CHARS: usize = 4_000;

/// Ask `model` for a summary of `messages`. The request text is cut to
/// about `max_input_tokens`. Returns the summary and the request's usage.
pub async fn request_summary(
    stream_fn: &StreamFn,
    model: &Model,
    options: SimpleStreamOptions,
    messages: &[AgentMessage],
    custom_instructions: Option<&str>,
    max_input_tokens: u64,
    cancel: &CancellationToken,
) -> Result<(String, Usage), AutoCompactionError> {
    let conversation = transcript_text(messages, max_input_tokens);
    let mut prompt = format!(
        "<conversation>\n{conversation}\n</conversation>\n\nWrite the summary of this conversation."
    );
    if let Some(extra) = custom_instructions.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n\nAlso follow these instructions: ");
        prompt.push_str(extra);
    }
    let context = Context {
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::user_text(prompt)],
        tools: None,
    };
    let mut options = options;
    options.base.cancel = Some(cancel.clone());

    let request = async {
        let mut stream = stream_fn(model.clone(), context, options).await;
        while stream.next().await.is_some() {}
        stream.result().await
    };
    let message = tokio::select! {
        message = request => message,
        _ = cancel.cancelled() => return Err(AutoCompactionError::Cancelled),
    };
    if cancel.is_cancelled() || message.stop_reason == StopReason::Aborted {
        return Err(AutoCompactionError::Cancelled);
    }
    if message.stop_reason == StopReason::Error {
        return Err(AutoCompactionError::Summary(
            message
                .error_message
                .unwrap_or_else(|| "the model returned an error".into()),
        ));
    }
    let text: String = message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err(AutoCompactionError::Summary(
            "the model returned an empty summary".into(),
        ));
    }
    Ok((text.to_string(), message.usage))
}

/// The conversation as plain text for the summary request, at most about
/// `max_tokens` (4 bytes per token). When it is longer, its start (an older
/// summary, the first request) and its end are kept.
fn transcript_text(messages: &[AgentMessage], max_tokens: u64) -> String {
    let mut out = String::new();
    for message in messages {
        let (label, body) = message_text(message);
        out.push('[');
        out.push_str(label);
        out.push_str("]\n");
        out.push_str(&clip(&body, MAX_ITEM_CHARS));
        out.push_str("\n\n");
    }
    let limit = usize::try_from(max_tokens.saturating_mul(4)).unwrap_or(usize::MAX);
    if out.len() <= limit {
        return out;
    }
    let head = floor_boundary(&out, limit / 4);
    let tail = ceil_boundary(&out, out.len() - (limit - limit / 4));
    format!(
        "{}\n[... older messages left out ...]\n{}",
        &out[..head],
        &out[tail..]
    )
}

fn message_text(message: &AgentMessage) -> (&'static str, String) {
    match message {
        AgentMessage::Llm(Message::User(user)) => (
            "user",
            match &user.content {
                UserMessageContent::Text(t) => t.clone(),
                UserMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        UserContent::Text(t) => t.text.clone(),
                        UserContent::Image(_) => "[image]".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
        ),
        AgentMessage::Llm(Message::Assistant(a)) => (
            "assistant",
            a.content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.clone()),
                    AssistantContent::ToolCall(call) => {
                        Some(format!("[tool call {}] {}", call.name, call.arguments))
                    }
                    AssistantContent::Thinking(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        AgentMessage::Llm(Message::ToolResult(result)) => (
            "tool result",
            format!(
                "{}{}: {}",
                result.tool_name,
                if result.is_error { " (error)" } else { "" },
                result
                    .content
                    .iter()
                    .map(|c| match c {
                        ToolResultContent::Text(t) => t.text.clone(),
                        ToolResultContent::Image(_) => "[image]".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ),
        AgentMessage::Custom(custom) => match custom {
            CustomAgentMessage::CompactionSummary { summary, .. } => {
                ("summary of earlier conversation", summary.clone())
            }
            CustomAgentMessage::BranchSummary { summary, .. } => {
                ("summary of another branch", summary.clone())
            }
            CustomAgentMessage::BashExecution {
                command, output, ..
            } => ("user shell command", format!("$ {command}\n{output}")),
            CustomAgentMessage::Custom {
                custom_type,
                content,
                ..
            } => ("note", format!("{custom_type}: {content}")),
        },
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let end = floor_boundary(text, max);
    format!("{}… [{} more bytes]", &text[..end], text.len() - end)
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_transcripts_keep_start_and_end() {
        let messages: Vec<AgentMessage> = (0..200)
            .map(|i| AgentMessage::user_text(format!("message {i} {}", "é".repeat(300))))
            .collect();
        let text = transcript_text(&messages, 2_000);
        assert!(text.len() <= 8_000 + 100, "{}", text.len());
        assert!(text.contains("message 0 "));
        assert!(text.contains("message 199 "));
        assert!(text.contains("older messages left out"));
    }
}
