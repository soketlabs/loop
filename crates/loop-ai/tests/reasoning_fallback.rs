//! A server that rejects reasoning parameters gets the request again without
//! them, and the model is remembered so later requests skip them.

use futures::StreamExt;
use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};
use loop_ai::{
    AssistantMessageEvent, Context, Message, Models, SimpleStreamOptions, ThinkingLevel,
};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};

const REJECTED: &str = r#"{"error":{"message":"custom_openai does not support parameters: ['reasoning_effort'], for model=m. To drop these, set `litellm.drop_params=True`","code":"400"}}"#;
const ANSWER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn route(request: &RecordedRequest) -> FakeResponse {
    let body = String::from_utf8_lossy(&request.body);
    if body.contains("reasoning_effort") {
        FakeResponse::new(400, "application/json", REJECTED)
    } else {
        FakeResponse::new(200, "text/event-stream", ANSWER)
    }
}

async fn reply(models: &Models, model: &loop_ai::Model) -> Result<String, String> {
    let context = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let options = SimpleStreamOptions::with_reasoning(ThinkingLevel::High);
    let mut stream = models.stream_simple(model, &context, options);
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::Error { error, .. } => {
                return Err(error.error_message.unwrap_or_default())
            }
            _ => {}
        }
    }
    Ok(text)
}

#[tokio::test]
async fn rejected_reasoning_is_dropped_and_remembered() {
    let server = FakeHttpServer::start(route);
    let models = Models::new();
    let mut spec = CustomModelSpec::new("m");
    spec.reasoning = true;
    models.set_provider(custom_provider(CustomProviderConfig {
        id: "fallback-test".into(),
        name: None,
        base_url: server.base_url().into(),
        api_key_env: Vec::new(),
        models: vec![spec],
        headers: None,
    }));
    let model = models.model("fallback-test", "m").unwrap();
    assert!(!loop_ai::reasoning_rejected(&model));

    assert_eq!(reply(&models, &model).await.unwrap(), "ok");
    assert!(loop_ai::reasoning_rejected(&model));
    let first: Vec<_> = server.requests();
    assert_eq!(first.len(), 2, "the rejected request, then the retry");

    // Later requests go straight through without reasoning parameters.
    assert_eq!(reply(&models, &model).await.unwrap(), "ok");
    let all = server.requests();
    assert_eq!(all.len(), 3);
    assert!(!String::from_utf8_lossy(&all[2].body).contains("reasoning_effort"));
}

#[tokio::test]
async fn rejected_zai_off_retries_without_thinking() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let server = FakeHttpServer::start(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("thinking").is_some() {
                FakeResponse::new(
                    422,
                    "application/json",
                    r#"{"error":{"message":"unsupported parameter: thinking"}}"#,
                )
            } else {
                FakeResponse::new(200, "text/event-stream", ANSWER)
            }
        });
        let models = Models::new();
        models.set_provider(custom_provider(CustomProviderConfig {
            id: "zai-off-fallback".into(),
            name: None,
            base_url: server.base_url().into(),
            api_key_env: vec![],
            headers: None,
            models: vec![CustomModelSpec::new("m").with_reasoning(true).with_compat(
                loop_ai::OpenAICompletionsCompat {
                    thinking_format: Some(loop_ai::ThinkingFormat::Zai),
                    ..Default::default()
                },
            )],
        }));
        let model = models.model("zai-off-fallback", "m").unwrap();
        let context = Context {
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        };
        for _ in 0..2 {
            let mut stream = models.stream_simple(&model, &context, SimpleStreamOptions::default());
            while let Some(event) = stream.next().await {
                assert!(
                    !matches!(event, AssistantMessageEvent::Error { .. }),
                    "{event:?}"
                );
            }
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(first["thinking"], serde_json::json!({"type": "disabled"}));
        for request in &requests[1..] {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert!(body.get("thinking").is_none());
        }
        assert!(loop_ai::reasoning_rejected(&model));
    })
    .await
    .expect("off fallback completed");
}
