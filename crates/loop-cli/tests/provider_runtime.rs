//! `/login` / `/logout` persistence against local fake providers (separate test binary:
//! it mutates process env).

use futures::StreamExt;
use loop_cli::config::paths::{auth_path, settings_path, ENV_AGENT_DIR, ENV_SESSION_DIR};
use loop_cli::config::ProviderLoginRequest;
use loop_cli::{bootstrap_cli, BootstrapOpts, CliRuntime};
use loop_test_support::{FakeHttpServer, FakeResponse};

const MODELS: &str = r#"{"data":[{"id":"m-one","supported_parameters":["tools","reasoning"]},{"id":"m-two","supported_parameters":["tools"]}]}"#;

fn models_server() -> FakeHttpServer {
    FakeHttpServer::start(|req| match req.path.as_str() {
        "/v1/models" => FakeResponse::json(MODELS),
        _ => FakeResponse::new(404, "text/plain", "nope"),
    })
}

async fn boot(cwd: &std::path::Path) -> CliRuntime {
    bootstrap_cli(BootstrapOpts {
        cwd: cwd.to_path_buf(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        no_web: false,
        permission_mode: None,
        interactive: false,
        session_id: None,
        agent: None,
    })
    .await
    .unwrap()
}

fn read(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn connect_reject_restart_and_disconnect() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let agent_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let existing = models_server();
        // A saved custom provider counts as connected, so non-interactive bootstrap works
        // without any real provider key.
        std::fs::write(
            settings_path(agent_dir.path()),
            format!(
                r#"{{"providers":[{{"id":"existing","name":"Existing","baseUrl":"{}/v1"}}]}}"#,
                existing.base_url()
            ),
        )
        .unwrap();
        // SAFETY: only test in this binary; no concurrent env access.
        unsafe {
            std::env::set_var(ENV_AGENT_DIR, agent_dir.path());
            std::env::set_var(ENV_SESSION_DIR, agent_dir.path().join("sessions"));
            for key in [
                "SOKET_API_KEY",
                "TENSORSTUDIO_API_KEY",
                "LOOP_API_KEY",
                "OPENROUTER_API_KEY",
                "OPENAI_API_KEY",
            ] {
                std::env::remove_var(key);
            }
        }

        let mut runtime = boot(cwd.path()).await;
        assert!(!runtime.needs_provider_setup);
        assert_eq!(runtime.connected_providers(), ["existing"]);

        // Connect a keyed custom provider.
        let gateway = models_server();
        let connected = runtime
            .connect_provider(&ProviderLoginRequest::Custom {
                name: "Gateway".into(),
                base_url: format!("{}/v1", gateway.base_url()),
                api_key: Some("k-secret".into()),
                models: Some(vec![
                    loop_app_core::config::providers::CustomProviderModel {
                        id: "m-one".into(),
                        reasoning: false,
                        thinking_format: None,
                        supports_metadata: None,
                    },
                    loop_app_core::config::providers::CustomProviderModel {
                        id: "m-two".into(),
                        reasoning: true,
                        thinking_format: Some(loop_ai::ThinkingFormat::Zai),
                        supports_metadata: None,
                    },
                ]),
            })
            .await
            .unwrap();
        assert_eq!(
            (connected.id.as_str(), connected.model_count),
            ("gateway", 2)
        );
        assert_eq!(
            gateway.requests()[0].header("authorization"),
            Some("Bearer k-secret")
        );
        assert!(read(settings_path(agent_dir.path())).contains(r#""id": "gateway""#));
        assert!(!read(settings_path(agent_dir.path())).contains("k-secret"));
        assert!(read(auth_path(agent_dir.path())).contains("k-secret"));

        // Picker order: providers alphabetically (no Soket key here), models by id.
        let order: Vec<String> = runtime
            .available_models()
            .await
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(
            order,
            [
                "existing/m-one",
                "existing/m-two",
                "gateway/m-one",
                "gateway/m-two"
            ]
        );

        // A rejected key leaves nothing behind.
        let rejecting = FakeHttpServer::always(FakeResponse::new(
            401,
            "application/json",
            r#"{"error":{"message":"bad key"}}"#,
        ));
        let err = runtime
            .connect_provider(&ProviderLoginRequest::Custom {
                name: "Broken".into(),
                base_url: format!("{}/v1", rejecting.base_url()),
                api_key: Some("k-bad".into()),
                models: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(!read(auth_path(agent_dir.path())).contains("k-bad"));
        assert!(!read(settings_path(agent_dir.path())).contains("broken"));
        assert!(runtime.models.provider("broken").is_none());

        // Nothing is selected until the user picks a model.
        assert!(runtime.selected_model.is_none());
        assert!(runtime.harness.model().await.is_none());
        let model = runtime.select_model("gateway", "m-two").await.unwrap();
        assert_eq!(model.id, "m-two");
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("gateway/m-two")
        );
        assert_eq!(runtime.harness.model().await.unwrap().id, "m-two");
        assert!(runtime.select_model("gateway", "nope").await.is_err());

        assert_eq!(
            runtime.efforts().await.unwrap().0,
            ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        );

        // A reconnect without metadata preserves the previous choice.
        let reconnect = ProviderLoginRequest::Custom {
            name: "Gateway".into(),
            base_url: format!("{}/v1", gateway.base_url()),
            api_key: Some("k-secret".into()),
            models: None,
        };
        runtime.connect_provider(&reconnect).await.unwrap();
        assert!(runtime.models.model("gateway", "m-two").unwrap().reasoning);
        assert!(!runtime.models.model("gateway", "m-one").unwrap().reasoning);
        runtime.select_model("gateway", "m-two").await.unwrap();

        // Restart: the connected provider, its key and the selection are still there.
        drop(runtime);
        let mut runtime = boot(cwd.path()).await;
        assert_eq!(runtime.connected_providers(), ["existing", "gateway"]);
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("gateway/m-two")
        );

        assert!(runtime.models.model("gateway", "m-two").unwrap().reasoning);
        assert_eq!(
            runtime
                .models
                .model("gateway", "m-two")
                .unwrap()
                .compat
                .unwrap()
                .thinking_format,
            Some(loop_ai::ThinkingFormat::Zai)
        );

        // models.json overrides settings for the same id, including across reconnect
        // and catalog refresh, while other saved models retain their metadata.
        std::fs::write(
            agent_dir.path().join("models.json"),
            serde_json::json!({
                "providers": [{"id": "gateway", "baseUrl": format!("{}/v1", gateway.base_url()),
                    "models": [{"id": "m-two", "reasoning": false},
                               {"id": "m-one", "reasoning": true, "thinkingFormat": "openai"}]}]
            })
            .to_string(),
        )
        .unwrap();
        runtime.connect_provider(&reconnect).await.unwrap();
        assert_eq!(runtime.efforts().await.unwrap().0, ["off"]);
        assert!(!runtime.harness.model().await.unwrap().reasoning);
        let model = runtime.select_model("gateway", "m-two").await.unwrap();
        assert!(!model.reasoning);
        assert_eq!(runtime.efforts().await.unwrap().0, ["off"]);
        let provider = runtime.models.provider("gateway").unwrap();
        provider
            .refresh_models(loop_ai::RefreshModelsContext {
                api_key: Some("k-secret".into()),
                store: None,
                allow_network: true,
                force: true,
            })
            .await
            .unwrap();
        assert!(!runtime.models.model("gateway", "m-two").unwrap().reasoning);
        assert!(runtime.models.model("gateway", "m-one").unwrap().reasoning);
        assert_eq!(
            runtime
                .models
                .model("gateway", "m-one")
                .unwrap()
                .compat
                .unwrap()
                .thinking_format,
            Some(loop_ai::ThinkingFormat::Openai)
        );
        drop(runtime);
        let mut runtime = boot(cwd.path()).await;
        assert!(!runtime.models.model("gateway", "m-two").unwrap().reasoning);
        // Remove the override before disconnecting.
        std::fs::remove_file(agent_dir.path().join("models.json")).unwrap();

        // Disconnect removes entry, key and provider.
        assert_eq!(
            runtime.disconnect_provider("gateway").await.unwrap(),
            "Gateway"
        );
        assert!(runtime.selected_model.is_none(), "its model is deselected");
        assert!(runtime.harness.model().await.is_none());
        assert_eq!(
            runtime.model_note.as_deref(),
            Some("gateway/m-two was disconnected")
        );
        assert_eq!(runtime.connected_providers(), ["existing"]);
        assert!(runtime.models.provider("gateway").is_none());
        assert!(!read(auth_path(agent_dir.path())).contains("k-secret"));
        assert!(!read(settings_path(agent_dir.path())).contains("gateway"));
        assert!(runtime.disconnect_provider("gateway").await.is_err());
        removed_pin_clears_selection(&mut runtime).await;
        corrected_reconnect_restores_thinking(&mut runtime).await;
    })
    .await
    .expect("provider lifecycle completed");
}

const ANSWER: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

async fn thinking_reply(runtime: &CliRuntime) {
    let model = runtime.harness.model().await.unwrap();
    let context = loop_ai::Context {
        messages: vec![loop_ai::Message::user_text("hi")],
        ..Default::default()
    };
    let mut stream = runtime.models.stream_simple(
        &model,
        &context,
        loop_ai::SimpleStreamOptions::with_reasoning(loop_ai::ThinkingLevel::High),
    );
    while let Some(event) = stream.next().await {
        assert!(
            !matches!(event, loop_ai::AssistantMessageEvent::Error { .. }),
            "{event:?}"
        );
    }
}

async fn corrected_reconnect_restores_thinking(runtime: &mut CliRuntime) {
    let server = FakeHttpServer::start(|request| match request.path.as_str() {
        "/v1/models" => FakeResponse::json(MODELS),
        _ => {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("reasoning_effort").is_some() {
                FakeResponse::new(
                    400,
                    "application/json",
                    r#"{"error":{"message":"unsupported parameter: reasoning_effort"}}"#,
                )
            } else {
                FakeResponse::new(200, "text/event-stream", ANSWER)
            }
        }
    });
    let request = |url: &str, format| ProviderLoginRequest::Custom {
        name: "Recovery".into(),
        base_url: format!("{url}/v1"),
        api_key: None,
        models: Some(vec![
            loop_app_core::config::providers::CustomProviderModel {
                id: "m-two".into(),
                reasoning: true,
                thinking_format: Some(format),
                supports_metadata: None,
            },
        ]),
    };
    runtime
        .connect_provider(&request(server.base_url(), loop_ai::ThinkingFormat::Openai))
        .await
        .unwrap();
    runtime.select_model("recovery", "m-two").await.unwrap();
    thinking_reply(runtime).await;
    assert_eq!(runtime.efforts().await.unwrap().0, ["off"]);
    // Unchanged reconnects retain the learned rejection.
    runtime
        .connect_provider(&request(server.base_url(), loop_ai::ThinkingFormat::Openai))
        .await
        .unwrap();
    assert_eq!(runtime.efforts().await.unwrap().0, ["off"]);
    runtime
        .connect_provider(&request(server.base_url(), loop_ai::ThinkingFormat::Zai))
        .await
        .unwrap();
    assert!(runtime
        .efforts()
        .await
        .unwrap()
        .0
        .contains(&"high".to_string()));
    thinking_reply(runtime).await;
    let body: serde_json::Value =
        serde_json::from_slice(&server.requests().last().unwrap().body).unwrap();
    assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));
    // A different endpoint must also get another chance with the same format.
    runtime
        .connect_provider(&request(server.base_url(), loop_ai::ThinkingFormat::Openai))
        .await
        .unwrap();
    thinking_reply(runtime).await;
    assert_eq!(runtime.efforts().await.unwrap().0, ["off"]);
    let corrected = FakeHttpServer::start(|request| match request.path.as_str() {
        "/v1/models" => FakeResponse::json(MODELS),
        _ => FakeResponse::new(200, "text/event-stream", ANSWER),
    });
    runtime
        .connect_provider(&request(
            corrected.base_url(),
            loop_ai::ThinkingFormat::Openai,
        ))
        .await
        .unwrap();
    assert!(runtime
        .efforts()
        .await
        .unwrap()
        .0
        .contains(&"high".to_string()));
    thinking_reply(runtime).await;
    let body: serde_json::Value =
        serde_json::from_slice(&corrected.requests().last().unwrap().body).unwrap();
    assert_eq!(body["reasoning_effort"], "high");
    runtime.disconnect_provider("recovery").await.unwrap();
}

