//! Anthropic Messages (`/v1/messages`) wire adapter.
//!
//! - Prompt caching: explicit `cache_control` breakpoints on the system prompt
//!   (which also covers the tools, rendered first), the last block of the
//!   conversation and the end of the previous request, so each turn of a tool
//!   loop reads what the turn before it wrote.
//! - Thinking: adaptive thinking with an effort level on current models, a token
//!   budget on budget-era models. Thinking blocks (including redacted ones) keep
//!   their signatures and go back unchanged on later requests.

use std::sync::Arc;

use eventsource_stream::Eventsource;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Map, Value};

use crate::api::sse::{
    apply_on_payload, check_cancelled, check_response, merge_headers, next_event, parse_data,
    secret_header, send_request, spawn_stream, stream_error, MessageBuilder, Next, StreamFail,
};
use crate::api::transform_messages::transform_messages;
use crate::auth::ModelAuth;
use crate::models::ApiAdapter;
use crate::stream::{AssistantMessageEventStream, AssistantMessageEventStreamHandle};
use crate::types::{
    AssistantContent, CacheRetention, Context, ImageContent, Message, Model, SimpleStreamOptions,
    StopReason, StreamOptions, ThinkingBudgets, ThinkingLevel, ToolResultContent, Usage,
    UserContent, UserMessageContent,
};

/// `anthropic-version` header sent with every request.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// Most `cache_control` breakpoints a request may carry.
const MAX_BREAKPOINTS: usize = 4;

/// Adapter for the Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicMessagesAdapter {
    client: reqwest::Client,
}

impl Default for AnthropicMessagesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicMessagesAdapter {
    /// Create with a streaming-optimised reqwest client (connect timeout, no overall timeout).
    pub fn new() -> Self {
        Self {
            client: super::http::streaming_http_client(),
        }
    }

    /// Create with a shared client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Shared adapter handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl ApiAdapter for AnthropicMessagesAdapter {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        self.stream_inner(model, context, options, auth, Thinking::Default)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        let thinking = match options.reasoning {
            Some(level) => Thinking::Level(level, options.thinking_budgets.unwrap_or_default()),
            None => Thinking::Default,
        };
        self.stream_inner(model, context, options.base, auth, thinking)
    }
}

impl AnthropicMessagesAdapter {
    fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
        thinking: Thinking,
    ) -> AssistantMessageEventStream {
        let client = self.client.clone();
        let (model_c, context) = (model.clone(), context.clone());
        spawn_stream(model, move |handle| {
            run_stream(client, model_c, context, options, auth, thinking, handle)
        })
    }
}

/// Requested thinking for one request.
#[derive(Debug, Clone)]
enum Thinking {
    /// Leave thinking to the model's default.
    Default,
    /// Think at this level.
    Level(ThinkingLevel, ThinkingBudgets),
}

