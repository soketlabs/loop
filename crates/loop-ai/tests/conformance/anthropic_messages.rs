//! Anthropic Messages (`/v1/messages`) through the shared scenarios.

use loop_ai::{
    AnthropicMessagesAdapter, InputModality, Model, ModelCost, SharedApiAdapter,
    API_ANTHROPIC_MESSAGES,
};
use loop_test_support::RecordedRequest;
use serde_json::{json, Value};

use crate::harness::{self, Call, Caps, HttpFailure, WireFixture};

pub struct Anthropic;

fn event(data: Value) -> String {
    let name = data["type"].as_str().unwrap().to_string();
    harness::sse(Some(&name), &data)
}

fn message_start(input: u64, cache_read: u64, cache_write: u64) -> String {
    event(json!({
        "type": "message_start",
        "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [], "stop_reason": null,
            "usage": {
                "input_tokens": input,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_write,
                "output_tokens": 1,
            },
        },
    }))
}

fn start() -> String {
    message_start(10, 0, 0) + &event(json!({"type": "ping"}))
}

fn end(reason: &str, output: u64) -> String {
    event(json!({
        "type": "message_delta",
        "delta": {"stop_reason": reason, "stop_sequence": null},
        "usage": {"output_tokens": output},
    })) + &event(json!({"type": "message_stop"}))
}

fn block_start(index: usize, block: Value) -> String {
    event(json!({"type": "content_block_start", "index": index, "content_block": block}))
}

fn delta(index: usize, delta: Value) -> String {
    event(json!({"type": "content_block_delta", "index": index, "delta": delta}))
}

fn block_stop(index: usize) -> String {
    event(json!({"type": "content_block_stop", "index": index}))
}

fn text_block(index: usize, chunks: &[&str]) -> String {
    let mut body = block_start(index, json!({"type": "text", "text": ""}));
    for c in chunks {
        body += &delta(index, json!({"type": "text_delta", "text": c}));
    }
    body + &block_stop(index)
}

impl WireFixture for Anthropic {
    fn adapter(&self) -> SharedApiAdapter {
        harness::shared(AnthropicMessagesAdapter::new())
    }

    fn model(&self, base_url: &str) -> Model {
        Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: API_ANTHROPIC_MESSAGES.into(),
            provider: harness::PROVIDER.into(),
            base_url: base_url.into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
            compat: None,
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            reasoning_signatures: true,
            cache_writes: true,
            stream_errors: true,
            reasoning_tokens: false,
        }
    }

    fn text(&self, chunks: &[&str]) -> String {
        start() + &text_block(0, chunks) + &end("end_turn", 5)
    }

    fn max_tokens(&self, text: &str) -> String {
        start() + &text_block(0, &[text]) + &end("max_tokens", 5)
    }

    fn tool_calls(&self, calls: &[Call]) -> String {
        let mut body = start() + &text_block(0, &["Checking."]);
        for (i, (id, name, args)) in calls.iter().enumerate() {
            let index = i + 1;
            body += &block_start(
                index,
                json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
            );
            let (a, b) = args.split_at(args.len() / 2);
            for piece in [a, b] {
                body += &delta(
                    index,
                    json!({"type": "input_json_delta", "partial_json": piece}),
                );
            }
            body += &block_stop(index);
        }
        body + &end("tool_use", 20)
    }

    fn thinking(&self, thinking: &str, text: &str) -> String {
        let (a, b) = thinking.split_at(4);
        let (s1, s2) = harness::SIGNATURE.split_at(3);
        start()
            + &block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            )
            + &delta(0, json!({"type": "thinking_delta", "thinking": a}))
            + &delta(0, json!({"type": "thinking_delta", "thinking": b}))
            + &delta(0, json!({"type": "signature_delta", "signature": s1}))
            + &delta(0, json!({"type": "signature_delta", "signature": s2}))
            + &block_stop(0)
            + &text_block(1, &[text])
            + &end("end_turn", 9)
    }

    fn usage(&self, text: &str) -> String {
        message_start(
            harness::USAGE_INPUT,
            harness::USAGE_CACHE_READ,
            harness::USAGE_CACHE_WRITE,
        ) + &text_block(0, &[text])
            + &end("end_turn", harness::USAGE_OUTPUT)
    }

    fn malformed(&self, chunks: &[&str]) -> String {
        let (first, rest) = chunks.split_first().unwrap();
        let mut body = start()
            + &block_start(0, json!({"type": "text", "text": ""}))
            + &delta(0, json!({"type": "text_delta", "text": first}))
            + "event: content_block_delta\ndata: {\"type\": \"content_block_delta\", \"ind\n\n";
        for c in rest {
            body += &delta(0, json!({"type": "text_delta", "text": c}));
        }
        body + &block_stop(0) + &end("end_turn", 5)
    }

    fn stream_error(&self) -> Option<String> {
        Some(
            start()
                + &text_block(0, &["Hel"])
                + &event(json!({
                    "type": "error",
                    "error": {"type": "overloaded_error", "message": "Overloaded"},
                })),
        )
    }

    fn http_error(&self, failure: HttpFailure) -> (u16, String) {
        let error = |status: u16, kind: &str, message: &str| {
            (
                status,
                json!({"type": "error", "error": {"type": kind, "message": message}}).to_string(),
            )
        };
        match failure {
            HttpFailure::Unauthorized => error(401, "authentication_error", "invalid x-api-key"),
            HttpFailure::Forbidden => error(
                403,
                "permission_error",
                "Your API key does not have permission to use the specified resource.",
            ),
            HttpFailure::RateLimited => error(
                429,
                "rate_limit_error",
                "Number of request tokens has exceeded your per-minute rate limit",
            ),
            HttpFailure::ServerError => error(529, "overloaded_error", "Overloaded"),
            HttpFailure::ContextTooLong => error(
                400,
                "invalid_request_error",
                "prompt is too long: 210000 tokens > 200000 maximum",
            ),
        }
    }

    fn api_key<'a>(&self, request: &'a RecordedRequest) -> Option<&'a str> {
        assert_eq!(request.path, "/messages");
        assert!(request.header("anthropic-version").is_some());
        assert!(request.header("authorization").is_none());
        request.header("x-api-key")
    }

    fn replayed_tool_calls(&self, body: &Value) -> Vec<(String, String, Value)> {
        blocks(body, "assistant", "tool_use")
            .map(|b| {
                (
                    b["id"].as_str().unwrap_or_default().to_string(),
                    b["name"].as_str().unwrap_or_default().to_string(),
                    b["input"].clone(),
                )
            })
            .collect()
    }

    fn tool_results(&self, body: &Value) -> Vec<(String, String)> {
        blocks(body, "user", "tool_result")
            .map(|b| {
                (
                    b["tool_use_id"].as_str().unwrap_or_default().to_string(),
                    b["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
    }

    fn replayed_signatures(&self, body: &Value) -> Vec<String> {
        blocks(body, "assistant", "thinking")
            .map(|b| b["signature"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

fn blocks<'a>(body: &'a Value, role: &'a str, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |m| m["role"] == role)
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(move |b| b["type"] == kind)
}

crate::conformance_suite!(Anthropic);
