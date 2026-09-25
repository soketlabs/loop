//! One builder for every OpenAI-compatible provider (Soket, OpenRouter, OpenAI, custom).
//!
//! Each provider gets a dynamic catalog: models-store cache → `GET {base}/models` →
//! cache write, falling back to cache and then to static `fallback` models.

use std::collections::HashMap;
use std::sync::Arc;

use crate::api::openai_completions::OpenAICompletionsAdapter;
use crate::api::openai_models::{list_openai_models, MapRemoteModelOptions};
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::models::{
    create_provider, CreateProviderApi, CreateProviderOptions, Provider, RefreshModelsContext,
};
use crate::models_store::ModelsStoreEntry;
use crate::types::Model;
use crate::utils::now_ms;

/// Keeps only model ids a provider can chat with (e.g. drops embeddings).
pub type ModelFilter = fn(&str) -> bool;

/// Defaults for models the provider's `/models` endpoint doesn't describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelDefaults {
    /// Context window in tokens.
    pub context_window: u64,
    /// Max output tokens.
    pub max_tokens: u64,
    /// Whether models accept reasoning parameters.
    pub reasoning: bool,
}

impl Default for ModelDefaults {
    fn default() -> Self {
        Self {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: false,
        }
    }
}

/// Everything needed to build an OpenAI-compatible [`Provider`].
#[derive(Clone)]
pub struct OpenAiCompatibleConfig {
    /// Provider id used in settings, credentials and model specs.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Base URL including the version path, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Env vars checked for the API key. Empty means the provider works without a key
    /// (a key saved with `/login` is still sent).
    pub api_key_env: Vec<String>,
    /// Extra request headers.
    pub headers: Option<HashMap<String, String>>,
    /// Defaults for listed models.
    pub defaults: ModelDefaults,
    /// Hand-written models (e.g. from `models.json`): always listed, and preferred over
    /// listed entries with the same id.
    pub pinned: Vec<Model>,
    /// Shown only until the first successful listing (or when nothing is cached).
    pub fallback: Vec<Model>,
    /// Optional filter over listed model ids.
    pub model_filter: Option<ModelFilter>,
}

impl OpenAiCompatibleConfig {
    fn requires_key(&self) -> bool {
        !self.api_key_env.is_empty()
    }

    fn map_opts(&self) -> MapRemoteModelOptions {
        MapRemoteModelOptions {
            provider: self.id.clone(),
            base_url: self.base_url.clone(),
            context_window: self.defaults.context_window,
            max_tokens: self.defaults.max_tokens,
            reasoning: self.defaults.reasoning,
        }
    }

    /// Listed models, filtered, with pinned entries taking precedence by id.
    fn merge_with_pinned(&self, listed: Vec<Model>) -> Vec<Model> {
        let mut out: Vec<Model> = listed
            .into_iter()
            .filter(|m| self.model_filter.is_none_or(|keep| keep(&m.id)))
            .map(|m| {
                self.pinned
                    .iter()
                    .find(|p| p.id == m.id)
                    .cloned()
                    .unwrap_or(m)
            })
            .collect();
        for pinned in &self.pinned {
            if !out.iter().any(|m| m.id == pinned.id) {
                out.push(pinned.clone());
            }
        }
        out
    }

    /// What to show when nothing was listed or cached.
    fn offline_models(&self) -> Vec<Model> {
        self.merge_with_pinned(self.fallback.clone())
    }
}

/// Build a provider wired to the OpenAI Completions adapter with a dynamic catalog.
pub fn openai_compatible_provider(config: OpenAiCompatibleConfig) -> Provider {
    let auth = if config.requires_key() {
        let envs: Vec<&str> = config.api_key_env.iter().map(String::as_str).collect();
        ProviderAuth::api_key(env_api_key_auth(format!("{} API key", config.name), &envs))
    } else {
        ProviderAuth::keyless(format!("{} (keyless)", config.name))
    };
    let config = Arc::new(config);
    let fetch_config = Arc::clone(&config);
    create_provider(CreateProviderOptions {
        id: config.id.clone(),
        name: Some(config.name.clone()),
        base_url: Some(config.base_url.clone()),
        headers: config.headers.clone(),
        auth,
        models: config.offline_models(),
        api: CreateProviderApi::Single(Arc::new(OpenAICompletionsAdapter::new())),
        fetch_models: Some(Arc::new(move |ctx| {
            let config = Arc::clone(&fetch_config);
            Box::pin(async move { fetch_models(&config, ctx).await })
        })),
    })
}

