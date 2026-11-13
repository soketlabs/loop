//! Context compaction and branch summarization helpers.
//!
//! Compaction replaces the older part of a Session's context with a summary
//! that the Session's model writes. It runs on `/compact` and, when
//! [`CompactionSettings::enabled`] is set, automatically inside a turn: before
//! a model request when the context is near the model's window, and once after
//! a request fails because the context is too long ([`CompactionTrigger`]).

mod auto;
mod summary;

pub use auto::{
    context_entry_ids, summarize, summary_input_limit, usage_valid_from, PendingTurnWrites,
    TurnCompactor,
};
pub use summary::{request_summary, SUMMARY_SYSTEM_PROMPT};

use serde::{Deserialize, Serialize};

use crate::harness::types::CompactionError;
use crate::types::{AgentMessage, CustomAgentMessage};
use loop_ai::{
    calculate_context_tokens, estimate_context_tokens, estimate_message_tokens, AssistantMessage,
    Message, StopReason,
};

/// Default compaction settings.
pub const DEFAULT_RESERVE_TOKENS: u64 = 16384;
/// Keep recent tokens.
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20000;

/// Compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettings {
    /// Enabled flag.
    pub enabled: bool,
    /// Reserve tokens under context window.
    pub reserve_tokens: u64,
    /// Keep recent tokens after cut.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

/// Default settings constant-like accessor.
pub fn default_compaction_settings() -> CompactionSettings {
    CompactionSettings::default()
}

/// Rough token estimate for LLM messages.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let ctx = loop_ai::Context {
        system_prompt: None,
        messages: messages.to_vec(),
        tools: None,
    };
    estimate_context_tokens(&ctx)
}

/// Whether compaction should run.
pub fn should_compact(
    total_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled || context_window == 0 {
        return false;
    }
    total_tokens + settings.reserve_tokens >= context_window
}

/// Find a cut index that keeps roughly `keep_recent_tokens` at the end.
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut kept = 0u64;
    for i in (0..messages.len()).rev() {
        // Per-message estimate: `estimate_tokens` on a single-message slice would
        // return the assistant's cumulative usage.total_tokens, not its own size.
        kept += estimate_message_tokens(&messages[i]);
        if kept >= keep_recent_tokens {
            let snap = find_turn_start_index(messages, i);
            if snap > 0 {
                return snap;
            }
            // Backward snap hit 0; look forward for a valid turn boundary
            // so compaction doesn't treat cut=0 as "nothing to compact".
            return messages[i + 1..]
                .iter()
                .position(|m| m.role() == "user")
                .map_or(0, |offset| i + 1 + offset);
        }
    }
    // The whole history fits within keep_recent_tokens. Fall back to cutting at
    // the start of the last user turn so an explicit compact still has effect.
    for i in (1..messages.len()).rev() {
        if messages[i].role() == "user" {
            return i;
        }
    }
    0
}

/// Snap cut to a turn boundary (prefer user message start).
pub fn find_turn_start_index(messages: &[Message], index: usize) -> usize {
    for i in (0..=index).rev() {
        if messages[i].role() == "user" {
            return i;
        }
    }
    index
}

/// Serialize conversation for a summarizer prompt.
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        out.push_str(&format!("[{}] {:?}\n", m.role(), m));
    }
    out
}

/// Prepared compaction plan.
#[derive(Debug, Clone)]
pub struct CompactionPreparation {
    /// Cut index in message list.
    pub cut_index: usize,
    /// Messages to summarize.
    pub to_summarize: Vec<AgentMessage>,
    /// Messages to retain.
    pub retained: Vec<AgentMessage>,
}

/// Prepare compaction over agent messages.
pub fn prepare_compaction(
    messages: &[AgentMessage],
    llm_messages: &[Message],
    settings: &CompactionSettings,
) -> Result<CompactionPreparation, CompactionError> {
    if messages.is_empty() {
        return Err(CompactionError::Failed("no messages".into()));
    }
    let cut = find_cut_point(llm_messages, settings.keep_recent_tokens);
    if cut == 0 {
        return Err(CompactionError::Failed("nothing to compact".into()));
    }
    Ok(CompactionPreparation {
        cut_index: cut,
        to_summarize: messages[..cut.min(messages.len())].to_vec(),
        retained: messages[cut.min(messages.len())..].to_vec(),
    })
}

