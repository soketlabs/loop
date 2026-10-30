//! One builder for providers with their own wire API (Anthropic, Gemini).
//!
//! Like the OpenAI-compatible builder, each provider gets a dynamic catalog:
//! models-store cache → the provider's model listing → cache write, falling back
//! to cache and then to the built-in catalog. Listed models take their pricing and
//! limits from the built-in catalog when it knows them.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::models::{
    create_provider, CreateProviderApi, CreateProviderOptions, Provider, RefreshModelsContext,
    SharedApiAdapter,
};
use crate::models_store::ModelsStoreEntry;
use crate::types::Model;
use crate::utils::now_ms;

/// Lists the models an API key can use: `(base_url, api_key)`.
pub type ListModelsFn = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<Vec<Model>, String>> + Send>>
        + Send
        + Sync,
>;

/// Everything needed to build a native [`Provider`].
#[derive(Clone)]
pub struct NativeProviderConfig {
    /// Provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Base URL including the version path.
    pub base_url: String,
    /// Env vars checked for the API key, first wins.
    pub api_key_env: Vec<String>,
    /// Adapters by wire api; models route by their `api`.
    pub adapters: Vec<(String, SharedApiAdapter)>,
    /// Built-in models: shown offline and used to enrich listed entries.
    pub catalog: Vec<Model>,
    /// Model listing.
    pub list: ListModelsFn,
}

impl NativeProviderConfig {
    /// Listed models, preferring the built-in entry with the same id.
    fn merge_with_catalog(&self, listed: Vec<Model>) -> Vec<Model> {
        listed
            .into_iter()
            .map(|m| {
                self.catalog
                    .iter()
                    .find(|c| c.id == m.id)
                    .cloned()
                    .unwrap_or(m)
            })
            .collect()
    }
}

/// Build a native provider with a dynamic catalog.
pub fn native_provider(config: NativeProviderConfig) -> Provider {
    let envs: Vec<&str> = config.api_key_env.iter().map(String::as_str).collect();
    let auth = ProviderAuth::api_key(env_api_key_auth(format!("{} API key", config.name), &envs));
    let adapters: HashMap<String, SharedApiAdapter> = config.adapters.iter().cloned().collect();
    let config = Arc::new(config);
    let fetch_config = Arc::clone(&config);
    create_provider(CreateProviderOptions {
        id: config.id.clone(),
        name: Some(config.name.clone()),
        base_url: Some(config.base_url.clone()),
        headers: None,
        auth,
        models: config.catalog.clone(),
        api: CreateProviderApi::ByApi(adapters),
        fetch_models: Some(Arc::new(move |ctx| {
            let config = Arc::clone(&fetch_config);
            Box::pin(async move { fetch_models(&config, ctx).await })
        })),
    })
}

async fn cached(config: &NativeProviderConfig, ctx: &RefreshModelsContext) -> Option<Vec<Model>> {
    let store = ctx.store.as_ref()?;
    match store.read(&config.id).await {
        Ok(Some(entry)) if !entry.models.is_empty() => Some(entry.models),
        _ => None,
    }
}

/// Cache when offline (or no key yet), else network with cache/catalog fallback.
async fn fetch_models(
    config: &NativeProviderConfig,
    ctx: RefreshModelsContext,
) -> Result<Vec<Model>, String> {
    let key = ctx.api_key.clone().filter(|k| !k.is_empty());
    let Some(key) = key.filter(|_| ctx.allow_network) else {
        return Ok(cached(config, &ctx)
            .await
            .unwrap_or_else(|| config.catalog.clone()));
    };

    match (config.list)(config.base_url.clone(), key).await {
        Ok(listed) => {
            let models = config.merge_with_catalog(listed);
            if models.is_empty() {
                return Ok(config.catalog.clone());
            }
            if let Some(store) = &ctx.store {
                let _ = store
                    .write(
                        &config.id,
                        ModelsStoreEntry {
                            models: models.clone(),
                            checked_at: now_ms(),
                        },
                    )
                    .await;
            }
            Ok(models)
        }
        Err(err) => match cached(config, &ctx).await {
            Some(models) if !ctx.force => Ok(models),
            _ => Err(err),
        },
    }
}
