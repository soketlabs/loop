//! Native providers against a local fake server: listing with the provider's own
//! auth, catalog enrichment, a rejected key, and a prompt end to end.

use std::sync::Arc;

use loop_ai::providers::{
    anthropic_provider, openai_compatible_provider, provider_preset, OpenAiCompatibleConfig,
};
use loop_ai::{
    Context, CreateModelsOptions, Credential, CredentialStore, InMemoryCredentialStore,
    InMemoryModelsStore, Message, Models, ModelsRefreshOptions, SimpleStreamOptions, StopReason,
    API_ANTHROPIC_MESSAGES, API_OPENAI_COMPLETIONS, API_OPENAI_RESPONSES,
};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};

fn models_with(provider: loop_ai::Provider, id: &str, key: &str) -> Models {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    credentials.set(id, Credential::api_key(key));
    let models = Models::create(CreateModelsOptions {
        credentials: Some(credentials),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
    });
    models.set_provider(provider);
    models
}

async fn refresh(models: &Models, id: &str) -> loop_ai::ModelsRefreshResult {
    models
        .refresh(ModelsRefreshOptions {
            allow_network: Some(true),
            force: true,
            provider_id: Some(id.into()),
        })
        .await
}

fn anthropic_route(req: &RecordedRequest) -> FakeResponse {
    if req.header("x-api-key") != Some("sk-ant-good") {
        return FakeResponse::new(
            401,
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        );
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/models?limit=1000") => FakeResponse::json(
            r#"{"data":[
                {"id":"claude-opus-5-5","display_name":"Claude Opus 5.5","max_input_tokens":1000000,"max_tokens":128000},
                {"id":"claude-next","display_name":"Claude Next","max_input_tokens":400000,"max_tokens":64000}
            ],"has_more":false}"#,
        ),
        ("POST", "/v1/messages") => FakeResponse::new(
            200,
            "text/event-stream",
            concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-next\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi there\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
        ),
        _ => FakeResponse::new(404, "text/plain", "not found"),
    }
}

#[tokio::test]
async fn anthropic_lists_models_and_answers_a_prompt() {
    let server = FakeHttpServer::start(anthropic_route);
    let base = format!("{}/v1", server.base_url());
    let models = models_with(anthropic_provider(&base), "anthropic", "sk-ant-good");

    // Offline: the built-in catalog.
    assert_eq!(
        models.get_models(Some("anthropic"))[0].id,
        "claude-opus-5-5"
    );

    assert!(refresh(&models, "anthropic").await.errors.is_empty());
    let listed = models.get_models(Some("anthropic"));
    let ids: Vec<_> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["claude-opus-5-5", "claude-next"]);
    assert!(
        listed[0].cost.input > 0.0,
        "known models keep catalog pricing"
    );
    assert_eq!(listed[1].context_window, 400_000);
    assert!(listed.iter().all(|m| m.api == API_ANTHROPIC_MESSAGES));

    let model = models.get_model("anthropic", "claude-next").unwrap();
    let context = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let reply = models
        .complete_simple(&model, &context, SimpleStreamOptions::default())
        .await;
    assert_eq!(
        reply.stop_reason,
        StopReason::Stop,
        "{:?}",
        reply.error_message
    );
    assert_eq!(reply.usage.output, 3);
}

#[tokio::test]
async fn anthropic_rejects_a_bad_key_when_listing() {
    let server = FakeHttpServer::start(anthropic_route);
    let base = format!("{}/v1", server.base_url());
    let models = models_with(anthropic_provider(&base), "anthropic", "sk-ant-bad");
    let result = refresh(&models, "anthropic").await;
    let error = &result.errors["anthropic"];
    assert!(error.starts_with("HTTP 401"), "{error}");
    assert!(!error.contains("sk-ant-bad"));
}

fn openai_route(req: &RecordedRequest) -> FakeResponse {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/models") => FakeResponse::json(
            r#"{"object":"list","data":[{"id":"gpt-5.1","object":"model"},{"id":"gpt-4.1","object":"model"}]}"#,
        ),
        ("POST", "/v1/responses") => FakeResponse::new(
            200,
            "text/event-stream",
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hi there\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n",
            ),
        ),
        _ => FakeResponse::new(404, "text/plain", "not found"),
    }
}

#[tokio::test]
async fn openai_preset_models_use_responses_and_pinned_completions_still_route() {
    let server = FakeHttpServer::start(openai_route);
    let mut pinned = loop_ai::map_remote_model(
        "legacy-chat",
        None,
        &loop_ai::MapRemoteModelOptions::default(),
    );
    pinned.provider = "openai".into();
    let config = OpenAiCompatibleConfig {
        base_url: format!("{}/v1", server.base_url()),
        pinned: vec![pinned],
        ..provider_preset("openai").unwrap().compatible_config()
    };
    let models = models_with(openai_compatible_provider(config), "openai", "sk-good");
    assert!(refresh(&models, "openai").await.errors.is_empty());

    let gpt5 = models.get_model("openai", "gpt-5.1").unwrap();
    assert_eq!(gpt5.api, API_OPENAI_RESPONSES);
    assert!(gpt5.reasoning, "gpt-5 family reasons");
    let gpt41 = models.get_model("openai", "gpt-4.1").unwrap();
    assert!(!gpt41.reasoning);
    assert_eq!(
        models.get_model("openai", "legacy-chat").unwrap().api,
        API_OPENAI_COMPLETIONS,
        "pinned models keep their api"
    );

    let context = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let reply = models
        .complete_simple(&gpt5, &context, SimpleStreamOptions::default())
        .await;
    assert_eq!(
        reply.stop_reason,
        StopReason::Stop,
        "{:?}",
        reply.error_message
    );
    assert_eq!(reply.api, API_OPENAI_RESPONSES);
}
