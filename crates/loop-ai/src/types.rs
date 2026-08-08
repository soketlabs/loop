//! Core domain types for the unified LLM API.
//!
//! All types are serde-friendly so a [`Context`] can be persisted and handed
//! off between models mid-session.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Known wire-protocol identifiers. Custom APIs may use any string.
pub const API_OPENAI_COMPLETIONS: &str = "openai-completions";

/// Thinking / reasoning effort levels used by [`SimpleStreamOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Minimal reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
    /// Extra-high reasoning.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum reasoning.
    Max,
}

/// Model-facing thinking level including an explicit off state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelThinkingLevel {
    /// Thinking disabled.
    Off,
    /// Minimal reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
    /// Extra-high reasoning.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum reasoning.
    Max,
}

impl From<ThinkingLevel> for ModelThinkingLevel {
    fn from(level: ThinkingLevel) -> Self {
        match level {
            ThinkingLevel::Minimal => Self::Minimal,
            ThinkingLevel::Low => Self::Low,
            ThinkingLevel::Medium => Self::Medium,
            ThinkingLevel::High => Self::High,
            ThinkingLevel::XHigh => Self::XHigh,
            ThinkingLevel::Max => Self::Max,
        }
    }
}

/// Maps model thinking levels to provider-specific wire values.
pub type ThinkingLevelMap = HashMap<ModelThinkingLevel, Option<String>>;

/// Token budgets for thinking levels (token-based providers).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    /// Budget for minimal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u32>,
    /// Budget for low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<u32>,
    /// Budget for medium.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medium: Option<u32>,
    /// Budget for high.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<u32>,
}

/// Prompt-cache retention preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    /// No caching preference.
    None,
    /// Short retention (default).
    #[default]
    Short,
    /// Long retention.
    Long,
}

/// Preferred transport for providers that support multiple transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Server-sent events.
    Sse,
    /// WebSocket.
    Websocket,
    /// Cached WebSocket session.
    WebsocketCached,
    /// Let the provider choose.
    #[default]
    Auto,
}

/// Session-affinity header format for prompt-cache / sticky routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAffinityFormat {
    /// OpenAI-style session affinity.
    Openai,
    /// OpenAI without session id.
    OpenaiNosession,
    /// OpenRouter-style affinity.
    Openrouter,
}

/// Input modality supported by a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    /// Plain text.
    Text,
    /// Images (vision).
    Image,
}

/// Why an assistant turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// Stream still in progress.
    Pending,
    /// Natural stop.
    Stop,
    /// Hit max tokens / length limit.
    Length,
    /// Model requested tool use (success for agent loops).
    ToolUse,
    /// Provider or runtime error.
    Error,
    /// Caller aborted the request.
    Aborted,
}

impl StopReason {
    /// Whether this reason terminates a successful stream (`done` event).
    pub fn is_done(self) -> bool {
        matches!(self, Self::Stop | Self::Length | Self::ToolUse)
    }

    /// Whether this reason terminates with an `error` event.
    pub fn is_error(self) -> bool {
        matches!(self, Self::Error | Self::Aborted)
    }
}

/// Dollar cost breakdown for a completion.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// Input token cost.
    pub input: f64,
    /// Output token cost.
    pub output: f64,
    /// Cache-read cost.
    pub cache_read: f64,
    /// Cache-write cost.
    pub cache_write: f64,
    /// Total cost.
    pub total: f64,
}

/// Token usage and cost for an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Input / prompt tokens.
    pub input: u64,
    /// Output / completion tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Optional 1h cache-write tokens (Anthropic-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    /// Reasoning tokens (subset of output when reported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// Total tokens.
    pub total_tokens: u64,
    /// Dollar cost.
    pub cost: Cost,
}

impl Usage {
    /// Empty usage with zeroed cost.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Plain text content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    /// Text payload.
    pub text: String,
    /// Optional provider signature for replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// Thinking / reasoning content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    /// Thinking text.
    pub thinking: String,
    /// Optional signature for replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// Whether content was redacted by safety filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

/// Base64 image content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type (e.g. `image/png`).
    pub mime_type: String,
}

