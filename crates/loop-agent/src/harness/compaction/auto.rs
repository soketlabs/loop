//! In-loop compaction: shrink the context between turns of one long agent run.
//!
//! `find_cut_point` snaps to user messages, which cannot split a single long turn (a
//! coding task is typically one user prompt followed by dozens of assistant/tool
//! messages). This planner cuts at an assistant message instead, so retained history
//! never starts with an orphaned tool result.

use futures::StreamExt;
use loop_ai::{
    estimate_context_tokens, estimate_message_tokens, AssistantContent, Context, Message, Model,
    SimpleStreamOptions, StopReason, ThinkingLevel, ToolResultContent, UserContent,
    UserMessageContent,
};

use crate::harness::compaction::{compaction_limit_with_reply_room, CompactionSettings};
use crate::messages::{convert_to_llm, COMPACTION_SUMMARY_PREFIX};
use crate::stream_fn::StreamFn;
use crate::types::{AgentContext, AgentMessage};

const SUMMARY_SYSTEM: &str = "You compress the working history of a coding agent so it can \
keep going with a much smaller context and still act exactly as correctly as if it had the full \
history. Write the summary in these sections, in this order.\n\
1. TASK: the user's original request and any required output format, constraints or rules, \
restated exactly.\n\
2. STATE: every number, total, counter, list, maximum or minimum, or other running value the \
agent is tracking, written out exactly as last computed and tagged with the item it was last \
updated for (for example 'running total 337198 after report_45'). Never round, never recompute, \
never omit. Take values from the agent's own messages; do not invent any. If an earlier summary \
appears in the transcript, carry every value in it forward.\n\
3. DONE: files read or changed (and what changed), commands run and their outcomes, test \
results, decisions made. When the task processes many items, list each item's result. Keep \
exact file paths, identifiers and error messages.\n\
4. NEXT: what is left to do and the very next step.\n\
No preamble.";
/// Opening line of the message that carries a summary in the context.
const SUMMARY_MARKER: &str = "[Earlier work was summarized to save context. Continue from here.]";
/// Cap on the summary kept in context.
const MAX_SUMMARY_CHARS: usize = 12_000;
/// Upper cap on the transcript sent to the summarizer; smaller windows get less (see
/// `max_transcript_chars`).
const MAX_TRANSCRIPT_CHARS: usize = 240_000;
/// Output budget for one summary.
const SUMMARY_MAX_OUTPUT_TOKENS: u64 = 3000;
/// Output budget for the second try, when the first ran out before any text (see `summarize`).
const SUMMARY_RETRY_OUTPUT_TOKENS: u64 = 12_000;

/// Max characters of one tool result / tool-call argument shown to the summarizer.
const RESULT_PREVIEW_CHARS: usize = 1200;
/// The preview a summary request starts from; it is halved until the transcript fits.
const MAX_RESULT_PREVIEW_CHARS: usize = 20_000;
/// Floor for that preview when a long history has to be squeezed into one summary request.
const MIN_RESULT_PREVIEW_CHARS: usize = 150;
const ARGS_PREVIEW_CHARS: usize = 400;
/// Tool-call arguments (the text of an edit or a write) get a quarter of the result preview.
const MAX_ARGS_PREVIEW_CHARS: usize = 4000;

/// Heading of the file list appended to every summary.
const FILES_HEADING: &str = "FILES (collected from the tool calls, not written by the model)";
/// Cap on that list; the oldest paths go first.
const MAX_FILES_CHARS: usize = 4000;
/// A tool result kept after compaction is never cut below this many characters.
const MIN_KEPT_RESULT_CHARS: usize = 2000;
const KEPT_RESULT_MARKER: &str =
    "\n[... middle removed to save context; run the tool again if you need it ...]\n";

/// How the context splits when compacting between turns.
#[derive(Debug, Clone)]
pub struct InLoopPlan {
    /// Kept verbatim at the front (the original task prompt).
    pub head: Vec<AgentMessage>,
    /// Older messages to replace with a summary.
    pub to_summarize: Vec<AgentMessage>,
    /// Recent messages kept verbatim; always starts with an assistant message.
    pub retained: Vec<AgentMessage>,
}

