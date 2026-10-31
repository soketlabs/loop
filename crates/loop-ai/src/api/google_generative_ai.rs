//! Gemini API (`models/{id}:streamGenerateContent?alt=sse`) wire adapter.
//!
//! API keys only, sent in the `x-goog-api-key` header (never the URL, which would
//! end up in error messages and logs). Thought signatures arrive on any part; each
//! is kept on the block it came with (thinking, text or tool call) and sent back on
//! the same part, so thinking models keep their reasoning across function calls.

use std::sync::Arc;

use eventsource_stream::Eventsource;
use reqwest::header::{HeaderMap, HeaderName};
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
    StreamOptions, ThinkingBudgets, ThinkingLevel, ToolResultContent, Usage, UserContent,
    UserMessageContent,
};

/// Prefix of tool-call ids Loop makes up when Gemini sends none.
const LOCAL_CALL_PREFIX: &str = "gemini_call_";

/// Signature Gemini 3 accepts on a function call that has none of its own (one
/// made by another model, for instance).
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Adapter for the Gemini API.
#[derive(Debug, Clone)]
pub struct GoogleGenerativeAiAdapter {
    client: reqwest::Client,
}

impl Default for GoogleGenerativeAiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleGenerativeAiAdapter {
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

impl ApiAdapter for GoogleGenerativeAiAdapter {
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
        let thinking = options
            .reasoning
            .map(|level| (level, options.thinking_budgets.unwrap_or_default()));
        self.stream_inner(model, context, options.base, auth, thinking)
    }
}

impl GoogleGenerativeAiAdapter {
    fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
        thinking: Option<(ThinkingLevel, ThinkingBudgets)>,
    ) -> AssistantMessageEventStream {
        let client = self.client.clone();
        let (model_c, context) = (model.clone(), context.clone());
        spawn_stream(model, move |handle| {
            run_stream(client, model_c, context, options, auth, thinking, handle)
        })
    }
}

/// The block that streamed parts are being appended to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Thinking(usize),
    Text(usize),
}

async fn run_stream(
    client: reqwest::Client,
    model: Model,
    context: Context,
    options: StreamOptions,
    auth: ModelAuth,
    thinking: Option<(ThinkingLevel, ThinkingBudgets)>,
    handle: AssistantMessageEventStreamHandle,
) -> Result<(), StreamFail> {
    check_cancelled(&options)?;

    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(model.base_url.as_str())
        .trim_end_matches('/');
    let url = format!(
        "{base_url}/models/{}:streamGenerateContent?alt=sse",
        model.id
    );
    let messages = transform_messages(&context.messages, &model);
    let payload = build_payload(&model, &context, &messages, &options, thinking.as_ref());
    let payload = apply_on_payload(payload, &model, &options);

    let mut headers = HeaderMap::new();
    merge_headers(&mut headers, &options);
    if let Some(key) = auth.api_key.as_deref().or(options.api_key.as_deref()) {
        if !key.is_empty() {
            headers.insert(
                HeaderName::from_static("x-goog-api-key"),
                secret_header(key)?,
            );
        }
    }

    let response = send_request(&client, &url, headers, &payload, &options).await?;
    let response = check_response(response, &model, &options).await?;

    let mut out = MessageBuilder::start(&model, handle);
    let mut open: Option<Open> = None;
    let mut finish: Option<String> = None;
    let mut calls = 0usize;
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
        let Some(chunk) = parse_data(&data) else {
            continue;
        };
        if let Some(error) = chunk.get("error") {
            let status = error["code"].as_u64().unwrap_or(500) as u16;
            let message = error["message"].as_str().unwrap_or("stream error");
            out.fail(stream_error(status, message), None);
            return Ok(());
        }
        if let Some(id) = chunk["responseId"].as_str() {
            out.partial.response_id = Some(id.into());
        }
        if let Some(version) = chunk["modelVersion"].as_str() {
            out.partial.response_model = Some(version.into());
        }
        if chunk.get("usageMetadata").is_some() {
            apply_usage(&mut out.partial.usage, &chunk["usageMetadata"]);
        }
        if let Some(reason) = chunk["promptFeedback"]["blockReason"].as_str() {
            out.fail(
                format!("the prompt was blocked ({reason})"),
                Some(reason.into()),
            );
            return Ok(());
        }
        let candidate = &chunk["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let signature = part["thoughtSignature"].as_str();
            if let Some(call) = part.get("functionCall") {
                if let Some(Open::Text(i) | Open::Thinking(i)) = open.take() {
                    out.close(i);
                }
                calls += 1;
                let id = call["id"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("{LOCAL_CALL_PREFIX}{calls}"));
                let i = out.open_tool(&id, call["name"].as_str().unwrap_or_default());
                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                out.set_tool_arguments(i, &args.to_string());
                if let Some(AssistantContent::ToolCall(c)) = out.block_mut(i) {
                    c.thought_signature = signature.map(String::from);
                }
                out.close(i);
                continue;
            }
            let Some(text) = part["text"].as_str() else {
                continue;
            };
            if part["thought"].as_bool() == Some(true) {
                let i = match open {
                    Some(Open::Thinking(i)) => i,
                    other => {
                        if let Some(Open::Text(i)) = other {
                            out.close(i);
                        }
                        let i = out.open_thinking(false);
                        open = Some(Open::Thinking(i));
                        i
                    }
                };
                out.thinking_delta(i, text);
                if let (Some(sig), Some(AssistantContent::Thinking(t))) =
                    (signature, out.block_mut(i))
                {
                    t.thinking_signature = Some(sig.into());
                }
            } else {
                // A signature may come alone on an empty text part.
                if text.is_empty() && signature.is_none() {
                    continue;
                }
                let i = match open {
                    Some(Open::Text(i)) => i,
                    other => {
                        if let Some(Open::Thinking(i)) = other {
                            out.close(i);
                        }
                        let i = out.open_text();
                        open = Some(Open::Text(i));
                        i
                    }
                };
                out.text_delta(i, text);
                if let (Some(sig), Some(AssistantContent::Text(t))) = (signature, out.block_mut(i))
                {
                    t.text_signature = Some(sig.into());
                }
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            finish = Some(reason.into());
        }
    }

    let has_calls = calls > 0;
    match finish.as_deref() {
        Some("STOP") | Some("FINISH_REASON_UNSPECIFIED") | None if has_calls => {
            out.finish(StopReason::ToolUse, finish)
        }
        Some("STOP") => out.finish(StopReason::Stop, finish),
        Some("MAX_TOKENS") => out.finish(StopReason::Length, finish),
        None => out.fail("stream ended before the response completed".into(), None),
        Some(other) => {
            let message = format!("the response stopped early ({other})");
            out.fail(message, finish);
        }
    }
    Ok(())
}

