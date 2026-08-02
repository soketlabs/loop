//! Context compaction and branch summarization helpers.

use serde::{Deserialize, Serialize};

use crate::harness::types::CompactionError;
use crate::types::AgentMessage;
use loop_ai::{estimate_context_tokens, estimate_message_tokens, Message};

/// Default compaction settings (pi parity).
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
pub fn should_compact(total_tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool {
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
            for j in (i + 1)..messages.len() {
                if messages[j].role() == "user" {
                    return j;
                }
            }
            return 0;
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
