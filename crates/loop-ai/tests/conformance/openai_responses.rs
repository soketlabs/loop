//! OpenAI Responses (`/responses`) through the shared scenarios.

use loop_ai::{
    InputModality, Model, ModelCost, OpenAIResponsesAdapter, SharedApiAdapter, API_OPENAI_RESPONSES,
};
use loop_test_support::RecordedRequest;
use serde_json::{json, Value};

use crate::harness::{self, Call, Caps, HttpFailure, WireFixture};

pub struct Responses;

fn event(data: Value) -> String {
    let name = data["type"].as_str().unwrap().to_string();
    harness::sse(Some(&name), &data)
}

fn created() -> String {
    event(json!({
        "type": "response.created",
        "response": {"id": "resp_1", "model": "gpt-test", "status": "in_progress"},
    }))
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": input + output,
    })
}

fn completed(usage: Value) -> String {
    event(json!({
        "type": "response.completed",
        "response": {"id": "resp_1", "status": "completed", "usage": usage},
    }))
}

fn message(output_index: usize, chunks: &[&str]) -> String {
    let id = format!("msg_{output_index}");
    let mut body = event(json!({
        "type": "response.output_item.added", "output_index": output_index,
        "item": {"type": "message", "id": id, "role": "assistant", "content": [], "status": "in_progress"},
    }));
    for c in chunks {
        body += &event(json!({
            "type": "response.output_text.delta", "item_id": id,
            "output_index": output_index, "content_index": 0, "delta": c,
        }));
    }
    body + &event(json!({
        "type": "response.output_item.done", "output_index": output_index,
        "item": {"type": "message", "id": id, "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": chunks.concat()}]},
    }))
}

impl WireFixture for Responses {
    fn adapter(&self) -> SharedApiAdapter {
        harness::shared(OpenAIResponsesAdapter::new())
    }