/// Tool call content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Provider-assigned call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Parsed arguments object.
    pub arguments: Value,
    /// Optional thought signature (Google-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Assistant content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    /// Text.
    Text(TextContent),
    /// Thinking.
    Thinking(ThinkingContent),
    /// Tool call.
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}

/// User content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContent {
    /// Text.
    Text(TextContent),
    /// Image.
    Image(ImageContent),
}

/// Tool-result content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolResultContent {
    /// Text.
    Text(TextContent),
    /// Image.
    Image(ImageContent),
}

/// User message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    /// Content: plain string or multimodal blocks.
    #[serde(with = "user_content_serde")]
    pub content: UserMessageContent,
    /// Unix epoch milliseconds.
    pub timestamp: i64,
}

/// User message content variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserMessageContent {
    /// Plain string.
    Text(String),
    /// Multimodal blocks.
    Blocks(Vec<UserContent>),
}

mod user_content_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &UserMessageContent,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<UserMessageContent, D::Error> {
        UserMessageContent::deserialize(deserializer)
    }
}

/// Assistant message produced by a model turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    /// Content blocks.
    pub content: Vec<AssistantContent>,
    /// Wire API used.
    pub api: String,
    /// Provider id.
    pub provider: String,
    /// Requested model id.
    pub model: String,
    /// Response model id if different.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Provider response id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Token usage and cost.
    pub usage: Usage,
    /// Stop reason.
    pub stop_reason: StopReason,
    /// Error message when `stop_reason` is error/aborted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Raw provider stop reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    /// Unix epoch milliseconds.
    pub timestamp: i64,
}

impl AssistantMessage {
    /// Create a pending assistant skeleton for streaming.
    pub fn pending(model: &Model) -> Self {
        Self {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: StopReason::Pending,
            error_message: None,
            raw_stop_reason: None,
            timestamp: crate::utils::id::now_ms(),
        }
    }
}

/// Tool result message appended after executing a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    /// Matching tool call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Result content.
    pub content: Vec<ToolResultContent>,
    /// Optional structured details for the agent host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Optional usage contributed by the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Deferred tool names discovered by this result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Unix epoch milliseconds.
    pub timestamp: i64,
}

/// Conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    /// User turn.
    User(UserMessage),
    /// Assistant turn.
    Assistant(AssistantMessage),
    /// Tool result.
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

impl Message {
    /// Convenience: user text message.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User(UserMessage {
            content: UserMessageContent::Text(text.into()),
            timestamp: crate::utils::id::now_ms(),
        })
    }

    /// Role string for debugging.
    pub fn role(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult(_) => "toolResult",
        }
    }
}

/// Tool definition passed to the model (JSON Schema parameters).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// Human description.
    pub description: String,
    /// JSON Schema object for parameters.
    pub parameters: Value,
}

/// Serializable conversation context shared across models.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    /// Optional system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Transcript.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Available tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

/// Pricing rates in $/million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    /// Input rate.
    pub input: f64,
    /// Output rate.
    pub output: f64,
    /// Cache-read rate.
    pub cache_read: f64,
    /// Cache-write rate.
    pub cache_write: f64,
}

impl Default for ModelCostRates {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }
}

/// Pricing tier selected when total input exceeds a threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    /// Apply when total input usage exceeds this count.
    pub input_tokens_above: u64,
    /// Input rate.
    pub input: f64,
    /// Output rate.
    pub output: f64,
    /// Cache-read rate.
    pub cache_read: f64,
    /// Cache-write rate.
    pub cache_write: f64,
}

/// Model pricing, optionally with tiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// Base input rate.
    pub input: f64,
    /// Base output rate.
    pub output: f64,
    /// Base cache-read rate.
    pub cache_read: f64,
    /// Base cache-write rate.
    pub cache_write: f64,
    /// Optional request-wide pricing tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

impl From<ModelCostRates> for ModelCost {
    fn from(rates: ModelCostRates) -> Self {
        Self {
            input: rates.input,
            output: rates.output,
            cache_read: rates.cache_read,
            cache_write: rates.cache_write,
            tiers: None,
        }
    }
}

