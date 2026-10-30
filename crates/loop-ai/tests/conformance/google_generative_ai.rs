//! Gemini API (`streamGenerateContent?alt=sse`) through the shared scenarios.

use loop_ai::{
    GoogleGenerativeAiAdapter, InputModality, Model, ModelCost, SharedApiAdapter,
    API_GOOGLE_GENERATIVE_AI,
};
use loop_test_support::RecordedRequest;
use serde_json::{json, Value};

use crate::harness::{self, Call, Caps, HttpFailure, WireFixture};

pub struct Gemini;

fn chunk(parts: Value, finish: Option<&str>, usage: Option<Value>) -> String {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(reason) = finish {
        candidate["finishReason"] = json!(reason);
    }
    let mut data = json!({
        "candidates": [candidate],
        "modelVersion": "gemini-test",
        "responseId": "resp-1",
    });
    if let Some(usage) = usage {
        data["usageMetadata"] = usage;
    }
    harness::sse(None, &data)
}

fn usage(prompt: u64, cached: u64, candidates: u64, thoughts: u64) -> Value {
    json!({
        "promptTokenCount": prompt,
        "cachedContentTokenCount": cached,
        "candidatesTokenCount": candidates,
        "thoughtsTokenCount": thoughts,
        "totalTokenCount": prompt + candidates + thoughts,
    })
}

fn texts(chunks: &[&str]) -> String {
    chunks
        .iter()
        .map(|c| chunk(json!([{"text": c}]), None, None))
        .collect()
}

fn stop(reason: &str) -> String {
    chunk(
        json!([{"text": ""}]),
        Some(reason),
        Some(usage(10, 0, 5, 0)),
    )
}

impl WireFixture for Gemini {
    fn adapter(&self) -> SharedApiAdapter {
        harness::shared(GoogleGenerativeAiAdapter::new())
    }

