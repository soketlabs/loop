//! Scenarios every wire adapter must pass, and the event contract they share.
//!
//! An adapter joins the suite with a [`WireFixture`] (canned SSE bodies in its own
//! wire format, plus readers for the requests it sends) and one
//! `conformance_suite!(fixture)` line. The scenarios assert Loop's side of the
//! contract, so the agent loop sees the same events, stop reasons, usage and
//! error categories whichever provider answers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use loop_ai::{
    create_provider, is_context_overflow, AssistantContent, AssistantMessage,
    AssistantMessageEvent, Context, CreateProviderApi, CreateProviderOptions, Message, Model,
    Models, ProviderAuth, SharedApiAdapter, SimpleStreamOptions, StopReason, StreamOptions,
    TextContent, ThinkingContent, ThinkingLevel, Tool, ToolCall, ToolResultContent,
    ToolResultMessage,
};
use loop_protocol::ErrorCategory;
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Provider id every fixture model uses.
pub const PROVIDER: &str = "conformance";
/// Key sent with every request; fixtures check it lands in the right header.
pub const API_KEY: &str = "test-key-123";

/// Thinking text and signature in the reasoning scenario.
pub const THINKING: &str = "Let me think.";
/// Opaque signature the adapter must keep and send back unchanged.
pub const SIGNATURE: &str = "sig-abc";

/// Canonical usage every fixture reports in its own wire format: 100 uncached input
/// tokens, 80 read from cache, 20 written to cache (where the provider reports
/// writes), 10 output tokens of which 4 were reasoning.
pub const USAGE_INPUT: u64 = 100;
/// Cache-read tokens in the usage scenario.
pub const USAGE_CACHE_READ: u64 = 80;
/// Cache-write tokens in the usage scenario (when the provider reports them).
pub const USAGE_CACHE_WRITE: u64 = 20;
/// Output tokens in the usage scenario.
pub const USAGE_OUTPUT: u64 = 10;
/// Reasoning tokens in the usage scenario (part of the output).
pub const USAGE_REASONING: u64 = 4;

/// A tool call in a scenario: `(id, name, arguments JSON)`.
pub type Call = (&'static str, &'static str, &'static str);

/// The single call in the tool-call scenario.
pub const WEATHER: Call = ("call_1", "get_weather", r#"{"city":"Paris"}"#);
/// The second call in the parallel scenario.
pub const TIME: Call = ("call_2", "get_time", r#"{"tz":"UTC"}"#);

/// HTTP failures every adapter must map to the same categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpFailure {
    /// 401: the key is wrong.
    Unauthorized,
    /// 403: the key may not use this model.
    Forbidden,
    /// 429: throttled.
    RateLimited,
    /// 5xx: the provider failed.
    ServerError,
    /// The prompt does not fit the model's context window.
    ContextTooLong,
}

/// What an adapter can carry, where providers differ.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Reasoning comes with an opaque signature that must be replayed.
    pub reasoning_signatures: bool,
    /// The provider reports cache writes separately from reads.
    pub cache_writes: bool,
    /// The provider can report a failure inside a `200` stream.
    pub stream_errors: bool,
    /// The provider reports reasoning tokens separately.
    pub reasoning_tokens: bool,
}

/// One adapter's wire format, for the shared scenarios.
pub trait WireFixture: Send + Sync + 'static {
    /// The adapter under test.
    fn adapter(&self) -> SharedApiAdapter;
    /// A model served from `base_url` with this adapter's `api`.
    fn model(&self, base_url: &str) -> Model;
    /// What this adapter supports.
    fn caps(&self) -> Caps;

    /// Text streamed in `chunks`, ending normally.
    fn text(&self, chunks: &[&str]) -> String;
    /// Text that stops at the output limit.
    fn max_tokens(&self, text: &str) -> String;
    /// Tool calls in order, ending with a tool-use stop.
    fn tool_calls(&self, calls: &[Call]) -> String;
    /// Thinking (with [`SIGNATURE`] when signatures are supported), then `text`.
    fn thinking(&self, thinking: &str, text: &str) -> String;
    /// `text` with the canonical usage.
    fn usage(&self, text: &str) -> String;
    /// `chunks` as text with one unparseable event between the first and the rest.
    fn malformed(&self, chunks: &[&str]) -> String;
    /// A stream that fails part way with an overloaded error (when supported).
    fn stream_error(&self) -> Option<String> {
        None
    }
    /// Status and body for an HTTP failure.
    fn http_error(&self, failure: HttpFailure) -> (u16, String);

    /// The API key the request carried.
    fn api_key<'a>(&self, request: &'a RecordedRequest) -> Option<&'a str>;
    /// Tool calls replayed from assistant history: `(id, name, arguments)`.
    fn replayed_tool_calls(&self, body: &Value) -> Vec<(String, String, Value)>;
    /// Tool results sent back: `(call id, text)`.
    fn tool_results(&self, body: &Value) -> Vec<(String, String)>;
    /// Reasoning signatures sent back, in order.
    fn replayed_signatures(&self, body: &Value) -> Vec<String>;
}

