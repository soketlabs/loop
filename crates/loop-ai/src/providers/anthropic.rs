//! Built-in Anthropic provider: the native Messages API with an API key.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::native::{native_provider, NativeProviderConfig};
use crate::api::anthropic_messages::{AnthropicMessagesAdapter, ANTHROPIC_VERSION};
use crate::api::ListModelsError;
use crate::models::Provider;
use crate::types::{InputModality, Model, ModelCost, ModelCostTier, API_ANTHROPIC_MESSAGES};

/// Provider id.
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
/// Display name.
pub const ANTHROPIC_PROVIDER_NAME: &str = "Anthropic";
/// Messages API base URL.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
/// Env vars checked for the API key.
pub const ANTHROPIC_API_KEY_ENVS: &[&str] = &["ANTHROPIC_API_KEY"];

/// `(id, name, context, max output, $/M input, output, cache read, cache write)`.
type Entry = (&'static str, &'static str, u64, u64, f64, f64, f64, f64);

/// Current models with list prices (5-minute cache writes at 1.25× input).
const CATALOG: &[Entry] = &[
    (
        "claude-opus-5-5",
        "Claude Opus 5.5",
        1_000_000,
        128_000,
        4.0,
        20.0,
        0.20,
        5.0,
    ),
    (
        "claude-sonnet-5-5",
        "Claude Sonnet 5.5",
        1_000_000,
        128_000,
        2.0,
        10.0,
        0.20,
        2.5,
    ),
    (
        "claude-haiku-5-5",
        "Claude Haiku 5.5",
        1_000_000,
        128_000,
        0.10,
        0.50,
        0.01,
        0.125,
    ),
    (
        "claude-fable-5-1",
        "Claude Fable 5.1",
        1_000_000,
        128_000,
        10.0,
        50.0,
        0.25,
        12.5,
    ),
    (
        "claude-opus-5",
        "Claude Opus 5",
        1_000_000,
        128_000,
        5.0,
        25.0,
        0.50,
        6.25,
    ),
    (
        "claude-sonnet-5",
        "Claude Sonnet 5",
        1_000_000,
        128_000,
        2.0,
        10.0,
        0.20,
        2.5,
    ),
    (
        "claude-opus-4-8",
        "Claude Opus 4.8",
        1_000_000,
        128_000,
        5.0,
        25.0,
        0.50,
        6.25,
    ),
    (
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        1_000_000,
        128_000,
        3.0,
        15.0,
        0.30,
        3.75,
    ),
    (
        "claude-haiku-4-5",
        "Claude Haiku 4.5",
        200_000,
        64_000,
        1.0,
        5.0,
        0.10,
        1.25,
    ),
];

/// Built-in Anthropic models served from `base_url`.
pub fn anthropic_models(base_url: &str) -> Vec<Model> {
    CATALOG
        .iter()
        .map(
            |&(id, name, context_window, max_tokens, input, output, cache_read, cache_write)| {
                let mut model = anthropic_model(id, name, base_url);
                model.context_window = context_window;
                model.max_tokens = max_tokens;
                model.cost = ModelCost {
                    input,
                    output,
                    cache_read,
                    cache_write,
                    tiers: None,
                };
                if id == "claude-haiku-5-5" {
                    // Prompts above 100K tokens are billed at the higher rate.
                    model.cost.tiers = Some(vec![ModelCostTier {
                        input_tokens_above: 100_000,
                        input: 0.50,
                        output: 2.50,
                        cache_read: 0.05,
                        cache_write: 0.625,
                    }]);
                }
                model
            },
        )
        .collect()
}

/// The built-in models at the default base URL.
pub fn anthropic_seed_models() -> Vec<Model> {
    anthropic_models(ANTHROPIC_BASE_URL)
}

fn anthropic_model(id: &str, name: &str, base_url: &str) -> Model {
    Model {
        id: id.into(),
        name: name.into(),
        api: API_ANTHROPIC_MESSAGES.into(),
        provider: ANTHROPIC_PROVIDER_ID.into(),
        base_url: base_url.into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 64_000,
        headers: None,
        compat: None,
    }
}

/// Build the Anthropic provider against `base_url`.
pub fn anthropic_provider(base_url: &str) -> Provider {
    native_provider(NativeProviderConfig {
        id: ANTHROPIC_PROVIDER_ID.into(),
        name: ANTHROPIC_PROVIDER_NAME.into(),
        base_url: base_url.into(),
        api_key_env: ANTHROPIC_API_KEY_ENVS.iter().map(|s| (*s).into()).collect(),
        adapters: vec![(
            API_ANTHROPIC_MESSAGES.into(),
            Arc::new(AnthropicMessagesAdapter::new()),
        )],
        catalog: anthropic_models(base_url),
        list: Arc::new(|base_url, key| {
            Box::pin(async move {
                list_anthropic_models(&base_url, &key)
                    .await
                    .map_err(|e| e.to_string())
            })
        }),
    })
}

#[derive(Debug, Deserialize)]
struct ModelsPage {
    data: Vec<RemoteModel>,
}

#[derive(Debug, Deserialize)]
struct RemoteModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Option<Value>,
}

impl RemoteModel {
    fn to_model(&self, base_url: &str) -> Model {
        let mut model = anthropic_model(
            &self.id,
            self.display_name.as_deref().unwrap_or(&self.id),
            base_url,
        );
        if let Some(context) = self.max_input_tokens.filter(|c| *c > 0) {
            model.context_window = context;
        }
        if let Some(max) = self.max_tokens.filter(|m| *m > 0) {
            model.max_tokens = max;
        }
        let supported = |path: &str| {
            self.capabilities
                .as_ref()
                .and_then(|c| c.pointer(path))
                .and_then(Value::as_bool)
        };
        if supported("/image_input/supported") == Some(false) {
            model.input = vec![InputModality::Text];
        }
        if let Some(thinking) = supported("/thinking/supported") {
            model.reasoning = thinking;
        }
        model
    }
}

/// `GET {base}/models` with the API key.
pub async fn list_anthropic_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<Model>, ListModelsError> {
    let base_url = base_url.trim_end_matches('/');
    let resp = crate::api::http_client()
        .get(format!("{base_url}/models?limit=1000"))
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
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
        .data
        .iter()
        .filter(|m| m.id.starts_with("claude-"))
        .map(|m| m.to_model(base_url))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_entries_take_published_limits_and_capabilities() {
        let page: ModelsPage = serde_json::from_str(
            r#"{"data":[{"id":"claude-new","display_name":"Claude New","max_input_tokens":500000,
            "max_tokens":32000,"capabilities":{"image_input":{"supported":false},
            "thinking":{"supported":true}}}],"has_more":false}"#,
        )
        .unwrap();
        let m = page.data[0].to_model("http://x/v1");
        assert_eq!(
            (m.name.as_str(), m.context_window, m.max_tokens),
            ("Claude New", 500_000, 32_000)
        );
        assert_eq!(m.input, vec![InputModality::Text]);
        assert!(m.reasoning);
        assert_eq!(m.api, API_ANTHROPIC_MESSAGES);
    }

    #[test]
    fn catalog_is_priced_and_leads_with_opus() {
        let models = anthropic_seed_models();
        assert_eq!(models[0].id, "claude-opus-5-5");
        for m in &models {
            assert!(
                m.cost.input > 0.0 && m.cost.cache_read < m.cost.input,
                "{}",
                m.id
            );
            assert_eq!(m.provider, ANTHROPIC_PROVIDER_ID);
        }
    }
}