    fn model(&self, base_url: &str) -> Model {
        Model {
            id: "gemini-test".into(),
            name: "Gemini Test".into(),
            api: API_GOOGLE_GENERATIVE_AI.into(),
            provider: harness::PROVIDER.into(),
            base_url: base_url.into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost::default(),
            context_window: 1_048_576,
            max_tokens: 65_536,
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
        texts(chunks) + &stop("STOP")
    }

    fn max_tokens(&self, text: &str) -> String {
        texts(&[text]) + &stop("MAX_TOKENS")
    }

    fn tool_calls(&self, calls: &[Call]) -> String {
        // Gemini sends each call whole; the first carries the thought signature.
        let parts: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (id, name, args))| {
                let mut part = json!({"functionCall": {
                    "id": id, "name": name,
                    "args": serde_json::from_str::<Value>(args).unwrap(),
                }});
                if i == 0 {
                    part["thoughtSignature"] = json!("call-sig");
                }
                part
            })
            .collect();
        chunk(json!(parts), Some("STOP"), Some(usage(10, 0, 20, 0)))
    }

    fn thinking(&self, thinking: &str, text: &str) -> String {
        let (a, b) = thinking.split_at(4);
        chunk(json!([{"text": a, "thought": true}]), None, None)
            + &chunk(
                json!([{"text": b, "thought": true, "thoughtSignature": harness::SIGNATURE}]),
                None,
                None,
            )
            + &chunk(
                json!([{"text": text}]),
                Some("STOP"),
                Some(usage(10, 0, 5, 4)),
            )
    }

    fn usage(&self, text: &str) -> String {
        chunk(
            json!([{"text": text}]),
            Some("STOP"),
            Some(usage(
                harness::USAGE_INPUT + harness::USAGE_CACHE_READ,
                harness::USAGE_CACHE_READ,
                harness::USAGE_OUTPUT - harness::USAGE_REASONING,
                harness::USAGE_REASONING,
            )),
        )
    }

    fn malformed(&self, chunks: &[&str]) -> String {
        let (first, rest) = chunks.split_first().unwrap();
        texts(&[first]) + "data: {\"candidates\": [{\"content\n\n" + &texts(rest) + &stop("STOP")
    }

    fn stream_error(&self) -> Option<String> {
        Some(
            texts(&["Hel"])
                + &harness::sse(
                    None,
                    &json!({"error": {"code": 503, "message": "The model is overloaded.",
                        "status": "UNAVAILABLE"}}),
                ),
        )
    }

    fn http_error(&self, failure: HttpFailure) -> (u16, String) {
        let error = |code: u16, status: &str, message: &str| {
            (
                code,
                json!({"error": {"code": code, "message": message, "status": status}}).to_string(),
            )
        };
        match failure {
            HttpFailure::Unauthorized => error(
                401,
                "UNAUTHENTICATED",
                "API key not valid. Please pass a valid API key.",
            ),
            HttpFailure::Forbidden => error(
                403,
                "PERMISSION_DENIED",
                "Method doesn't allow unregistered callers. Please use an API key.",
            ),
            HttpFailure::RateLimited => {
                error(429, "RESOURCE_EXHAUSTED", "Resource has been exhausted.")
            }
            HttpFailure::ServerError => {
                error(500, "INTERNAL", "An internal error has occurred.")
            }
            HttpFailure::ContextTooLong => error(
                400,
                "INVALID_ARGUMENT",
                "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).",
            ),
        }
    }

    fn api_key<'a>(&self, request: &'a RecordedRequest) -> Option<&'a str> {
        assert_eq!(
            request.path,
            "/models/gemini-test:streamGenerateContent?alt=sse"
        );
        assert!(
            !request.path.contains("key="),
            "the key never goes in the URL"
        );
        request.header("x-goog-api-key")
    }

    fn replayed_tool_calls(&self, body: &Value) -> Vec<(String, String, Value)> {
        parts(body, "model")
            .filter_map(|p| p.get("functionCall"))
            .map(|c| {
                (
                    c["id"].as_str().unwrap_or_default().to_string(),
                    c["name"].as_str().unwrap_or_default().to_string(),
                    c["args"].clone(),
                )
            })
            .collect()
    }

    fn tool_results(&self, body: &Value) -> Vec<(String, String)> {
        parts(body, "user")
            .filter_map(|p| p.get("functionResponse"))
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["response"]["output"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
    }

    fn replayed_signatures(&self, body: &Value) -> Vec<String> {
        parts(body, "model")
            .filter(|p| p["thought"] == true)
            .filter_map(|p| p["thoughtSignature"].as_str().map(String::from))
            .collect()
    }
}

fn parts<'a>(body: &'a Value, role: &'a str) -> impl Iterator<Item = &'a Value> {
    body["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |c| c["role"] == role)
        .filter_map(|c| c["parts"].as_array())
        .flatten()
}

crate::conformance_suite!(Gemini);

#[tokio::test]
async fn call_signatures_go_back_on_their_parts() {
    use crate::harness::{Rig, TIME, WEATHER};

    let fixture = Gemini;
    let first = Rig::sse(&fixture, fixture.tool_calls(&[WEATHER, TIME]));
    let context = loop_ai::Context {
        messages: vec![loop_ai::Message::user_text("both")],
        ..Default::default()
    };
    let turn = first
        .run(&context, loop_ai::SimpleStreamOptions::default())
        .await;

    let next = Rig::sse(&fixture, fixture.text(&["ok"]));
    let mut context = context;
    context
        .messages
        .push(loop_ai::Message::Assistant(turn.message));
    next.run(&context, loop_ai::SimpleStreamOptions::default())
        .await;
    let body = next.request_body();
    let signatures: Vec<_> = parts(&body, "model")
        .filter(|p| p.get("functionCall").is_some())
        .map(|p| p.get("thoughtSignature").cloned())
        .collect();
    // Only the first of parallel calls carries a signature, and it goes back on
    // that part; the second goes back without one.
    assert_eq!(signatures, [Some(json!("call-sig")), None]);
}
