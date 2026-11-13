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
    compaction_details, estimate_agent_messages_tokens, find_auto_cut_point, has_user_message,
    latest_user_text, request_summary, summary_with_request, AutoCompactionError,
    CompactionSettings,
};
use crate::harness::hooks::{HarnessHookEvent, HookRegistry};
use crate::harness::session::types::{PendingSessionWrite, SessionTreeEntry};
use crate::messages::{convert_to_llm, create_compaction_summary_message};
use crate::stream_fn::StreamFn;
use crate::types::{AgentMessage, CompactionOutcome, CompactionRequest};

/// Room left for the summary request's own prompt and answer.
const SUMMARY_REQUEST_MARGIN: u64 = 2_048;

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
    state: tokio::sync::Mutex<TurnTreeState>,
}

struct TurnTreeState {
    /// Entries behind the first `refs.len()` context messages; `None` when
    /// the starting context did not match the branch.
    refs: Option<Vec<EntryRef>>,
    /// Loop message writes already mapped into `refs`.
    mapped: usize,
    /// The user's request that an earlier compaction in this turn quoted in
    /// its summary; a later one quotes it again.
    quoted_request: Option<String>,
}

impl TurnCompactor {
    /// A compactor for a turn whose starting context is built from `entries`
    /// (`base_len` messages).
    pub fn new(
        stream_fn: StreamFn,
        options: SimpleStreamOptions,
        settings: CompactionSettings,
        hooks: HookRegistry,
        pending: Arc<Mutex<PendingTurnWrites>>,
        entries: &[SessionTreeEntry],
        base_len: usize,
    ) -> Self {
        let ids = context_entry_ids(entries);
        let refs =
            (ids.len() == base_len).then(|| ids.into_iter().map(EntryRef::Written).collect());
        let mapped = pending.lock().loop_messages.len();
        Self {
            stream_fn,
            options,
            settings,
            hooks,
            pending,
            state: tokio::sync::Mutex::new(TurnTreeState {
                refs,
                mapped,
                quoted_request: None,
            }),
        }
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
        let usable = request
            .model
            .context_window
            .saturating_sub(self.settings.reserve_tokens);
        let cut = find_auto_cut_point(&llm, self.settings.keep_recent_tokens, usable / 2);
        if cut == 0 {
            return Err(AutoCompactionError::NothingToCompact);
        }
        let tokens_before = estimate_agent_messages_tokens(&messages);
        let (summary, usage) = summarize(
            &self.hooks,
            &self.stream_fn,
            &request.model,
            self.options.clone(),
            &messages[..cut],
            cut,
            None,
            summary_input_limit(&request.model, &self.settings),
            &cancel,
        )
        .await?;
        let kept = &messages[cut..];
        let request_text = if has_user_message(kept) {
            None
        } else {
            latest_user_text(&messages[..cut]).or_else(|| state.quoted_request.clone())
        };
        let summary = summary_with_request(&summary, request_text.as_deref());
        let mut compacted = Vec::with_capacity(kept.len() + 1);
        compacted.push(create_compaction_summary_message(summary.clone()));
        compacted.extend_from_slice(kept);
        let tokens_after = estimate_agent_messages_tokens(&compacted);
        if tokens_after >= tokens_before {
            return Err(AutoCompactionError::NotReduced {
                before: tokens_before,
                after: tokens_after,
            });
        }
        if cancel.is_cancelled() {
            return Err(AutoCompactionError::Cancelled);
        }

        let details =
            compaction_details(request.trigger, tokens_before, tokens_after, usage.as_ref());
        let first_kept = refs[cut].clone();
        {
            let mut pending = self.pending.lock();
            let index = pending.writes.len();
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
            state.quoted_request = request_text;
        }
        Ok(CompactionOutcome {
            messages: compacted,
            tokens_before,
            tokens_after,
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