async fn run_stream(
    client: reqwest::Client,
    model: Model,
    context: Context,
    options: StreamOptions,
    auth: ModelAuth,
    thinking: Thinking,
    handle: AssistantMessageEventStreamHandle,
) -> Result<(), StreamFail> {
    check_cancelled(&options)?;

    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(model.base_url.as_str())
        .trim_end_matches('/');
    let url = format!("{base_url}/messages");
    let messages = transform_messages(&context.messages, &model);
    let payload = build_payload(&model, &context, &messages, &options, &thinking);
    let payload = apply_on_payload(payload, &model, &options);

    let mut headers = HeaderMap::new();
    merge_headers(&mut headers, &options);
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    if let Some(key) = auth.api_key.as_deref().or(options.api_key.as_deref()) {
        if !key.is_empty() {
            headers.insert(HeaderName::from_static("x-api-key"), secret_header(key)?);
        }
    }

    let response = send_request(&client, &url, headers, &payload, &options).await?;
    let response = check_response(response, &model, &options).await?;

    let mut out = MessageBuilder::start(&model, handle);
    // Anthropic block index -> content index.
    let mut blocks: Vec<Option<usize>> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut refusal: Option<String> = None;
    let mut events = response.bytes_stream().eventsource();

    loop {
        let (name, data) = match next_event(&mut events, options.cancel.as_ref()).await? {
            Next::Event { name, data } => (name, data),
            Next::End => break,
            Next::Cancelled => {
                out.abort();
                return Ok(());
            }
        };
        let Some(event) = parse_data(&data) else {
            continue;
        };
        let kind = event["type"].as_str().unwrap_or(name.as_str());
        match kind {
            "message_start" => {
                let message = &event["message"];
                if let Some(id) = message["id"].as_str() {
                    out.partial.response_id = Some(id.into());
                }
                if let Some(m) = message["model"].as_str() {
                    out.partial.response_model = Some(m.into());
                }
                apply_usage(&mut out.partial.usage, &message["usage"]);
            }
            "content_block_start" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                let block = &event["content_block"];
                let content_index = match block["type"].as_str() {
                    Some("text") => {
                        let i = out.open_text();
                        out.text_delta(i, block["text"].as_str().unwrap_or_default());
                        Some(i)
                    }
                    Some("thinking") => {
                        let i = out.open_thinking(false);
                        out.thinking_delta(i, block["thinking"].as_str().unwrap_or_default());
                        set_signature(&mut out, i, block["signature"].as_str());
                        Some(i)
                    }
                    Some("redacted_thinking") => {
                        let i = out.open_thinking(true);
                        set_signature(&mut out, i, block["data"].as_str());
                        Some(i)
                    }
                    Some("tool_use") => {
                        let i = out.open_tool(
                            block["id"].as_str().unwrap_or_default(),
                            block["name"].as_str().unwrap_or_default(),
                        );
                        // Input normally streams as JSON deltas; take it whole if it came here.
                        if block["input"].as_object().is_some_and(|o| !o.is_empty()) {
                            out.set_tool_arguments(i, &block["input"].to_string());
                        }
                        Some(i)
                    }
                    // Server-side blocks Loop doesn't request.
                    _ => None,
                };
                if blocks.len() <= index {
                    blocks.resize(index + 1, None);
                }
                blocks[index] = content_index;
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                let Some(Some(i)) = blocks.get(index).copied() else {
                    continue;
                };
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        out.text_delta(i, delta["text"].as_str().unwrap_or_default())
                    }
                    Some("thinking_delta") => {
                        out.thinking_delta(i, delta["thinking"].as_str().unwrap_or_default())
                    }
                    Some("signature_delta") => {
                        set_signature(&mut out, i, delta["signature"].as_str())
                    }
                    Some("input_json_delta") => {
                        out.tool_delta(i, delta["partial_json"].as_str().unwrap_or_default())
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                if let Some(Some(i)) = blocks.get(index).copied() {
                    out.close(i);
                }
            }
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    stop_reason = Some(reason.into());
                    if reason == "refusal" {
                        let category = event["delta"]["stop_details"]["category"].as_str();
                        refusal = Some(match category {
                            Some(c) => format!("the model declined to respond ({c})"),
                            None => "the model declined to respond".into(),
                        });
                    }
                }
                apply_usage(&mut out.partial.usage, &event["usage"]);
            }
            "error" => {
                let error = &event["error"];
                let message = error["message"].as_str().unwrap_or("stream error");
                let status = error_status(error["type"].as_str().unwrap_or_default());
                out.fail(stream_error(status, message), stop_reason);
                return Ok(());
            }
            // message_stop, ping
            _ => {}
        }
    }

    if let Some(message) = refusal {
        out.fail(message, stop_reason);
        return Ok(());
    }
    let reason = map_stop_reason(stop_reason.as_deref(), &out.partial.content);
    out.finish(reason, stop_reason);
    Ok(())
}

fn set_signature(out: &mut MessageBuilder, index: usize, signature: Option<&str>) {
    let Some(signature) = signature.filter(|s| !s.is_empty()) else {
        return;
    };
    if let Some(AssistantContent::Thinking(t)) = out.block_mut(index) {
        t.thinking_signature
            .get_or_insert_with(String::new)
            .push_str(signature);
    }
}

/// The HTTP status an in-stream error type stands for.
fn error_status(kind: &str) -> u16 {
    match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        _ => 500,
    }
}

/// `input_tokens` excludes cached tokens; writes split by TTL when reported.
fn apply_usage(usage: &mut Usage, value: &Value) {
    let field = |name: &str| value.get(name).and_then(Value::as_u64);
    if let Some(v) = field("input_tokens") {
        usage.input = v;
    }
    if let Some(v) = field("output_tokens") {
        usage.output = v;
    }
    if let Some(v) = field("cache_read_input_tokens") {
        usage.cache_read = v;
    }
    if let Some(v) = field("cache_creation_input_tokens") {
        let long = value
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // `cost.rs` prices 1h writes separately, so keep them out of `cache_write`.
        usage.cache_write = v.saturating_sub(long);
        usage.cache_write_1h = (long > 0).then_some(long);
    }
}

