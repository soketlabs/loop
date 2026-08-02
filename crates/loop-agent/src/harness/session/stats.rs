//! Session statistics: token/cost aggregation for `/session`.

use std::collections::BTreeMap;

use loop_ai::{
    calculate_context_tokens, estimate_context_tokens, AssistantContent, Context, Cost, Message,
    Model, Models, StopReason, Usage,
};

use crate::harness::session::types::SessionTreeEntry;
use crate::messages::convert_to_llm;
use crate::types::AgentMessage;

/// Prompt-cache TTL: idle gaps longer than this often cause misses (Anthropic default).
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// Cumulative usage totals.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Optional 1h cache-write tokens.
    pub cache_write_1h: u64,
    /// Reasoning tokens (when reported).
    pub reasoning: u64,
    /// Dollar cost totals.
    pub cost: Cost,
}

impl UsageTotals {
    /// Prompt volume: input + cache read + cache write.
    pub fn prompt_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// All billed token components.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens().saturating_add(self.output)
    }

    /// Fold another usage record in.
    pub fn add_usage(&mut self, usage: &Usage) {
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write);
        if let Some(v) = usage.cache_write_1h {
            self.cache_write_1h = self.cache_write_1h.saturating_add(v);
        }
        if let Some(v) = usage.reasoning {
            self.reasoning = self.reasoning.saturating_add(v);
        }
        self.cost.input += usage.cost.input;
        self.cost.output += usage.cost.output;
        self.cost.cache_read += usage.cost.cache_read;
        self.cost.cache_write += usage.cost.cache_write;
        self.cost.total += usage.cost.total;
    }
}

/// Current context window occupancy.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextUsage {
    /// Estimated tokens in the active context, or `None` when unknown (e.g. post-compaction).
    pub tokens: Option<u64>,
    /// Model context window.
    pub context_window: u64,
    /// Percent of window used, or `None` when tokens are unknown.
    pub percent: Option<f64>,
}

/// Cache re-bill / waste totals across the session.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    /// Prompt tokens that should have been cache reads but were re-billed.
    pub missed_tokens: u64,
    /// Extra dollars paid vs a full cache hit.
    pub missed_cost: f64,
    /// Number of counted misses (above the noise floor).
    pub miss_count: u64,
}

/// Per-model (or tools/summaries) usage bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageBreakdownEntry {
    /// Display key, e.g. `provider/model` or `Tools/summaries`.
    pub key: String,
    /// Token totals for this key.
    pub tokens: UsageTotals,
    /// Number of assistant (or tool) usage contributions.
    pub turns: u64,
}

/// Snapshot of the latest assistant turn with non-zero usage.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestTurnUsage {
    /// Provider.
    pub provider: String,
    /// Requested model.
    pub model: String,
    /// Response model if different.
    pub response_model: Option<String>,
    /// Stop reason.
    pub stop_reason: StopReason,
    /// Usage for that turn.
    pub usage: Usage,
}

/// Full session statistics for `/session`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStats {
    /// Session id.
    pub session_id: String,
    /// Optional display name.
    pub session_name: Option<String>,
    /// Optional on-disk path (JSONL).
    pub session_path: Option<String>,
    /// Working directory label.
    pub cwd: Option<String>,
    /// Created at unix ms.
    pub created_at: i64,
    /// Parent session when forked.
    pub parent_session_id: Option<String>,

    /// Total LLM + custom messages on all entries.
    pub total_messages: u64,
    /// User messages.
    pub user_messages: u64,
    /// Assistant messages.
    pub assistant_messages: u64,
    /// Assistant messages that aborted.
    pub assistant_aborted: u64,
    /// Assistant messages that errored.
    pub assistant_error: u64,
    /// Tool result messages.
    pub tool_results: u64,
    /// Tool calls embedded in assistant content.
    pub tool_calls: u64,
    /// Custom / harness messages.
    pub custom_messages: u64,
    /// Compaction entries.
    pub compactions: u64,
    /// Branch summary entries.
    pub branch_summaries: u64,
    /// Model change entries.
    pub model_changes: u64,

    /// Lifetime token/cost totals (all session entries, including compacted history).
    pub tokens: UsageTotals,
    /// Assistant turns that contributed usage.
    pub usage_turns: u64,
    /// Per-model / tools breakdown.
    pub breakdown: Vec<UsageBreakdownEntry>,
    /// Cache waste.
    pub cache_waste: CacheWasteTotals,
    /// Current context occupancy for the active model.
    pub context_usage: Option<ContextUsage>,
    /// Latest assistant usage snapshot.
    pub latest_turn: Option<LatestTurnUsage>,
    /// Active model id label (`provider/id`).
    pub active_model: String,
    /// Active model max output tokens.
    pub max_tokens: u64,
}

