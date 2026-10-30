//! OpenAI Responses (`/v1/responses`) wire adapter.
//!
//! Stateless: every request sets `store: false` and carries the whole
//! conversation. Reasoning items come back with `encrypted_content`; each is kept
//! whole (as JSON) in the thinking block's signature and replayed untouched, with
//! the function calls that followed it, so reasoning models keep their reasoning
//! across tool calls. Item ids ride along: a message's id in `text_signature`, a
//! function call's in `thought_signature`.

use std::collections::HashMap;
use std::sync::Arc;

use eventsource_stream::Eventsource;
use reqwest::header::{HeaderMap, AUTHORIZATION};
use serde_json::{json, Value};

use crate::api::sse::{
    apply_on_payload, check_cancelled, check_response, merge_headers, next_event, parse_data,
    secret_header, send_request, spawn_stream, stream_error, MessageBuilder, Next, StreamFail,
};
use crate::api::transform_messages::transform_messages;
use crate::auth::ModelAuth;
use crate::models::ApiAdapter;
use crate::stream::{AssistantMessageEventStream, AssistantMessageEventStreamHandle};
use crate::types::{
    AssistantContent, Context, ImageContent, Message, Model, SimpleStreamOptions, StopReason,
    StreamOptions, ThinkingLevel, ToolResultContent, Usage, UserContent, UserMessageContent,
};

/// Adapter for the OpenAI Responses API.
#[derive(Debug, Clone)]
pub struct OpenAIResponsesAdapter {
    client: reqwest::Client,
}

impl Default for OpenAIResponsesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAIResponsesAdapter {
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

impl ApiAdapter for OpenAIResponsesAdapter {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        self.stream_inner(model, context, options, auth, None)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        self.stream_inner(model, context, options.base, auth, options.reasoning)
    }
}

impl OpenAIResponsesAdapter {
    fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
        reasoning: Option<ThinkingLevel>,
    ) -> AssistantMessageEventStream {
        let client = self.client.clone();
        let (model_c, context) = (model.clone(), context.clone());
        spawn_stream(model, move |handle| {
            run_stream(client, model_c, context, options, auth, reasoning, handle)
        })
    }
}