fn map_stop_reason(reason: Option<&str>, content: &[AssistantContent]) -> StopReason {
    match reason {
        Some("max_tokens") | Some("model_context_window_exceeded") => StopReason::Length,
        Some("tool_use") => StopReason::ToolUse,
        Some(_) => StopReason::Stop,
        None if content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_))) =>
        {
            StopReason::ToolUse
        }
        None => StopReason::Stop,
    }
}

/// Models that take a fixed thinking budget instead of adaptive thinking.
fn uses_thinking_budget(model_id: &str) -> bool {
    const BUDGET_ERA: &[&str] = &[
        "claude-haiku-4-5",
        "claude-sonnet-4-5",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-opus-4-2025",
        "claude-sonnet-4-2025",
        "claude-3",
    ];
    BUDGET_ERA.iter().any(|p| model_id.starts_with(p))
}

/// Effort for a thinking level; `xhigh` arrived with Opus 4.7.
fn effort(model_id: &str, level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh if model_id.contains("-4-6") => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

/// Token budget for a level on budget-era models.
fn thinking_budget(level: ThinkingLevel, budgets: &ThinkingBudgets) -> u32 {
    match level {
        ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024),
        ThinkingLevel::Low => budgets.low.unwrap_or(4096),
        ThinkingLevel::Medium => budgets.medium.unwrap_or(10_240),
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => {
            budgets.high.unwrap_or(32_000)
        }
    }
}

fn build_payload(
    model: &Model,
    context: &Context,
    messages: &[Message],
    options: &StreamOptions,
    thinking: &Thinking,
) -> Value {
    let mut max_tokens = options
        .max_tokens
        .map(u64::from)
        .unwrap_or(model.max_tokens)
        .max(1);
    let mut payload = json!({
        "model": model.id,
        "stream": true,
    });

    if let (true, Thinking::Level(level, budgets)) = (model.reasoning, thinking) {
        if uses_thinking_budget(&model.id) {
            // The budget must stay below max_tokens and at least 1024.
            let budget = u64::from(thinking_budget(*level, budgets)).max(1024);
            max_tokens = max_tokens.max(budget + 1024);
            payload["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        } else {
            payload["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            payload["output_config"] = json!({ "effort": effort(&model.id, *level) });
        }
    }
    payload["max_tokens"] = json!(max_tokens);

    let cache = cache_control(options.cache_retention.unwrap_or_default());
    let mut breakpoints = 0;

    if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        let mut block = json!({ "type": "text", "text": system });
        if let Some(cc) = &cache {
            // Covers the tools too: they render before the system prompt.
            block["cache_control"] = cc.clone();
            breakpoints += 1;
        }
        payload["system"] = json!([block]);
    }

    if let Some(tools) = context.tools.as_ref().filter(|t| !t.is_empty()) {
        let mut defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        if let (Some(cc), 0) = (&cache, breakpoints) {
            if let Some(last) = defs.last_mut() {
                last["cache_control"] = cc.clone();
                breakpoints += 1;
            }
        }
        payload["tools"] = Value::Array(defs);
    }

    let mut wire = convert_messages(messages, model);
    if let Some(cc) = &cache {
        mark_conversation(&mut wire, cc, MAX_BREAKPOINTS - breakpoints);
    }
    payload["messages"] = Value::Array(wire);
    payload
}

fn cache_control(retention: CacheRetention) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        CacheRetention::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

/// Breakpoints on the last block of the conversation and on the last block of
/// the previous user turn (where the previous request ended), so a tool loop
/// reads the prefix the turn before it wrote even when a turn adds many blocks.
fn mark_conversation(messages: &mut [Value], cache: &Value, budget: usize) {
    let users: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(i, _)| i)
        .collect();
    let mut targets: Vec<usize> = Vec::new();
    if let Some(last) = messages.len().checked_sub(1) {
        targets.push(last);
    }
    if users.len() >= 2 {
        targets.push(users[users.len() - 2]);
    }
    for index in targets.into_iter().take(budget) {
        let Some(blocks) = messages[index]["content"].as_array_mut() else {
            continue;
        };
        // Thinking blocks can't carry a breakpoint.
        if let Some(block) = blocks.iter_mut().rev().find(|b| {
            !matches!(
                b["type"].as_str(),
                Some("thinking") | Some("redacted_thinking")
            )
        }) {
            block["cache_control"] = cache.clone();
        }
    }
}

fn image_block(image: &ImageContent) -> Value {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": image.mime_type, "data": image.data },
    })
}

fn text_block(text: &str) -> Option<Value> {
    (!text.is_empty()).then(|| json!({ "type": "text", "text": text }))
}