async fn removed_pin_clears_selection(runtime: &mut CliRuntime) {
    let server = models_server();
    let mut request = ProviderLoginRequest::Custom {
        name: "Pins".into(),
        base_url: format!("{}/v1", server.base_url()),
        api_key: None,
        models: Some(vec![
            loop_app_core::config::providers::CustomProviderModel {
                id: "removed".into(),
                reasoning: true,
                thinking_format: Some(loop_ai::ThinkingFormat::Zai),
                supports_metadata: None,
            },
        ]),
    };
    runtime.connect_provider(&request).await.unwrap();
    runtime.select_model("pins", "removed").await.unwrap();
    if let ProviderLoginRequest::Custom { models, .. } = &mut request {
        *models = Some(vec![]);
    }
    runtime.connect_provider(&request).await.unwrap();
    assert!(runtime.models.model("pins", "removed").is_none());
    assert!(runtime.selected_model.is_none());
    assert!(runtime.harness.model().await.is_none());
    assert!(runtime.settings.default_model.is_none());
    assert!(runtime.settings.default_provider.is_none());
    let saved: serde_json::Value =
        serde_json::from_str(&read(settings_path(&runtime.agent_dir))).unwrap();
    assert!(saved
        .get("defaultModel")
        .is_none_or(|value| value.is_null()));
    assert!(saved
        .get("defaultProvider")
        .is_none_or(|value| value.is_null()));
}