/// `promptTokenCount` includes cached tokens; thoughts are billed as output.
fn apply_usage(usage: &mut Usage, value: &Value) {
    let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let thoughts = field("thoughtsTokenCount");
    usage.cache_read = field("cachedContentTokenCount");
    usage.input = (field("promptTokenCount") + field("toolUsePromptTokenCount"))
        .saturating_sub(usage.cache_read);
    usage.output = field("candidatesTokenCount") + thoughts;
    usage.reasoning = (thoughts > 0).then_some(thoughts);
}

/// Gemini 3 and later take a thinking level; 2.x takes a token budget.
fn uses_thinking_level(model_id: &str) -> bool {
    !model_id.starts_with("gemini-2") && !model_id.starts_with("gemini-1")
}

fn thinking_config(model: &Model, level: ThinkingLevel, budgets: &ThinkingBudgets) -> Value {
    if uses_thinking_level(&model.id) {
        let level = match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low | ThinkingLevel::Medium => "low",
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => "high",
        };
        return json!({ "includeThoughts": true, "thinkingLevel": level });
    }
    let budget: i64 = match level {
        ThinkingLevel::Minimal => budgets.minimal.map_or(1024, i64::from),
        ThinkingLevel::Low => budgets.low.map_or(4096, i64::from),
        ThinkingLevel::Medium => budgets.medium.map_or(8192, i64::from),
        ThinkingLevel::High => budgets.high.map_or(24_576, i64::from),
        // Dynamic: the model decides.
        ThinkingLevel::XHigh | ThinkingLevel::Max => -1,
    };
    json!({ "includeThoughts": true, "thinkingBudget": budget })
}

