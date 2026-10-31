//! Plumbing shared by the streaming wire adapters: sending the request, reading
//! SSE events under cancellation and an idle timeout, and building the
//! assistant message while emitting the events the agent loop consumes.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use futures::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream,
    AssistantMessageEventStreamHandle,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Model, ProviderResponse, StopReason,
    StreamOptions, TextContent, ThinkingContent, ToolCall,
};
use crate::utils::{calculate_cost, parse_streaming_json, summarize_http_error};

/// Why a stream ended early.
pub(crate) struct StreamFail {
    pub(crate) message: String,
    pub(crate) aborted: bool,
}

impl StreamFail {
    /// A failure reported to the caller as an error.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    /// The caller cancelled.
    pub(crate) fn aborted() -> Self {
        Self {
            message: "aborted".into(),
            aborted: true,
        }
    }
}

/// If no SSE chunk arrives within this window, treat the connection as stalled.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Default bound on time-to-first-byte.
const DEFAULT_TTFB_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `run` on a background task, turning an early failure into the terminal
/// `error` event.
pub(crate) fn spawn_stream<F, Fut>(model: &Model, run: F) -> AssistantMessageEventStream
where
    F: FnOnce(AssistantMessageEventStreamHandle) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), StreamFail>> + Send + 'static,
{
    let stream = create_assistant_message_event_stream();
    let handle = stream.handle();
    let model = model.clone();
    tokio::spawn(async move {
        if let Err(err) = run(handle.clone()).await {
            let mut msg = AssistantMessage::pending(&model);
            let reason = if err.aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            msg.stop_reason = reason;
            msg.error_message = Some(err.message);
            handle.push(AssistantMessageEvent::Error { reason, error: msg });
        }
    });
    stream
}

/// Fail fast when the caller cancelled before the request went out.
pub(crate) fn check_cancelled(options: &StreamOptions) -> Result<(), StreamFail> {
    match &options.cancel {
        Some(token) if token.is_cancelled() => Err(StreamFail::aborted()),
        _ => Ok(()),
    }
}

/// Add the caller's extra headers (`None` values are skipped) and a JSON
/// content type to `headers`.
pub(crate) fn merge_headers(headers: &mut HeaderMap, options: &StreamOptions) {
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(extra) = &options.headers {
        for (k, v) in extra {
            if let Some(val) = v {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(val),
                ) {
                    headers.insert(name, value);
                }
            }
        }
    }
}