async fn run_stream(
    client: reqwest::Client,
    model: Model,
    context: Context,
    options: StreamOptions,
    auth: ModelAuth,
    reasoning: Option<ThinkingLevel>,
    handle: AssistantMessageEventStreamHandle,
) -> Result<(), StreamFail> {
    check_cancelled(&options)?;

    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(model.base_url.as_str())
        .trim_end_matches('/');
    let url = format!("{base_url}/responses");
    let messages = transform_messages(&context.messages, &model);
    let payload = build_payload(&model, &context, &messages, &options, reasoning);
    let payload = apply_on_payload(payload, &model, &options);

    let mut headers = HeaderMap::new();
    merge_headers(&mut headers, &options);
    if let Some(key) = auth.api_key.as_deref().or(options.api_key.as_deref()) {
        if !key.is_empty() {
            headers.insert(AUTHORIZATION, secret_header(&format!("Bearer {key}"))?);
        }
    }

    let response = send_request(&client, &url, headers, &payload, &options).await?;
    let response = check_response(response, &model, &options).await?;

    let mut out = MessageBuilder::start(&model, handle);
    // Output index -> content index.
    let mut items: HashMap<u64, usize> = HashMap::new();
    let mut finish: Option<(StopReason, Option<String>)> = None;
    let mut events = response.bytes_stream().eventsource();

    loop {
        let data = match next_event(&mut events, options.cancel.as_ref()).await? {
            Next::Event { data, .. } => data,
            Next::End => break,
            Next::Cancelled => {
                out.abort();
                return Ok(());
            }
        };
        if data.trim() == "[DONE]" {
            break;
        }
        let Some(event) = parse_data(&data) else {
            continue;
        };
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        match event["type"].as_str().unwrap_or_default() {
            "response.created" => {
                let response = &event["response"];
                if let Some(id) = response["id"].as_str() {
                    out.partial.response_id = Some(id.into());
                }
                if let Some(m) = response["model"].as_str() {
                    out.partial.response_model = Some(m.into());
                }
            }
            "response.output_item.added" => {
                let item = &event["item"];
                let index = match item["type"].as_str() {
                    Some("message") => {
                        let i = out.open_text();
                        if let Some(AssistantContent::Text(t)) = out.block_mut(i) {
                            t.text_signature = item["id"].as_str().map(String::from);
                        }
                        i
                    }
                    Some("reasoning") => out.open_thinking(false),
                    Some("function_call") => {
                        let i = out.open_tool(
                            item["call_id"].as_str().unwrap_or_default(),
                            item["name"].as_str().unwrap_or_default(),
                        );
                        if let Some(AssistantContent::ToolCall(call)) = out.block_mut(i) {
                            call.thought_signature = item["id"].as_str().map(String::from);
                        }
                        i
                    }
                    _ => continue,
                };
                items.insert(output_index, index);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(&i) = items.get(&output_index) {
                    out.text_delta(i, event["delta"].as_str().unwrap_or_default());
                }
            }
            "response.reasoning_summary_part.added" => {
                // Separate summary parts with a blank line.
                if let (Some(&i), Some(part)) =
                    (items.get(&output_index), event["summary_index"].as_u64())
                {
                    if part > 0 {
                        out.thinking_delta(i, "\n\n");
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(&i) = items.get(&output_index) {
                    out.thinking_delta(i, event["delta"].as_str().unwrap_or_default());
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(&i) = items.get(&output_index) {
                    out.tool_delta(i, event["delta"].as_str().unwrap_or_default());
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                let Some(&i) = items.get(&output_index) else {
                    continue;
                };
                match item["type"].as_str() {
                    Some("reasoning") => {
                        if let Some(AssistantContent::Thinking(t)) = out.block_mut(i) {
                            t.thinking_signature = Some(reasoning_item(item).to_string());
                        }
                    }
                    Some("function_call") => {
                        if let Some(args) = item["arguments"].as_str() {
                            out.set_tool_arguments(i, args);
                        }
                    }
                    _ => {}
                }
                out.close(i);
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                apply_usage(&mut out.partial.usage, &response["usage"]);
                let incomplete = response["incomplete_details"]["reason"].as_str();
                let has_calls = out
                    .partial
                    .content
                    .iter()
                    .any(|c| matches!(c, AssistantContent::ToolCall(_)));
                let reason = match incomplete {
                    Some("max_output_tokens") => StopReason::Length,
                    _ if has_calls => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
                let raw = incomplete.or(response["status"].as_str()).map(String::from);
                finish = Some((reason, raw));
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                out.fail(failure(error), Some("failed".into()));
                return Ok(());
            }
            "error" => {
                let error = if event["error"].is_object() {
                    &event["error"]
                } else {
                    &event
                };
                out.fail(failure(error), None);
                return Ok(());
            }
            _ => {}
        }
    }

    match finish {
        Some((reason, raw)) => out.finish(reason, raw),
        None => out.fail("stream ended before the response completed".into(), None),
    }
    Ok(())
}

/// The reasoning item to replay: what the API sent, minus streaming-only fields.
fn reasoning_item(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(obj) = item.as_object_mut() {
        obj.remove("status");
    }
    item
}

/// One-line error for a failure inside the stream, with an HTTP-equivalent status.
fn failure(error: &Value) -> String {
    let code = error["code"].as_str().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or("the response failed");
    let status = if code.contains("rate_limit") {
        429
    } else if code.contains("server_error") || code.contains("internal") {
        500
    } else if code.contains("overloaded") || code.contains("unavailable") {
        503
    } else {
        400
    };
    let message = if code.is_empty() {
        message.to_string()
    } else {
        format!("{code}: {message}")
    };
    stream_error(status, &message)
}

/// `input_tokens` includes cached tokens; [`Usage::input`] counts only the uncached ones.
fn apply_usage(usage: &mut Usage, value: &Value) {
    let field = |path: &str| value.pointer(path).and_then(Value::as_u64);
    if let Some(v) = field("/input_tokens_details/cached_tokens") {
        usage.cache_read = v;
    }
    if let Some(v) = field("/input_tokens") {
        usage.input = v.saturating_sub(usage.cache_read);
    }
    if let Some(v) = field("/output_tokens") {
        usage.output = v;
    }
    if let Some(v) = field("/output_tokens_details/reasoning_tokens") {
        usage.reasoning = Some(v);
    }
}

fn effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn build_payload(
    model: &Model,
    context: &Context,
    messages: &[Message],
    options: &StreamOptions,
    reasoning: Option<ThinkingLevel>,
) -> Value {
    let mut payload = json!({
        "model": model.id,
        "input": convert_messages(messages),
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        payload["instructions"] = json!(system);
    }
    if let Some(max) = options.max_tokens {
        payload["max_output_tokens"] = json!(max);
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(tools) = context.tools.as_ref().filter(|t| !t.is_empty()) {
        payload["tools"] = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": false,
                })
            })
            .collect();
    }
    if let (true, Some(level)) = (model.reasoning, reasoning) {
        payload["reasoning"] = json!({ "effort": effort(level), "summary": "auto" });
    }
    if let Some(session_id) = &options.session_id {
        payload["prompt_cache_key"] = json!(session_id);
    }
    payload
}

fn image_url(image: &ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data)
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message {
            Message::User(u) => {
                let content: Vec<Value> = match &u.content {
                    UserMessageContent::Text(t) => vec![json!({"type": "input_text", "text": t})],
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContent::Text(t) => json!({"type": "input_text", "text": t.text}),
                            UserContent::Image(img) => {
                                json!({"type": "input_image", "image_url": image_url(img)})
                            }
                        })
                        .collect(),
                };
                input.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        AssistantContent::Thinking(t) => {
                            // Only reasoning items this API produced can go back.
                            if let Some(item) = t
                                .thinking_signature
                                .as_deref()
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                .filter(|v| v["type"] == "reasoning")
                            {
                                input.push(item);
                            }
                        }
                        AssistantContent::Text(t) if !t.text.is_empty() => {
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "status": "completed",
                                "content": [{"type": "output_text", "text": t.text, "annotations": []}],
                            });
                            if let Some(id) =
                                t.text_signature.as_deref().filter(|s| s.starts_with("msg"))
                            {
                                item["id"] = json!(id);
                            }
                            input.push(item);
                        }
                        AssistantContent::Text(_) => {}
                        AssistantContent::ToolCall(tc) => {
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": tc.id,
                                "name": tc.name,
                                "arguments": tc.arguments.to_string(),
                            });
                            if let Some(id) = tc
                                .thought_signature
                                .as_deref()
                                .filter(|s| s.starts_with("fc"))
                            {
                                item["id"] = json!(id);
                            }
                            input.push(item);
                        }
                    }
                }
            }
            Message::ToolResult(t) => {
                let has_images = t
                    .content
                    .iter()
                    .any(|c| matches!(c, ToolResultContent::Image(_)));
                let output = if has_images {
                    Value::Array(
                        t.content
                            .iter()
                            .map(|c| match c {
                                ToolResultContent::Text(t) => {
                                    json!({"type": "input_text", "text": t.text})
                                }
                                ToolResultContent::Image(img) => {
                                    json!({"type": "input_image", "image_url": image_url(img)})
                                }
                            })
                            .collect(),
                    )
                } else {
                    json!(t
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ToolResultContent::Text(t) => Some(t.text.as_str()),
                            ToolResultContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"))
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": t.tool_call_id,
                    "output": output,
                }));
            }
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, InputModality, ModelCost, TextContent, ThinkingContent, ToolCall,
        API_OPENAI_RESPONSES,
    };

    fn model() -> Model {
        Model {
            id: "gpt-test".into(),
            name: "gpt-test".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 400_000,
            max_tokens: 128_000,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn stateless_request_asks_for_encrypted_reasoning() {
        let context = Context {
            system_prompt: Some("sys".into()),
            messages: vec![Message::user_text("hi")],
            tools: None,
        };
        let options = StreamOptions {
            session_id: Some("s-1".into()),
            ..Default::default()
        };
        let p = build_payload(
            &model(),
            &context,
            &context.messages,
            &options,
            Some(ThinkingLevel::High),
        );
        assert_eq!(p["store"], false);
        assert_eq!(p["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(p["instructions"], "sys");
        assert_eq!(p["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(p["prompt_cache_key"], "s-1");
    }

    #[test]
    fn replays_reasoning_and_item_ids_in_order() {
        let item =
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
        let mut a = AssistantMessage::pending(&model());
        a.content = vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some(item.to_string()),
                redacted: None,
            }),
            AssistantContent::Text(TextContent {
                text: "checking".into(),
                text_signature: Some("msg_1".into()),
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "t".into(),
                arguments: json!({"a": 1}),
                thought_signature: Some("fc_1".into()),
            }),
        ];
        let input = convert_messages(&[Message::Assistant(a)]);
        assert_eq!(input[0], item);
        assert_eq!(input[1]["id"], "msg_1");
        assert_eq!(input[2]["id"], "fc_1");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["arguments"], r#"{"a":1}"#);
    }
}
