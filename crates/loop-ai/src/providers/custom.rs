//! OpenAI-compatible custom provider builder.

use std::sync::Arc;

use crate::api::openai_completions::OpenAICompletionsAdapter;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::models::{create_provider, CreateProviderApi, CreateProviderOptions, Provider};
use crate::types::{
    InputModality, Model, ModelCost, OpenAICompletionsCompat, API_OPENAI_COMPLETIONS,
};

/// Spec for a model registered on a custom OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct CustomModelSpec {
    /// Model id.
    pub id: String,
    /// Display name (defaults to id).
    pub name: Option<String>,
    /// Supports reasoning.
    pub reasoning: bool,
    /// Input modalities (defaults to text).
    pub input: Option<Vec<InputModality>>,
    /// Pricing (defaults to zero).
    pub cost: Option<ModelCost>,
    /// Context window (default 128_000).
    pub context_window: Option<u64>,
    /// Max output tokens (default 16_384).
    pub max_tokens: Option<u64>,
    /// Compat overrides.
    pub compat: Option<OpenAICompletionsCompat>,
}

impl CustomModelSpec {
    /// Create a minimal text model spec.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            reasoning: false,
            input: None,
            cost: None,
            context_window: None,
            max_tokens: None,
            compat: None,
        }
    }

    /// Enable reasoning.
    pub fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Set compat overrides.
    pub fn with_compat(mut self, compat: OpenAICompletionsCompat) -> Self {
        self.compat = Some(compat);
        self
    }
}

/// Configuration for an OpenAI-compatible custom provider.
#[derive(Debug, Clone)]
pub struct CustomProviderConfig {
    /// Provider id (e.g. `ollama`, `vllm`, `my-gateway`).
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Base URL including version path if needed (e.g. `http://localhost:11434/v1`).
    pub base_url: String,
    /// Env vars to try for the API key. Empty / omitted → keyless.
    pub api_key_env: Vec<String>,
    /// Models to register.
    pub models: Vec<CustomModelSpec>,
    /// Default headers.
    pub headers: Option<std::collections::HashMap<String, String>>,
}

/// Build a provider wired to the OpenAI Completions adapter.
pub fn custom_provider(config: CustomProviderConfig) -> Provider {
    let provider_id = config.id.clone();
    let models: Vec<Model> = config
        .models
        .into_iter()
        .map(|spec| Model {
            id: spec.id.clone(),
            name: spec.name.unwrap_or_else(|| spec.id.clone()),
            api: API_OPENAI_COMPLETIONS.to_string(),
            provider: provider_id.clone(),
            base_url: config.base_url.clone(),
            reasoning: spec.reasoning,
            thinking_level_map: None,
            input: spec.input.unwrap_or_else(|| vec![InputModality::Text]),
            cost: spec.cost.unwrap_or_default(),
            context_window: spec.context_window.unwrap_or(128_000),
            max_tokens: spec.max_tokens.unwrap_or(16_384),
            headers: None,
            compat: spec.compat,
        })
        .collect();

    let auth = if config.api_key_env.is_empty() {
        ProviderAuth::keyless(format!("{} (keyless)", provider_id))
    } else {
        let refs: Vec<&str> = config.api_key_env.iter().map(|s| s.as_str()).collect();
        ProviderAuth::api_key(env_api_key_auth(format!("{provider_id} API key"), &refs))
    };

    create_provider(CreateProviderOptions {
        id: provider_id,
        name: config.name,
        base_url: Some(config.base_url),
        headers: config.headers,
        auth,
        models,
        api: CreateProviderApi::Single(Arc::new(OpenAICompletionsAdapter::new())),
    })
}