fn message_tokens(m: &AgentMessage) -> u64 {
    convert_to_llm(std::slice::from_ref(m))
        .iter()
        .map(estimate_message_tokens)
        .sum()
}

/// The first message stays verbatim: the task prompt or, in a session that was compacted
/// before (`/compact`, or an earlier run of this compaction), the summary standing in for it.
fn is_head(m: &AgentMessage) -> bool {
    matches!(m.role(), "user" | "compactionSummary")
}

/// Split `messages` so roughly `keep_recent_tokens` stay verbatim. Returns `None` when
/// there is nothing worth summarizing (fewer than two messages would be folded away).
pub fn plan_in_loop_compaction(
    messages: &[AgentMessage],
    keep_recent_tokens: u64,
) -> Option<InLoopPlan> {
    let head_len = usize::from(messages.first().is_some_and(is_head));

    let mut kept = 0u64;
    let mut cut = None;
    for i in (head_len..messages.len()).rev() {
        kept += message_tokens(&messages[i]);
        if kept >= keep_recent_tokens && messages[i].role() == "assistant" {
            cut = Some(i);
            break;
        }
    }
    let cut = cut?;
    if cut < head_len + 2 {
        return None;
    }
    Some(InLoopPlan {
        head: messages[..head_len].to_vec(),
        to_summarize: messages[head_len..cut].to_vec(),
        retained: messages[cut..].to_vec(),
    })
}

fn preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}… [truncated]")
    } else {
        head
    }
}

/// Plain-text transcript of `messages` for the summarizer prompt. Keeps what the
/// summary needs (what was asked, which tools ran with which arguments, how they
/// ended) and drops bulk (full tool output, thinking blocks).
pub fn render_for_summary(messages: &[AgentMessage]) -> String {
    render_with_previews(messages, RESULT_PREVIEW_CHARS)
}

/// [`render_for_summary`] that fits `max_chars`. Tool output is shown as fully as the budget
/// allows: the preview starts large and is halved until the transcript fits, so every step
/// still shows up; only when that is not enough is the oldest part dropped (the newest is closest to what the agent is doing now).
fn render_within(messages: &[AgentMessage], max_chars: usize) -> String {
    let mut result_chars = MAX_RESULT_PREVIEW_CHARS;
    let mut transcript = render_with_previews(messages, result_chars);
    while transcript.len() > max_chars && result_chars > MIN_RESULT_PREVIEW_CHARS {
        result_chars = (result_chars / 2).max(MIN_RESULT_PREVIEW_CHARS);
        transcript = render_with_previews(messages, result_chars);
    }
    if transcript.len() > max_chars {
        let start = transcript.len() - max_chars;
        let start = (start..transcript.len())
            .find(|i| transcript.is_char_boundary(*i))
            .unwrap_or(start);
        transcript = format!("[earlier part omitted]\n{}", &transcript[start..]);
    }
    transcript
}