/// Inputs for computing [`SessionStats`].
pub struct SessionStatsInput<'a> {
    /// All session entries (full tree history).
    pub all_entries: &'a [SessionTreeEntry],
    /// Active branch path (root/compaction → leaf).
    pub branch_entries: &'a [SessionTreeEntry],
    /// Active branch agent messages (from `build_context`).
    pub branch_messages: &'a [AgentMessage],
    /// Session metadata fields.
    pub session_id: &'a str,
    /// Display name.
    pub session_name: Option<&'a str>,
    /// On-disk path.
    pub session_path: Option<&'a str>,
    /// CWD.
    pub cwd: Option<&'a str>,
    /// Created at.
    pub created_at: i64,
    /// Parent session.
    pub parent_session_id: Option<&'a str>,
    /// Active model.
    pub model: &'a Model,
    /// System prompt used for context estimate.
    pub system_prompt: &'a str,
    /// Tools included in context estimate (optional).
    pub tools: Option<&'a [loop_ai::Tool]>,
    /// Model registry for cache-read pricing fallback.
    pub models: Option<&'a Models>,
}

/// Aggregate session statistics from entries + active context.
pub fn compute_session_stats(input: SessionStatsInput<'_>) -> SessionStats {
    let mut totals = UsageTotals::default();
    let mut breakdown: BTreeMap<String, (UsageTotals, u64)> = BTreeMap::new();

    let mut total_messages = 0u64;
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut assistant_aborted = 0u64;
    let mut assistant_error = 0u64;
    let mut tool_results = 0u64;
    let mut tool_calls = 0u64;
    let mut custom_messages = 0u64;
    let mut compactions = 0u64;
    let mut branch_summaries = 0u64;
    let mut model_changes = 0u64;
    let mut usage_turns = 0u64;
    let mut latest_turn: Option<LatestTurnUsage> = None;

    for entry in input.all_entries {
        match entry {
            SessionTreeEntry::Compaction { .. } => {
                compactions += 1;
            }
            SessionTreeEntry::BranchSummary { .. } => {
                branch_summaries += 1;
            }
            SessionTreeEntry::ModelChange { .. } => {
                model_changes += 1;
            }
            SessionTreeEntry::Message { message, .. } => match message {
                AgentMessage::Custom(_) => {
                    total_messages += 1;
                    custom_messages += 1;
                }
                AgentMessage::Llm(Message::User(_)) => {
                    total_messages += 1;
                    user_messages += 1;
                }
                AgentMessage::Llm(Message::ToolResult(tr)) => {
                    total_messages += 1;
                    tool_results += 1;
                    if let Some(usage) = &tr.usage {
                        if calculate_context_tokens(usage) > 0 || usage.cost.total > 0.0 {
                            totals.add_usage(usage);
                            usage_turns += 1;
                            add_breakdown(&mut breakdown, "Tools/summaries", usage);
                        }
                    }
                }
                AgentMessage::Llm(Message::Assistant(a)) => {
                    total_messages += 1;
                    assistant_messages += 1;
                    if matches!(a.stop_reason, StopReason::Aborted) {
                        assistant_aborted += 1;
                    }
                    if matches!(a.stop_reason, StopReason::Error) {
                        assistant_error += 1;
                    }
                    tool_calls += a
                        .content
                        .iter()
                        .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                        .count() as u64;

                    let has_usage =
                        calculate_context_tokens(&a.usage) > 0 || a.usage.cost.total > 0.0;
                    if has_usage {
                        totals.add_usage(&a.usage);
                        usage_turns += 1;
                        let model_id = a.response_model.as_deref().unwrap_or(a.model.as_str());
                        let key = format!("{}/{}", a.provider, model_id);
                        add_breakdown(&mut breakdown, &key, &a.usage);
                        latest_turn = Some(LatestTurnUsage {
                            provider: a.provider.clone(),
                            model: a.model.clone(),
                            response_model: a.response_model.clone(),
                            stop_reason: a.stop_reason,
                            usage: a.usage.clone(),
                        });
                    }
                }
            },
            _ => {}
        }
    }

    let mut breakdown_vec: Vec<UsageBreakdownEntry> = breakdown
        .into_iter()
        .map(|(key, (tokens, turns))| UsageBreakdownEntry { key, tokens, turns })
        .filter(|e| e.tokens.total_tokens() > 0 || e.tokens.cost.total > 0.0)
        .collect();
    breakdown_vec.sort_by(|a, b| {
        b.tokens
            .cost
            .total
            .partial_cmp(&a.tokens.cost.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.tokens.total_tokens().cmp(&a.tokens.total_tokens()))
    });

    let cache_waste = compute_cache_waste(input.all_entries, input.models);
    let context_usage = compute_context_usage(
        input.model,
        input.branch_entries,
        input.branch_messages,
        input.system_prompt,
        input.tools,
    );

    SessionStats {
        session_id: input.session_id.to_string(),
        session_name: input.session_name.map(str::to_string),
        session_path: input.session_path.map(str::to_string),
        cwd: input.cwd.map(str::to_string),
        created_at: input.created_at,
        parent_session_id: input.parent_session_id.map(str::to_string),
        total_messages,
        user_messages,
        assistant_messages,
        assistant_aborted,
        assistant_error,
        tool_results,
        tool_calls,
        custom_messages,
        compactions,
        branch_summaries,
        model_changes,
        tokens: totals,
        usage_turns,
        breakdown: breakdown_vec,
        cache_waste,
        context_usage,
        latest_turn,
        active_model: format!("{}/{}", input.model.provider, input.model.id),
        max_tokens: input.model.max_tokens,
    }
}