/// Thinking format for OpenAI-compatible endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingFormat {
    /// `reasoning_effort` field.
    #[default]
    Openai,
    /// OpenRouter `reasoning: { effort }`.
    Openrouter,
    /// DeepSeek `thinking: { type }` + effort.
    Deepseek,
    /// Together `reasoning: { enabled }` + effort.
    Together,
    /// z.ai `thinking: { type }`.
    Zai,
    /// Qwen `enable_thinking`.
    Qwen,
    /// Configurable `chat_template_kwargs`.
    ChatTemplate,
    /// Qwen chat-template variant.
    QwenChatTemplate,
    /// Top-level `thinking: string`.
    StringThinking,
    /// Ant Ling effort-only reasoning.
    AntLing,
}

/// Which max-tokens field an OpenAI-compatible server expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// Modern OpenAI field.
    MaxCompletionTokens,
    /// Classic field.
    MaxTokens,
}

/// Compatibility overrides for OpenAI Chat Completions endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsCompat {
    /// Whether the provider supports the `store` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    /// Whether the provider supports the `developer` role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    /// Whether the provider supports `reasoning_effort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    /// Whether streaming includes usage via `stream_options`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    /// Whether streamed responses include `finish_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_finish_reason: Option<bool>,
    /// Max tokens field name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    /// Whether tool results require a `name` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    /// Whether a user message after tool results needs an assistant in between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    /// Whether thinking must be sent as delimited text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    /// Thinking / reasoning parameter format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    /// Session affinity header format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<SessionAffinityFormat>,
}

/// Resolved compat with concrete defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOpenAICompletionsCompat {
    /// Supports `store`.
    pub supports_store: bool,
    /// Supports `developer` role.
    pub supports_developer_role: bool,
    /// Supports `reasoning_effort`.
    pub supports_reasoning_effort: bool,
    /// Supports usage in streaming.
    pub supports_usage_in_streaming: bool,
    /// Supports finish_reason.
    pub supports_finish_reason: bool,
    /// Max tokens field.
    pub max_tokens_field: MaxTokensField,
    /// Tool results require name.
    pub requires_tool_result_name: bool,
    /// Assistant required after tool result.
    pub requires_assistant_after_tool_result: bool,
    /// Thinking as text.
    pub requires_thinking_as_text: bool,
    /// Thinking format.
    pub thinking_format: ThinkingFormat,
    /// Session affinity format.
    pub session_affinity_format: Option<SessionAffinityFormat>,
}

/// Model definition (plain data — no attached behavior).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// Model id within the provider.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Wire API id (e.g. `openai-completions`).
    pub api: String,
    /// Provider id.
    pub provider: String,
    /// Base URL for requests.
    pub base_url: String,
    /// Whether the model supports reasoning/thinking.
    pub reasoning: bool,
    /// Optional thinking level map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    /// Supported input modalities.
    pub input: Vec<InputModality>,
    /// Pricing.
    pub cost: ModelCost,
    /// Context window size in tokens.
    pub context_window: u64,
    /// Default / max output tokens.
    pub max_tokens: u64,
    /// Optional default request headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// OpenAI-completions compatibility overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<OpenAICompletionsCompat>,
}

impl Model {
    /// Whether this model uses the openai-completions wire API.
    pub fn is_openai_completions(&self) -> bool {
        self.api == API_OPENAI_COMPLETIONS
    }

    /// Whether vision/image input is supported.
    pub fn supports_images(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }
}

/// HTTP response metadata passed to [`StreamOptions::on_response`].
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    /// Status code.
    pub status: u16,
    /// Response headers (lowercased keys).
    pub headers: HashMap<String, String>,
}

/// Header map; `None` value suppresses a default header.
pub type ProviderHeaders = HashMap<String, Option<String>>;

