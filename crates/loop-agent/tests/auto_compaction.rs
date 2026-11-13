//! Automatic context compaction in a turn: before a request near the window,
//! between tool rounds, and once after a context-length error.
//!
//! The faux provider acts like a real one: it measures each request and
//! refuses one larger than the model's window with a context-length error.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::compaction::{CompactionSettings, SUMMARY_SYSTEM_PROMPT};
use loop_agent::harness::session::types::SessionTreeEntry;
use loop_agent::harness::session::SessionForkSelection;
use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    HostExecutionEnv, SandboxMode, SessionStore,
};
use loop_agent::native_backend::NativeBackend;
use loop_agent::{convert_to_llm, AgentMessage, AgentTool, AgentToolResult, CustomAgentMessage};
use loop_ai::providers::{faux_provider_fn, FauxResponse};
use loop_ai::{
    estimate_message_tokens, AssistantContent, AssistantMessage, AssistantMessageEvent, Context,
    Message, Model, Models, StopReason, TextContent, ToolCall,
};
use loop_protocol::{AgentBackend, ContentBlock, SessionUpdate, TurnOutcome};
use parking_lot::Mutex;
use serde_json::json;

const WINDOW: u64 = 8_000;

/// What the provider answers to the next main (not summary) request.
#[derive(Clone)]
enum Reply {
    Text(&'static str),
    /// A tool call to `big`, which returns about 750 tokens.
    Tool,
    /// A context-length error, whatever the size.
    Overflow,
    /// `n` tool calls to `big` in one response (run in parallel).
    Tools(usize),
    /// Text of about this many tokens cut at the output limit (the loop
    /// asks it to continue).
    Length(usize),
    /// A tool call after signed thinking.
    ThinkingTool,
}

/// What the provider answers to a summary request.
#[derive(Clone)]
enum SummaryReply {
    Text,
    Fail,
    /// Never answers; `started` is notified.
    Hang,
}

#[derive(Clone)]
struct Request {
    summary: bool,
    tokens: u64,
    messages: Vec<Message>,
}

#[derive(Clone)]
struct Provider {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    summaries: Arc<Mutex<VecDeque<SummaryReply>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    /// Report real usage on answers; `false` reports none, so only an
    /// estimate or an overflow error tells the loop the size.
    usage: Arc<Mutex<bool>>,
    started: Arc<tokio::sync::Notify>,
    /// The provider's real limit, which may be below the model's declared
    /// window.
    limit: Arc<Mutex<u64>>,
}

impl Provider {
    fn main_requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .iter()
            .filter(|r| !r.summary)
            .cloned()
            .collect()
    }
    fn summary_requests(&self) -> usize {
        self.requests.lock().iter().filter(|r| r.summary).count()
    }
    fn push(&self, replies: impl IntoIterator<Item = Reply>) {
        self.replies.lock().extend(replies);
    }
}

fn request_tokens(ctx: &Context) -> u64 {
    let system = ctx
        .system_prompt
        .as_deref()
        .map_or(0, |s| s.len() as u64 / 4);
    system
        + ctx
            .messages
            .iter()
            .map(estimate_message_tokens)
            .sum::<u64>()
}

fn answer(
    model: &Model,
    content: Vec<AssistantContent>,
    stop: StopReason,
    input: u64,
) -> FauxResponse {
    let mut message = AssistantMessage::pending(model);
    message.content = content;
    message.stop_reason = stop;
    if input > 0 {
        message.usage.input = input;
        message.usage.output = 10;
        message.usage.total_tokens = input + 10;
    }
    FauxResponse::Events(vec![
        AssistantMessageEvent::Start {
            partial: AssistantMessage::pending(model),
        },
        AssistantMessageEvent::Done {
            reason: stop,
            message,
        },
    ])
}

fn text(t: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: t.into(),
        text_signature: None,
    })
}

struct Fixture {
    harness: Arc<AgentHarness>,
    provider: Provider,
    model: Model,
}

async fn fixture(settings: CompactionSettings) -> Fixture {
    fixture_on(create_in_memory_session_store(), settings).await
}

async fn fixture_on(store: Arc<dyn SessionStore>, settings: CompactionSettings) -> Fixture {
    fixture_with(store, settings, WINDOW, "sys".into()).await
}