fn add_breakdown(map: &mut BTreeMap<String, (UsageTotals, u64)>, key: &str, usage: &Usage) {
    let entry = map.entry(key.to_string()).or_default();
    entry.0.add_usage(usage);
    entry.1 = entry.1.saturating_add(1);
}

fn compute_context_usage(
    model: &Model,
    branch_entries: &[SessionTreeEntry],
    branch_messages: &[AgentMessage],
    system_prompt: &str,
    tools: Option<&[loop_ai::Tool]>,
) -> Option<ContextUsage> {
    let context_window = model.context_window;
    if context_window == 0 {
        return None;
    }

    if let Some(comp_idx) = branch_entries
        .iter()
        .rposition(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
    {
        let mut has_post = false;
        for entry in &branch_entries[comp_idx + 1..] {
            if let SessionTreeEntry::Message {
                message: AgentMessage::Llm(Message::Assistant(a)),
                ..
            } = entry
            {
                if !matches!(a.stop_reason, StopReason::Aborted | StopReason::Error)
                    && calculate_context_tokens(&a.usage) > 0
                {
                    has_post = true;
                    break;
                }
            }
        }
        if !has_post {
            return Some(ContextUsage {
                tokens: None,
                context_window,
                percent: None,
            });
        }
    }

    let llm = convert_to_llm(branch_messages);
    let ctx = Context {
        system_prompt: if system_prompt.is_empty() {
            None
        } else {
            Some(system_prompt.to_string())
        },
        messages: llm,
        tools: tools.map(|t| t.to_vec()),
    };
    let tokens = estimate_context_tokens(&ctx);
    let percent = (tokens as f64 / context_window as f64) * 100.0;
    Some(ContextUsage {
        tokens: Some(tokens),
        context_window,
        percent: Some(percent),
    })
}

struct PreviousRequest {
    prompt_tokens: u64,
    #[allow(dead_code)]
    model_key: String,
    #[allow(dead_code)]
    timestamp: i64,
    reported_cache: bool,
}

/// Cumulative cache waste: prompt tokens that should have been cache reads but were re-billed.
pub fn compute_cache_waste(
    entries: &[SessionTreeEntry],
    models: Option<&Models>,
) -> CacheWasteTotals {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();

    for entry in entries {
        match entry {
            SessionTreeEntry::Compaction { .. } | SessionTreeEntry::BranchSummary { .. } => {
                prev = None;
            }
            SessionTreeEntry::Message {
                message: AgentMessage::Llm(Message::Assistant(a)),
                ..
            } => {
                if let Some(miss) = detect_cache_miss(prev.as_ref(), a, models) {
                    totals.missed_tokens = totals.missed_tokens.saturating_add(miss.0);
                    totals.missed_cost += miss.1;
                    totals.miss_count = totals.miss_count.saturating_add(1);
                }
                let reported = prev.as_ref().map(|p| p.reported_cache).unwrap_or(false);
                if let Some(next) = as_previous_request(a, reported) {
                    prev = Some(next);
                }
            }
            _ => {}
        }
    }

    totals
}

fn detect_cache_miss(
    prev: Option<&PreviousRequest>,
    message: &loop_ai::AssistantMessage,
    models: Option<&Models>,
) -> Option<(u64, f64)> {
    let usage = &message.usage;
    let prompt_tokens = usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);
    let prev = prev?;
    if prompt_tokens == 0 {
        return None;
    }
    if usage.cache_read + usage.cache_write == 0 && !prev.reported_cache {
        return None;
    }

    let missed_tokens = prev.prompt_tokens.min(prompt_tokens).saturating_sub(usage.cache_read);
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    let paid_tokens = usage.input.saturating_add(usage.cache_write);
    let paid_per_token = if paid_tokens > 0 {
        (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost.cache_read / usage.cache_read as f64
    } else {
        models
            .and_then(|m| m.get_model(&message.provider, &message.model))
            .map(|m| m.cost.cache_read / 1_000_000.0)
            .unwrap_or(0.0)
    };

    Some((
        missed_tokens,
        missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
    ))
}

fn as_previous_request(
    message: &loop_ai::AssistantMessage,
    reported_cache: bool,
) -> Option<PreviousRequest> {
    let usage = &message.usage;
    let prompt_tokens = usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);
    if prompt_tokens == 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens,
        model_key: format!("{}/{}", message.provider, message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || usage.cache_read + usage.cache_write > 0,
    })
}

