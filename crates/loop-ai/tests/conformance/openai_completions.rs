//! OpenAI Chat Completions (`/chat/completions`) through the shared scenarios.

use loop_ai::{
    InputModality, Model, ModelCost, OpenAICompletionsAdapter, SharedApiAdapter,
    API_OPENAI_COMPLETIONS,
};
use loop_test_support::RecordedRequest;
use serde_json::{json, Value};

use crate::harness::{self, Call, Caps, HttpFailure, WireFixture};

pub struct Completions;

fn chunk(delta: Value, finish: Option<&str>) -> String {
    harness::sse(
        None,
        &json!({
            "id": "chatcmpl-1",
            "model": "gpt-test",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }),
    )
}

fn text_chunks(chunks: &[&str]) -> String {
    chunks
        .iter()
        .map(|c| chunk(json!({"content": c}), None))
        .collect()
}

const DONE: &str = "data: [DONE]\n\n";

impl WireFixture for Completions {
    fn adapter(&self) -> SharedApiAdapter {
        harness::shared(OpenAICompletionsAdapter::new())
    }

    fn model(&self, base_url: &str) -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT Test".into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: harness::PROVIDER.into(),
            base_url: base_url.into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            reasoning_signatures: false,
            cache_writes: false,
            stream_errors: false,
        }
    }

    fn text(&self, chunks: &[&str]) -> String {
        format!(
            "{}{}{DONE}",
            text_chunks(chunks),
            chunk(json!({}), Some("stop"))
        )
    }

    fn max_tokens(&self, text: &str) -> String {
        format!(
            "{}{}{DONE}",
            text_chunks(&[text]),
            chunk(json!({}), Some("length"))
        )
    }

    fn tool_calls(&self, calls: &[Call]) -> String {
        let mut body = String::new();
        for (i, (id, name, args)) in calls.iter().enumerate() {
            body += &chunk(
                json!({"tool_calls": [{"index": i, "id": id, "type": "function",
                    "function": {"name": name, "arguments": ""}}]}),
                None,
            );
            // Arguments arrive in two pieces.
            let (a, b) = args.split_at(args.len() / 2);
            for piece in [a, b] {
                body += &chunk(
                    json!({"tool_calls": [{"index": i, "function": {"arguments": piece}}]}),
                    None,
                );
            }
        }
        body + &chunk(json!({}), Some("tool_calls")) + DONE
    }

    fn thinking(&self, thinking: &str, text: &str) -> String {
        let (a, b) = thinking.split_at(4);
        format!(
            "{}{}{}{}{DONE}",
            chunk(json!({"reasoning_content": a}), None),
            chunk(json!({"reasoning_content": b}), None),
            text_chunks(&[text]),
            chunk(json!({}), Some("stop"))
        )
    }

    fn usage(&self, text: &str) -> String {
        let prompt = harness::USAGE_INPUT + harness::USAGE_CACHE_READ;
        let usage = json!({
            "id": "chatcmpl-1",
            "choices": [],
            "usage": {
                "prompt_tokens": prompt,
                "completion_tokens": harness::USAGE_OUTPUT,
                "total_tokens": prompt + harness::USAGE_OUTPUT,
                "prompt_tokens_details": {"cached_tokens": harness::USAGE_CACHE_READ},
                "completion_tokens_details": {"reasoning_tokens": harness::USAGE_REASONING},
            },
        });
        format!(
            "{}{}{}{DONE}",
            text_chunks(&[text]),
            chunk(json!({}), Some("stop")),
            harness::sse(None, &usage)
        )
    }

    fn malformed(&self, chunks: &[&str]) -> String {
        let (first, rest) = chunks.split_first().unwrap();
        format!(
            "{}data: {{\"choices\": [{{\"delta\"\n\n{}{}{DONE}",
            text_chunks(&[first]),
            text_chunks(rest),
            chunk(json!({}), Some("stop"))
        )
    }

    fn http_error(&self, failure: HttpFailure) -> (u16, String) {
        let error = |status: u16, kind: &str, message: &str| {
            (
                status,
                json!({"error": {"message": message, "type": kind, "code": kind}}).to_string(),
            )
        };
        match failure {
            HttpFailure::Unauthorized => error(
                401,
                "invalid_api_key",
                "Incorrect API key provided: test-key***123.",
            ),
            HttpFailure::Forbidden => error(
                403,
                "model_not_found",
                "Project does not have access to model gpt-test",
            ),
            HttpFailure::RateLimited => error(
                429,
                "rate_limit_exceeded",
                "Rate limit reached for requests",
            ),
            HttpFailure::ServerError => error(500, "server_error", "The server had an error"),
            HttpFailure::ContextTooLong => error(
                400,
                "context_length_exceeded",
                "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
            ),
        }
    }

    fn api_key<'a>(&self, request: &'a RecordedRequest) -> Option<&'a str> {
        request.header("authorization")?.strip_prefix("Bearer ")
    }

    fn replayed_tool_calls(&self, body: &Value) -> Vec<(String, String, Value)> {
        messages(body)
            .filter_map(|m| m["tool_calls"].as_array())
            .flatten()
            .map(|c| {
                (
                    c["id"].as_str().unwrap_or_default().to_string(),
                    c["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("null"))
                        .unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    fn tool_results(&self, body: &Value) -> Vec<(String, String)> {
        messages(body)
            .filter(|m| m["role"] == "tool")
            .map(|m| {
                (
                    m["tool_call_id"].as_str().unwrap_or_default().to_string(),
                    m["content"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    fn replayed_signatures(&self, _body: &Value) -> Vec<String> {
        // Chat Completions has no reasoning signatures to send back.
        Vec::new()
    }
}

fn messages(body: &Value) -> impl Iterator<Item = &Value> {
    body["messages"].as_array().into_iter().flatten()
}

crate::conformance_suite!(Completions);