/// A fixture with its own window and system prompt.
async fn fixture_with(
    store: Arc<dyn SessionStore>,
    settings: CompactionSettings,
    window: u64,
    system_prompt: String,
) -> Fixture {
    let provider = Provider {
        replies: Default::default(),
        summaries: Default::default(),
        requests: Default::default(),
        usage: Arc::new(Mutex::new(true)),
        started: Default::default(),
        limit: Arc::new(Mutex::new(window)),
    };
    let model_slot: Arc<Mutex<Option<Model>>> = Default::default();
    let (p, slot) = (provider.clone(), Arc::clone(&model_slot));
    let faux = faux_provider_fn(Arc::new(move |ctx: &Context| {
        let model = slot.lock().clone().expect("model");
        let summary = ctx.system_prompt.as_deref() == Some(SUMMARY_SYSTEM_PROMPT);
        let tokens = request_tokens(ctx);
        p.requests.lock().push(Request {
            summary,
            tokens,
            messages: ctx.messages.clone(),
        });
        let p = p.clone();
        Box::pin(async move {
            if summary {
                let reply = p.summaries.lock().pop_front().unwrap_or(SummaryReply::Text);
                return match reply {
                    SummaryReply::Text => answer(
                        &model,
                        vec![text("SUMMARY: the user asked for work; it is in progress.")],
                        StopReason::Stop,
                        tokens,
                    ),
                    SummaryReply::Fail => FauxResponse::Error("HTTP 500 summary failed".into()),
                    SummaryReply::Hang => {
                        p.started.notify_one();
                        std::future::pending().await
                    }
                };
            }
            let reply = p.replies.lock().pop_front().unwrap_or(Reply::Text("done"));
            if tokens > *p.limit.lock() || matches!(reply, Reply::Overflow) {
                return FauxResponse::Error(format!(
                    "400 context_length_exceeded: prompt is too long ({tokens} tokens)"
                ));
            }
            let usage = if *p.usage.lock() { tokens } else { 0 };
            match reply {
                Reply::Text(t) => answer(&model, vec![text(t)], StopReason::Stop, usage),
                Reply::Tool => {
                    let n = p.requests.lock().len();
                    answer(
                        &model,
                        vec![AssistantContent::ToolCall(ToolCall {
                            id: format!("call-{n}"),
                            name: "big".into(),
                            arguments: json!({}),
                            thought_signature: None,
                        })],
                        StopReason::ToolUse,
                        usage,
                    )
                }
                Reply::Tools(count) => {
                    let n = p.requests.lock().len();
                    let calls = (0..count)
                        .map(|i| {
                            AssistantContent::ToolCall(ToolCall {
                                id: format!("call-{n}-{i}"),
                                name: "big".into(),
                                arguments: json!({}),
                                thought_signature: None,
                            })
                        })
                        .collect();
                    answer(&model, calls, StopReason::ToolUse, usage)
                }
                Reply::Length(tokens) => answer(
                    &model,
                    vec![text(&"partial ".repeat(tokens / 2))],
                    StopReason::Length,
                    usage,
                ),
                Reply::ThinkingTool => {
                    let n = p.requests.lock().len();
                    answer(
                        &model,
                        vec![
                            AssistantContent::Thinking(loop_ai::ThinkingContent {
                                thinking: "thinking it over".into(),
                                thinking_signature: Some("SIGNATURE-123".into()),
                                redacted: None,
                            }),
                            AssistantContent::ToolCall(ToolCall {
                                id: format!("call-{n}"),
                                name: "big".into(),
                                arguments: json!({}),
                                thought_signature: Some("TOOL-SIGNATURE".into()),
                            }),
                        ],
                        StopReason::ToolUse,
                        usage,
                    )
                }
                Reply::Overflow => unreachable!(),
            }
        })
    }));
    let models = Arc::new(Models::new());
    models.set_provider(faux);
    let mut model = models.get_model("faux", "faux-model").unwrap();
    model.context_window = window;
    *model_slot.lock() = Some(model.clone());

    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    let big = AgentTool::simple(
        "big",
        "Big",
        "Returns a large result",
        json!({"type":"object","properties":{}}),
        |_id, _args, _c, _u| async move { Ok(AgentToolResult::text("r".repeat(3_000))) },
    );
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model.clone()),
        session,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![big],
        system_prompt,
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    harness.set_compaction_settings(settings).await;
    Fixture {
        harness,
        provider,
        model,
    }
}

fn settings() -> CompactionSettings {
    CompactionSettings {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 1_500,
    }
}

/// A user message of about `tokens` tokens.
fn long(label: &str, tokens: usize) -> String {
    format!("{label} {}", "w".repeat(tokens * 4))
}