/// A secret header value, kept out of `Debug` output.
pub(crate) fn secret_header(value: &str) -> Result<HeaderValue, StreamFail> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| StreamFail::new("the API key contains characters a header cannot carry"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Apply the caller's payload hook.
pub(crate) fn apply_on_payload(payload: Value, model: &Model, options: &StreamOptions) -> Value {
    match &options.on_payload {
        Some(cb) => cb(&payload, model).unwrap_or(payload),
        None => payload,
    }
}

/// Send one streaming request, bounded by the time-to-first-byte limit and the
/// caller's cancellation.
pub(crate) async fn send_request(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    payload: &Value,
    options: &StreamOptions,
) -> Result<reqwest::Response, StreamFail> {
    let req = client.post(url).headers(headers).json(payload);
    // Do not set an overall reqwest timeout here — it would abort long SSE bodies.
    // Bound only time-to-first-byte (headers) via `response_header_timeout_ms`.

    let cancel = options.cancel.clone();
    let response_fut = req.send();
    // None => default 60s; Some(0) => no limit; Some(n) => n ms.
    let ttfb: Option<Duration> = match options.response_header_timeout_ms {
        None => Some(DEFAULT_TTFB_TIMEOUT),
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    };
    let timed_out = |bound: Duration| {
        StreamFail::new(format!(
            "timed out waiting for model response ({}s); set responseHeaderTimeoutMs to 0 for long-running waits",
            bound.as_secs()
        ))
    };

    match (cancel.as_ref(), ttfb) {
        (Some(token), Some(bound)) => {
            tokio::select! {
                _ = token.cancelled() => return Err(StreamFail::aborted()),
                res = tokio::time::timeout(bound, response_fut) => match res {
                    Ok(inner) => inner,
                    Err(_) => return Err(timed_out(bound)),
                },
            }
        }
        (Some(token), None) => {
            tokio::select! {
                _ = token.cancelled() => return Err(StreamFail::aborted()),
                res = response_fut => res,
            }
        }
        (None, Some(bound)) => match tokio::time::timeout(bound, response_fut).await {
            Ok(inner) => inner,
            Err(_) => return Err(timed_out(bound)),
        },
        (None, None) => response_fut.await,
    }
    .map_err(|e| StreamFail::new(e.without_url().to_string()))
}

/// Report the response to `on_response`, and turn a failed status into the
/// one-line error every adapter uses.
pub(crate) async fn check_response(
    response: reqwest::Response,
    model: &Model,
    options: &StreamOptions,
) -> Result<reqwest::Response, StreamFail> {
    let status = response.status();
    if let Some(cb) = &options.on_response {
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        cb(
            &ProviderResponse {
                status: status.as_u16(),
                headers,
            },
            model,
        );
    }
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(StreamFail::new(summarize_http_error(
        status.as_u16(),
        &body,
    )))
}

/// What reading the next SSE event produced.
pub(crate) enum Next {
    /// One event's `event` name and `data`.
    Event { name: String, data: String },
    /// The body ended.
    End,
    /// The caller cancelled.
    Cancelled,
}

/// Read the next non-empty SSE event, honouring cancellation and the idle timeout.
pub(crate) async fn next_event<S, E>(
    events: &mut S,
    cancel: Option<&CancellationToken>,
) -> Result<Next, StreamFail>
where
    S: Stream<Item = Result<eventsource_stream::Event, E>> + Unpin,
    E: std::fmt::Display,
{
    loop {
        let next = tokio::time::timeout(SSE_IDLE_TIMEOUT, events.next());
        let next = match cancel {
            Some(token) => tokio::select! {
                _ = token.cancelled() => return Ok(Next::Cancelled),
                next = next => next,
            },
            None => next.await,
        };
        let Ok(next) = next else {
            return Err(StreamFail::new(format!(
                "server stopped responding (no data for {}s)",
                SSE_IDLE_TIMEOUT.as_secs()
            )));
        };
        let Some(event) = next else {
            return Ok(Next::End);
        };
        let event = event.map_err(|e| StreamFail::new(format!("sse error: {e}")))?;
        if event.data.trim().is_empty() {
            continue;
        }
        return Ok(Next::Event {
            name: event.event,
            data: event.data,
        });
    }
}

/// Parse an SSE `data` payload, or `None` (logged) when it is not JSON.
pub(crate) fn parse_data(data: &str) -> Option<Value> {
    match serde_json::from_str(data) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "skipping unparseable SSE chunk");
            None
        }
    }
}

/// A failure the provider reported inside a `200` stream, as the same one-line
/// error its HTTP status would give, so it classifies the same way.
pub(crate) fn stream_error(status: u16, message: &str) -> String {
    summarize_http_error(
        status,
        &json!({ "error": { "message": message } }).to_string(),
    )
}

/// Builds the assistant message block by block, emitting the matching events.
pub(crate) struct MessageBuilder {
    pub(crate) partial: AssistantMessage,
    handle: AssistantMessageEventStreamHandle,
    model: Model,
    tool_args: HashMap<usize, String>,
    open: Vec<usize>,
}

