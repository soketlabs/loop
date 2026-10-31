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

/// Default bound on time-to-first-byte.
const DEFAULT_TTFB_TIMEOUT: Duration = Duration::from_secs(120);

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
        if let Err(err) = until_closed(&handle, run(handle.clone())).await {
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

/// Stop the producer when its receiver disappears.
pub(crate) async fn until_closed<F>(
    handle: &AssistantMessageEventStreamHandle,
    work: F,
) -> Result<(), StreamFail>
where
    F: Future<Output = Result<(), StreamFail>>,
{
    let cancel = handle.receiver_cancel();
    tokio::select! { biased; _ = cancel.cancelled() => Ok(()), result = work => result }
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
#[derive(Clone)]
struct RequestStarted(tokio::time::Instant, u32);

pub(crate) async fn send_request(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    payload: &Value,
    options: &StreamOptions,
) -> Result<reqwest::Response, StreamFail> {
    let started = options
        .generation_started_at
        .unwrap_or_else(tokio::time::Instant::now);
    let budget = options
        .timeouts
        .generation_budget_ms
        .or(options.timeout_ms)
        .unwrap_or(0);
    let work = async {
        for attempt in 0..=2u32 {
            match send_once(client, url, headers.clone(), payload, options).await {
                Ok(mut response) => {
                    response
                        .extensions_mut()
                        .insert(RequestStarted(started, attempt + 1));
                    return Ok(response);
                }
                Err(err)
                    if !err.aborted
                        && attempt < 2
                        && !err.message.contains("timed out waiting") =>
                {
                    tracing::warn!(
                        attempt = attempt + 1,
                        "request failed before response headers; retrying"
                    );
                    let delay = tokio::time::sleep(Duration::from_secs(1 << attempt));
                    match &options.cancel {
                        Some(c) => {
                            tokio::select! { biased; _ = c.cancelled() => return Err(StreamFail::aborted()), _ = delay => {} }
                        }
                        None => delay.await,
                    }
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!()
    };
    if budget == 0 {
        work.await
    } else {
        tokio::time::timeout_at(started + Duration::from_millis(budget), work)
            .await
            .map_err(|_| StreamFail::new(format!("generation_budget timeout after {budget}ms")))?
    }
}

async fn send_once(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    payload: &Value,
    options: &StreamOptions,
) -> Result<reqwest::Response, StreamFail> {
    let owned;
    let client = if let Some(ms) = options.timeouts.connection_ms {
        owned = crate::api::http::streaming_http_client_with_connect(
            (ms > 0).then(|| Duration::from_millis(ms)),
        );
        &owned
    } else {
        client
    };
    let req = client.post(url).headers(headers).json(payload);
    // Do not set an overall reqwest timeout here — it would abort long SSE bodies.
    // Bound only time-to-first-byte (headers) via `response_header_timeout_ms`.

    let cancel = options.cancel.clone();
    let response_fut = req.send();
    // None => default 120s; Some(0) => no limit; Some(n) => n ms.
    let ttfb: Option<Duration> = match options
        .timeouts
        .response_header_ms
        .or(options.response_header_timeout_ms)
    {
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
    let body = error_body(response, options).await?;
    Err(StreamFail::new(summarize_http_error(
        status.as_u16(),
        &body,
    )))
}

/// Read a bounded error body without bypassing cancellation or generation limits.
pub(crate) async fn error_body(
    response: reqwest::Response,
    options: &StreamOptions,
) -> Result<String, StreamFail> {
    let started = options
        .generation_started_at
        .unwrap_or_else(tokio::time::Instant::now);
    let total = options
        .timeouts
        .generation_budget_ms
        .or(options.timeout_ms)
        .unwrap_or(0);
    let idle = options.timeouts.transport_idle_ms.unwrap_or(900_000);
    let work = async {
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        loop {
            let next = async {
                if idle == 0 {
                    Ok(stream.next().await)
                } else {
                    tokio::time::timeout(Duration::from_millis(idle), stream.next())
                        .await
                        .map_err(|_| {
                            StreamFail::new("transport_idle timeout reading error response")
                        })
                }
            };
            let next = match &options.cancel {
                Some(c) => {
                    tokio::select! { biased; _ = c.cancelled() => return Err(StreamFail::aborted()), next = next => next? }
                }
                None => next.await?,
            };
            match next {
                Some(Ok(bytes)) => {
                    let count = bytes.len().min(65_536 - body.len());
                    body.extend_from_slice(&bytes[..count]);
                    if body.len() == 65_536 {
                        break;
                    }
                }
                Some(Err(_)) => {
                    return Err(StreamFail::new("error reading provider error response"))
                }
                None => break,
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    };
    if total == 0 {
        work.await
    } else {
        tokio::time::timeout_at(started + Duration::from_millis(total), work)
            .await
            .map_err(|_| StreamFail::new("generation_budget timeout reading error response"))?
    }
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
        let next = match cancel {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Ok(Next::Cancelled),
                next = events.next() => next,
            },
            None => events.next().await,
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
    pub(crate) fn finish(mut self, mut reason: StopReason, raw: Option<String>) {
        if !reason.is_error()
            && self
                .tool_args
                .values()
                .any(|raw| serde_json::from_str::<Value>(raw).is_err())
        {
            reason = StopReason::Error;
            self.partial.error_message = Some("incomplete tool arguments".into());
        }
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

/// Read bytes under independent transport, semantic-progress and total deadlines.
pub(crate) fn tracked_events(
    response: reqwest::Response,
    options: &StreamOptions,
) -> impl Stream<Item = Result<eventsource_stream::Event, String>> + Unpin {
    let started = response
        .extensions()
        .get::<RequestStarted>()
        .map(|s| s.0)
        .unwrap_or_else(tokio::time::Instant::now);
    let attempt = response
        .extensions()
        .get::<RequestStarted>()
        .map(|s| s.1)
        .unwrap_or(1);
    tracked_bytes(response.bytes_stream(), options, started, attempt)
}

fn tracked_bytes<S, E>(
    bytes: S,
    options: &StreamOptions,
    started: tokio::time::Instant,
    attempt: u32,
) -> impl Stream<Item = Result<eventsource_stream::Event, String>> + Unpin
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    use eventsource_stream::Eventsource;
    use std::sync::{Arc, Mutex};
    use tokio::time::Instant;
    let policy = options.timeouts.clone();
    let total = policy
        .generation_budget_ms
        .or(options.timeout_ms)
        .unwrap_or(0);
    let transport = policy.transport_idle_ms.unwrap_or(900_000);
    let progress = policy.model_progress_idle_ms.unwrap_or(900_000);
    let headers_at = Instant::now();
    let activity = Arc::new(Mutex::new((headers_at, 0u64, 0u64)));
    let bytes_activity = activity.clone();
    let bytes = bytes.inspect(move |chunk| {
        if let Ok(bytes) = chunk {
            if !bytes.is_empty() {
                let mut state = bytes_activity.lock().expect("activity lock");
                state.0 = Instant::now();
                state.1 += bytes.len() as u64;
                state.2 += 1;
            }
        }
    });
    let cancel = options.cancel.clone();
    Box::pin(async_stream::stream! {
        let mut events = bytes.eventsource();
        let mut last_progress = headers_at;
        let mut count = 0u64;
        loop {
            let (last_byte, _, _) = *activity.lock().expect("activity lock");
            let deadline = [("transport_idle", transport, last_byte), ("model_progress_idle", progress, last_progress), ("generation_budget", total, started)]
                .into_iter().filter(|(_, ms, _)| *ms > 0)
                .map(|(kind, ms, base)| (kind, ms, base + Duration::from_millis(ms)))
                .min_by_key(|(_, _, at)| *at);
            let wait = async { match deadline { Some((_, _, at)) => tokio::time::sleep_until(at).await, None => std::future::pending().await } };
            let next = tokio::select! {
                biased;
                _ = async { match &cancel { Some(c) => c.cancelled().await, None => std::future::pending().await } } => { yield Err("aborted".into()); break; }
                next = events.next() => next,
                _ = wait => {
                    // Reading a partial SSE frame can update transport activity without yielding an event.
                    let (byte, byte_count, chunks) = *activity.lock().expect("activity lock");
                    let now = Instant::now();
                    let expired = [("transport_idle", transport, byte), ("model_progress_idle", progress, last_progress), ("generation_budget", total, started)]
                        .into_iter().find(|(_, ms, base)| *ms > 0 && now.duration_since(*base) >= Duration::from_millis(*ms));
                    if let Some((kind, threshold, _)) = expired {
                        tracing::warn!(timeout_type=kind, threshold_ms=threshold, elapsed_ms=now.duration_since(started).as_millis() as u64, last_byte_ms=now.duration_since(byte).as_millis() as u64, last_progress_ms=now.duration_since(last_progress).as_millis() as u64, byte_count, chunks, events=count, attempt, "stream deadline exceeded");
                        yield Err(format!("{kind} timeout after {threshold}ms")); break;
                    }
                    continue;
                }
            };
            match next {
                Some(Ok(event)) => {
                    count += 1;
                    if serde_json::from_str::<Value>(&event.data).is_ok_and(|v| model_progress(&v)) { last_progress = Instant::now(); }
                    yield Ok(event);
                }
                Some(Err(e)) => { yield Err(format!("sse error: {e}")); break; }
                None => break,
            }
        }
    })
}

fn model_progress(v: &Value) -> bool {
    fn nonempty(v: &Value) -> bool {
        v.as_str().is_some_and(|s| !s.is_empty())
    }
    if let Some(choices) = v["choices"].as_array() {
        return choices.iter().any(|c| {
            let d = &c["delta"];
            ["content", "reasoning_content", "reasoning"]
                .iter()
                .any(|k| nonempty(&d[k]))
                || d["tool_calls"].as_array().is_some_and(|calls| {
                    calls.iter().any(|c| {
                        nonempty(&c["id"])
                            || nonempty(&c["function"]["name"])
                            || nonempty(&c["function"]["arguments"])
                    })
                })
        });
    }
    match v["type"].as_str() {
        Some("content_block_delta") => ["text", "thinking", "partial_json"]
            .iter()
            .any(|k| nonempty(&v["delta"][k])),
        Some(
            "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta",
        ) => nonempty(&v["delta"]),
        _ => v["candidates"].as_array().is_some_and(|cs| {
            cs.iter().any(|c| {
                c["content"]["parts"].as_array().is_some_and(|ps| {
                    ps.iter()
                        .any(|p| nonempty(&p["text"]) || p.get("functionCall").is_some())
                })
            })
        }),
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    #[tokio::test(start_paused = true)]
    async fn silence_expires_without_waiting_minutes() {
        let mut options = StreamOptions::default();
        options.timeouts.transport_idle_ms = Some(100);
        let input = stream::pending::<Result<Bytes, String>>();
        let mut events = tracked_bytes(input, &options, tokio::time::Instant::now(), 1);
        assert!(events
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .contains("transport_idle"));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeats_do_not_extend_model_progress() {
        let input = stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Some((Ok::<_, String>(Bytes::from_static(b": ping\n\n")), ()))
        });
        let mut options = StreamOptions::default();
        options.timeouts.model_progress_idle_ms = Some(100);
        options.timeouts.transport_idle_ms = Some(50);
        let mut events = tracked_bytes(Box::pin(input), &options, tokio::time::Instant::now(), 1);
        assert!(events
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .contains("model_progress_idle"));
    }

    #[tokio::test(start_paused = true)]
    async fn active_reasoning_survives_old_deadline() {
        let input = stream::unfold(0, |i| async move {
            if i == 5 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(100)).await;
            Some((
                Ok::<_, String>(Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"x\"}}]}\n\n",
                )),
                i + 1,
            ))
        });
        let mut events = tracked_bytes(
            Box::pin(input),
            &StreamOptions::default(),
            tokio::time::Instant::now(),
            1,
        );
        let mut count = 0;
        while let Some(event) = events.next().await {
            event.unwrap();
            count += 1;
        }
        assert_eq!(count, 5);
    }
    #[tokio::test(start_paused = true)]
    async fn total_budget_is_not_reset_by_progress() {
        let input = stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Some((
                Ok::<_, String>(Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                )),
                (),
            ))
        });
        let mut options = StreamOptions::default();
        options.timeouts.generation_budget_ms = Some(50);
        let mut events = tracked_bytes(Box::pin(input), &options, tokio::time::Instant::now(), 1);
        loop {
            if let Err(error) = events.next().await.unwrap() {
                assert!(error.contains("generation_budget"));
                break;
            }
        }
    }

    #[test]
    fn model_override_preserves_unrelated_global_settings() {
        let mut policy = crate::types::TimeoutPolicy {
            transport_idle_ms: Some(100),
            ..Default::default()
        };
        policy.overlay(&crate::types::TimeoutPolicy {
            generation_budget_ms: Some(0),
            ..Default::default()
        });
        assert_eq!(policy.transport_idle_ms, Some(100));
        assert_eq!(policy.generation_budget_ms, Some(0));
    }
}