/// Loop messages as Anthropic turns: tool results become `user` blocks and
/// consecutive turns of the same role merge, so all parallel results arrive
/// together.
fn convert_messages(messages: &[Message], model: &Model) -> Vec<Value> {
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in messages {
        let (role, blocks) = match message {
            Message::User(u) => {
                let blocks = match &u.content {
                    UserMessageContent::Text(t) => text_block(t).into_iter().collect(),
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            UserContent::Text(t) => text_block(&t.text),
                            UserContent::Image(img) => {
                                model.supports_images().then(|| image_block(img))
                            }
                        })
                        .collect(),
                };
                ("user", blocks)
            }
            Message::Assistant(a) => {
                let blocks = a
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(t) => text_block(&t.text),
                        AssistantContent::Thinking(t) => {
                            let signature = t.thinking_signature.as_deref()?;
                            Some(if t.redacted == Some(true) {
                                json!({ "type": "redacted_thinking", "data": signature })
                            } else {
                                json!({
                                    "type": "thinking",
                                    "thinking": t.thinking,
                                    "signature": signature,
                                })
                            })
                        }
                        AssistantContent::ToolCall(tc) => Some(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": if tc.arguments.is_object() {
                                tc.arguments.clone()
                            } else {
                                Value::Object(Map::new())
                            },
                        })),
                    })
                    .collect();
                ("assistant", blocks)
            }
            Message::ToolResult(t) => {
                let content: Vec<Value> = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => text_block(&t.text),
                        ToolResultContent::Image(img) => Some(image_block(img)),
                    })
                    .collect();
                let mut block = json!({ "type": "tool_result", "tool_use_id": t.tool_call_id });
                if !content.is_empty() {
                    block["content"] = Value::Array(content);
                }
                if t.is_error {
                    block["is_error"] = json!(true);
                }
                ("user", vec![block])
            }
        };
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => {
                // Tool results lead a user turn.
                if role == "user" && blocks[0]["type"] == "tool_result" {
                    let at = last_blocks
                        .iter()
                        .position(|b| b["type"] != "tool_result")
                        .unwrap_or(last_blocks.len());
                    last_blocks.splice(at..at, blocks);
                } else {
                    last_blocks.extend(blocks);
                }
            }
            _ => turns.push((role, blocks)),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, InputModality, ModelCost, TextContent, ThinkingContent, Tool, ToolCall,
        ToolResultMessage, API_ANTHROPIC_MESSAGES,
    };

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: API_ANTHROPIC_MESSAGES.into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost::default(),
            context_window: 1_000_000,
            max_tokens: 64_000,
            headers: None,
            compat: None,
        }
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "t".into(),
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

    fn assistant(model: &Model, content: Vec<AssistantContent>) -> Message {
        let mut a = AssistantMessage::pending(model);
        a.content = content;
        a.stop_reason = StopReason::ToolUse;
        Message::Assistant(a)
    }

    fn call(id: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "t".into(),
            arguments: json!({"a": 1}),
            thought_signature: None,
        })
    }

    fn context(messages: Vec<Message>) -> Context {
        Context {
            system_prompt: Some("sys".into()),
            messages,
            tools: Some(vec![Tool {
                name: "t".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
            }]),
        }
    }

    fn payload(
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        thinking: Thinking,
    ) -> Value {
        let messages = transform_messages(&context.messages, model);
        build_payload(model, context, &messages, options, &thinking)
    }

    fn marked(payload: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if payload["system"][0].get("cache_control").is_some() {
            out.push("system".to_string());
        }
        for t in payload["tools"].as_array().into_iter().flatten() {
            if t.get("cache_control").is_some() {
                out.push(format!("tool {}", t["name"]));
            }
        }
        for (i, m) in payload["messages"].as_array().unwrap().iter().enumerate() {
            for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
                if b.get("cache_control").is_some() {
                    out.push(format!("message {i} block {j}"));
                }
            }
        }
        out
    }

    #[test]
    fn caches_system_last_block_and_previous_turn() {
        let m = model("claude-opus-5-5");
        let ctx = context(vec![
            Message::user_text("go"),
            assistant(&m, vec![call("a"), call("b")]),
            tool_result("a", "1"),
            tool_result("b", "2"),
            assistant(&m, vec![call("c")]),
            tool_result("c", "3"),
        ]);
        let p = payload(&m, &ctx, &StreamOptions::default(), Thinking::Default);
        // user, assistant, user(2 results), assistant, user(1 result)
        assert_eq!(p["messages"].as_array().unwrap().len(), 5);
        assert_eq!(
            marked(&p),
            ["system", "message 2 block 1", "message 4 block 0"]
        );
        assert_eq!(
            p["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn caches_tools_without_a_system_prompt_and_honours_retention() {
        let m = model("claude-opus-5-5");
        let mut ctx = context(vec![Message::user_text("go")]);
        ctx.system_prompt = None;
        let long = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let p = payload(&m, &ctx, &long, Thinking::Default);
        assert_eq!(marked(&p), ["tool \"t\"", "message 0 block 0"]);
        assert_eq!(p["tools"][0]["cache_control"]["ttl"], "1h");

        let none = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        assert!(marked(&payload(&m, &ctx, &none, Thinking::Default)).is_empty());
    }

    #[test]
    fn current_models_get_adaptive_thinking_with_effort() {
        let m = model("claude-opus-5-5");
        let ctx = context(vec![Message::user_text("go")]);
        let level = Thinking::Level(ThinkingLevel::XHigh, ThinkingBudgets::default());
        let p = payload(&m, &ctx, &StreamOptions::default(), level);
        assert_eq!(
            p["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(p["output_config"], json!({"effort": "xhigh"}));
        assert!(p.get("temperature").is_none());

        // No level: leave thinking to the model; never send `disabled`.
        let p = payload(&m, &ctx, &StreamOptions::default(), Thinking::Default);
        assert!(p.get("thinking").is_none());
        assert!(p.get("output_config").is_none());

        let level = Thinking::Level(ThinkingLevel::XHigh, ThinkingBudgets::default());
        let p = payload(
            &model("claude-sonnet-4-6"),
            &ctx,
            &StreamOptions::default(),
            level,
        );
        assert_eq!(p["output_config"]["effort"], "high", "4.6 has no xhigh");
    }

    #[test]
    fn budget_era_models_get_a_budget_below_max_tokens() {
        let m = model("claude-haiku-4-5");
        let ctx = context(vec![Message::user_text("go")]);
        let options = StreamOptions {
            max_tokens: Some(2000),
            ..Default::default()
        };
        let level = Thinking::Level(
            ThinkingLevel::Medium,
            ThinkingBudgets {
                medium: Some(4000),
                ..Default::default()
            },
        );
        let p = payload(&m, &ctx, &options, level);
        assert_eq!(
            p["thinking"],
            json!({"type": "enabled", "budget_tokens": 4000})
        );
        assert_eq!(p["max_tokens"], 5024);
        assert!(p.get("output_config").is_none());
    }

    #[test]
    fn thinking_replays_unchanged_including_redacted_and_empty() {
        let m = model("claude-opus-5-5");
        let thinking = |text: &str, sig: &str, redacted: bool| {
            AssistantContent::Thinking(ThinkingContent {
                thinking: text.into(),
                thinking_signature: Some(sig.into()),
                redacted: redacted.then_some(true),
            })
        };
        let ctx = context(vec![
            Message::user_text("go"),
            assistant(
                &m,
                vec![
                    thinking("", "sig-empty", false),
                    thinking("", "opaque", true),
                    thinking("plan", "sig-1", false),
                    call("a"),
                ],
            ),
            tool_result("a", ""),
        ]);
        let p = payload(&m, &ctx, &StreamOptions::default(), Thinking::Default);
        let content = &p["messages"][1]["content"];
        assert_eq!(
            content[0],
            json!({"type": "thinking", "thinking": "", "signature": "sig-empty"})
        );
        assert_eq!(
            content[1],
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
        assert_eq!(content[2]["signature"], "sig-1");
        // An empty tool result sends no empty text block.
        let result = &p["messages"][2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert!(result.get("content").is_none());
    }

    #[test]
    fn tool_results_lead_a_merged_user_turn() {
        let m = model("claude-opus-5-5");
        let ctx = context(vec![
            Message::user_text("go"),
            assistant(&m, vec![call("a")]),
            tool_result("a", "1"),
            Message::user_text("also this"),
        ]);
        let p = payload(&m, &ctx, &StreamOptions::default(), Thinking::Default);
        let messages = p["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][1]["text"], "also this");
    }

    #[test]
    fn usage_keeps_one_hour_writes_apart() {
        let mut usage = Usage::default();
        apply_usage(
            &mut usage,
            &json!({
                "input_tokens": 10,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 30,
                "cache_creation": {"ephemeral_5m_input_tokens": 10, "ephemeral_1h_input_tokens": 20},
                "output_tokens": 1,
            }),
        );
        assert_eq!(usage.input, 10);
        assert_eq!(usage.cache_read, 5);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.cache_write_1h, Some(20));
    }
}
