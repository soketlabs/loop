//! An organization that may not receive reasoning summaries gets the request again
//! without them, and the model is remembered so later requests skip them.

use std::collections::HashMap;

use loop_ai::{
    create_provider, Context, CreateProviderApi, CreateProviderOptions, InputModality, Message,
    Model, ModelCost, Models, OpenAIResponsesAdapter, ProviderAuth, SimpleStreamOptions,
    StopReason, ThinkingLevel, API_OPENAI_RESPONSES,
};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};

const REJECTED: &str = r#"{"error":{"message":"Your organization must be verified to generate reasoning summaries. Please go to: https://platform.openai.com/settings/organization/general and click on Verify Organization.","type":"invalid_request_error","param":"reasoning.summary","code":"unsupported_value"}}"#;
const ANSWER: &str = concat!(
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
);

fn route(request: &RecordedRequest) -> FakeResponse {
    let body = String::from_utf8_lossy(&request.body);
    if body.contains("\"summary\"") {
        FakeResponse::new(400, "application/json", REJECTED)
    } else {
        FakeResponse::new(200, "text/event-stream", ANSWER)
    }
}

#[tokio::test]
async fn unverified_orgs_lose_summaries_but_keep_reasoning() {
    let server = FakeHttpServer::start(route);
    let model = Model {
        id: "gpt-summary-test".into(),
        name: "m".into(),
        api: API_OPENAI_RESPONSES.into(),
        provider: "summary-test".into(),
        base_url: server.base_url().into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 400_000,
        max_tokens: 1000,
        headers: None,
        compat: None,
    };
    let models = Models::new();
    models.set_provider(create_provider(CreateProviderOptions {
        id: "summary-test".into(),
        name: None,
        base_url: None,
        headers: None,
        auth: ProviderAuth::keyless("test"),
        models: vec![model.clone()],
        api: CreateProviderApi::ByApi(HashMap::from([(
            API_OPENAI_RESPONSES.to_string(),
            OpenAIResponsesAdapter::shared() as loop_ai::SharedApiAdapter,
        )])),
        fetch_models: None,
    }));
    let context = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let options = SimpleStreamOptions::with_reasoning(ThinkingLevel::High);

    let reply = models
        .complete_simple(&model, &context, options.clone())
        .await;
    assert_eq!(
        reply.stop_reason,
        StopReason::Stop,
        "{:?}",
        reply.error_message
    );
    let first = server.requests();
    assert_eq!(first.len(), 2, "the rejected request, then the retry");
    let retry: serde_json::Value = serde_json::from_slice(&first[1].body).unwrap();
    assert_eq!(retry["reasoning"], serde_json::json!({"effort": "high"}));

    // Later requests go straight through without a summary.
    models.complete_simple(&model, &context, options).await;
    assert_eq!(server.requests().len(), 3);
}
