//! Automatic compaction inside a turn, and the summary step it shares with
//! manual compaction.
//!
//! A turn writes its entries to the Session tree only when it ends
//! ([`PendingTurnWrites`]), so a compaction in the middle of a turn is
//! queued in order with the turn's messages. Its first kept message may be
//! one of those queued messages; its entry id is known only when the queue
//! is written, so the queue resolves it then.

use std::collections::HashMap;
use std::sync::Arc;

use loop_ai::{Model, SimpleStreamOptions, Usage};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::{
    compaction_details, estimate_agent_message_tokens, estimate_agent_messages_tokens,
    find_auto_cut_point, has_user_message, request_summary, request_text, shorten_tool_result,
    summary_with_request, AutoCompactionError, CompactionSettings, CompactionTrigger,
    SHORTENED_OUTPUT_CHARS,
};
use crate::harness::hooks::{HarnessHookEvent, HookRegistry};
use crate::harness::session::types::{PendingSessionWrite, SessionTreeEntry};
use crate::messages::{convert_to_llm, create_compaction_summary_message};
use crate::stream_fn::StreamFn;
use crate::types::{AgentMessage, CompactionOutcome, CompactionRequest, CustomAgentMessage};

/// Room left for the summary request's own prompt and answer.
const SUMMARY_REQUEST_MARGIN: u64 = 2_048;

/// The turn's stream options for a summary request: no reasoning and a
/// bounded answer.
pub fn summary_options(options: &SimpleStreamOptions) -> SimpleStreamOptions {
    let mut options = options.clone();
    options.reasoning = None;
    options.thinking_budgets = None;
    options.base.max_tokens = Some(SUMMARY_MAX_TOKENS);
    options
}

/// The most context tokens a summary request may carry for `model`.
pub fn summary_input_limit(model: &Model, settings: &CompactionSettings) -> u64 {
    model
        .context_window
        .saturating_sub(settings.reserve_tokens)
        .saturating_sub(SUMMARY_REQUEST_MARGIN)
        .max(1_024)
}

/// Run the `session_before_compact` hooks, then ask the model for a summary
/// of `to_summarize` unless a hook gave one. The usage is the summary
/// request's, `None` when a hook wrote the summary.
#[allow(clippy::too_many_arguments)]
pub async fn summarize(
    hooks: &HookRegistry,
    stream_fn: &StreamFn,
    model: &Model,
    options: SimpleStreamOptions,
    to_summarize: &[AgentMessage],
    cut: usize,
    custom_instructions: Option<&str>,
    max_input_tokens: u64,
    cancel: &CancellationToken,
) -> Result<(String, Option<Usage>), AutoCompactionError> {
    let hook = hooks
        .emit(HarnessHookEvent::SessionBeforeCompact {
            preparation_cut: cut,
            custom_instructions: custom_instructions.map(str::to_string),
        })
        .await;
    if hook.cancel {
        return Err(AutoCompactionError::HookCancelled);
    }
    if let Some(summary) = hook.summary {
        return Ok((summary, None));
    }
    let (summary, usage) = request_summary(
        stream_fn,
        model,
        options,
        to_summarize,
        custom_instructions,
        max_input_tokens,
        cancel,
    )
    .await?;
    Ok((summary, Some(usage)))
}

/// The ids of the branch entries that produce the context messages, in
/// context order (as `default_context_from_entries` builds them).
pub fn context_entry_ids(entries: &[SessionTreeEntry]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    // `ids[0]` is a compaction's summary.
    let mut summary_first = false;
    for entry in entries {
        match entry {
            SessionTreeEntry::Message { .. } | SessionTreeEntry::BranchSummary { .. } => {
                ids.push(entry.id().to_string());
            }
            SessionTreeEntry::Compaction {
                first_kept_entry_id,
                ..
            } => {
                if first_kept_entry_id.is_none() {
                    ids.clear();
                    summary_first = false;
                }
                // A later summary replaces an earlier one at the front.
                if summary_first {
                    ids.remove(0);
                }
                ids.insert(0, entry.id().to_string());
                summary_first = true;
            }
            _ => {}
        }
    }
    ids
}

/// Index of the first context message whose response usage is current:
/// messages written after the branch's newest compaction. Usage on the
/// summary and the kept messages before it measured the older context.
pub fn usage_valid_from(entries: &[SessionTreeEntry], context_len: usize) -> usize {
    let Some(last) = entries
        .iter()
        .rposition(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
    else {
        return 0;
    };
    let after = entries[last + 1..]
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionTreeEntry::Message { .. } | SessionTreeEntry::BranchSummary { .. }
            )
        })
        .count();
    context_len.saturating_sub(after)
}