fn build_payload(
    model: &Model,
    context: &Context,
    messages: &[Message],
    options: &StreamOptions,
    thinking: Option<&(ThinkingLevel, ThinkingBudgets)>,
) -> Value {
    let mut payload = json!({ "contents": convert_messages(messages, model) });
    if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        payload["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if let Some(tools) = context.tools.as_ref().filter(|t| !t.is_empty()) {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parametersJsonSchema": t.parameters,
                })
            })
            .collect();
        payload["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    let mut config = serde_json::Map::new();
    if let Some(max) = options.max_tokens {
        config.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(temperature) = options.temperature {
        config.insert("temperature".into(), json!(temperature));
    }
    if let (true, Some((level, budgets))) = (model.reasoning, thinking) {
        config.insert(
            "thinkingConfig".into(),
            thinking_config(model, *level, budgets),
        );
    }
    if !config.is_empty() {
        payload["generationConfig"] = Value::Object(config);
    }
    payload
}

fn inline_data(image: &ImageContent) -> Value {
    json!({ "inlineData": { "mimeType": image.mime_type, "data": image.data } })
}

fn with_signature(mut part: Value, signature: Option<&str>) -> Value {
    if let Some(sig) = signature {
        part["thoughtSignature"] = json!(sig);
    }
    part
}

/// Loop messages as Gemini contents; tool results are `user` function responses
/// and consecutive turns of the same role merge.
fn convert_messages(messages: &[Message], model: &Model) -> Vec<Value> {
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in messages {
        let (role, parts): (&'static str, Vec<Value>) = match message {
            Message::User(u) => {
                let parts = match &u.content {
                    UserMessageContent::Text(t) => (!t.is_empty())
                        .then(|| json!({ "text": t }))
                        .into_iter()
                        .collect(),
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            UserContent::Text(t) => {
                                (!t.text.is_empty()).then(|| json!({ "text": t.text }))
                            }
                            UserContent::Image(img) => Some(inline_data(img)),
                        })
                        .collect(),
                };
                ("user", parts)
            }
            Message::Assistant(a) => {
                // Gemini 3 validates the signature on a turn's first function call.
                let mut first_call = true;
                let parts = a
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(t) => {
                            let sig = t.thinking_signature.as_deref()?;
                            Some(with_signature(
                                json!({ "text": t.thinking, "thought": true }),
                                Some(sig),
                            ))
                        }
                        AssistantContent::Text(t) => {
                            if t.text.is_empty() && t.text_signature.is_none() {
                                return None;
                            }
                            Some(with_signature(
                                json!({ "text": t.text }),
                                t.text_signature.as_deref(),
                            ))
                        }
                        AssistantContent::ToolCall(tc) => {
                            let mut call = json!({ "name": tc.name, "args": tc.arguments });
                            if !tc.id.starts_with(LOCAL_CALL_PREFIX) {
                                call["id"] = json!(tc.id);
                            }
                            let needs_one = first_call && uses_thinking_level(&model.id);
                            first_call = false;
                            let signature = tc
                                .thought_signature
                                .as_deref()
                                .or(needs_one.then_some(SKIP_SIGNATURE));
                            Some(with_signature(json!({ "functionCall": call }), signature))
                        }
                    })
                    .collect();
                ("model", parts)
            }
            Message::ToolResult(t) => {
                let text = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let key = if t.is_error { "error" } else { "output" };
                let mut response = json!({ "name": t.tool_name, "response": { key: text } });
                if !t.tool_call_id.starts_with(LOCAL_CALL_PREFIX) {
                    response["id"] = json!(t.tool_call_id);
                }
                let mut parts = vec![json!({ "functionResponse": response })];
                parts.extend(t.content.iter().filter_map(|c| match c {
                    ToolResultContent::Image(img) => Some(inline_data(img)),
                    ToolResultContent::Text(_) => None,
                }));
                ("user", parts)
            }
        };
        if parts.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(parts),
            _ => turns.push((role, parts)),
        }
    }
    turns
        .into_iter()
        .map(|(role, parts)| json!({ "role": role, "parts": parts }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputModality, ModelCost, API_GOOGLE_GENERATIVE_AI};

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            provider: "gemini".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn thinking_takes_a_level_on_gemini_3_and_a_budget_on_2_5() {
        let budgets = ThinkingBudgets::default();
        assert_eq!(
            thinking_config(
                &model("gemini-3-pro-preview"),
                ThinkingLevel::High,
                &budgets
            ),
            json!({"includeThoughts": true, "thinkingLevel": "high"})
        );
        assert_eq!(
            thinking_config(&model("gemini-2.5-flash"), ThinkingLevel::Low, &budgets),
            json!({"includeThoughts": true, "thinkingBudget": 4096})
        );
    }

    #[test]
    fn unsigned_calls_get_the_skip_signature_on_gemini_3_only() {
        let mut a = crate::types::AssistantMessage::pending(&model("x"));
        a.content = vec![AssistantContent::ToolCall(crate::types::ToolCall {
            id: "gemini_call_1".into(),
            name: "t".into(),
            arguments: json!({}),
            thought_signature: None,
        })];
        let messages = [Message::Assistant(a)];
        let parts = &convert_messages(&messages, &model("gemini-3-flash"))[0]["parts"];
        assert_eq!(parts[0]["thoughtSignature"], SKIP_SIGNATURE);
        assert!(parts[0]["functionCall"].get("id").is_none());
        let parts = &convert_messages(&messages, &model("gemini-2.5-pro"))[0]["parts"];
        assert!(parts[0].get("thoughtSignature").is_none());
    }
}