/// Generate a local (non-LLM) summary fallback.
pub fn generate_summary_fallback(messages: &[AgentMessage]) -> String {
    format!(
        "Summary of {} messages. Latest roles: {}",
        messages.len(),
        messages
            .iter()
            .rev()
            .take(5)
            .map(|m| m.role())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Branch summary helpers — collect entries between leaves (simplified).
pub fn generate_branch_summary_fallback(entry_count: usize) -> String {
    format!("Branch with {entry_count} entries.")
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// `/compact` or `session/compact`.
    Manual,
    /// The context reached the window less the reserve before a request.
    Threshold,
    /// The provider refused a request because the context was too long.
    Overflow,
}

impl CompactionTrigger {
    /// The name recorded in the compaction entry's details.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// Why an automatic compaction did not replace the context.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutoCompactionError {
    /// The turn was cancelled while compacting.
    #[error("compaction was cancelled")]
    Cancelled,
    /// No older part of the context can be summarized.
    #[error("nothing to compact")]
    NothingToCompact,
    /// The compacted context is not smaller than the original.
    #[error("compaction did not reduce the context (~{before} to ~{after} tokens)")]
    NotReduced {
        /// Estimated tokens before.
        before: u64,
        /// Estimated tokens after.
        after: u64,
    },
    /// The model did not write a summary.
    #[error("the summary request failed: {0}")]
    Summary(String),
    /// A `session_before_compact` hook refused the compaction.
    #[error("a hook cancelled the compaction")]
    HookCancelled,
    /// The context does not match the Session tree, so the compaction cannot
    /// be recorded.
    #[error("the context does not match the Session tree: {0}")]
    Tree(String),
}

/// Rough token size of one agent message, without converting it.
pub fn estimate_agent_message_tokens(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::Llm(m) => estimate_message_tokens(m),
        AgentMessage::Custom(custom) => match custom {
            CustomAgentMessage::BashExecution {
                command, output, ..
            } => text_tokens(command) + text_tokens(output),
            CustomAgentMessage::Custom { content, .. } => text_tokens(content),
            CustomAgentMessage::BranchSummary { summary, .. }
            | CustomAgentMessage::CompactionSummary { summary, .. } => text_tokens(summary),
        },
    }
}

/// Rough token size of messages, each counted on its own (no usage numbers).
pub fn estimate_agent_messages_tokens(messages: &[AgentMessage]) -> u64 {
    messages.iter().map(estimate_agent_message_tokens).sum()
}

fn text_tokens(text: &str) -> u64 {
    // Same ~4 bytes per token as `loop_ai`'s estimate; bytes avoid a char walk.
    (text.len() as u64).div_ceil(4)
}

/// Whether an assistant message's usage measures the context it was sent.
fn usable_usage(message: &AssistantMessage) -> Option<u64> {
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        return None;
    }
    Some(calculate_context_tokens(&message.usage)).filter(|tokens| *tokens > 0)
}

/// Tokens the next request will carry: the usage of the newest response at
/// or after `usage_valid_from` plus an estimate of the messages after it.
/// Usage on messages before `usage_valid_from` describes a context that a
/// compaction has since replaced, so it is not used. Without usage, every
/// message and the system prompt are estimated.
///
/// The usage path reads only the messages after the newest response, so a
/// check per model request costs about one tool round, not the history.
pub fn context_tokens(
    messages: &[AgentMessage],
    usage_valid_from: usize,
    system_prompt: &str,
) -> u64 {
    let mut trailing = 0u64;
    for index in (usage_valid_from.min(messages.len())..messages.len()).rev() {
        let message = &messages[index];
        if let Some(tokens) = message.as_assistant().and_then(usable_usage) {
            return tokens.saturating_add(trailing);
        }
        trailing = trailing.saturating_add(estimate_agent_message_tokens(message));
    }
    let older: u64 = messages[..usage_valid_from.min(messages.len())]
        .iter()
        .map(estimate_agent_message_tokens)
        .sum();
    text_tokens(system_prompt)
        .saturating_add(older)
        .saturating_add(trailing)
}