/// Format [`SessionStats`] as a multi-line report for the chat transcript.
pub fn format_session_stats(stats: &SessionStats) -> String {
    let mut out = String::new();
    out.push_str("Session Info\n");
    if let Some(name) = &stats.session_name {
        out.push_str(&format!("  Name: {name}\n"));
    }
    out.push_str(&format!("  ID: {}\n", stats.session_id));
    if let Some(path) = &stats.session_path {
        out.push_str(&format!("  File: {path}\n"));
    }
    if let Some(cwd) = &stats.cwd {
        out.push_str(&format!("  CWD: {cwd}\n"));
    }
    if let Some(parent) = &stats.parent_session_id {
        out.push_str(&format!("  Parent: {parent}\n"));
    }
    out.push_str(&format!("  Created: {}\n", format_unix_ms(stats.created_at)));
    out.push_str(&format!("  Active model: {}\n", stats.active_model));

    out.push_str("\nMessages\n");
    out.push_str(&format!("  Total: {}\n", stats.total_messages));
    out.push_str(&format!("  User: {}\n", stats.user_messages));
    let mut asst = format!("  Assistant: {}", stats.assistant_messages);
    let mut asst_bits = Vec::new();
    if stats.assistant_aborted > 0 {
        asst_bits.push(format!("{} aborted", stats.assistant_aborted));
    }
    if stats.assistant_error > 0 {
        asst_bits.push(format!("{} error", stats.assistant_error));
    }
    if !asst_bits.is_empty() {
        asst.push_str(&format!(" ({})", asst_bits.join(", ")));
    }
    out.push_str(&format!("{asst}\n"));
    out.push_str(&format!(
        "  Tools: {} calls, {} results\n",
        stats.tool_calls, stats.tool_results
    ));
    if stats.custom_messages > 0 {
        out.push_str(&format!("  Custom: {}\n", stats.custom_messages));
    }
    if stats.compactions > 0 {
        out.push_str(&format!("  Compactions: {}\n", stats.compactions));
    }
    if stats.branch_summaries > 0 {
        out.push_str(&format!("  Branch summaries: {}\n", stats.branch_summaries));
    }
    if stats.model_changes > 0 {
        out.push_str(&format!("  Model changes: {}\n", stats.model_changes));
    }

    out.push_str("\nTokens (lifetime)\n");
    let t = &stats.tokens;
    let prompt = t.prompt_tokens();
    out.push_str(&format!("  Input (prompt): {}\n", fmt_u64(prompt)));
    if prompt > 0 && (t.cache_read > 0 || t.cache_write > 0) {
        let hit = (t.cache_read as f64 / prompt as f64) * 100.0;
        out.push_str(&format!(
            "    Cached: {} ({:.1}%)\n",
            fmt_u64(t.cache_read),
            hit
        ));
        let uncached = t.input.saturating_add(t.cache_write);
        let written = if t.cache_write > 0 {
            format!(" ({} written to cache)", fmt_u64(t.cache_write))
        } else {
            String::new()
        };
        out.push_str(&format!(
            "    Uncached: {}{}\n",
            fmt_u64(uncached),
            written
        ));
    } else {
        out.push_str(&format!("    Uncached input: {}\n", fmt_u64(t.input)));
    }
    out.push_str(&format!("  Output: {}\n", fmt_u64(t.output)));
    if t.reasoning > 0 {
        out.push_str(&format!("    Reasoning: {}\n", fmt_u64(t.reasoning)));
    }
    if t.cache_write_1h > 0 {
        out.push_str(&format!(
            "  Cache write (1h): {}\n",
            fmt_u64(t.cache_write_1h)
        ));
    }
    out.push_str(&format!("  Total: {}\n", fmt_u64(t.total_tokens())));
    out.push_str(&format!("  Turns with usage: {}\n", stats.usage_turns));
    if stats.usage_turns > 0 {
        let avg = t.total_tokens() as f64 / stats.usage_turns as f64;
        out.push_str(&format!("  Avg tokens / turn: {:.0}\n", avg));
    }

    if let Some(latest) = &stats.latest_turn {
        let u = &latest.usage;
        let model_label = latest
            .response_model
            .as_deref()
            .unwrap_or(latest.model.as_str());
        out.push_str(&format!(
            "  Latest turn ({}/{}, {:?}): in={} out={} cache_read={} cache_write={} total={}\n",
            latest.provider,
            model_label,
            latest.stop_reason,
            fmt_u64(u.input),
            fmt_u64(u.output),
            fmt_u64(u.cache_read),
            fmt_u64(u.cache_write),
            fmt_u64(calculate_context_tokens(u)),
        ));
    }

    let show_cost = t.cost.total > 0.0
        || t.cost.input > 0.0
        || t.cost.output > 0.0
        || stats.cache_waste.missed_tokens > 0
        || stats.breakdown.iter().any(|b| b.tokens.cost.total > 0.0);
    if show_cost {
        out.push_str("\nCost\n");
        out.push_str(&format!("  Total: ${:.4}\n", t.cost.total));
        if t.cost.input > 0.0 || t.cost.output > 0.0 || t.cost.cache_read > 0.0 || t.cost.cache_write > 0.0
        {
            out.push_str(&format!("    Input: ${:.4}\n", t.cost.input));
            out.push_str(&format!("    Output: ${:.4}\n", t.cost.output));
            out.push_str(&format!("    Cache read: ${:.4}\n", t.cost.cache_read));
            out.push_str(&format!("    Cache write: ${:.4}\n", t.cost.cache_write));
        }
        if t.total_tokens() > 0 && t.cost.total > 0.0 {
            let per_m = t.cost.total / (t.total_tokens() as f64 / 1_000_000.0);
            out.push_str(&format!("  Effective rate: ${:.4} / 1M tokens\n", per_m));
        }
        if stats.breakdown.len() > 1
            || stats
                .breakdown
                .first()
                .is_some_and(|b| b.key != stats.active_model)
        {
            for entry in &stats.breakdown {
                out.push_str(&format!(
                    "  {}: ${:.4} ({} tokens, {} turns)\n",
                    entry.key,
                    entry.tokens.cost.total,
                    fmt_u64(entry.tokens.total_tokens()),
                    entry.turns
                ));
            }
        }
        if stats.cache_waste.missed_tokens > 0 {
            let miss_label = if stats.cache_waste.miss_count == 1 {
                "1 miss".to_string()
            } else {
                format!("{} misses", stats.cache_waste.miss_count)
            };
            let detail = format!(
                "{} tokens, {miss_label}",
                fmt_u64(stats.cache_waste.missed_tokens)
            );
            if stats.cache_waste.missed_cost >= 0.0001 {
                out.push_str(&format!(
                    "  Cache re-billed: ${:.4} ({detail})\n",
                    stats.cache_waste.missed_cost
                ));
            } else {
                out.push_str(&format!("  Cache re-billed: {detail}\n"));
            }
        }
    }

    if let Some(ctx) = &stats.context_usage {
        out.push_str("\nContext\n");
        match (ctx.tokens, ctx.percent) {
            (Some(tokens), Some(pct)) => {
                out.push_str(&format!(
                    "  Current: {} / {} ({:.1}%)\n",
                    fmt_u64(tokens),
                    fmt_u64(ctx.context_window),
                    pct
                ));
                let headroom = ctx.context_window.saturating_sub(tokens);
                out.push_str(&format!("  Headroom: {}\n", fmt_u64(headroom)));
            }
            _ => {
                out.push_str(&format!(
                    "  Current: unknown (after compaction) / {}\n",
                    fmt_u64(ctx.context_window)
                ));
            }
        }
        out.push_str(&format!("  Max output: {}\n", fmt_u64(stats.max_tokens)));
    }

    out
}