fn render_with_previews(messages: &[AgentMessage], result_chars: usize) -> String {
    let args_chars = (result_chars / 4).clamp(ARGS_PREVIEW_CHARS, MAX_ARGS_PREVIEW_CHARS);
    let mut out = String::new();
    for m in messages {
        let llm = convert_to_llm(std::slice::from_ref(m));
        let Some(msg) = llm.first() else { continue };
        match msg {
            Message::User(u) => {
                let text = match &u.content {
                    UserMessageContent::Text(t) => t.clone(),
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            UserContent::Text(t) => Some(t.text.as_str()),
                            UserContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                // An earlier summary is the only record of the work it replaced: keep it whole.
                let limit = if text.starts_with(SUMMARY_MARKER)
                    || text.starts_with(COMPACTION_SUMMARY_PREFIX)
                {
                    MAX_SUMMARY_CHARS + MAX_FILES_CHARS + 500
                } else {
                    2000
                };
                out.push_str(&format!("USER: {}\n", preview(&text, limit)));
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                            out.push_str(&format!("ASSISTANT: {}\n", preview(&t.text, 1200)));
                        }
                        AssistantContent::ToolCall(tc) => {
                            out.push_str(&format!(
                                "CALL {}({})\n",
                                tc.name,
                                preview(&tc.arguments.to_string(), args_chars)
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(t) => {
                let text: String = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push_str(&format!(
                    "RESULT {}: {}\n",
                    t.tool_name,
                    preview(&text, result_chars)
                ));
            }
        }
    }
    out
}

fn user_text(m: &AgentMessage) -> Option<String> {
    match convert_to_llm(std::slice::from_ref(m)).first()? {
        Message::User(u) => Some(match &u.content {
            UserMessageContent::Text(t) => t.clone(),
            UserMessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    UserContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }),
        _ => None,
    }
}

/// Files the summarized work touched, oldest first.
#[derive(Debug, Default, PartialEq)]
struct TouchedFiles {
    changed: Vec<String>,
    read: Vec<String>,
}

impl TouchedFiles {
    fn add_changed(&mut self, path: &str) {
        self.read.retain(|p| p != path);
        self.changed.retain(|p| p != path);
        self.changed.push(path.to_string());
    }

    fn add_read(&mut self, path: &str) {
        if self.changed.iter().any(|p| p == path) {
            return;
        }
        self.read.retain(|p| p != path);
        self.read.push(path.to_string());
    }

    /// Pick up the list an earlier summary carries, so it survives being summarized again.
    fn add_from_summary(&mut self, text: &str) {
        let Some((_, list)) = text.split_once(FILES_HEADING) else {
            return;
        };
        let mut changed = false;
        for line in list.lines().skip(1) {
            match line.trim() {
                "changed:" => changed = true,
                "read:" => changed = false,
                item => match item.strip_prefix("- ") {
                    Some(path) if changed => self.add_changed(path),
                    Some(path) => self.add_read(path),
                    None => break,
                },
            }
        }
    }

    /// The `read`, `write` and `edit` calls of `messages`, on top of any list in an earlier
    /// summary among them.
    fn collect(messages: &[AgentMessage]) -> Self {
        let mut files = Self::default();
        for m in messages {
            if let Some(text) = user_text(m) {
                if text.starts_with(SUMMARY_MARKER) || text.starts_with(COMPACTION_SUMMARY_PREFIX) {
                    files.add_from_summary(&text);
                }
                continue;
            }
            let llm = convert_to_llm(std::slice::from_ref(m));
            let Some(Message::Assistant(a)) = llm.first() else {
                continue;
            };
            for block in &a.content {
                let AssistantContent::ToolCall(tc) = block else {
                    continue;
                };
                let Some(path) = ["path", "file_path"]
                    .iter()
                    .find_map(|k| tc.arguments.get(k).and_then(|v| v.as_str()))
                    .map(str::trim)
                    .filter(|p| !p.is_empty() && !p.contains('\n'))
                else {
                    continue;
                };
                match tc.name.as_str() {
                    "write" | "edit" => files.add_changed(path),
                    "read" => files.add_read(path),
                    _ => {}
                }
            }
        }
        files
    }

    /// The block appended to a summary; empty when no file was touched.
    fn render(mut self) -> String {
        let size =
            |f: &Self| -> usize { f.changed.iter().chain(&f.read).map(|p| p.len() + 3).sum() };
        while size(&self) > MAX_FILES_CHARS {
            // Reads are the cheaper loss; then the oldest changes.
            if self.read.is_empty() {
                self.changed.remove(0);
            } else {
                self.read.remove(0);
            }
        }
        if self.changed.is_empty() && self.read.is_empty() {
            return String::new();
        }
        let mut out = format!("\n\n{FILES_HEADING}\n");
        for (label, paths) in [("changed:", &self.changed), ("read:", &self.read)] {
            if paths.is_empty() {
                continue;
            }
            out.push_str(label);
            out.push('\n');
            for p in paths {
                out.push_str(&format!("- {p}\n"));
            }
        }
        out.truncate(out.trim_end().len());
        out
    }
}

fn result_text_chars(m: &AgentMessage) -> usize {
    match m {
        AgentMessage::Llm(Message::ToolResult(t)) => t
            .content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text(t) => t.text.chars().count(),
                ToolResultContent::Image(_) => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Cut the text of a tool result to about `max_chars`, keeping its start and its end.
fn shrink_result(m: &mut AgentMessage, max_chars: usize) {
    let AgentMessage::Llm(Message::ToolResult(t)) = m else {
        return;
    };
    let total: usize = t
        .content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => t.text.chars().count(),
            ToolResultContent::Image(_) => 0,
        })
        .sum();
    if total <= max_chars {
        return;
    }
    for c in &mut t.content {
        let ToolResultContent::Text(text) = c else {
            continue;
        };
        let chars: Vec<char> = text.text.chars().collect();
        // Each block keeps its share of the budget.
        let keep = max_chars * chars.len() / total;
        if keep >= chars.len() {
            continue;
        }
        let head = keep * 2 / 3;
        let tail = keep - head;
        let mut cut: String = chars[..head].iter().collect();
        cut.push_str(KEPT_RESULT_MARKER);
        cut.extend(&chars[chars.len() - tail..]);
        text.text = cut;
    }
}

/// Compaction keeps the newest messages word for word, so one very large tool result among
/// them can leave the context over the limit however much was summarized. Shorten the largest
/// kept results, largest first, until `messages` fits in `budget` tokens or nothing more can
/// be cut. Returns how many results were shortened.
fn shrink_kept_results(messages: &mut [AgentMessage], budget: u64) -> usize {
    let mut shortened = Vec::new();
    loop {
        let total: u64 = messages.iter().map(message_tokens).sum();
        if total <= budget {
            break;
        }
        let Some((i, chars)) = messages
            .iter()
            .enumerate()
            .filter(|(i, _)| !shortened.contains(i))
            .map(|(i, m)| (i, result_text_chars(m)))
            .filter(|(_, chars)| *chars > MIN_KEPT_RESULT_CHARS)
            .max_by_key(|(_, chars)| *chars)
        else {
            break;
        };
        let excess_chars =
            usize::try_from((total - budget).saturating_mul(4)).unwrap_or(usize::MAX);
        let target = chars
            .saturating_sub(excess_chars.saturating_add(KEPT_RESULT_MARKER.len()))
            .max(MIN_KEPT_RESULT_CHARS);
        shrink_result(&mut messages[i], target);
        shortened.push(i);
    }
    shortened.len()
}

/// The summary request must fit in the window next to the system prompt and the answer, so
/// small windows get a smaller transcript: about 40% of the window at ~3 characters a token.
fn max_transcript_chars(model: &Model) -> usize {
    if model.context_window == 0 {
        return MAX_TRANSCRIPT_CHARS;
    }
    let chars = (model.context_window as usize).saturating_mul(40) / 100 * 3;
    chars.clamp(8_000, MAX_TRANSCRIPT_CHARS)
}

/// Ask the model for a summary of `transcript`. `Err` says why there is none, so the caller
/// can leave the context untouched instead of losing history.
async fn summarize(
    model: &Model,
    stream_fn: &StreamFn,
    options: SimpleStreamOptions,
    transcript: &str,
) -> Result<String, String> {
    match summarize_once(
        model,
        stream_fn,
        options.clone(),
        transcript,
        SUMMARY_MAX_OUTPUT_TOKENS,
    )
    .await
    {
        // Out of output tokens before any visible text: a model that thinks first spent the
        // budget on reasoning. One more try with room for both.
        Err(SummaryFailure::OutOfTokens) => summarize_once(
            model,
            stream_fn,
            options,
            transcript,
            SUMMARY_RETRY_OUTPUT_TOKENS,
        )
        .await
        .map_err(|e| e.to_string()),
        other => other.map_err(|e| e.to_string()),
    }
}

/// Output budget of a summary request: `wanted`, but never more than the model can write, since
/// a provider rejects a request that asks for more than the model's maximum output.
fn summary_output_tokens(model: &Model, wanted: u64) -> u64 {
    if model.max_tokens == 0 {
        wanted
    } else {
        wanted.min(model.max_tokens)
    }
}

enum SummaryFailure {
    /// The request itself failed.
    Request(String),
    /// The reply hit the output limit without any text.
    OutOfTokens,
    /// The reply ended normally without any text.
    Empty,
}

impl std::fmt::Display for SummaryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(e) => write!(f, "the summary request failed: {e}"),
            Self::OutOfTokens => {
                f.write_str("the model ran out of output tokens before writing a summary")
            }
            Self::Empty => f.write_str("the model returned an empty summary"),
        }
    }
}

async fn summarize_once(
    model: &Model,
    stream_fn: &StreamFn,
    mut options: SimpleStreamOptions,
    transcript: &str,
    max_output_tokens: u64,
) -> Result<String, SummaryFailure> {
    options.reasoning = Some(ThinkingLevel::Minimal);
    options.base.max_tokens = Some(summary_output_tokens(model, max_output_tokens) as u32);
    let context = Context {
        system_prompt: Some(SUMMARY_SYSTEM.to_string()),
        messages: vec![Message::user_text(format!(
            "Transcript to summarize:\n\n{transcript}"
        ))],
        tools: None,
    };
    let mut stream = stream_fn(model.clone(), context, options).await;
    while stream.next().await.is_some() {}
    let reply = stream.result().await;
    if let Some(error) = reply.failure() {
        return Err(SummaryFailure::Request(error));
    }
    let text: String = reply
        .content
        .iter()
        .filter_map(|b| match b {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err(if reply.stop_reason == StopReason::Length {
            SummaryFailure::OutOfTokens
        } else {
            SummaryFailure::Empty
        });
    }
    Ok(preview(text, MAX_SUMMARY_CHARS))
}

fn context_tokens(messages: &[AgentMessage]) -> u64 {
    estimate_context_tokens(&Context {
        system_prompt: None,
        messages: convert_to_llm(messages),
        tools: None,
    })
}

/// Between turns: once the context reaches the compaction limit (90% of the model's window by
/// default), older messages are replaced by a model-written summary. The first message (the
/// task) and the newest messages stay verbatim.
///
/// `Ok(None)` means there is nothing to do: below the limit, or nothing worth cutting. `Err`
/// says why the summary could not be made; the context is left alone either way (the history
/// is never dropped without a summary).
pub async fn auto_compact_context(
    context: &AgentContext,
    model: &Model,
    stream_fn: &StreamFn,
    summary_options: SimpleStreamOptions,
    settings: &CompactionSettings,
) -> Result<Option<AutoCompaction>, String> {
    compact_context(context, model, stream_fn, summary_options, settings, false).await
}

/// Same as [`auto_compact_context`] but without waiting for the trigger: used when the provider
/// has just rejected a request as too long. `Ok(None)` when compaction is disabled or nothing
/// can be shrunk.
pub async fn force_compact_context(
    context: &AgentContext,
    model: &Model,
    stream_fn: &StreamFn,
    summary_options: SimpleStreamOptions,
    settings: &CompactionSettings,
) -> Result<Option<AutoCompaction>, String> {
    if !settings.enabled {
        return Ok(None);
    }
    compact_context(context, model, stream_fn, summary_options, settings, true).await
}

/// A compacted context plus a one-line description for the user.
pub struct AutoCompaction {
    /// Context for the next request.
    pub context: AgentContext,
    /// For example "Context compacted: 118,092 → 22,990 tokens".
    pub notice: String,
    /// The model-written summary, as it should be saved in the session.
    pub summary: String,
    /// How many of the newest messages in `context` are kept verbatim after the summary.
    pub tail_len: usize,
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

async fn compact_context(
    context: &AgentContext,
    model: &Model,
    stream_fn: &StreamFn,
    summary_options: SimpleStreamOptions,
    settings: &CompactionSettings,
    force: bool,
) -> Result<Option<AutoCompaction>, String> {
    let tokens_before = context_tokens(&context.messages);
    let limit = compaction_limit_with_reply_room(model.context_window, model.max_tokens, settings);
    if !force && !(settings.enabled && limit.is_some_and(|l| tokens_before >= l)) {
        return Ok(None);
    }
    // What the provider counts on top of the messages (system prompt, tool definitions): the
    // kept part has to fit next to it.
    let counted: u64 = context.messages.iter().map(message_tokens).sum();
    let overhead = tokens_before.saturating_sub(counted);
    let room = limit.map(|l| l.saturating_sub(overhead));
    let mut keep = super::effective_keep_recent_tokens(room, settings.keep_recent_tokens);
    if force {
        // The provider thinks the context is too big whatever we estimated: keep less.
        keep = keep.min((tokens_before / 3).max(1));
    }
    let Some(plan) = plan_in_loop_compaction(&context.messages, keep) else {
        return Ok(None);
    };

    let transcript = render_within(&plan.to_summarize, max_transcript_chars(model));
    let mut summary = summarize(model, stream_fn, summary_options, &transcript).await?;
    // The model's account of which files it touched can be incomplete; the tool calls are not.
    let mut summarized = plan.head.clone();
    summarized.extend(plan.to_summarize);
    summary.push_str(&TouchedFiles::collect(&summarized).render());

    let tail_len = plan.retained.len();
    let mut out = plan.head;
    out.push(AgentMessage::user_text(format!(
        "{SUMMARY_MARKER}\n\n{summary}"
    )));
    out.extend(plan.retained);
    if let Some(limit) = limit {
        let budget = (limit / 10 * 9).saturating_sub(overhead);
        let shortened = shrink_kept_results(&mut out, budget);
        if shortened > 0 {
            tracing::info!(
                shortened,
                "shortened large tool results kept after compaction"
            );
        }
    }
    let tokens_after: u64 = out.iter().map(message_tokens).sum();

    tracing::info!(
        tokens_before,
        tokens_after,
        messages_before = context.messages.len(),
        messages_after = out.len(),
        "auto-compacted context between turns"
    );
    Ok(Some(AutoCompaction {
        context: AgentContext {
            system_prompt: context.system_prompt.clone(),
            messages: out,
            tools: context.tools.clone(),
        },
        notice: format!(
            "Context compacted automatically: {} → {} tokens (older work summarized)",
            thousands(tokens_before),
            thousands(tokens_after)
        ),
        summary,
        tail_len,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantMessage, TextContent};

    fn assistant(text: &str) -> AgentMessage {
        let model = loop_ai::providers::soket_seed_models().remove(0);
        let mut a = AssistantMessage::pending(&model);
        a.content = vec![AssistantContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })];
        AgentMessage::assistant(a)
    }

    fn long_history(n: usize) -> Vec<AgentMessage> {
        let mut v = vec![AgentMessage::user_text("task prompt")];
        for i in 0..n {
            v.push(assistant(&format!("step {i} {}", "x".repeat(400))));
        }
        v
    }

    #[test]
    fn summary_never_asks_for_more_output_than_the_model_has() {
        let mut model = loop_ai::providers::soket_seed_models().remove(0);
        model.max_tokens = 2_048;
        assert_eq!(
            summary_output_tokens(&model, SUMMARY_MAX_OUTPUT_TOKENS),
            2_048
        );
        assert_eq!(
            summary_output_tokens(&model, SUMMARY_RETRY_OUTPUT_TOKENS),
            2_048
        );
        model.max_tokens = 16_384;
        assert_eq!(
            summary_output_tokens(&model, SUMMARY_RETRY_OUTPUT_TOKENS),
            SUMMARY_RETRY_OUTPUT_TOKENS
        );
        // Unknown maximum: ask for what the summary needs.
        model.max_tokens = 0;
        assert_eq!(
            summary_output_tokens(&model, SUMMARY_MAX_OUTPUT_TOKENS),
            SUMMARY_MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn splits_a_single_long_turn_at_an_assistant_message() {
        let msgs = long_history(30);
        let plan = plan_in_loop_compaction(&msgs, 300).expect("should plan");
        assert_eq!(plan.head.len(), 1);
        assert_eq!(plan.head[0].role(), "user");
        assert_eq!(plan.retained.first().map(|m| m.role()), Some("assistant"));
        assert_eq!(
            plan.head.len() + plan.to_summarize.len() + plan.retained.len(),
            msgs.len()
        );
        assert!(plan.to_summarize.len() >= 2);
    }

    #[test]
    fn nothing_to_do_for_short_history() {
        assert!(plan_in_loop_compaction(&long_history(2), 10_000).is_none());
        assert!(plan_in_loop_compaction(&[], 100).is_none());
    }

    #[test]
    fn earlier_summary_is_kept_whole_when_summarizing_again() {
        let long = "k".repeat(5000);
        let msgs = vec![
            AgentMessage::user_text("fix it"),
            AgentMessage::user_text(format!("{SUMMARY_MARKER}\n\nSTATE: {long}")),
        ];
        let text = render_for_summary(&msgs);
        assert!(
            text.contains(&long),
            "a prior summary must not be cut to 2000 chars"
        );
        assert!(!text.contains("[truncated]"));
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(118_092), "118,092");
        assert_eq!(thousands(1_048_576), "1,048,576");
    }

    #[test]
    fn transcript_cap_scales_with_the_window() {
        let mut model = loop_ai::providers::soket_seed_models().remove(0);
        model.context_window = 1_048_576;
        assert_eq!(max_transcript_chars(&model), MAX_TRANSCRIPT_CHARS);
        model.context_window = 32_768;
        assert_eq!(max_transcript_chars(&model), 32_768 * 40 / 100 * 3);
        model.context_window = 2_000;
        assert_eq!(max_transcript_chars(&model), 8_000);
        model.context_window = 0;
        assert_eq!(max_transcript_chars(&model), MAX_TRANSCRIPT_CHARS);
    }

    fn call(name: &str, args: serde_json::Value) -> AgentMessage {
        let model = loop_ai::providers::soket_seed_models().remove(0);
        let mut a = AssistantMessage::pending(&model);
        a.content = vec![AssistantContent::ToolCall(loop_ai::ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
        })];
        AgentMessage::assistant(a)
    }

    fn result(text: String) -> AgentMessage {
        AgentMessage::tool_result(loop_ai::ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "read".into(),
            content: vec![ToolResultContent::Text(TextContent {
                text,
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 0,
        })
    }

    fn result_text(m: &AgentMessage) -> String {
        match m {
            AgentMessage::Llm(Message::ToolResult(t)) => match &t.content[0] {
                ToolResultContent::Text(t) => t.text.clone(),
                ToolResultContent::Image(_) => String::new(),
            },
            _ => String::new(),
        }
    }

    #[test]
    fn file_list_comes_from_the_tool_calls() {
        let msgs = vec![
            AgentMessage::user_text("task"),
            call("read", serde_json::json!({"path": "src/a.rs"})),
            call("read", serde_json::json!({"path": "src/b.rs"})),
            call("bash", serde_json::json!({"command": "cargo test"})),
            call(
                "edit",
                serde_json::json!({"path": "src/a.rs", "oldText": "x", "newText": "y"}),
            ),
            call(
                "write",
                serde_json::json!({"path": "src/c.rs", "content": "z"}),
            ),
            call("read", serde_json::json!({"path": "src/c.rs"})),
        ];
        assert_eq!(
            TouchedFiles::collect(&msgs).render(),
            format!("\n\n{FILES_HEADING}\nchanged:\n- src/a.rs\n- src/c.rs\nread:\n- src/b.rs")
        );
        assert_eq!(TouchedFiles::collect(&msgs[..1]).render(), "");
    }

    #[test]
    fn file_list_of_an_earlier_summary_is_carried_forward() {
        let first = TouchedFiles::collect(&[
            call("read", serde_json::json!({"path": "old/read.rs"})),
            call(
                "write",
                serde_json::json!({"path": "old/new.rs", "content": ""}),
            ),
            call("read", serde_json::json!({"path": "later-changed.rs"})),
        ])
        .render();
        let msgs = vec![
            AgentMessage::user_text("task"),
            AgentMessage::user_text(format!("{SUMMARY_MARKER}\n\nSTATE: ok{first}")),
            call(
                "edit",
                serde_json::json!({"path": "later-changed.rs", "oldText": "a", "newText": "b"}),
            ),
        ];
        let files = TouchedFiles::collect(&msgs);
        assert_eq!(files.changed, ["old/new.rs", "later-changed.rs"]);
        assert_eq!(files.read, ["old/read.rs"]);
    }

    #[test]
    fn file_list_is_capped_and_drops_the_oldest_reads_first() {
        let mut msgs = vec![call(
            "write",
            serde_json::json!({"path": "kept/changed.rs", "content": ""}),
        )];
        for i in 0..400 {
            msgs.push(call(
                "read",
                serde_json::json!({"path": format!("some/long/dir/file-{i:04}.rs")}),
            ));
        }
        let block = TouchedFiles::collect(&msgs).render();
        assert!(block.len() <= MAX_FILES_CHARS + 200, "{}", block.len());
        assert!(block.contains("kept/changed.rs"));
        assert!(block.contains("file-0399.rs"));
        assert!(!block.contains("file-0000.rs"));
    }

    #[test]
    fn a_large_kept_result_is_shortened_to_fit() {
        let big = format!("START{}END", "m".repeat(80_000));
        let mut msgs = vec![
            AgentMessage::user_text("task"),
            assistant("reading"),
            result(big.clone()),
            assistant("small"),
            result("tiny".into()),
        ];
        let before: u64 = msgs.iter().map(message_tokens).sum();
        assert!(before > 20_000);
        assert_eq!(shrink_kept_results(&mut msgs, 5_000), 1);
        let after: u64 = msgs.iter().map(message_tokens).sum();
        assert!(after <= 5_000, "{after}");
        let text = result_text(&msgs[2]);
        assert!(text.starts_with("START") && text.ends_with("END"));
        assert!(text.contains("middle removed to save context"));
        assert_eq!(result_text(&msgs[4]), "tiny");
    }

    #[test]
    fn kept_results_are_left_alone_when_they_fit_and_never_cut_below_the_floor() {
        let mut msgs = vec![assistant("a"), result("r".repeat(6_000))];
        assert_eq!(shrink_kept_results(&mut msgs, 10_000), 0);
        assert_eq!(result_text(&msgs[1]).len(), 6_000);
        // An impossible budget: cut to the floor once, then stop.
        assert_eq!(shrink_kept_results(&mut msgs, 1), 1);
        let left = result_text(&msgs[1]).chars().count();
        assert_eq!(left, MIN_KEPT_RESULT_CHARS + KEPT_RESULT_MARKER.len());
        assert_eq!(shrink_kept_results(&mut msgs, 1), 1);
        assert_eq!(result_text(&msgs[1]).chars().count(), left);
    }

    #[test]
    fn summarizer_sees_long_tool_output_when_the_budget_allows() {
        let needle = "value-deep-inside";
        let body = format!("{}{needle}{}", "a".repeat(6_000), "b".repeat(6_000));
        let msgs = vec![
            call(
                "write",
                serde_json::json!({"path": "f", "content": "c".repeat(3_000)}),
            ),
            result(body),
        ];
        // The default preview cuts it; a roomy summary request does not.
        assert!(!render_for_summary(&msgs).contains(needle));
        let roomy = render_within(&msgs, 100_000);
        assert!(roomy.contains(needle));
        assert!(roomy.contains(&"c".repeat(3_000)));
        // A tight one falls back to short previews, and still shows both steps.
        let tight = render_within(&msgs, 2_000);
        assert!(!tight.contains(needle));
        assert!(tight.contains("CALL write(") && tight.contains("RESULT read:"));
        assert!(tight.len() <= 2_000);
    }

    #[test]
    fn render_truncates_bulk() {
        let msgs = vec![
            AgentMessage::user_text("fix it"),
            assistant(&"y".repeat(5000)),
        ];
        let text = render_for_summary(&msgs);
        assert!(text.contains("USER: fix it"));
        assert!(text.contains("[truncated]"));
        assert!(text.len() < 2000);
    }
}