/// Whether a response failed because the request did not fit the model's
/// context window.
pub fn is_context_overflow_error(model: &loop_ai::Model, message: &AssistantMessage) -> bool {
    loop_ai::is_context_overflow(model, message)
        || message.raw_stop_reason.as_deref() == Some("model_context_window_exceeded")
}

/// Where an automatic compaction cuts `messages` (LLM view, one per agent
/// message): the first kept message. It never splits a tool call from its
/// results, because it cuts only before a user or assistant message.
///
/// It keeps about `keep_recent_tokens`. It prefers the start of a user turn;
/// when that would keep more than `max_kept_tokens` (one long turn of tool
/// rounds), it cuts at the start of a tool round instead. Returns 0 when
/// nothing can be cut.
pub fn find_auto_cut_point(
    messages: &[Message],
    keep_recent_tokens: u64,
    max_kept_tokens: u64,
) -> usize {
    let n = messages.len();
    if n < 2 {
        return 0;
    }
    // suffix[i] = tokens of messages[i..].
    let mut suffix = vec![0u64; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1].saturating_add(estimate_message_tokens(&messages[i]));
    }
    // The first index where the kept tail reaches `keep_recent_tokens`; the
    // whole history when it is smaller.
    let reach = (0..n)
        .rev()
        .find(|&i| suffix[i] >= keep_recent_tokens)
        .unwrap_or(0);
    let boundary = |i: usize| i > 0 && matches!(messages[i].role(), "user" | "assistant");

    // A user turn start at or before `reach` keeps whole turns.
    if let Some(turn) = (1..=reach).rev().find(|&i| messages[i].role() == "user") {
        if suffix[turn] <= max_kept_tokens.max(keep_recent_tokens) {
            return turn;
        }
    }
    // Otherwise the nearest round start: at or after `reach` keeps less,
    // never the whole history and never nothing.
    if let Some(round) = (reach.max(1)..n).find(|&i| boundary(i)) {
        return round;
    }
    (1..reach.min(n)).rev().find(|&i| boundary(i)).unwrap_or(0)
}

/// The text of the newest user message in `messages`, for a summary that
/// replaces it.
pub fn latest_user_text(messages: &[AgentMessage]) -> Option<String> {
    messages.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(Message::User(user)) => {
            let text = match &user.content {
                loop_ai::UserMessageContent::Text(t) => t.clone(),
                loop_ai::UserMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        loop_ai::UserContent::Text(t) => Some(t.text.as_str()),
                        loop_ai::UserContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            Some(text)
        }
        _ => None,
    })
}

/// Whether `messages` hold a user message (not a summary or other custom
/// message).
pub fn has_user_message(messages: &[AgentMessage]) -> bool {
    messages
        .iter()
        .any(|m| matches!(m, AgentMessage::Llm(Message::User(_))))
}

/// The summary text when the cut removes the user's current request: the
/// request follows the summary, quoted, so the model still sees it.
pub fn summary_with_request(summary: &str, request: Option<&str>) -> String {
    match request {
        Some(request) if !request.trim().is_empty() => format!(
            "{summary}\n\nThe user's current request, quoted exactly:\n<request>\n{request}\n</request>"
        ),
        _ => summary.to_string(),
    }
}

/// The `details` value of a compaction entry: what started it, the estimated
/// sizes and the summary request's usage, which session statistics count.
pub fn compaction_details(
    trigger: CompactionTrigger,
    tokens_before: u64,
    tokens_after: u64,
    usage: Option<&loop_ai::Usage>,
) -> serde_json::Value {
    let mut details = serde_json::json!({
        "trigger": trigger.as_str(),
        "tokensBefore": tokens_before,
        "tokensAfter": tokens_after,
    });
    if let Some(usage) = usage {
        details["usage"] = serde_json::to_value(usage).unwrap_or_default();
    }
    details
}

/// The summary request's usage stored in a compaction entry's `details`.
pub fn usage_from_details(details: Option<&serde_json::Value>) -> Option<loop_ai::Usage> {
    details
        .and_then(|d| d.get("usage"))
        .and_then(|u| serde_json::from_value(u.clone()).ok())
}
