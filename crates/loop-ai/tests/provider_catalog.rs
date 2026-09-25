//! OpenAI-compatible providers against a local fake server: listing, auth, headers, errors.

use std::collections::HashMap;
use std::sync::Arc;

use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};
use loop_ai::{
    Context, CreateModelsOptions, Credential, CredentialStore, InMemoryCredentialStore,
    InMemoryModelsStore, Message, Models, ModelsRefreshOptions, SimpleStreamOptions, StopReason,
};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};

const MODELS_JSON: &str = r#"{"object":"list","data":[
    {"id":"alpha","object":"model"},
    {"id":"beta","object":"model","context_length":32000}
]}"#;

fn sse_reply(text: &str) -> FakeResponse {
    let chunk = |delta: &str, finish: &str| {
        format!(
            r#"data: {{"id":"r","object":"chat.completion.chunk","choices":[{{"index":0,"delta":{delta},"finish_reason":{finish}}}]}}"#
        )
    };
    let body = format!(
        "{}\n\n{}\n\ndata: [DONE]\n\n",
        chunk(
            &format!(r#"{{"role":"assistant","content":"{text}"}}"#),
            "null"
        ),
        chunk("{}", r#""stop""#)
    );
    FakeResponse::new(200, "text/event-stream", body)
}

fn route(req: &RecordedRequest) -> FakeResponse {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/models") => FakeResponse::json(MODELS_JSON),
        ("POST", "/v1/chat/completions") => sse_reply("hi there"),
        _ => FakeResponse::new(404, "text/plain", "not found"),
    }
}

fn models_with(provider: loop_ai::Provider, key: Option<&str>) -> Models {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    if let Some(key) = key {
        credentials.set("gateway", Credential::api_key(key));
    }
    let models = Models::create(CreateModelsOptions {
        credentials: Some(credentials),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
    });
    models.set_provider(provider);
    models
}

fn gateway(base_url: &str, pinned: Vec<CustomModelSpec>) -> loop_ai::Provider {
    custom_provider(CustomProviderConfig {
        id: "gateway".into(),
        name: Some("Gateway".into()),
        base_url: format!("{base_url}/v1"),
        api_key_env: vec![],
        models: pinned,
        headers: Some(HashMap::from([("X-Title".to_string(), "Loop".to_string())])),
    })
}

async fn refresh(models: &Models) -> loop_ai::ModelsRefreshResult {
    models
        .refresh(ModelsRefreshOptions {
            allow_network: Some(true),
            force: true,
            provider_id: Some("gateway".into()),
        })
        .await
}

#[tokio::test]
async fn custom_provider_lists_models_with_saved_key() {
    let server = FakeHttpServer::start(route);
    let models = models_with(gateway(server.base_url(), vec![]), Some("k-123"));
    let result = refresh(&models).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let ids: Vec<_> = models
        .get_models(Some("gateway"))
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["alpha", "beta"]);
    let beta = models.get_model("gateway", "beta").unwrap();
    assert_eq!(beta.context_window, 32_000);

    let listing = &server.requests()[0];
    assert_eq!(listing.path, "/v1/models");
    assert_eq!(listing.header("authorization"), Some("Bearer k-123"));
}

#[tokio::test]
async fn pinned_models_survive_listing_and_keep_their_metadata() {
    let server = FakeHttpServer::start(route);
    let pinned = CustomModelSpec {
        context_window: Some(8_000),
        ..CustomModelSpec::new("alpha")
    };
    let extra = CustomModelSpec::new("local-only");
    let models = models_with(gateway(server.base_url(), vec![pinned, extra]), None);
    refresh(&models).await;
    let mut ids: Vec<_> = models
        .get_models(Some("gateway"))
        .into_iter()
        .map(|m| m.id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["alpha", "beta", "local-only"]);
    assert_eq!(
        models.get_model("gateway", "alpha").unwrap().context_window,
        8_000
    );
}

#[tokio::test]
async fn listing_failure_is_reported_on_forced_refresh() {
    let server = FakeHttpServer::always(FakeResponse::new(
        401,
        "application/json",
        r#"{"error":{"message":"Invalid API key"}}"#,
    ));
    let models = models_with(gateway(server.base_url(), vec![]), Some("bad"));
    let result = refresh(&models).await;
    let err = &result.errors["gateway"];
    assert!(err.contains("401"), "{err}");
    assert!(models.get_models(Some("gateway")).is_empty());
}

#[tokio::test]
async fn chat_requests_carry_key_and_provider_headers() {
    let server = FakeHttpServer::start(route);
    let models = models_with(gateway(server.base_url(), vec![]), Some("k-123"));
    refresh(&models).await;
    let model = models.get_model("gateway", "alpha").unwrap();
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user_text("hello")],
        tools: None,
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

    let chat = server
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/chat/completions")
        .unwrap();
    assert_eq!(chat.header("authorization"), Some("Bearer k-123"));
    assert_eq!(chat.header("x-title"), Some("Loop"));
    let body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(body["model"], "alpha");
}

#[tokio::test]
async fn listing_skips_models_without_tool_support() {
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"data":[
            {"id":"agentic","supported_parameters":["tools"]},
            {"id":"chat-only:free","supported_parameters":["temperature"]}
        ]}"#,
    ));
    let models = models_with(gateway(server.base_url(), vec![]), None);
    refresh(&models).await;
    let ids: Vec<_> = models
        .get_models(Some("gateway"))
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["agentic"]);
}