/// Generate the suite's tests for one fixture.
#[macro_export]
macro_rules! conformance_suite {
    ($fixture:expr) => {
        #[tokio::test]
        async fn streams_text() {
            $crate::harness::streams_text($fixture).await;
        }
        #[tokio::test]
        async fn streams_a_tool_call() {
            $crate::harness::streams_a_tool_call($fixture).await;
        }
        #[tokio::test]
        async fn streams_parallel_tool_calls() {
            $crate::harness::streams_parallel_tool_calls($fixture).await;
        }
        #[tokio::test]
        async fn sends_tool_results_back() {
            $crate::harness::sends_tool_results_back($fixture).await;
        }
        #[tokio::test]
        async fn keeps_reasoning_across_tool_calls() {
            $crate::harness::keeps_reasoning_across_tool_calls($fixture).await;
        }
        #[tokio::test]
        async fn reports_usage_with_cached_tokens() {
            $crate::harness::reports_usage_with_cached_tokens($fixture).await;
        }
        #[tokio::test]
        async fn maps_stop_reasons() {
            $crate::harness::maps_stop_reasons($fixture).await;
        }
        #[tokio::test]
        async fn maps_http_errors() {
            $crate::harness::maps_http_errors($fixture).await;
        }
        #[tokio::test]
        async fn maps_stream_errors() {
            $crate::harness::maps_stream_errors($fixture).await;
        }
        #[tokio::test]
        async fn cancels_mid_stream() {
            $crate::harness::cancels_mid_stream($fixture).await;
        }
        #[tokio::test]
        async fn skips_a_malformed_chunk() {
            $crate::harness::skips_a_malformed_chunk($fixture).await;
        }
    };
}

/// A fixture wired to a fake server.
pub struct Rig {
    /// Collection with the fixture's provider.
    pub models: Models,
    /// Model under test.
    pub model: Model,
    /// The fake server (keeps it alive).
    pub server: FakeHttpServer,
}

impl Rig {
    /// Serve `response` for every request.
    pub fn new(fixture: &dyn WireFixture, response: FakeResponse) -> Self {
        let server = FakeHttpServer::always(response);
        let model = fixture.model(server.base_url());
        let models = Models::new();
        models.set_provider(create_provider(CreateProviderOptions {
            id: PROVIDER.into(),
            name: None,
            base_url: Some(server.base_url().into()),
            headers: None,
            auth: ProviderAuth::keyless("conformance"),
            models: vec![model.clone()],
            api: CreateProviderApi::ByApi(HashMap::from([(model.api.clone(), fixture.adapter())])),
            fetch_models: None,
        }));
        Self {
            models,
            model,
            server,
        }
    }

    /// Serve an SSE body.
    pub fn sse(fixture: &dyn WireFixture, body: String) -> Self {
        Self::new(fixture, FakeResponse::new(200, "text/event-stream", body))
    }