fn compactions(entries: &[SessionTreeEntry]) -> Vec<&SessionTreeEntry> {
    entries
        .iter()
        .filter(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
        .collect()
}

fn trigger(entry: &SessionTreeEntry) -> String {
    match entry {
        SessionTreeEntry::Compaction { details, .. } => details
            .as_ref()
            .and_then(|d| d["trigger"].as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Every tool result follows the call it answers, and every call has a result.
fn assert_pairs(messages: &[Message]) {
    let mut open: HashSet<String> = HashSet::new();
    for (i, message) in messages.iter().enumerate() {
        match message {
            Message::Assistant(a) => {
                assert!(
                    open.is_empty(),
                    "calls {open:?} have no result before message {i}"
                );
                for c in &a.content {
                    if let AssistantContent::ToolCall(call) = c {
                        open.insert(call.id.clone());
                    }
                }
            }
            Message::ToolResult(r) => {
                assert!(
                    open.remove(&r.tool_call_id),
                    "result {} at {i} has no call before it",
                    r.tool_call_id
                );
            }
            Message::User(_) => {
                assert!(
                    open.is_empty(),
                    "calls {open:?} have no result before message {i}"
                );
            }
        }
    }
}

/// Debug text of `messages` with summary timestamps cleared: a summary
/// message is rebuilt with the current time each time a context is built.
fn same_llm(messages: &[Message]) -> String {
    let messages: Vec<Message> = messages
        .iter()
        .cloned()
        .map(|mut m| {
            if let Message::User(u) = &mut m {
                if format!("{:?}", u.content).contains("<compaction-summary>") {
                    u.timestamp = 0;
                }
            }
            m
        })
        .collect();
    format!("{messages:?}")
}

fn same_agent(messages: &[AgentMessage]) -> String {
    let messages: Vec<AgentMessage> = messages
        .iter()
        .cloned()
        .map(|mut m| {
            if let AgentMessage::Custom(CustomAgentMessage::CompactionSummary {
                timestamp, ..
            }) = &mut m
            {
                *timestamp = 0;
            }
            m
        })
        .collect();
    format!("{messages:?}")
}

fn is_summary(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::Custom(CustomAgentMessage::CompactionSummary { .. })
    )
}

/// (a) A conversation that grows past the window less the reserve compacts
/// before the request that would cross it; the next request fits.
#[tokio::test]
async fn a_long_conversation_compacts_before_the_window() {
    let f = fixture(settings()).await;
    for i in 0..6 {
        f.harness
            .prompt(long(&format!("turn {i}"), 1_500))
            .await
            .unwrap();
    }
    assert!(f.provider.summary_requests() >= 1, "no compaction ran");
    for r in f.provider.main_requests() {
        assert!(
            r.tokens <= WINDOW,
            "a request of {} tokens was sent",
            r.tokens
        );
    }
    let entries = f.harness.read_session_entries().await.unwrap();
    let found = compactions(&entries);
    assert!(!found.is_empty());
    assert!(found.iter().all(|e| trigger(e) == "threshold"));
    // No request failed, so no error reached the tree.
    assert!(!entries.iter().any(|e| matches!(e,
        SessionTreeEntry::Message { message, .. }
            if message.as_assistant().is_some_and(|a| a.stop_reason == StopReason::Error))));
    // The request after the last compaction starts with its summary.
    let after = f
        .provider
        .main_requests()
        .into_iter()
        .rfind(
            |r| matches!(&r.messages[0], Message::User(u) if format!("{u:?}").contains("SUMMARY")),
        )
        .expect("a request after compaction");
    assert!(after.tokens + settings().reserve_tokens <= WINDOW + 1_500);
    let ctx = f.harness.session_context().await.unwrap();
    assert!(is_summary(&ctx.messages[0]));
}

/// (b) The provider's real limit is below the declared window, so the
/// check does not fire; the provider refuses the request; one compaction and
/// one retry succeed.
#[tokio::test]
async fn an_overflow_compacts_once_and_retries_once() {
    let f = fixture(settings()).await;
    *f.provider.limit.lock() = 6_000;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 1_500))
            .await
            .unwrap();
    }
    assert_eq!(f.provider.summary_requests(), 0);
    let before = f.provider.main_requests().len();
    // About 6.1k tokens: under the 7k threshold, over the provider's 6k.
    f.harness.prompt(long("turn 3", 1_500)).await.unwrap();
    let turn: Vec<_> = f.provider.main_requests()[before..].to_vec();
    assert_eq!(turn.len(), 2, "the refused request and one retry");
    assert!(turn[0].tokens > 6_000);
    assert!(turn[1].tokens <= 6_000);
    assert_eq!(f.provider.summary_requests(), 1);
    let entries = f.harness.read_session_entries().await.unwrap();
    let found = compactions(&entries);
    assert_eq!(found.len(), 1, "one compaction");
    assert_eq!(trigger(found[0]), "overflow");
    let ctx = f.harness.session_context().await.unwrap();
    let last = ctx.messages.last().and_then(|m| m.as_assistant()).unwrap();
    assert_eq!(last.stop_reason, StopReason::Stop);
    // The refused attempt is not in the tree.
    assert!(!entries.iter().any(|e| matches!(e,
        SessionTreeEntry::Message { message, .. }
            if message.as_assistant().is_some_and(|a| a.stop_reason == StopReason::Error))));
}

/// (b) An overflow with nothing near the threshold: the provider refuses a
/// small request; one compaction and one retry.
#[tokio::test]
async fn an_overflow_error_triggers_one_compaction_and_one_retry() {
    let f = fixture(settings()).await;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 600))
            .await
            .unwrap();
    }
    let before = f.provider.main_requests().len();
    f.provider
        .push([Reply::Overflow, Reply::Text("after retry")]);
    let answer = f.harness.prompt("next").await.unwrap();
    let answer = answer.as_assistant().unwrap();
    assert_eq!(answer.stop_reason, StopReason::Stop);
    assert_eq!(f.provider.main_requests().len() - before, 2);
    assert_eq!(f.provider.summary_requests(), 1);
    let entries = f.harness.read_session_entries().await.unwrap();
    let found = compactions(&entries);
    assert_eq!(found.len(), 1);
    assert_eq!(trigger(found[0]), "overflow");
    let ctx = f.harness.session_context().await.unwrap();
    assert!(is_summary(&ctx.messages[0]));
    // The current request is kept after the summary.
    assert!(ctx.messages.iter().any(
        |m| matches!(m, AgentMessage::Llm(Message::User(u)) if format!("{u:?}").contains("next"))
    ));
}

