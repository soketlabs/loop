//! Deterministic faux provider for tests.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::auth::{ModelAuth, ProviderAuth};
use crate::models::{
    create_provider, ApiAdapter, CreateProviderApi, CreateProviderOptions, Provider,
};
use crate::stream::{create_assistant_message_event_stream, AssistantMessageEventStream};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, InputModality, Model,
    ModelCost, SimpleStreamOptions, StopReason, StreamOptions, TextContent, ToolCall,
    API_OPENAI_COMPLETIONS,
};
use crate::utils::calculate_cost;

/// Scripted response for the faux adapter.
#[derive(Debug, Clone)]
pub enum FauxResponse {
    /// Emit text then done(stop).
    Text(String),
    /// Emit tool calls then done(toolUse).
    ToolCalls(Vec<ToolCall>),
    /// Emit an error.
    Error(String),
    /// Custom event sequence ending in done/error (advanced).
    Events(Vec<AssistantMessageEvent>),
}

/// Ordered script of responses (one per stream call).
#[derive(Debug, Default, Clone)]
pub struct FauxScript {
    responses: Arc<Mutex<Vec<FauxResponse>>>,
}

impl FauxScript {
    /// Create an empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a response.
    pub fn push(&self, response: FauxResponse) {
        self.responses.lock().push(response);
    }

    /// Queue many responses.
    pub fn extend(&self, responses: impl IntoIterator<Item = FauxResponse>) {
        self.responses.lock().extend(responses);
    }

    fn next(&self) -> FauxResponse {
        let mut q = self.responses.lock();
        if q.is_empty() {
            FauxResponse::Text("faux".into())
        } else {
            q.remove(0)
        }
    }
}

struct FauxAdapter {
    script: FauxScript,
}

impl ApiAdapter for FauxAdapter {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: StreamOptions,
        _auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        emit(model, self.script.next())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, options.base, auth)
    }
}

fn emit(model: &Model, response: FauxResponse) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let handle = stream.handle();
    let model = model.clone();

    tokio::spawn(async move {
        match response {
            FauxResponse::Text(text) => {
                let mut partial = AssistantMessage::pending(&model);
                handle.push(AssistantMessageEvent::Start {
                    partial: partial.clone(),
                });
                partial.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                handle.push(AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: partial.clone(),
                });
                if let AssistantContent::Text(t) = &mut partial.content[0] {
                    t.text.push_str(&text);
                }
                handle.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: text.clone(),
                    partial: partial.clone(),
                });
                handle.push(AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: text,
                    partial: partial.clone(),
                });
                partial.usage.output = 1;
                partial.usage.total_tokens = 1;
                calculate_cost(&model, &mut partial.usage);
                partial.stop_reason = StopReason::Stop;
                handle.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: partial,
                });
            }
            FauxResponse::ToolCalls(calls) => {
                let mut partial = AssistantMessage::pending(&model);
                handle.push(AssistantMessageEvent::Start {
                    partial: partial.clone(),
                });
                for (i, call) in calls.into_iter().enumerate() {
                    let args_json = call.arguments.to_string();
                    partial
                        .content
                        .push(AssistantContent::ToolCall(call.clone()));
                    handle.push(AssistantMessageEvent::ToolcallStart {
                        content_index: i,
                        partial: partial.clone(),
                    });
                    handle.push(AssistantMessageEvent::ToolcallDelta {
                        content_index: i,
                        delta: args_json,
                        partial: partial.clone(),
                    });
                    handle.push(AssistantMessageEvent::ToolcallEnd {
                        content_index: i,
                        tool_call: call,
                        partial: partial.clone(),
                    });
                }
                partial.stop_reason = StopReason::ToolUse;
                handle.push(AssistantMessageEvent::Done {
                    reason: StopReason::ToolUse,
                    message: partial,
                });
            }
            FauxResponse::Error(message) => {
                let mut partial = AssistantMessage::pending(&model);
                partial.stop_reason = StopReason::Error;
                partial.error_message = Some(message);
                handle.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: partial,
                });
            }
            FauxResponse::Events(events) => {
                for ev in events {
                    handle.push(ev);
                }
            }
        }
    });

    stream
}

/// Build a faux provider with a shared script.
pub fn faux_provider(script: FauxScript) -> Provider {
    let model = Model {
        id: "faux-model".into(),
        name: "Faux Model".into(),
        api: API_OPENAI_COMPLETIONS.to_string(),
        provider: "faux".into(),
        base_url: "http://faux.local".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        headers: None,
        compat: None,
    };

    create_provider(CreateProviderOptions {
        id: "faux".into(),
        name: Some("Faux".into()),
        base_url: Some("http://faux.local".into()),
        headers: None,
        auth: ProviderAuth::keyless("faux"),
        models: vec![model],
        api: CreateProviderApi::Single(Arc::new(FauxAdapter { script })),
    })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::models::Models;
    use crate::types::Message;

    #[tokio::test]
    async fn faux_text_round_trip() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("hello".into()));
        let models = Models::new();
        models.set_provider(faux_provider(script));
        let model = models.get_model("faux", "faux-model").unwrap();
        let ctx = Context {
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        };
        let stream = models.stream_simple(&model, &ctx, SimpleStreamOptions::default());
        let mut saw_delta = false;
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            if let AssistantMessageEvent::TextDelta { delta, .. } = ev {
                assert_eq!(delta, "hello");
                saw_delta = true;
            }
        }
        assert!(saw_delta);
        let result = stream.result().await;
        assert_eq!(result.stop_reason, StopReason::Stop);
    }

    #[tokio::test]
    async fn faux_tool_calls() {
        let script = FauxScript::new();
        script.push(FauxResponse::ToolCalls(vec![ToolCall {
            id: "1".into(),
            name: "get_time".into(),
            arguments: json!({"timezone":"UTC"}),
            thought_signature: None,
        }]));
        let models = Models::new();
        models.set_provider(faux_provider(script));
        let model = models.get_model("faux", "faux-model").unwrap();
        let result = models
            .complete_simple(&model, &Context::default(), SimpleStreamOptions::default())
            .await;
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert!(matches!(
            &result.content[0],
            AssistantContent::ToolCall(tc) if tc.name == "get_time"
        ));
    }
}
