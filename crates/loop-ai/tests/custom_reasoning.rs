//! Request payloads for custom providers, using only a local fake endpoint.
use futures::StreamExt;
use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};
use loop_ai::{
    Context, Message, Models, OpenAICompletionsCompat, SimpleStreamOptions, ThinkingFormat,
    ThinkingLevel,
};
use loop_test_support::{FakeHttpServer, FakeResponse};
use serde_json::{json, Value};

#[tokio::test]
async fn custom_thinking_formats_send_every_effort_and_off() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        for format in [ThinkingFormat::Openai, ThinkingFormat::Zai] {
            check_format(format).await;
        }
    })
    .await
    .expect("payload requests completed");
}

async fn check_format(format: ThinkingFormat) {
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = FakeHttpServer::always(FakeResponse::new(200, "text/event-stream", answer));
    let models = Models::new();
    models.set_provider(custom_provider(CustomProviderConfig {
        id: "payload-test".into(),
        name: None,
        base_url: server.base_url().into(),
        api_key_env: vec![],
        headers: None,
        models: vec![CustomModelSpec::new("glm")
            .with_reasoning(true)
            .with_compat(OpenAICompletionsCompat {
                thinking_format: Some(format),
                ..Default::default()
            })],
    }));
    let model = models.model("payload-test", "glm").unwrap();
    for (level, effort) in [
        (Some(ThinkingLevel::Minimal), "minimal"),
        (Some(ThinkingLevel::Low), "low"),
        (Some(ThinkingLevel::Medium), "medium"),
        (Some(ThinkingLevel::High), "high"),
        (Some(ThinkingLevel::XHigh), "xhigh"),
        (Some(ThinkingLevel::Max), "max"),
        (None, "off"),
    ] {
        let options = match level {
            Some(level) => SimpleStreamOptions::with_reasoning(level),
            None => SimpleStreamOptions::default(),
        };
        let context = Context {
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        };
        let mut stream = models.stream_simple(&model, &context, options);
        let previous_count = server.requests().len();
        while let Some(event) = stream.next().await {
            assert!(
                !matches!(event, loop_ai::AssistantMessageEvent::Error { .. }),
                "{event:?}"
            );
        }
        let requests = server.requests();
        assert_eq!(requests.len(), previous_count + 1);
        let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        match format {
            ThinkingFormat::Openai => {
                if level.is_some() {
                    assert_eq!(body["reasoning_effort"], effort);
                } else {
                    assert!(body.get("reasoning_effort").is_none());
                }
                assert!(body.get("thinking").is_none());
            }
            ThinkingFormat::Zai => {
                let state = if level.is_some() {
                    "enabled"
                } else {
                    "disabled"
                };
                assert_eq!(body["thinking"], json!({"type": state}));
                assert!(body.get("reasoning_effort").is_none());
            }
            _ => unreachable!(),
        }
    }
}