/// (c) The retry overflows too: the turn ends with a clear error, after one
/// compaction and two requests.
#[tokio::test]
async fn a_second_overflow_ends_the_turn_with_an_error() {
    let f = fixture(settings()).await;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 600))
            .await
            .unwrap();
    }
    let before = f.provider.main_requests().len();
    f.provider
        .push([Reply::Overflow, Reply::Overflow, Reply::Overflow]);
    let answer = f.harness.prompt("next").await.unwrap();
    let answer = answer.as_assistant().unwrap().clone();
    assert_eq!(answer.stop_reason, StopReason::Error);
    let error = answer.error_message.unwrap();
    assert!(error.contains("still too long"), "{error}");
    assert!(error.contains("context_length_exceeded"), "{error}");
    assert_eq!(f.provider.main_requests().len() - before, 2, "no loop");
    assert_eq!(f.provider.summary_requests(), 1);
    // The Session works afterwards.
    f.harness.prompt("again").await.unwrap();
}

/// (c) The compaction itself fails on an overflow: a clear error, no retry.
#[tokio::test]
async fn a_failed_compaction_on_overflow_ends_the_turn() {
    let f = fixture(settings()).await;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 600))
            .await
            .unwrap();
    }
    let before = f.provider.main_requests().len();
    let entries_before = f.harness.read_session_entries().await.unwrap().len();
    f.provider.push([Reply::Overflow]);
    f.provider.summaries.lock().push_back(SummaryReply::Fail);
    let answer = f.harness.prompt("next").await.unwrap();
    let answer = answer.as_assistant().unwrap().clone();
    assert_eq!(answer.stop_reason, StopReason::Error);
    let error = answer.error_message.unwrap();
    assert!(error.contains("automatic compaction failed"), "{error}");
    assert!(error.contains("summary failed"), "{error}");
    assert_eq!(f.provider.main_requests().len() - before, 1);
    let entries = f.harness.read_session_entries().await.unwrap();
    assert!(compactions(&entries).is_empty());
    // The user message and the error.
    assert_eq!(entries.len(), entries_before + 2);
}

/// (d) One turn of many tool rounds outgrows the window: it compacts
/// between rounds, never between a call and its result, and keeps the
/// user's request.
#[tokio::test]
async fn tool_rounds_compact_between_rounds_without_splitting_pairs() {
    let f = fixture(settings()).await;
    f.provider.push(std::iter::repeat_n(Reply::Tool, 24));
    f.provider.push([Reply::Text("all done")]);
    let answer = f.harness.prompt("please run big many times").await.unwrap();
    assert_eq!(answer.as_assistant().unwrap().stop_reason, StopReason::Stop);
    // Twice: the second compaction keeps messages queued after the first.
    // A turn with no earlier turns has nothing to summarize: the threshold
    // path shortens the turn's older tool outputs, with no model call.
    let found = compactions(&f.harness.read_session_entries().await.unwrap()).len();
    assert!(found >= 2, "{found} compactions");
    assert_eq!(f.provider.summary_requests(), 0);
    let main = f.provider.main_requests();
    assert_eq!(main.len(), 25, "every round ran once");
    for r in &main {
        assert!(r.tokens <= WINDOW, "a request of {} tokens", r.tokens);
        assert_pairs(&r.messages);
        // The threshold path never cuts the current request.
        assert!(
            r.messages.iter().any(|m| matches!(m, Message::User(u)
                if format!("{u:?}").contains("please run big many times")
                    && !format!("{u:?}").contains("<compaction-summary>"))),
            "a request lost the user's message"
        );
    }
    // The Session tree holds what the last request held, then the answer.
    let ctx = f.harness.session_context().await.unwrap();
    let llm = convert_to_llm(&ctx.messages);
    assert_pairs(&llm);
    let last = main.last().unwrap();
    assert_eq!(llm.len(), last.messages.len() + 1);
    assert_eq!(same_llm(&llm[..llm.len() - 1]), same_llm(&last.messages));
    let entries = f.harness.read_session_entries().await.unwrap();
    for c in compactions(&entries) {
        assert_eq!(trigger(c), "threshold");
        if let SessionTreeEntry::Compaction {
            first_kept_entry_id: Some(id),
            ..
        } = c
        {
            assert!(entries.iter().any(|e| e.id() == id));
        } else {
            panic!("a compaction without its kept entry");
        }
    }
}

/// (e) Disabled: no compaction; an overflow is the provider's error.
#[tokio::test]
async fn disabled_settings_never_compact() {
    let mut s = settings();
    s.enabled = false;
    let f = fixture(s).await;
    for i in 0..6 {
        f.harness
            .prompt(long(&format!("turn {i}"), 1_500))
            .await
            .unwrap();
    }
    assert_eq!(f.provider.summary_requests(), 0);
    let entries = f.harness.read_session_entries().await.unwrap();
    assert!(compactions(&entries).is_empty());
    let last = f.harness.session_context().await.unwrap();
    let last = last.messages.last().unwrap().as_assistant().unwrap();
    assert_eq!(last.stop_reason, StopReason::Error);
    assert!(last
        .error_message
        .as_deref()
        .unwrap()
        .contains("context_length_exceeded"));
}

