//! Built-in Soket / TensorStudio provider with dynamic model catalog.

use std::sync::Arc;

use crate::api::openai_completions::OpenAICompletionsAdapter;
use crate::api::openai_models::{list_openai_models, MapRemoteModelOptions};
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::models::{
    create_provider, CreateProviderApi, CreateProviderOptions, Provider, RefreshModelsContext,
};
use crate::models_store::ModelsStoreEntry;
use crate::types::{InputModality, Model, ModelCost, API_OPENAI_COMPLETIONS};
use crate::utils::now_ms;

/// Provider id used in settings and model lookups.
pub const SOKET_PROVIDER_ID: &str = "soket";
/// Display name.
pub const SOKET_PROVIDER_NAME: &str = "Soket";
/// OpenAI-compatible base URL.
pub const SOKET_BASE_URL: &str = "https://api.tensorstudio.ai/v1";
/// Default model id (settings / first-run default).
pub const SOKET_DEFAULT_MODEL_ID: &str = "qwen3-30b";

/// Env vars checked for the Soket API key (first wins).
pub const SOKET_API_KEY_ENVS: &[&str] = &["SOKET_API_KEY", "TENSORSTUDIO_API_KEY", "LOOP_API_KEY"];

/// Seed catalog used offline / before the first successful refresh.
pub fn soket_seed_models() -> Vec<Model> {
    vec![Model {
        id: SOKET_DEFAULT_MODEL_ID.into(),
        name: SOKET_DEFAULT_MODEL_ID.into(),
        api: API_OPENAI_COMPLETIONS.to_string(),
        provider: SOKET_PROVIDER_ID.into(),
        base_url: SOKET_BASE_URL.into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 16_384,
        headers: None,
        compat: None,
    }]
}

fn map_opts() -> MapRemoteModelOptions {
    MapRemoteModelOptions {
        provider: SOKET_PROVIDER_ID.into(),
        base_url: SOKET_BASE_URL.into(),
        context_window: 128_000,
        max_tokens: 16_384,
        reasoning: true,
    }
}

async fn fetch_soket_models(context: RefreshModelsContext) -> Result<Vec<Model>, String> {
    // Prefer persisted cache when offline / before network.
    if let Some(store) = &context.store {
        if let Ok(Some(entry)) = store.read(SOKET_PROVIDER_ID).await {
            if !entry.models.is_empty() && !context.allow_network {
                return Ok(entry.models);
            }
            // Warm in-memory from cache even when we will refresh.
            if !context.allow_network {
                return Ok(if entry.models.is_empty() {
                    soket_seed_models()
                } else {
                    entry.models
                });
            }
        } else if !context.allow_network {
            return Ok(soket_seed_models());
        }
    } else if !context.allow_network {
        return Ok(soket_seed_models());
    }

    let api_key = context.api_key.as_deref();
    match list_openai_models(SOKET_BASE_URL, api_key, &map_opts()).await {
        Ok(models) if !models.is_empty() => {
            if let Some(store) = &context.store {
                let _ = store
                    .write(
                        SOKET_PROVIDER_ID,
                        ModelsStoreEntry {
                            models: models.clone(),
                            checked_at: now_ms(),
                        },
                    )
                    .await;
            }
            Ok(models)
        }
        Ok(_) => {
            // Empty list — keep seed.
            Ok(soket_seed_models())
        }
        Err(e) => {
            // On failure, try cache then seed.
            if let Some(store) = &context.store {
                if let Ok(Some(entry)) = store.read(SOKET_PROVIDER_ID).await {
                    if !entry.models.is_empty() {
                        return Ok(entry.models);
                    }
                }
            }
            Err(e.to_string())
        }
    }
}

/// Build the built-in Soket provider (dynamic catalog via `/v1/models`).
pub fn soket_provider() -> Provider {
    create_provider(CreateProviderOptions {
        id: SOKET_PROVIDER_ID.into(),
        name: Some(SOKET_PROVIDER_NAME.into()),
        base_url: Some(SOKET_BASE_URL.into()),
        headers: None,
        auth: ProviderAuth::api_key(env_api_key_auth(
            "Soket API key",
            SOKET_API_KEY_ENVS,
        )),
        models: soket_seed_models(),
        api: CreateProviderApi::Single(Arc::new(OpenAICompletionsAdapter::new())),
        fetch_models: Some(Arc::new(|ctx| {
            Box::pin(async move { fetch_soket_models(ctx).await })
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_contains_default() {
        let models = soket_seed_models();
        assert!(models.iter().any(|m| m.id == SOKET_DEFAULT_MODEL_ID));
        assert_eq!(models[0].provider, SOKET_PROVIDER_ID);
        assert_eq!(models[0].base_url, SOKET_BASE_URL);
    }

    #[test]
    fn provider_registers_seed() {
        let p = soket_provider();
        assert_eq!(p.id, SOKET_PROVIDER_ID);
        assert!(p.get_model(SOKET_DEFAULT_MODEL_ID).is_some());
        assert!(p.is_dynamic());
    }
}