impl MessageBuilder {
    /// Start the message and emit `start`.
    pub(crate) fn start(model: &Model, handle: AssistantMessageEventStreamHandle) -> Self {
        let partial = AssistantMessage::pending(model);
        handle.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        });
        Self {
            partial,
            handle,
            model: model.clone(),
            tool_args: HashMap::new(),
            open: Vec::new(),
        }
    }

    fn push_block(&mut self, block: AssistantContent) -> usize {
        let index = self.partial.content.len();
        self.partial.content.push(block);
        self.open.push(index);
        index
    }

    /// Open a text block.
    pub(crate) fn open_text(&mut self) -> usize {
        let index = self.push_block(AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        }));
        self.handle.push(AssistantMessageEvent::TextStart {
            content_index: index,
            partial: self.partial.clone(),
        });
        index
    }

    /// Open a thinking block.
    pub(crate) fn open_thinking(&mut self, redacted: bool) -> usize {
        let index = self.push_block(AssistantContent::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: None,
            redacted: redacted.then_some(true),
        }));
        self.handle.push(AssistantMessageEvent::ThinkingStart {
            content_index: index,
            partial: self.partial.clone(),
        });
        index
    }

    /// Open a tool call.
    pub(crate) fn open_tool(&mut self, id: &str, name: &str) -> usize {
        let index = self.push_block(AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
            thought_signature: None,
        }));
        self.tool_args.insert(index, String::new());
        self.handle.push(AssistantMessageEvent::ToolcallStart {
            content_index: index,
            partial: self.partial.clone(),
        });
        index
    }

    /// Append text to block `index`.
    pub(crate) fn text_delta(&mut self, index: usize, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(AssistantContent::Text(t)) = self.partial.content.get_mut(index) {
            t.text.push_str(delta);
            self.handle.push(AssistantMessageEvent::TextDelta {
                content_index: index,
                delta: delta.into(),
                partial: self.partial.clone(),
            });
        }
    }

    /// Append thinking to block `index`.
    pub(crate) fn thinking_delta(&mut self, index: usize, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(AssistantContent::Thinking(t)) = self.partial.content.get_mut(index) {
            t.thinking.push_str(delta);
            self.handle.push(AssistantMessageEvent::ThinkingDelta {
                content_index: index,
                delta: delta.into(),
                partial: self.partial.clone(),
            });
        }
    }

    /// Append raw arguments JSON to tool call `index`.
    pub(crate) fn tool_delta(&mut self, index: usize, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let Some(raw) = self.tool_args.get_mut(&index) else {
            return;
        };
        raw.push_str(delta);
        if let Some(AssistantContent::ToolCall(call)) = self.partial.content.get_mut(index) {
            call.arguments = parse_streaming_json(raw);
        }
        self.handle.push(AssistantMessageEvent::ToolcallDelta {
            content_index: index,
            delta: delta.into(),
            partial: self.partial.clone(),
        });
    }

    /// Replace a tool call's raw arguments (providers that send them whole).
    pub(crate) fn set_tool_arguments(&mut self, index: usize, raw: &str) {
        if self.tool_args.get(&index).is_some_and(|r| r == raw) {
            return;
        }
        if let Some(buf) = self.tool_args.get_mut(&index) {
            buf.clear();
        }
        self.tool_delta(index, raw);
    }

    /// Mutable access to block `index`, e.g. to attach a signature.
    pub(crate) fn block_mut(&mut self, index: usize) -> Option<&mut AssistantContent> {
        self.partial.content.get_mut(index)
    }

    /// Close block `index` and emit its end event.
    pub(crate) fn close(&mut self, index: usize) {
        let Some(pos) = self.open.iter().position(|i| *i == index) else {
            return;
        };
        self.open.remove(pos);
        if let Some(raw) = self.tool_args.get(&index) {
            if let Some(AssistantContent::ToolCall(call)) = self.partial.content.get_mut(index) {
                call.arguments = if raw.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(raw).unwrap_or_else(|_| parse_streaming_json(raw))
                };
            }
        }
        let partial = self.partial.clone();
        let event = match &self.partial.content[index] {
            AssistantContent::Text(t) => AssistantMessageEvent::TextEnd {
                content_index: index,
                content: t.text.clone(),
                partial,
            },
            AssistantContent::Thinking(t) => AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: t.thinking.clone(),
                partial,
            },
            AssistantContent::ToolCall(call) => AssistantMessageEvent::ToolcallEnd {
                content_index: index,
                tool_call: call.clone(),
                partial,
            },
        };
        self.handle.push(event);
    }

    /// Close every open block in content order.
    pub(crate) fn close_all(&mut self) {
        let mut open = self.open.clone();
        open.sort_unstable();
        for index in open {
            self.close(index);
        }
    }

    /// Close everything, price the usage and emit the terminal event.
    pub(crate) fn finish(mut self, reason: StopReason, raw: Option<String>) {
        self.close_all();
        let usage = &mut self.partial.usage;
        usage.total_tokens = usage.input
            + usage.output
            + usage.cache_read
            + usage.cache_write
            + usage.cache_write_1h.unwrap_or(0);
        calculate_cost(&self.model, usage);
        self.partial.stop_reason = reason;
        self.partial.raw_stop_reason = raw;
        let message = self.partial;
        if reason.is_error() {
            self.handle.push(AssistantMessageEvent::Error {
                reason,
                error: message,
            });
        } else {
            self.handle
                .push(AssistantMessageEvent::Done { reason, message });
        }
    }

    /// End with an error, keeping what arrived.
    pub(crate) fn fail(mut self, message: String, raw: Option<String>) {
        self.partial.error_message = Some(message);
        self.finish(StopReason::Error, raw);
    }

    /// End as cancelled, keeping what arrived.
    pub(crate) fn abort(mut self) {
        self.partial.error_message = Some("aborted".into());
        self.finish(StopReason::Aborted, None);
    }
}