/// (f) After an automatic compaction the Session resumes, replays and forks
/// from before and after the compaction entry.
#[tokio::test]
async fn resume_transcript_and_fork_work_after_automatic_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = loop_agent::harness::create_sqlite_session_store(dir.path().join("s.db")).unwrap();
    let f = fixture_on(Arc::clone(&store), settings()).await;
    for i in 0..6 {
        f.harness
            .prompt(long(&format!("turn {i}"), 1_500))
            .await
            .unwrap();
    }
    let entries = f.harness.read_session_entries().await.unwrap();
    let compaction_at = entries
        .iter()
        .position(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
        .expect("a compaction");
    let live = f.harness.session_context().await.unwrap();
    let id = f.harness.session_id().await;
    let points = f.harness.fork_points().await.unwrap();
    assert!(!points.is_empty());

    // Resume: a new harness on the same store builds the same context.
    drop(f.harness);
    let resumed = fixture_on(Arc::clone(&store), settings()).await;
    resumed.harness.switch_session(&id).await.unwrap();
    let ctx = resumed.harness.session_context().await.unwrap();
    assert_eq!(same_agent(&ctx.messages), same_agent(&live.messages));

    // Transcript: the replay shows the summary.
    let backend = NativeBackend::new(Arc::clone(&resumed.harness)).await;
    let session = backend.current_session().await;
    let replay = backend.transcript(&session).await.unwrap();
    assert!(replay.iter().any(|u| matches!(u,
        SessionUpdate::Notice { text, .. } if text.starts_with("compaction summary"))));

    // Fork before the compaction entry: the older, uncompacted history.
    let before_id = entries[..compaction_at]
        .iter()
        .rfind(
            |e| matches!(e, SessionTreeEntry::Message { message, .. } if message.role() == "user"),
        )
        .unwrap()
        .id()
        .to_string();
    resumed
        .harness
        .fork_session(SessionForkSelection::ThroughEntry, Some(&before_id), None)
        .await
        .unwrap();
    let forked = resumed.harness.session_context().await.unwrap();
    assert!(!forked.messages.iter().any(is_summary));
    resumed.harness.prompt("in the old fork").await.unwrap();

    // Fork after it: the compacted context, and it keeps working.
    resumed.harness.switch_session(&id).await.unwrap();
    let after_id = entries.last().unwrap().id().to_string();
    resumed
        .harness
        .fork_session(SessionForkSelection::ThroughEntry, Some(&after_id), None)
        .await
        .unwrap();
    let forked = resumed.harness.session_context().await.unwrap();
    assert!(is_summary(&forked.messages[0]));
    resumed.harness.prompt("in the new fork").await.unwrap();
    let _ = resumed.model;
}

/// (f) Through the backend: the compaction counts in the Session revision,
/// which a reload computes the same way.
#[tokio::test]
async fn the_backend_counts_an_automatic_compaction_in_the_revision() {
    let f = fixture(settings()).await;
    let backend = NativeBackend::new(Arc::clone(&f.harness)).await;
    let session = backend.current_session().await;
    let mut rx = backend.subscribe();
    let start = backend.revision();
    for i in 0..6 {
        let turn = backend
            .prompt(
                &session,
                vec![ContentBlock::text(long(&format!("turn {i}"), 1_500))],
            )
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
    }
    let entries = f.harness.read_session_entries().await.unwrap();
    let count = compactions(&entries).len() as u64;
    assert!(count >= 1);
    assert_eq!(backend.revision(), start + count);
    let mut notices = Vec::new();
    let mut revisions = Vec::new();
    while let Ok(Ok(envelope)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        match envelope.update {
            SessionUpdate::Notice { text, .. } => notices.push(text),
            SessionUpdate::ConfigChanged(c) => revisions.push(c.revision),
            _ => {}
        }
    }
    assert!(
        notices.iter().any(|t| t.contains("compacting")),
        "{notices:?}"
    );
    assert!(
        notices.iter().any(|t| t.starts_with("context compacted")),
        "{notices:?}"
    );
    assert_eq!(revisions.last().copied(), Some(start + count));
    let reloaded = loop_agent::harness::session::types::config_from_entries(&entries, None);
    assert_eq!(reloaded.revision, count);
}