/// The tree entry behind one context message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryRef {
    /// Already in the tree.
    Written(String),
    /// The write at this index of the turn's pending writes.
    Pending(usize),
}

/// A turn's queued tree writes, and the compactions among them whose first
/// kept entry is a queued message.
#[derive(Default)]
pub struct PendingTurnWrites {
    /// Writes in order.
    pub writes: Vec<PendingSessionWrite>,
    /// Compaction write index to the index of its first kept message write.
    pub first_kept: HashMap<usize, usize>,
    /// Compaction write index to the message writes of the tool results it
    /// shortened, recorded in its details by entry id once written.
    pub shortened: HashMap<usize, Vec<usize>>,
    /// Indices of the message writes the agent loop made (its context
    /// messages, in order).
    pub loop_messages: Vec<usize>,
}

impl PendingTurnWrites {
    /// Queue a write.
    pub fn push(&mut self, write: PendingSessionWrite) {
        self.writes.push(write);
    }

    /// Queue a message from the agent loop.
    pub fn push_loop_message(&mut self, message: AgentMessage) {
        self.loop_messages.push(self.writes.len());
        self.writes.push(PendingSessionWrite::Message { message });
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Take the queue for writing.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

/// Automatic compaction for one turn: maps the agent loop's context to the
/// tree and queues the compaction entry.
pub struct TurnCompactor {
    stream_fn: StreamFn,
    options: SimpleStreamOptions,
    settings: CompactionSettings,
    hooks: HookRegistry,
    pending: Arc<Mutex<PendingTurnWrites>>,
    /// The user's request that started the turn, captured then, for a
    /// summary that has to quote it.
    request: Option<String>,
    state: tokio::sync::Mutex<TurnTreeState>,
}

struct TurnTreeState {
    /// Entries behind the first `refs.len()` context messages; `None` when
    /// the starting context did not match the branch.
    refs: Option<Vec<EntryRef>>,
    /// Loop message writes already mapped into `refs`.
    mapped: usize,
    /// Context index of the turn's first user message; `None` when the turn
    /// has none or an overflow recovery summarized it.
    turn_start: Option<usize>,
}

/// What a compaction will do, decided before any model call.
struct Plan {
    cut: usize,
    /// Context indices of current-turn results to shorten (by position:
    /// providers may repeat a tool call id).
    shorten: Vec<usize>,
    /// The summary text when no model call is needed (an earlier summary is
    /// all that is cut).
    reuse: Option<String>,
}

/// Output tokens a summary may use, and the room the plan keeps for it.
pub const SUMMARY_MAX_TOKENS: u32 = 2_048;

/// The cut for an overflow recovery: the first message boundary (before a
/// user or assistant message, so no tool call loses its result) whose kept
/// part, with `extra(cut)` tokens the summary must carry, fits `keep`.
/// Without one, the last boundary. Returns 0 when there is no boundary.
fn find_overflow_cut(
    messages: &[loop_ai::Message],
    keep: u64,
    extra: impl Fn(usize) -> u64,
) -> usize {
    let n = messages.len();
    let mut suffix = vec![0u64; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1].saturating_add(loop_ai::estimate_message_tokens(&messages[i]));
    }
    let boundaries: Vec<usize> = (1..n)
        .filter(|&i| matches!(messages[i].role(), "user" | "assistant"))
        .collect();
    boundaries
        .iter()
        .copied()
        .find(|&i| suffix[i] + extra(i) <= keep)
        .or_else(|| boundaries.last().copied())
        .unwrap_or(0)
}

/// The note a summary carries when tool outputs were shortened.
const SHORTENED_SUMMARY_NOTE: &str = "Some older tool outputs of the current turn were \
shortened to save context; run a tool again if its full output is needed.";

impl TurnCompactor {
    /// A compactor for a turn whose starting context is built from `entries`
    /// (`base_len` messages) and continues with `prompts`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream_fn: StreamFn,
        options: SimpleStreamOptions,
        settings: CompactionSettings,
        hooks: HookRegistry,
        pending: Arc<Mutex<PendingTurnWrites>>,
        entries: &[SessionTreeEntry],
        base_len: usize,
        prompts: &[AgentMessage],
    ) -> Self {
        let ids = context_entry_ids(entries);
        let refs =
            (ids.len() == base_len).then(|| ids.into_iter().map(EntryRef::Written).collect());
        let mapped = pending.lock().loop_messages.len();
        let turn_start = prompts
            .iter()
            .position(|m| matches!(m, AgentMessage::Llm(loop_ai::Message::User(_))))
            .map(|offset| base_len + offset);
        Self {
            stream_fn,
            options,
            settings,
            hooks,
            pending,
            request: request_text(prompts),
            state: tokio::sync::Mutex::new(TurnTreeState {
                refs,
                mapped,
                turn_start,
            }),
        }
    }