    fn model(&self, base_url: &str) -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT Test".into(),
            api: API_OPENAI_RESPONSES.into(),
            provider: harness::PROVIDER.into(),
            base_url: base_url.into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost::default(),
            context_window: 400_000,
            max_tokens: 128_000,
            headers: None,
            compat: None,
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            reasoning_signatures: true,
            cache_writes: false,
            stream_errors: true,
            reasoning_tokens: true,
        }
    }

    fn text(&self, chunks: &[&str]) -> String {
        created() + &message(0, chunks) + &completed(usage(10, 0, 5, 0))
    }

    fn max_tokens(&self, text: &str) -> String {
        created()
            + &message(0, &[text])
            + &event(json!({
                "type": "response.incomplete",
                "response": {"id": "resp_1", "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": usage(10, 0, 5, 0)},
            }))
    }

    fn tool_calls(&self, calls: &[Call]) -> String {
        let mut body = created();
        for (i, (id, name, args)) in calls.iter().enumerate() {
            let item_id = format!("fc_{i}");
            body += &event(json!({
                "type": "response.output_item.added", "output_index": i,
                "item": {"type": "function_call", "id": item_id, "call_id": id, "name": name,
                    "arguments": "", "status": "in_progress"},
            }));
            let (a, b) = args.split_at(args.len() / 2);
            for piece in [a, b] {
                body += &event(json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item_id, "output_index": i, "delta": piece,
                }));
            }
            body += &event(json!({
                "type": "response.function_call_arguments.done",
                "item_id": item_id, "output_index": i, "arguments": args,
            }));
            body += &event(json!({
                "type": "response.output_item.done", "output_index": i,
                "item": {"type": "function_call", "id": item_id, "call_id": id, "name": name,
                    "arguments": args, "status": "completed"},
            }));
        }
        body + &completed(usage(10, 0, 20, 0))
    }

    fn thinking(&self, thinking: &str, text: &str) -> String {
        let (a, b) = thinking.split_at(4);
        let summary = json!([{"type": "summary_text", "text": thinking}]);
        created()
            + &event(json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "reasoning", "id": "rs_1", "summary": []},
            }))
            + &event(json!({
                "type": "response.reasoning_summary_part.added", "item_id": "rs_1",
                "output_index": 0, "summary_index": 0,
                "part": {"type": "summary_text", "text": ""},
            }))
            + &event(json!({
                "type": "response.reasoning_summary_text.delta", "item_id": "rs_1",
                "output_index": 0, "summary_index": 0, "delta": a,
            }))
            + &event(json!({
                "type": "response.reasoning_summary_text.delta", "item_id": "rs_1",
                "output_index": 0, "summary_index": 0, "delta": b,
            }))
            + &event(json!({
                "type": "response.output_item.done", "output_index": 0,
                "item": {"type": "reasoning", "id": "rs_1", "summary": summary,
                    "encrypted_content": harness::SIGNATURE},
            }))
            + &message(1, &[text])
            + &completed(usage(10, 0, 9, 4))
    }

    fn usage(&self, text: &str) -> String {
        created()
            + &message(0, &[text])
            + &completed(usage(
                harness::USAGE_INPUT + harness::USAGE_CACHE_READ,
                harness::USAGE_CACHE_READ,
                harness::USAGE_OUTPUT,
                harness::USAGE_REASONING,
            ))
    }

    fn malformed(&self, chunks: &[&str]) -> String {
        let (first, rest) = chunks.split_first().unwrap();
        let mut body = created()
            + &event(json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "message", "id": "msg_0", "role": "assistant", "content": []},
            }))
            + &event(json!({
                "type": "response.output_text.delta", "item_id": "msg_0",
                "output_index": 0, "content_index": 0, "delta": first,
            }))
            + "event: response.output_text.delta\ndata: {\"type\": \"response.outp\n\n";
        for c in rest {
            body += &event(json!({
                "type": "response.output_text.delta", "item_id": "msg_0",
                "output_index": 0, "content_index": 0, "delta": c,
            }));
        }
        body + &event(json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"type": "message", "id": "msg_0", "role": "assistant", "content": []},
        })) + &completed(usage(10, 0, 5, 0))
    }

    fn stream_error(&self) -> Option<String> {
        Some(
            created()
                + &message(0, &["Hel"])
                + &event(json!({
                    "type": "response.failed",
                    "response": {"id": "resp_1", "status": "failed",
                        "error": {"code": "server_error", "message": "The server had an error"}},
                })),
        )
    }

    fn http_error(&self, failure: HttpFailure) -> (u16, String) {
        let error = |status: u16, code: &str, message: &str| {
            (
                status,
                json!({"error": {"message": message, "type": "invalid_request_error", "code": code}})
                    .to_string(),
            )
        };
        match failure {
            HttpFailure::Unauthorized => error(401, "invalid_api_key", "Incorrect API key provided."),
            HttpFailure::Forbidden => error(
                403,
                "model_not_found",
                "Project does not have access to model gpt-test",
            ),
            HttpFailure::RateLimited => {
                error(429, "rate_limit_exceeded", "Rate limit reached for requests")
            }
            HttpFailure::ServerError => error(503, "server_error", "The engine is overloaded"),
            HttpFailure::ContextTooLong => error(
                400,
                "context_length_exceeded",
                "Your input exceeds the context window of this model. Please adjust your input and try again.",
            ),
        }
    }

    fn api_key<'a>(&self, request: &'a RecordedRequest) -> Option<&'a str> {
        assert_eq!(request.path, "/responses");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["store"], false);
        request.header("authorization")?.strip_prefix("Bearer ")
    }

    fn replayed_tool_calls(&self, body: &Value) -> Vec<(String, String, Value)> {
        items(body, "function_call")
            .map(|i| {
                (
                    i["call_id"].as_str().unwrap_or_default().to_string(),
                    i["name"].as_str().unwrap_or_default().to_string(),
                    serde_json::from_str(i["arguments"].as_str().unwrap_or("null"))
                        .unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    fn tool_results(&self, body: &Value) -> Vec<(String, String)> {
        items(body, "function_call_output")
            .map(|i| {
                (
                    i["call_id"].as_str().unwrap_or_default().to_string(),
                    i["output"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    fn replayed_signatures(&self, body: &Value) -> Vec<String> {
        items(body, "reasoning")
            .map(|i| {
                i["encrypted_content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }
}

fn items<'a>(body: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |i| i["type"] == kind)
}

crate::conformance_suite!(Responses);