async fn cached(config: &OpenAiCompatibleConfig, ctx: &RefreshModelsContext) -> Option<Vec<Model>> {
    let store = ctx.store.as_ref()?;
    match store.read(&config.id).await {
        Ok(Some(entry)) if !entry.models.is_empty() => Some(entry.models),
        _ => None,
    }
}

/// Cache when offline (or no key yet), else network with cache/seed fallback.
async fn fetch_models(
    config: &OpenAiCompatibleConfig,
    ctx: RefreshModelsContext,
) -> Result<Vec<Model>, String> {
    let has_key = ctx.api_key.as_deref().is_some_and(|k| !k.is_empty());
    let offline = !ctx.allow_network || (config.requires_key() && !has_key);
    if offline {
        return Ok(match cached(config, &ctx).await {
            Some(models) => config.merge_with_pinned(models),
            None => config.offline_models(),
        });
    }

    match list_openai_models(&config.base_url, ctx.api_key.as_deref(), &config.map_opts()).await {
        Ok(listed) => {
            let models = config.merge_with_pinned(listed);
            if models.is_empty() {
                return Ok(config.offline_models());
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
            Some(models) if !ctx.force => Ok(config.merge_with_pinned(models)),
            _ => Err(err.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::types::{InputModality, ModelCost, API_OPENAI_COMPLETIONS};

    use super::*;

    fn model(id: &str, name: &str) -> Model {
        Model {
            id: id.into(),
            name: name.into(),
            api: API_OPENAI_COMPLETIONS.into(),
            provider: "p".into(),
            base_url: "http://x".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            headers: None,
            compat: None,
        }
    }

    fn config(pinned: Vec<Model>, model_filter: Option<ModelFilter>) -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig {
            id: "p".into(),
            name: "P".into(),
            base_url: "http://x".into(),
            api_key_env: vec![],
            headers: None,
            defaults: ModelDefaults::default(),
            pinned,
            fallback: vec![],
            model_filter,
        }
    }

    #[test]
    fn pinned_entries_win_and_unlisted_pins_are_kept() {
        let cfg = config(
            vec![model("a", "hand-written"), model("z", "pinned-only")],
            None,
        );
        let merged = cfg.merge_with_pinned(vec![model("a", "listed"), model("b", "listed")]);
        let names: Vec<_> = merged
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [("a", "hand-written"), ("b", "listed"), ("z", "pinned-only")]
        );
    }

    #[test]
    fn fallback_is_only_used_when_nothing_is_listed() {
        let mut cfg = config(vec![], None);
        cfg.fallback = vec![model("seed", "")];
        assert_eq!(cfg.offline_models()[0].id, "seed");
        let merged = cfg.merge_with_pinned(vec![model("live", "")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "live");
    }

    #[test]
    fn filter_drops_listed_ids() {
        let cfg = config(vec![], Some(|id| !id.contains("embedding")));
        let merged = cfg.merge_with_pinned(vec![model("gpt-x", ""), model("text-embedding-3", "")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "gpt-x");
    }

    #[tokio::test]
    async fn keyed_provider_without_key_stays_offline() {
        let mut cfg = config(vec![], None);
        cfg.fallback = vec![model("seed", "")];
        cfg.api_key_env = vec!["SOME_UNSET_ENV".into()];
        // Unroutable base URL: any network attempt would error instead of returning seed.
        cfg.base_url = "http://127.0.0.1:9/v1".into();
        let ctx = RefreshModelsContext {
            api_key: None,
            store: None,
            allow_network: true,
            force: true,
        };
        let models = fetch_models(&cfg, ctx).await.unwrap();
        assert_eq!(models[0].id, "seed");
    }
}
