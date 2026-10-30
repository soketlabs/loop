//! Built-in Gemini provider: the Gemini API with an API key (no Google sign-in).

use std::sync::Arc;

use serde::Deserialize;

use super::native::{native_provider, NativeProviderConfig};
use crate::api::google_generative_ai::GoogleGenerativeAiAdapter;
use crate::api::ListModelsError;
use crate::models::Provider;
use crate::types::{InputModality, Model, ModelCost, ModelCostTier, API_GOOGLE_GENERATIVE_AI};

/// Provider id.
pub const GEMINI_PROVIDER_ID: &str = "gemini";
/// Display name.
pub const GEMINI_PROVIDER_NAME: &str = "Gemini";
/// Gemini API base URL.
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Env vars checked for the API key, first wins.
pub const GEMINI_API_KEY_ENVS: &[&str] = &["GEMINI_API_KEY", "GOOGLE_API_KEY"];

/// Built-in Gemini models served from `base_url`, shown until the key's models
/// are listed.
pub fn gemini_models(base_url: &str) -> Vec<Model> {
    let mut pro = gemini_model("gemini-2.5-pro", "Gemini 2.5 Pro", base_url);
    pro.cost = ModelCost {
        input: 1.25,
        output: 10.0,
        cache_read: 0.125,
        cache_write: 0.0,
        // Prompts above 200K tokens are billed at the higher rate.
        tiers: Some(vec![ModelCostTier {
            input_tokens_above: 200_000,
            input: 2.50,
            output: 15.0,
            cache_read: 0.25,
            cache_write: 0.0,
        }]),
    };
    let mut flash = gemini_model("gemini-2.5-flash", "Gemini 2.5 Flash", base_url);
    flash.cost = ModelCost {
        input: 0.30,
        output: 2.50,
        cache_read: 0.03,
        cache_write: 0.0,
        tiers: None,
    };
    vec![pro, flash]
}

/// The built-in models at the default base URL.
pub fn gemini_seed_models() -> Vec<Model> {
    gemini_models(GEMINI_BASE_URL)
}

fn gemini_model(id: &str, name: &str, base_url: &str) -> Model {
    Model {
        id: id.into(),
        name: name.into(),
        api: API_GOOGLE_GENERATIVE_AI.into(),
        provider: GEMINI_PROVIDER_ID.into(),
        base_url: base_url.into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost::default(),
        context_window: 1_048_576,
        max_tokens: 65_536,
        headers: None,
        compat: None,
    }
}

/// Build the Gemini provider against `base_url`.
pub fn gemini_provider(base_url: &str) -> Provider {
    native_provider(NativeProviderConfig {
        id: GEMINI_PROVIDER_ID.into(),
        name: GEMINI_PROVIDER_NAME.into(),
        base_url: base_url.into(),
        api_key_env: GEMINI_API_KEY_ENVS.iter().map(|s| (*s).into()).collect(),
        adapters: vec![(
            API_GOOGLE_GENERATIVE_AI.into(),
            Arc::new(GoogleGenerativeAiAdapter::new()),
        )],
        catalog: gemini_models(base_url),
        list: Arc::new(|base_url, key| {
            Box::pin(async move {
                list_gemini_models(&base_url, &key)
                    .await
                    .map_err(|e| e.to_string())
            })
        }),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsPage {
    #[serde(default)]
    models: Vec<RemoteModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteModel {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    output_token_limit: Option<u64>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
    #[serde(default)]
    thinking: Option<bool>,
}

impl RemoteModel {
    fn id(&self) -> &str {
        self.name.strip_prefix("models/").unwrap_or(&self.name)
    }

    /// Gemini chat models that take tools (not embeddings, speech or images).
    fn is_chat(&self) -> bool {
        const NOT_CHAT: &[&str] = &["embedding", "tts", "image", "audio", "live"];
        let id = self.id();
        id.starts_with("gemini-")
            && self
                .supported_generation_methods
                .iter()
                .any(|m| m == "generateContent")
            && !NOT_CHAT.iter().any(|n| id.contains(n))
    }

    fn to_model(&self, base_url: &str) -> Model {
        let mut model = gemini_model(
            self.id(),
            self.display_name.as_deref().unwrap_or(self.id()),
            base_url,
        );
        if let Some(context) = self.input_token_limit.filter(|c| *c > 0) {
            model.context_window = context;
        }
        if let Some(max) = self.output_token_limit.filter(|m| *m > 0) {
            model.max_tokens = max;
        }
        if let Some(thinking) = self.thinking {
            model.reasoning = thinking;
        }
        model
    }
}

/// `GET {base}/models` with the API key in a header.
pub async fn list_gemini_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<Model>, ListModelsError> {
    let base_url = base_url.trim_end_matches('/');
    let resp = crate::api::http_client()
        .get(format!("{base_url}/models?pageSize=1000"))
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(ListModelsError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let page: ModelsPage = serde_json::from_str(&body)?;
    Ok(page
        .models
        .iter()
        .filter(|m| m.is_chat())
        .map(|m| m.to_model(base_url))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_keeps_chat_models_with_their_limits() {
        let page: ModelsPage = serde_json::from_str(
            r#"{"models":[
                {"name":"models/gemini-2.5-flash","displayName":"Gemini 2.5 Flash",
                 "inputTokenLimit":1048576,"outputTokenLimit":65536,"thinking":true,
                 "supportedGenerationMethods":["generateContent","countTokens"]},
                {"name":"models/gemini-embedding-001","supportedGenerationMethods":["embedContent"]},
                {"name":"models/gemini-2.5-flash-preview-tts","supportedGenerationMethods":["generateContent"]}
            ]}"#,
        )
        .unwrap();
        let kept: Vec<_> = page.models.iter().filter(|m| m.is_chat()).collect();
        assert_eq!(kept.len(), 1);
        let m = kept[0].to_model("http://x/v1beta");
        assert_eq!(m.id, "gemini-2.5-flash");
        assert_eq!((m.context_window, m.max_tokens), (1_048_576, 65_536));
        assert!(m.reasoning);
        assert_eq!(m.api, API_GOOGLE_GENERATIVE_AI);
    }
}