/// (g) A cancel while the summary is written: the turn stops, nothing of it
/// reaches the tree, and the Session works afterwards.
#[tokio::test]
async fn cancel_during_compaction_leaves_the_session_unchanged() {
    let f = fixture(settings()).await;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 600))
            .await
            .unwrap();
    }
    let entries_before = f.harness.read_session_entries().await.unwrap();
    let context_before = f.harness.session_context().await.unwrap();
    f.provider.push([Reply::Overflow]);
    f.provider.summaries.lock().push_back(SummaryReply::Hang);
    let started = Arc::clone(&f.provider.started);
    let harness = Arc::clone(&f.harness);
    let turn = tokio::spawn(async move { harness.prompt("next").await });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the summary request started");
    f.harness.abort();
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn stopped")
        .unwrap();
    if let Ok(message) = result {
        assert_eq!(
            message.as_assistant().unwrap().stop_reason,
            StopReason::Aborted
        );
    }
    f.harness.wait_for_idle().await;
    let entries_after = f.harness.read_session_entries().await.unwrap();
    assert_eq!(entries_after.len(), entries_before.len());
    let context_after = f.harness.session_context().await.unwrap();
    assert_eq!(
        same_agent(&context_after.messages),
        same_agent(&context_before.messages)
    );
    // The next turn runs, and can compact.
    f.provider.push([Reply::Overflow, Reply::Text("fine")]);
    let answer = f.harness.prompt("again").await.unwrap();
    assert_eq!(answer.as_assistant().unwrap().stop_reason, StopReason::Stop);
    assert_eq!(
        compactions(&f.harness.read_session_entries().await.unwrap()).len(),
        1
    );
}

/// Manual compaction writes the same entry type, with a model summary whose
/// usage the statistics count.
#[tokio::test]
async fn manual_compaction_uses_the_model_and_counts_usage() {
    let f = fixture(settings()).await;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 600))
            .await
            .unwrap();
    }
    let result = f.harness.compact(None).await.unwrap();
    assert!(result.summary.starts_with("SUMMARY"));
    assert_eq!(f.provider.summary_requests(), 1);
    let entries = f.harness.read_session_entries().await.unwrap();
    let found = compactions(&entries);
    assert_eq!(found.len(), 1);
    assert_eq!(trigger(found[0]), "manual");
    let stats = f.harness.session_stats().await.unwrap();
    assert!(stats.breakdown.iter().any(|b| b.key == "Tools/summaries"));
}

