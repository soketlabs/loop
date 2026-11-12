//! Context compaction and branch summarization helpers.

use serde::{Deserialize, Serialize};

use crate::harness::types::CompactionError;
use crate::types::AgentMessage;
use loop_ai::{estimate_context_tokens, estimate_message_tokens, Message};

/// Default compaction settings.
pub const DEFAULT_RESERVE_TOKENS: u64 = 16384;
/// Keep recent tokens.
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20000;
/// Summarize once the context reaches this percentage of the model's context window.
pub const DEFAULT_TRIGGER_PERCENT: u64 = 90;

/// Compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettings {
    /// Enabled flag.
    pub enabled: bool,
    /// Reserve tokens under context window.
    pub reserve_tokens: u64,
    /// Keep recent tokens after cut.
    pub keep_recent_tokens: u64,
    /// Compact once the context reaches this many tokens, but never later than
    /// `trigger_percent` of the model's window (Codex's `auto_compact_token_limit` rule).
    #[serde(default)]
    pub trigger_tokens: Option<u64>,
    /// Compact once the context reaches this percentage of the model's context window.
    #[serde(default = "default_trigger_percent")]
    pub trigger_percent: u64,
}

fn default_trigger_percent() -> u64 {
    DEFAULT_TRIGGER_PERCENT
}
impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            trigger_tokens: None,
            trigger_percent: DEFAULT_TRIGGER_PERCENT,
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

/// Token count at which summarizing starts, Codex style: `trigger_percent` (90) of the model's
/// context window, and a configured `trigger_tokens` can only lower that, never raise it.
/// `None` when neither the window nor a configured trigger is known.
pub fn compaction_limit(context_window: u64, settings: &CompactionSettings) -> Option<u64> {
    let percent = settings.trigger_percent.min(100);
    let from_window = (context_window > 0 && percent > 0)
        .then(|| (u128::from(context_window) * u128::from(percent) / 100) as u64);
    let configured = settings.trigger_tokens.filter(|t| *t > 0);
    match (from_window, configured) {
        (Some(w), Some(c)) => Some(w.min(c)),
        (w, c) => w.or(c),
    }
}

/// [`compaction_limit`] that also leaves room for the reply. A request reserves the model's
/// maximum output on top of its input, so the limit is the lower of the configured one and
/// the window minus `max_output_tokens`. A model whose maximum output is more than half its
/// window still gets half the window to work in.
pub fn compaction_limit_with_reply_room(
    context_window: u64,
    max_output_tokens: u64,
    settings: &CompactionSettings,
) -> Option<u64> {
    let limit = compaction_limit(context_window, settings)?;
    if context_window == 0 {
        return Some(limit);
    }
    let room = context_window
        .saturating_sub(max_output_tokens)
        .max(context_window / 2);
    Some(limit.min(room))
}

/// How many recent tokens a summary keeps verbatim: `keep_recent_tokens`, but at most a third of the
/// trigger, so on small windows there is always an older part to summarize and the result lands
/// well under the limit instead of re-triggering every turn.
pub fn effective_keep_recent_tokens(limit: Option<u64>, keep_recent_tokens: u64) -> u64 {
    limit.map_or(keep_recent_tokens, |l| {
        keep_recent_tokens.min((l / 3).max(1))
    })
}

/// Whether compaction should run.
pub fn should_compact(
    total_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled
        && compaction_limit(context_window, settings).is_some_and(|limit| total_tokens >= limit)
}

mod auto;
pub use auto::{
    auto_compact_context, force_compact_context, plan_in_loop_compaction, render_for_summary,
    AutoCompaction, InLoopPlan,
};

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