    /// Stream `context` and collect every event.
    pub async fn run(&self, context: &Context, options: SimpleStreamOptions) -> Run {
        let mut options = options;
        options.base.api_key = Some(API_KEY.into());
        let mut stream = self.models.stream_simple(&self.model, context, options);
        let mut events = Vec::new();
        let collect = async {
            while let Some(event) = stream.next().await {
                events.push(event);
            }
        };
        tokio::time::timeout(Duration::from_secs(10), collect)
            .await
            .expect("stream finished within 10s");
        let message = stream.result().await;
        Run { events, message }
    }

    /// The only request the server received, as JSON.
    pub fn request_body(&self) -> Value {
        let requests = self.server.requests();
        let last = requests.last().expect("a request was sent");
        serde_json::from_slice(&last.body).expect("request body is JSON")
    }
}

/// Events and the final message of one stream.
pub struct Run {
    /// Every event, in order.
    pub events: Vec<AssistantMessageEvent>,
    /// The resolved message.
    pub message: AssistantMessage,
}

impl Run {
    /// Text deltas in order.
    pub fn text_deltas(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    /// Final text, all text blocks joined.
    pub fn text(&self) -> String {
        self.message
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Final tool calls in content order.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.message
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Check the event contract the agent loop relies on.
pub fn assert_contract(run: &Run, model: &Model) {
    let events = &run.events;
    // A request that fails before the response starts may end with a lone error.
    let lone_error = matches!(events.as_slice(), [AssistantMessageEvent::Error { .. }]);
    assert!(
        lone_error || matches!(events.first(), Some(AssistantMessageEvent::Start { .. })),
        "first event is start: {events:#?}"
    );
    let terminal = events.iter().filter(|e| e.is_terminal()).count();
    assert_eq!(terminal, 1, "exactly one terminal event");
    assert!(
        events.last().unwrap().is_terminal(),
        "terminal event is last"
    );

    let message = &run.message;
    assert_eq!(message.api, model.api, "message keeps the model's api");
    assert_eq!(message.provider, model.provider);
    assert_eq!(message.model, model.id);

    // Every block opens once, closes once, and its deltas add up to its content.
    let mut open: HashMap<usize, String> = HashMap::new();
    let mut closed: Vec<usize> = Vec::new();
    for event in events {
        match event {
            AssistantMessageEvent::TextStart { content_index, .. }
            | AssistantMessageEvent::ThinkingStart { content_index, .. }
            | AssistantMessageEvent::ToolcallStart { content_index, .. } => {
                assert!(
                    open.insert(*content_index, String::new()).is_none()
                        && !closed.contains(content_index),
                    "block {content_index} opened twice"
                );
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            }
            | AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => open
                .get_mut(content_index)
                .unwrap_or_else(|| panic!("delta for unopened block {content_index}"))
                .push_str(delta),
            AssistantMessageEvent::ToolcallDelta { content_index, .. } => {
                assert!(open.contains_key(content_index), "unopened tool call");
            }
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            }
            | AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => {
                let streamed = open
                    .remove(content_index)
                    .unwrap_or_else(|| panic!("end for unopened block {content_index}"));
                assert_eq!(&streamed, content, "deltas add up to block {content_index}");
                closed.push(*content_index);
            }
            AssistantMessageEvent::ToolcallEnd {
                content_index,
                tool_call,
                ..
            } => {
                open.remove(content_index)
                    .unwrap_or_else(|| panic!("end for unopened call {content_index}"));
                closed.push(*content_index);
                if message.stop_reason.is_done() {
                    assert_eq!(
                        message.content.get(*content_index),
                        Some(&AssistantContent::ToolCall(tool_call.clone())),
                        "tool_call end matches the final message"
                    );
                }
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "blocks left open: {open:?}");
    assert!(
        closed.windows(2).all(|w| w[0] < w[1]),
        "blocks end in content order: {closed:?}"
    );

    match events.last().unwrap() {
        AssistantMessageEvent::Done { reason, .. } => {
            assert!(reason.is_done());
            assert_eq!(*reason, message.stop_reason);
            assert_eq!(closed.len(), message.content.len(), "every block streamed");
            let u = &message.usage;
            assert_eq!(
                u.total_tokens,
                u.input + u.output + u.cache_read + u.cache_write + u.cache_write_1h.unwrap_or(0),
                "total is the sum of the parts"
            );
        }
        AssistantMessageEvent::Error { reason, .. } => {
            assert!(reason.is_error());
            assert_eq!(*reason, message.stop_reason);
        }
        _ => unreachable!(),
    }
}

fn user(text: &str) -> Context {
    Context {
        system_prompt: Some("You are terse.".into()),
        messages: vec![Message::user_text(text)],
        tools: Some(tools()),
    }
}

fn tools() -> Vec<Tool> {
    [WEATHER, TIME]
        .iter()
        .map(|(_, name, _)| Tool {
            name: (*name).into(),
            description: format!("{name} tool"),
            parameters: json!({"type": "object", "properties": {}}),
        })
        .collect()
}

fn call(c: Call) -> ToolCall {
    ToolCall {
        id: c.0.into(),
        name: c.1.into(),
        arguments: serde_json::from_str(c.2).unwrap(),
        thought_signature: None,
    }
}

fn tool_result(c: Call, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: c.0.into(),
        tool_name: c.1.into(),
        content: vec![ToolResultContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 0,
    })
}

fn assert_tool_call(actual: &ToolCall, expected: Call) {
    assert_eq!(actual.id, expected.0);
    assert_eq!(actual.name, expected.1);
    assert_eq!(
        actual.arguments,
        serde_json::from_str::<Value>(expected.2).unwrap()
    );
}

/// Streamed text arrives as deltas and ends with `stop`.
pub async fn streams_text(fixture: impl WireFixture) {
    let rig = Rig::sse(&fixture, fixture.text(&["Hel", "lo"]));
    let run = rig.run(&user("hi"), SimpleStreamOptions::default()).await;
    assert_contract(&run, &rig.model);
    assert_eq!(run.message.stop_reason, StopReason::Stop);
    assert_eq!(run.text_deltas(), ["Hel", "lo"]);
    assert_eq!(run.text(), "Hello");
    let request = &rig.server.requests()[0];
    assert_eq!(fixture.api_key(request), Some(API_KEY));
}

/// One tool call, parsed, with a tool-use stop.
pub async fn streams_a_tool_call(fixture: impl WireFixture) {
    let rig = Rig::sse(&fixture, fixture.tool_calls(&[WEATHER]));
    let run = rig
        .run(&user("weather?"), SimpleStreamOptions::default())
        .await;
    assert_contract(&run, &rig.model);
    assert_eq!(run.message.stop_reason, StopReason::ToolUse);
    let calls = run.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_tool_call(&calls[0], WEATHER);
}

/// Several calls in one turn keep their order and arguments.
pub async fn streams_parallel_tool_calls(fixture: impl WireFixture) {
    let rig = Rig::sse(&fixture, fixture.tool_calls(&[WEATHER, TIME]));
    let run = rig.run(&user("both"), SimpleStreamOptions::default()).await;
    assert_contract(&run, &rig.model);
    assert_eq!(run.message.stop_reason, StopReason::ToolUse);
    let calls = run.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_tool_call(&calls[0], WEATHER);
    assert_tool_call(&calls[1], TIME);
}

/// A model's tool calls and their results are sent back on the next request.
pub async fn sends_tool_results_back(fixture: impl WireFixture) {
    let first = Rig::sse(&fixture, fixture.tool_calls(&[WEATHER, TIME]));
    let turn = first
        .run(&user("both"), SimpleStreamOptions::default())
        .await;

    let rig = Rig::sse(&fixture, fixture.text(&["done"]));
    let mut context = user("both");
    context.messages.push(Message::Assistant(turn.message));
    context.messages.push(tool_result(WEATHER, "sunny"));
    context.messages.push(tool_result(TIME, "12:00"));
    let run = rig.run(&context, SimpleStreamOptions::default()).await;
    assert_contract(&run, &rig.model);
    assert_eq!(run.text(), "done");

    let body = rig.request_body();
    let replayed = fixture.replayed_tool_calls(&body);
    let expected: Vec<_> = [WEATHER, TIME]
        .iter()
        .map(|c| {
            (
                c.0.to_string(),
                c.1.to_string(),
                serde_json::from_str(c.2).unwrap(),
            )
        })
        .collect();
    assert_eq!(replayed, expected);
    assert_eq!(
        fixture.tool_results(&body),
        [
            ("call_1".to_string(), "sunny".to_string()),
            ("call_2".to_string(), "12:00".to_string())
        ]
    );
}

/// Thinking streams as thinking blocks; its signature goes back unchanged on the
/// next request of a tool loop.
pub async fn keeps_reasoning_across_tool_calls(fixture: impl WireFixture) {
    let reasoning = SimpleStreamOptions::with_reasoning(ThinkingLevel::High);
    let rig = Rig::sse(&fixture, fixture.thinking(THINKING, "ok"));
    let run = rig.run(&user("think"), reasoning.clone()).await;
    assert_contract(&run, &rig.model);
    let thinking: Vec<&ThinkingContent> = run
        .message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Thinking(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(thinking.len(), 1, "one thinking block");
    assert_eq!(thinking[0].thinking, THINKING);
    assert_eq!(run.text(), "ok");

    // Replay the turn (as a tool loop would) and look at what goes back.
    let next = Rig::sse(&fixture, fixture.text(&["next"]));
    let mut context = user("think");
    let mut turn = run.message.clone();
    turn.content.push(AssistantContent::ToolCall(call(WEATHER)));
    turn.stop_reason = StopReason::ToolUse;
    context.messages.push(Message::Assistant(turn));
    context.messages.push(tool_result(WEATHER, "sunny"));
    next.run(&context, reasoning).await;
    let replayed = fixture.replayed_signatures(&next.request_body());
    if fixture.caps().reasoning_signatures {
        assert!(thinking[0].thinking_signature.is_some(), "signature kept");
        assert_eq!(replayed, [SIGNATURE], "signature replayed unchanged");
    } else {
        assert!(replayed.is_empty());
    }
}

/// Usage separates uncached input, cache reads and writes, output and reasoning.
pub async fn reports_usage_with_cached_tokens(fixture: impl WireFixture) {
    let rig = Rig::sse(&fixture, fixture.usage("ok"));
    let run = rig.run(&user("hi"), SimpleStreamOptions::default()).await;
    assert_contract(&run, &rig.model);
    let usage = &run.message.usage;
    let cache_write = if fixture.caps().cache_writes {
        USAGE_CACHE_WRITE
    } else {
        0
    };
    assert_eq!(usage.input, USAGE_INPUT, "input excludes cached tokens");
    assert_eq!(usage.cache_read, USAGE_CACHE_READ);
    assert_eq!(usage.cache_write, cache_write);
    assert_eq!(usage.output, USAGE_OUTPUT);
    let reasoning = fixture.caps().reasoning_tokens.then_some(USAGE_REASONING);
    assert_eq!(usage.reasoning, reasoning);
    assert_eq!(
        usage.total_tokens,
        USAGE_INPUT + USAGE_CACHE_READ + cache_write + USAGE_OUTPUT
    );
}

/// End, max tokens and tool use map to the same stop reasons everywhere.
pub async fn maps_stop_reasons(fixture: impl WireFixture) {
    for (body, expected) in [
        (fixture.text(&["a"]), StopReason::Stop),
        (fixture.max_tokens("cut"), StopReason::Length),
        (fixture.tool_calls(&[WEATHER]), StopReason::ToolUse),
    ] {
        let rig = Rig::sse(&fixture, body);
        let run = rig.run(&user("hi"), SimpleStreamOptions::default()).await;
        assert_contract(&run, &rig.model);
        assert_eq!(run.message.stop_reason, expected);
    }
}

/// HTTP failures end the stream with an error the agent can categorize.
pub async fn maps_http_errors(fixture: impl WireFixture) {
    for failure in [
        HttpFailure::Unauthorized,
        HttpFailure::Forbidden,
        HttpFailure::RateLimited,
        HttpFailure::ServerError,
        HttpFailure::ContextTooLong,
    ] {
        let (status, body) = fixture.http_error(failure);
        let rig = Rig::new(
            &fixture,
            FakeResponse::new(status, "application/json", body),
        );
        let run = rig.run(&user("hi"), SimpleStreamOptions::default()).await;
        assert!(
            matches!(
                run.events.last(),
                Some(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    ..
                })
            ),
            "{failure:?} ends with an error event"
        );
        assert_eq!(run.message.stop_reason, StopReason::Error);
        assert_eq!(run.message.api, rig.model.api);
        let text = run.message.failure().unwrap();
        assert!(!text.contains(API_KEY), "error text never carries the key");
        let info = loop_protocol::ErrorInfo::from_message(text.clone());
        match failure {
            HttpFailure::Unauthorized | HttpFailure::Forbidden => {
                assert_eq!(info.category, ErrorCategory::Auth, "{failure:?}: {text}");
            }
            HttpFailure::RateLimited => {
                assert_eq!(info.category, ErrorCategory::RateLimit, "{text}");
                assert!(info.retryable);
            }
            HttpFailure::ServerError => {
                assert_eq!(info.category, ErrorCategory::Network, "{text}");
                assert!(info.retryable);
            }
            HttpFailure::ContextTooLong => {
                assert!(is_context_overflow(&rig.model, &run.message), "{text}");
            }
        }
    }
}

/// A failure reported inside a `200` stream classifies like the HTTP status.
pub async fn maps_stream_errors(fixture: impl WireFixture) {
    let Some(body) = fixture.stream_error() else {
        assert!(!fixture.caps().stream_errors);
        return;
    };
    let rig = Rig::sse(&fixture, body);
    let run = rig.run(&user("hi"), SimpleStreamOptions::default()).await;
    assert_contract(&run, &rig.model);
    assert_eq!(run.message.stop_reason, StopReason::Error);
    let text = run.message.failure().unwrap();
    let info = loop_protocol::ErrorInfo::from_message(text.clone());
    assert_eq!(info.category, ErrorCategory::Network, "{text}");
    assert!(info.retryable);
}

/// Cancelling mid-stream ends with `aborted` and keeps what arrived.
pub async fn cancels_mid_stream(fixture: impl WireFixture) {
    let body = fixture.text(&["Hel", "lo"]);
    // Stall right after the first text delta's event.
    let stall_at = body.find("Hel").expect("first chunk in body");
    let stall_at = stall_at + body[stall_at..].find("\n\n").unwrap() + 2;
    let rig = Rig::new(
        &fixture,
        FakeResponse::new(200, "text/event-stream", body)
            .with_stall_after(stall_at, Duration::from_secs(5)),
    );
    let cancel = CancellationToken::new();
    let options = SimpleStreamOptions {
        base: StreamOptions {
            cancel: Some(cancel.clone()),
            api_key: Some(API_KEY.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut stream = rig.models.stream_simple(&rig.model, &user("hi"), options);
    let mut events = Vec::new();
    let collect = async {
        while let Some(event) = stream.next().await {
            if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
                cancel.cancel();
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(3), collect)
        .await
        .expect("cancellation ends the stream promptly");
    let run = Run {
        events,
        message: stream.result().await,
    };
    assert_contract(&run, &rig.model);
    assert_eq!(run.message.stop_reason, StopReason::Aborted);
    assert_eq!(run.text(), "Hel", "partial text is kept");
}

/// An unparseable chunk is skipped; the rest of the stream still arrives.
pub async fn skips_a_malformed_chunk(fixture: impl WireFixture) {
    let rig = Rig::sse(&fixture, fixture.malformed(&["Hel", "lo"]));
    let run = rig.run(&user("hi"), SimpleStreamOptions::default()).await;
    assert_contract(&run, &rig.model);
    assert_eq!(run.message.stop_reason, StopReason::Stop);
    assert_eq!(run.text(), "Hello");
}

/// Build a fixture adapter handle.
pub fn shared<A: loop_ai::ApiAdapter + 'static>(adapter: A) -> SharedApiAdapter {
    Arc::new(adapter)
}

/// One SSE event: `data: <json>` (with an optional `event:` line).
pub fn sse(event: Option<&str>, data: &Value) -> String {
    match event {
        Some(name) => format!("event: {name}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}