    /// Decide the cut, and for a threshold compaction which tool outputs of
    /// the current turn to shorten, with estimates only. Fails when the
    /// result would not be worth a summary request.
    fn plan(
        &self,
        messages: &[AgentMessage],
        llm: &[loop_ai::Message],
        trigger: CompactionTrigger,
        window: u64,
        overhead: u64,
        turn_start: Option<usize>,
    ) -> Result<Plan, AutoCompactionError> {
        let reserve = self.settings.reserve_tokens;
        let tokens_before = estimate_agent_messages_tokens(messages);
        let allowance = u64::from(SUMMARY_MAX_TOKENS);
        // The expected size of a summary of `messages[..cut]`: shorter than
        // what it summarizes, and bounded by the summary's output limit.
        let room = |cut: usize| -> u64 {
            if cut == 0 {
                0
            } else {
                (estimate_agent_messages_tokens(&messages[..cut]) / 2 + 64).min(allowance)
            }
        };
        let threshold = trigger != CompactionTrigger::Overflow;
        // Room for messages under the threshold, after the fixed part.
        let budget = if window > 0 {
            window.saturating_sub(reserve).saturating_sub(overhead)
        } else {
            tokens_before / 2
        };
        if threshold && budget <= allowance / 2 {
            return Err(AutoCompactionError::WindowTooSmall {
                window,
                reserve,
                overhead,
            });
        }
        let mut keep = self.settings.keep_recent_tokens.min(budget / 2);
        if !threshold {
            keep = keep.min(tokens_before / 2);
        }
        // A cut past the turn's request quotes it in the summary.
        let quote = self.request.as_deref().map_or(0, |r| {
            estimate_agent_message_tokens(&AgentMessage::user_text(r))
        });
        let quoted = |cut: usize| turn_start.is_some_and(|start| cut > start);
        let mut cut = if threshold {
            find_auto_cut_point(llm, keep, budget / 2)
        } else {
            find_overflow_cut(llm, keep, |i| if quoted(i) { quote } else { 0 })
        };
        let mut shorten = Vec::new();
        let mut freed = 0u64;
        if threshold {
            // Never cut into the current turn: its request stays verbatim.
            let start = turn_start.unwrap_or(messages.len());
            cut = cut.min(start);
            // Keeping the end of earlier turns would leave less than half
            // the room: summarize all earlier turns now rather than pay for
            // a second summary a few rounds later.
            if cut < start
                && estimate_agent_messages_tokens(&messages[cut..]) + room(cut) > budget / 2
            {
                cut = start;
            }
            let summary_room = room(cut).max(if cut == 0 { 64 } else { 0 });
            let mut kept: u64 = estimate_agent_messages_tokens(&messages[cut..]);
            let unshortened = kept;
            if kept + summary_room > budget / 2 {
                // Shorten the outputs of all the turn's older rounds; the
                // newest round stays whole. All at once, so later rounds
                // have room before the next compaction.
                let last_round = messages
                    .iter()
                    .rposition(|m| m.as_assistant().is_some())
                    .unwrap_or(messages.len());
                for (index, message) in messages
                    .iter()
                    .enumerate()
                    .take(last_round)
                    .skip(turn_start.unwrap_or(0).max(cut))
                {
                    if let AgentMessage::Llm(loop_ai::Message::ToolResult(result)) = message {
                        let mut short = result.clone();
                        if shorten_tool_result(&mut short, SHORTENED_OUTPUT_CHARS) {
                            let before = estimate_agent_message_tokens(message);
                            let after = estimate_agent_message_tokens(&AgentMessage::Llm(
                                loop_ai::Message::ToolResult(short),
                            ));
                            kept = kept.saturating_sub(before.saturating_sub(after));
                            // By position: providers may repeat a call id.
                            shorten.push(index);
                        }
                    }
                }
                freed = unshortened.saturating_sub(kept);
                if kept + summary_room > budget {
                    return Err(AutoCompactionError::StillTooLarge {
                        after: kept + summary_room + overhead,
                        limit: window.saturating_sub(reserve),
                    });
                }
            }
        }
        // A summary at the front can only be replaced by one carrying it.
        let first_is_summary = matches!(
            messages.first(),
            Some(AgentMessage::Custom(
                CustomAgentMessage::CompactionSummary { .. }
            ))
        );
        if cut == 0 && !shorten.is_empty() && first_is_summary {
            cut = 1;
        }
        if cut == 0 && shorten.is_empty() {
            return Err(AutoCompactionError::NothingToCompact);
        }
        let reuse = match &messages[..cut] {
            [] => Some(String::new()),
            [AgentMessage::Custom(CustomAgentMessage::CompactionSummary { summary, .. })] => {
                Some(summary.clone())
            }
            _ => None,
        };
        if reuse.is_some() && shorten.is_empty() {
            return Err(AutoCompactionError::NothingToCompact);
        }
        // A compaction with no summary costs nothing, so it is bounded by
        // progress: one that frees under a tenth of the room is not done.
        if reuse.is_some() && freed < budget / 10 {
            return Err(AutoCompactionError::StillTooLarge {
                after: tokens_before.saturating_sub(freed) + overhead,
                limit: window.saturating_sub(reserve),
            });
        }
        let after = if reuse.is_some() { 0 } else { room(cut) }
            + estimate_agent_messages_tokens(&messages[cut..])
            + if quoted(cut) { quote } else { 0 };
        if shorten.is_empty() && after >= tokens_before {
            return Err(AutoCompactionError::NotReduced {
                before: tokens_before,
                after,
            });
        }
        Ok(Plan {
            cut,
            shorten,
            reuse,
        })
    }