/// A compaction whose kept part reaches back past an earlier compaction:
/// the context holds only the newer summary, and the entry map agrees.
#[tokio::test]
async fn a_later_compaction_replaces_an_earlier_summary() {
    use loop_agent::harness::compaction::context_entry_ids;
    use loop_agent::harness::session::types::{default_context_from_entries, PendingSessionWrite};
    let store = create_in_memory_session_store();
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    let sid = session.metadata().id.clone();
    let append = |w: PendingSessionWrite| {
        let store = Arc::clone(&store);
        let sid = sid.clone();
        async move { store.append_entry(&sid, w).await.unwrap() }
    };
    let message = |m: AgentMessage| PendingSessionWrite::Message { message: m };
    let a = append(message(AgentMessage::user_text("A"))).await;
    append(message(AgentMessage::user_text("B"))).await;
    let compaction = |summary: &str, kept: &str| PendingSessionWrite::Compaction {
        summary: summary.into(),
        first_kept_entry_id: Some(kept.into()),
        details: None,
    };
    append(compaction("s1", a.id())).await;
    append(message(AgentMessage::user_text("D"))).await;
    let c2 = append(compaction("s2", a.id())).await;
    let branch = session.read_branch().await.unwrap();
    let ctx = default_context_from_entries(&branch);
    let summaries: Vec<_> = ctx
        .messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Custom(CustomAgentMessage::CompactionSummary { summary, .. }) => {
                Some(summary.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(summaries, ["s2"]);
    assert_eq!(ctx.messages.len(), 4);
    let ids = context_entry_ids(&branch);
    assert_eq!(ids.len(), ctx.messages.len());
    assert_eq!(ids[0], c2.id());
    assert_eq!(ids[1], a.id());
}

/// A `keepRecentTokens` larger than the model's window still frees room:
/// the kept part is limited to half the usable window.
#[tokio::test]
async fn a_keep_budget_above_the_window_still_compacts() {
    let mut s = settings();
    s.keep_recent_tokens = 20_000;
    let f = fixture(s).await;
    f.provider.push(std::iter::repeat_n(Reply::Tool, 24));
    f.provider.push([Reply::Text("all done")]);
    let answer = f.harness.prompt("run big").await.unwrap();
    assert_eq!(answer.as_assistant().unwrap().stop_reason, StopReason::Stop);
    assert!(!compactions(&f.harness.read_session_entries().await.unwrap()).is_empty());
    for r in f.provider.main_requests() {
        assert!(r.tokens <= WINDOW, "a request of {} tokens", r.tokens);
        assert_pairs(&r.messages);
    }
}

/// `abort` stops a manual compaction's summary request; nothing is written.
#[tokio::test]
async fn abort_stops_a_manual_compaction() {
    let f = fixture(settings()).await;
    for i in 0..3 {
        f.harness
            .prompt(long(&format!("turn {i}"), 600))
            .await
            .unwrap();
    }
    let before = f.harness.read_session_entries().await.unwrap().len();
    f.provider.summaries.lock().push_back(SummaryReply::Hang);
    let started = Arc::clone(&f.provider.started);
    let harness = Arc::clone(&f.harness);
    let compact = tokio::spawn(async move { harness.compact(None).await });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the summary request started");
    f.harness.abort();
    let result = tokio::time::timeout(Duration::from_secs(5), compact)
        .await
        .expect("the compaction stopped")
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(
        f.harness.read_session_entries().await.unwrap().len(),
        before
    );
    f.harness.prompt("still works").await.unwrap();
}

/// The reviewer's scenarios: default settings, a first long prompt, then
/// three prompts of six tool rounds each. Returns the summary requests per
/// prompt.
async fn summary_requests_per_prompt(window: u64, system_chars: usize) -> Vec<usize> {
    let f = fixture_with(
        create_in_memory_session_store(),
        CompactionSettings::default(),
        window,
        "s".repeat(system_chars),
    )
    .await;
    f.harness.prompt(long("first", 2_000)).await.unwrap();
    let mut counts = Vec::new();
    for p in 0..3 {
        let before = f.provider.summary_requests();
        f.provider.push(std::iter::repeat_n(Reply::Tool, 6));
        f.provider.push([Reply::Text("all done")]);
        f.harness
            .prompt(format!("prompt {p}: please run big many times"))
            .await
            .unwrap();
        counts.push(f.provider.summary_requests() - before);
    }
    for e in compactions(&f.harness.read_session_entries().await.unwrap()) {
        if let SessionTreeEntry::Compaction { details, .. } = e {
            eprintln!("  compaction {details:?}");
        }
    }
    eprintln!("window {window}, system {system_chars} chars: summaries per prompt {counts:?}");
    counts
}

/// A 32k window with a 12k-token system prompt: the system prompt is a fixed
/// cost that compaction cannot remove, so the kept budget must leave room
/// for it. At most one summary per prompt, none wasted.
#[tokio::test]
async fn a_large_system_prompt_does_not_compact_before_every_request() {
    let counts = summary_requests_per_prompt(32_000, 48_000).await;
    assert!(counts.iter().all(|&c| c <= 1), "{counts:?}");
}

/// A 16k window below the 16384 reserve: automatic compaction cannot help
/// and must not run (one notice, no summary requests).
#[tokio::test]
async fn a_window_below_the_reserve_never_compacts_automatically() {
    let counts = summary_requests_per_prompt(16_000, 12).await;
    assert_eq!(counts, [0, 0, 0]);
}

/// The summary text of the newest compaction summary in a request.
fn summary_text(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| match m {
        Message::User(u) if format!("{u:?}").contains("<compaction-summary>") => {
            Some(format!("{u:?}"))
        }
        _ => None,
    })
}

/// Earlier turns that the threshold can summarize.
async fn with_history(f: &Fixture, turns: usize, tokens: usize) {
    for i in 0..turns {
        f.harness
            .prompt(long(&format!("turn {i}"), tokens))
            .await
            .unwrap();
    }
}

/// M3: at the threshold, a steering message in the turn does not move the
/// cut past the turn's request: the request stays verbatim in every request.
#[tokio::test]
async fn threshold_keeps_the_request_verbatim_with_a_steering_message() {
    let f = fixture(settings()).await;
    with_history(&f, 3, 1_000).await;
    f.harness.steer(AgentMessage::user_text("STEERING NOTE"));
    f.provider.push(std::iter::repeat_n(Reply::Tool, 8));
    f.provider.push([Reply::Text("done")]);
    f.harness.prompt("THE REAL REQUEST").await.unwrap();
    let entries = f.harness.read_session_entries().await.unwrap();
    assert!(!compactions(&entries).is_empty());
    for r in f.provider.main_requests().iter().rev().take(5) {
        assert!(r.messages.iter().any(|m| matches!(m, Message::User(u)
            if format!("{u:?}").contains("THE REAL REQUEST")
                && !format!("{u:?}").contains("<compaction-summary>"))));
    }
    // The request stays a fork point.
    let points = f.harness.fork_points().await.unwrap();
    assert!(points.iter().any(|p| p.text == "THE REAL REQUEST"));
}

/// M3: an overflow recovery that cuts into the turn quotes the request
/// captured when the turn started, not the newest user message (a steering
/// message).
#[tokio::test]
async fn overflow_quotes_the_turn_request_not_a_steering_message() {
    let f = fixture(settings()).await;
    f.harness.steer(AgentMessage::user_text("STEERING NOTE"));
    f.provider.push(std::iter::repeat_n(Reply::Tool, 4));
    f.provider.push([Reply::Overflow, Reply::Text("done")]);
    let answer = f.harness.prompt(long("THE REAL REQUEST", 400)).await.unwrap();
    assert_eq!(
        answer.as_assistant().unwrap().stop_reason,
        StopReason::Stop,
        "{:?}",
        answer.as_assistant().unwrap().error_message
    );
    let retry = f.provider.main_requests().last().unwrap().clone();
    let summary = summary_text(&retry.messages).expect("a summary");
    assert!(summary.contains("THE REAL REQUEST"), "{summary}");
    assert!(!summary.contains("<request>\\nSTEERING NOTE"), "{summary}");
    assert_pairs(&retry.messages);
}

/// M3: after a length stop the loop adds a continue prompt; an overflow
/// recovery still quotes the turn's request.
#[tokio::test]
async fn overflow_quotes_the_turn_request_not_the_continue_prompt() {
    let f = fixture(settings()).await;
    f.provider.push([Reply::Length(3_000), Reply::Overflow, Reply::Text("done")]);
    let answer = f.harness.prompt("THE REAL REQUEST").await.unwrap();
    assert_eq!(
        answer.as_assistant().unwrap().stop_reason,
        StopReason::Stop,
        "{:?}",
        answer.as_assistant().unwrap().error_message
    );
    let retry = f.provider.main_requests().last().unwrap().clone();
    let summary = summary_text(&retry.messages).expect("a summary");
    assert!(summary.contains("<request>"), "{summary}");
    assert!(summary.contains("THE REAL REQUEST"), "{summary}");
    assert!(!summary.contains("output token limit"), "{summary}");
}

/// Parallel tool calls: a cut never separates any of the results of one
/// response from it.
#[tokio::test]
async fn parallel_tool_calls_survive_a_mid_turn_cut() {
    let f = fixture(settings()).await;
    f.provider.push(std::iter::repeat_n(Reply::Tools(3), 5));
    f.provider.push([Reply::Overflow]);
    f.provider.push(std::iter::repeat_n(Reply::Tools(3), 2));
    f.provider.push([Reply::Text("done")]);
    let answer = f.harness.prompt("parallel please").await.unwrap();
    assert_eq!(answer.as_assistant().unwrap().stop_reason, StopReason::Stop);
    assert!(!compactions(&f.harness.read_session_entries().await.unwrap()).is_empty());
    for r in f.provider.main_requests() {
        assert_pairs(&r.messages);
    }
    let ctx = f.harness.session_context().await.unwrap();
    assert_pairs(&convert_to_llm(&ctx.messages));
}

/// Images in the request and signed thinking in the kept part reach the
/// next request unchanged.
#[tokio::test]
async fn images_and_thinking_signatures_are_kept() {
    let f = fixture(settings()).await;
    with_history(&f, 3, 1_000).await;
    f.provider.push([Reply::ThinkingTool]);
    f.provider.push(std::iter::repeat_n(Reply::Tool, 6));
    f.provider.push([Reply::Text("done")]);
    f.harness
        .prompt_with_images(
            "look at this",
            vec![loop_ai::ImageContent {
                data: "aW1hZ2U=".into(),
                mime_type: "image/png".into(),
            }],
        )
        .await
        .unwrap();
    let entries = f.harness.read_session_entries().await.unwrap();
    assert!(!compactions(&entries).is_empty());
    let last = f.provider.main_requests().last().unwrap().clone();
    let text = format!("{:?}", last.messages);
    assert!(text.contains("aW1hZ2U="), "the image is gone");
    assert!(text.contains("SIGNATURE-123"), "the thinking signature is gone");
    assert!(text.contains("TOOL-SIGNATURE"), "the tool call signature is gone");
}

/// A nested compaction on the SQLite store builds the same context as the
/// loop sent, and survives a reopen.
#[tokio::test]
async fn nested_compaction_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        loop_agent::harness::create_sqlite_session_store(dir.path().join("s.db")).unwrap();
    let f = fixture_on(Arc::clone(&store), settings()).await;
    with_history(&f, 8, 1_500).await;
    f.provider.push(std::iter::repeat_n(Reply::Tool, 20));
    f.provider.push([Reply::Text("done")]);
    f.harness.prompt("many rounds").await.unwrap();
    let entries = f.harness.read_session_entries().await.unwrap();
    assert!(compactions(&entries).len() >= 2);
    let ctx = f.harness.session_context().await.unwrap();
    let llm = convert_to_llm(&ctx.messages);
    let last = f.provider.main_requests().last().unwrap().clone();
    assert_eq!(same_llm(&llm[..llm.len() - 1]), same_llm(&last.messages));
    let id = f.harness.session_id().await;
    drop(f);
    let again = fixture_on(Arc::clone(&store), settings()).await;
    again.harness.switch_session(&id).await.unwrap();
    let reopened = again.harness.session_context().await.unwrap();
    assert_eq!(same_agent(&reopened.messages), same_agent(&ctx.messages));
}

/// M5: "nothing to compact" does not turn automatic compaction off for the
/// rest of the turn: a later round compacts.
#[tokio::test]
async fn nothing_to_compact_does_not_stop_later_compaction() {
    // One huge request with no earlier turns and no tool output yet:
    // nothing to cut at first; the tool rounds later can be shortened.
    let f = fixture(settings()).await;
    f.provider.push(std::iter::repeat_n(Reply::Tool, 14));
    f.provider.push([Reply::Text("done")]);
    let answer = f.harness.prompt(long("start", 300)).await.unwrap();
    assert_eq!(answer.as_assistant().unwrap().stop_reason, StopReason::Stop);
    assert!(!compactions(&f.harness.read_session_entries().await.unwrap()).is_empty());
}