/// Shared stream options.
#[derive(Clone, Default)]
pub struct StreamOptions {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Max output tokens.
    pub max_tokens: Option<u32>,
    /// Cancellation token (maps to abort).
    pub cancel: Option<CancellationToken>,
    /// Explicit API key override.
    pub api_key: Option<String>,
    /// Preferred transport.
    pub transport: Option<Transport>,
    /// Cache retention preference.
    pub cache_retention: Option<CacheRetention>,
    /// Session id for provider sticky routing / caching.
    pub session_id: Option<String>,
    /// Extra / overriding headers (`None` value suppresses).
    pub headers: Option<ProviderHeaders>,
    /// Request timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Optional metadata bag.
    pub metadata: Option<HashMap<String, Value>>,
    /// Provider-scoped env overrides.
    pub env: Option<HashMap<String, String>>,
    /// Inspect/replace payload before send. Return `None` to keep unchanged.
    pub on_payload: Option<OnPayload>,
    /// Called after HTTP response headers are received.
    pub on_response: Option<OnResponse>,
}

/// Callback that may rewrite the outbound JSON payload.
pub type OnPayload = std::sync::Arc<dyn Fn(&Value, &Model) -> Option<Value> + Send + Sync>;

/// Callback after HTTP response headers arrive.
pub type OnResponse = std::sync::Arc<dyn Fn(&ProviderResponse, &Model) + Send + Sync>;

/// Unified options for [`crate::Models::stream_simple`].
#[derive(Clone, Default)]
pub struct SimpleStreamOptions {
    /// Base stream options.
    pub base: StreamOptions,
    /// Unified reasoning level.
    pub reasoning: Option<ThinkingLevel>,
    /// Optional thinking budgets.
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl SimpleStreamOptions {
    /// Create options with a reasoning level.
    pub fn with_reasoning(reasoning: ThinkingLevel) -> Self {
        Self {
            reasoning: Some(reasoning),
            ..Default::default()
        }
    }
}

impl From<StreamOptions> for SimpleStreamOptions {
    fn from(base: StreamOptions) -> Self {
        Self {
            base,
            ..Default::default()
        }
    }
}

/// Streaming event protocol consumed by agents and UIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    /// Stream started; `partial` is the pending assistant message.
    Start {
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Text block started.
    TextStart {
        /// Content index.
        content_index: usize,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Text delta.
    TextDelta {
        /// Content index.
        content_index: usize,
        /// Text fragment.
        delta: String,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Text block ended.
    TextEnd {
        /// Content index.
        content_index: usize,
        /// Final text.
        content: String,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Thinking block started.
    ThinkingStart {
        /// Content index.
        content_index: usize,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Thinking delta.
    ThinkingDelta {
        /// Content index.
        content_index: usize,
        /// Thinking fragment.
        delta: String,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Thinking block ended.
    ThinkingEnd {
        /// Content index.
        content_index: usize,
        /// Final thinking text.
        content: String,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Tool call started.
    ToolcallStart {
        /// Content index.
        content_index: usize,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Tool-call arguments JSON delta (may be partial JSON).
    ToolcallDelta {
        /// Content index.
        content_index: usize,
        /// Arguments JSON fragment.
        delta: String,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Tool call completed with parsed arguments.
    ToolcallEnd {
        /// Content index.
        content_index: usize,
        /// Completed tool call.
        tool_call: ToolCall,
        /// Partial assistant message.
        partial: AssistantMessage,
    },
    /// Successful terminal event.
    Done {
        /// Terminal stop reason.
        reason: StopReason,
        /// Final assistant message.
        message: AssistantMessage,
    },
    /// Error / aborted terminal event.
    Error {
        /// Error stop reason.
        reason: StopReason,
        /// Assistant message carrying the error.
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// Whether this event terminates the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_round_trips_json() {
        let ctx = Context {
            system_prompt: Some("You are helpful.".into()),
            messages: vec![Message::user_text("hi")],
            tools: Some(vec![Tool {
                name: "get_time".into(),
                description: "Get time".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "timezone": { "type": "string" }
                    }
                }),
            }]),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: Context = serde_json::from_str(&json).unwrap();
        assert_eq!(back.system_prompt.as_deref(), Some("You are helpful."));
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.tools.as_ref().unwrap()[0].name, "get_time");
    }
}