    /// Compact the loop's context: summarize its older part with the model,
    /// queue the compaction entry and return the new context.
    pub async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionOutcome, AutoCompactionError> {
        let cancel = request.cancel.clone().unwrap_or_default();
        let mut state = self.state.lock().await;
        let mut refs = self.map_context(&mut state, &request.messages)?;
        let messages = request.messages;

        let llm = convert_to_llm(&messages);
        if llm.len() != messages.len() {
            return Err(AutoCompactionError::Tree(
                "a context message has no model message".into(),
            ));
        }
        let window = request.model.context_window;
        let tokens_before = estimate_agent_messages_tokens(&messages);
        let plan = self.plan(
            &messages,
            &llm,
            request.trigger,
            window,
            request.overhead_tokens,
            state.turn_start,
        )?;
        let cut = plan.cut;
        if let Some(emit) = &request.emit {
            emit(crate::types::AgentEvent::Progress {
                message: match request.trigger {
                    CompactionTrigger::Overflow => {
                        "the context is too long for the model: compacting…".into()
                    }
                    _ => format!(
                        "context near the window (~{} of {window} tokens): compacting…",
                        tokens_before + request.overhead_tokens
                    ),
                },
            })
            .await;
        }

        let (summary, usage) = match plan.reuse.clone() {
            Some(text) => (text, None),
            None => {
                summarize(
                    &self.hooks,
                    &self.stream_fn,
                    &request.model,
                    summary_options(&self.options),
                    &messages[..cut],
                    cut,
                    None,
                    summary_input_limit(&request.model, &self.settings),
                    &cancel,
                )
                .await?
            }
        };
        // The request is quoted only when the cut removes it (overflow
        // recovery), from the text captured when the turn started.
        let request_cut = state.turn_start.is_some_and(|start| cut > start)
            || (state.turn_start.is_none() && !has_user_message(&messages[cut..]));
        // A reused summary may already quote it.
        let mut summary = if request_cut && !summary.contains("<request>") {
            summary_with_request(&summary, self.request.as_deref())
        } else {
            summary
        };
        if !plan.shorten.is_empty() && !summary.contains(SHORTENED_SUMMARY_NOTE) {
            if !summary.is_empty() {
                summary.push_str("\n\n");
            }
            summary.push_str(SHORTENED_SUMMARY_NOTE);
        }
        let mut compacted = Vec::with_capacity(messages.len() - cut + 1);
        compacted.push(create_compaction_summary_message(summary.clone()));
        compacted.extend_from_slice(&messages[cut..]);
        // `compacted[0]` is the summary, then `messages[cut..]`.
        for &index in &plan.shorten {
            if let Some(AgentMessage::Llm(loop_ai::Message::ToolResult(result))) =
                compacted.get_mut(index + 1 - cut)
            {
                shorten_tool_result(result, SHORTENED_OUTPUT_CHARS);
            }
        }
        let tokens_after = estimate_agent_messages_tokens(&compacted);
        if tokens_after >= tokens_before {
            return Err(AutoCompactionError::NotReduced {
                before: tokens_before,
                after: tokens_after,
            });
        }
        if request.trigger != CompactionTrigger::Overflow && window > 0 {
            let limit = window.saturating_sub(self.settings.reserve_tokens);
            let after = tokens_after + request.overhead_tokens;
            if after > limit {
                return Err(AutoCompactionError::StillTooLarge { after, limit });
            }
        }
        if cancel.is_cancelled() {
            return Err(AutoCompactionError::Cancelled);
        }

        let mut details =
            compaction_details(request.trigger, tokens_before, tokens_after, usage.as_ref());
        // Shortened outputs are recorded by entry id; an entry of this turn
        // gets its id when the turn is written.
        let mut written_shortened = Vec::new();
        let mut pending_shortened = Vec::new();
        for &index in &plan.shorten {
            match &refs[index] {
                EntryRef::Written(id) => written_shortened.push(id.clone()),
                EntryRef::Pending(write) => pending_shortened.push(*write),
            }
        }
        if !plan.shorten.is_empty() {
            details["shortened"] = serde_json::json!(written_shortened);
        }
        let first_kept = refs[cut].clone();
        {
            let mut pending = self.pending.lock();
            let index = pending.writes.len();
            if !pending_shortened.is_empty() {
                pending.shortened.insert(index, pending_shortened);
            }
            let first_kept_entry_id = match first_kept {
                EntryRef::Written(id) => Some(id),
                EntryRef::Pending(write) => {
                    pending.first_kept.insert(index, write);
                    None
                }
            };
            pending.push(PendingSessionWrite::Compaction {
                summary,
                first_kept_entry_id,
                details: Some(details),
            });
            let mut next = Vec::with_capacity(refs.len() - cut + 1);
            next.push(EntryRef::Pending(index));
            next.extend(refs.drain(cut..));
            state.refs = Some(next);
            state.turn_start = match state.turn_start {
                Some(start) if cut <= start => Some(start - cut + 1),
                // The summary, which quotes the request, now starts the turn.
                Some(_) => Some(0),
                None => None,
            };
        }
        Ok(CompactionOutcome {
            messages: compacted,
            tokens_before,
            tokens_after,
            summarized: usage.is_some() || plan.reuse.is_none(),
        })
    }