fn fmt_u64(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn format_unix_ms(ms: i64) -> String {
    if ms <= 0 {
        return "unknown".into();
    }
    // Keep it simple and dependency-free: ISO-ish UTC from epoch seconds.
    let secs = ms / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hours = rem / 3600;
    let mins = (rem % 3600) / 60;
    let s = rem % 60;
    // Civil date from days since Unix epoch (1970-01-01), Howard Hinnant algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hours:02}:{mins:02}:{s:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{
        AssistantMessage, Cost, InputModality, ModelCost, TextContent, ToolResultMessage,
        UserMessage, UserMessageContent,
    };

    fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test".into(),
            api: "openai-completions".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.25,
                tiers: None,
            },
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost_total: f64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
            cost: Cost {
                input: cost_total * 0.4,
                output: cost_total * 0.4,
                cache_read: cost_total * 0.1,
                cache_write: cost_total * 0.1,
                total: cost_total,
            },
            ..Usage::empty()
        }
    }

    fn assistant(usage: Usage) -> AgentMessage {
        AgentMessage::assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "test-model".into(),
            response_model: None,
            response_id: None,
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 1_000,
        })
    }

    fn msg_entry(id: &str, message: AgentMessage) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: None,
            timestamp: 1,
            message,
        }
    }

    #[test]
    fn aggregates_assistant_and_tool_usage() {
        let model = test_model();
        let entries = vec![
            msg_entry(
                "1",
                AgentMessage::Llm(Message::User(UserMessage {
                    content: UserMessageContent::Text("hello".into()),
                    timestamp: 1,
                })),
            ),
            msg_entry("2", assistant(usage(100, 20, 50, 10, 0.01))),
            msg_entry(
                "3",
                AgentMessage::tool_result(ToolResultMessage {
                    tool_call_id: "t1".into(),
                    tool_name: "bash".into(),
                    content: vec![],
                    details: None,
                    usage: Some(usage(5, 0, 0, 0, 0.001)),
                    added_tool_names: None,
                    is_error: false,
                    timestamp: 2,
                }),
            ),
        ];

        let stats = compute_session_stats(SessionStatsInput {
            all_entries: &entries,
            branch_entries: &entries,
            branch_messages: &[],
            session_id: "sid",
            session_name: Some("demo"),
            session_path: None,
            cwd: Some("/tmp"),
            created_at: 0,
            parent_session_id: None,
            model: &model,
            system_prompt: "",
            tools: None,
            models: None,
        });

        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_results, 1);
        assert_eq!(stats.tokens.input, 105);
        assert_eq!(stats.tokens.output, 20);
        assert_eq!(stats.tokens.cache_read, 50);
        assert_eq!(stats.tokens.cache_write, 10);
        assert!((stats.tokens.cost.total - 0.011).abs() < 1e-9);
        assert_eq!(stats.usage_turns, 2);
        assert!(stats.breakdown.iter().any(|b| b.key == "test/test-model"));
        assert!(stats.breakdown.iter().any(|b| b.key == "Tools/summaries"));

        let report = format_session_stats(&stats);
        assert!(report.contains("Tokens (lifetime)"));
        assert!(report.contains("Input (prompt):"));
        assert!(report.contains("Cost"));
    }

    #[test]
    fn context_unknown_immediately_after_compaction() {
        let model = test_model();
        let entries = vec![
            msg_entry("1", assistant(usage(200, 0, 0, 0, 0.0))),
            SessionTreeEntry::Compaction {
                id: "c1".into(),
                parent_id: Some("1".into()),
                timestamp: 2,
                summary: "summary".into(),
                first_kept_entry_id: None,
                details: None,
            },
        ];
        let stats = compute_session_stats(SessionStatsInput {
            all_entries: &entries,
            branch_entries: &entries,
            branch_messages: &[],
            session_id: "sid",
            session_name: None,
            session_path: None,
            cwd: None,
            created_at: 0,
            parent_session_id: None,
            model: &model,
            system_prompt: "",
            tools: None,
            models: None,
        });
        let ctx = stats.context_usage.expect("context");
        assert!(ctx.tokens.is_none());
        assert!(ctx.percent.is_none());
        assert_eq!(ctx.context_window, 100_000);
        assert_eq!(stats.compactions, 1);
    }
}