    /// The entry behind each context message, mapping the messages the loop
    /// added since the last call to their queued writes.
    fn map_context(
        &self,
        state: &mut TurnTreeState,
        messages: &[AgentMessage],
    ) -> Result<Vec<EntryRef>, AutoCompactionError> {
        let Some(refs) = state.refs.as_ref() else {
            return Err(AutoCompactionError::Tree(
                "the turn's starting context does not match its branch".into(),
            ));
        };
        let mut refs = refs.clone();
        let base = refs.len();
        let added = messages.len().checked_sub(base).ok_or_else(|| {
            AutoCompactionError::Tree("the context is shorter than its recorded part".into())
        })?;
        let pending = self.pending.lock();
        let writes = pending
            .loop_messages
            .get(state.mapped..state.mapped + added)
            .ok_or_else(|| {
                AutoCompactionError::Tree("a context message was not queued for the tree".into())
            })?;
        for (offset, &write) in writes.iter().enumerate() {
            let queued = match pending.writes.get(write) {
                Some(PendingSessionWrite::Message { message }) => message,
                _ => {
                    return Err(AutoCompactionError::Tree(
                        "a queued context message is missing".into(),
                    ))
                }
            };
            if queued.role() != messages[base + offset].role() {
                return Err(AutoCompactionError::Tree(format!(
                    "context message {} is not the queued message",
                    base + offset
                )));
            }
            refs.push(EntryRef::Pending(write));
        }
        drop(pending);
        state.mapped += added;
        state.refs = Some(refs.clone());
        Ok(refs)
    }
}
