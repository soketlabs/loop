//! OpenAI-compatible `GET /v1/models` catalog client.

use serde::Deserialize;
use thiserror::Error;

use crate::types::{InputModality, Model, ModelCost, API_OPENAI_COMPLETIONS};

/// Errors listing remote models.
#[derive(Debug, Error)]
pub enum ListModelsError {
    /// HTTP / transport failure.
    #[error("list models http: {0}")]
    Http(#[from] reqwest::Error),
    /// Non-success status.
    #[error("list models status {status}: {body}")]
    Status {
        /// HTTP status.
        status: u16,
        /// Response body.
        body: String,
    },
    /// JSON parse failure.
    #[error("list models json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<RemoteModel>,
}

/// One entry of a `/models` response. Only `id` is standard; the rest are optional
/// extensions (OpenRouter publishes context, limits, pricing and modalities).
#[derive(Debug, Default, Deserialize)]
struct RemoteModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    top_provider: Option<RemoteTopProvider>,
    #[serde(default)]
    pricing: Option<RemotePricing>,
    #[serde(default)]
    architecture: Option<RemoteArchitecture>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteTopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

/// USD per token, as decimal strings (OpenRouter).
#[derive(Debug, Default, Deserialize)]
struct RemotePricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteArchitecture {
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

impl RemoteModel {
    /// Map into a loop [`Model`], preferring published metadata over `opts` defaults.
    fn to_model(&self, opts: &MapRemoteModelOptions) -> Model {
        let mut model = map_remote_model(&self.id, self.name.as_deref(), opts);
        if let Some(context) = self.context_length.filter(|c| *c > 0) {
            model.context_window = context;
        }
        if let Some(max) = self
            .top_provider
            .as_ref()
            .and_then(|t| t.max_completion_tokens)
            .filter(|m| *m > 0)
        {
            model.max_tokens = max;
        }
        if let Some(pricing) = &self.pricing {
            model.cost = pricing.to_cost();
        }
        if let Some(modalities) = self
            .architecture
            .as_ref()
            .and_then(|a| a.input_modalities.as_ref())
        {
            if modalities.iter().any(|m| m == "image") {
                model.input = vec![InputModality::Text, InputModality::Image];
            }
        }
        if let Some(params) = &self.supported_parameters {
            model.reasoning = params
                .iter()
                .any(|p| p == "reasoning" || p == "include_reasoning");
        }
        model
    }
}

impl RemotePricing {
    /// Per-token USD strings → per-million [`ModelCost`] (unparseable or negative = 0).
    fn to_cost(&self) -> ModelCost {
        let per_million = |v: &Option<String>| {
            v.as_deref()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|p| p.is_finite() && *p > 0.0)
                .map_or(0.0, |p| p * 1_000_000.0)
        };
        ModelCost {
            input: per_million(&self.prompt),
            output: per_million(&self.completion),
            cache_read: per_million(&self.input_cache_read),
            cache_write: per_million(&self.input_cache_write),
            tiers: None,
        }
    }
}

/// Options for mapping a remote id into a [`Model`].
#[derive(Debug, Clone)]
pub struct MapRemoteModelOptions {
    /// Provider id.
    pub provider: String,
    /// Base URL for chat completions.
    pub base_url: String,
    /// Default context window.
    pub context_window: u64,
    /// Default max tokens.
    pub max_tokens: u64,
    /// Whether models support reasoning by default.
    pub reasoning: bool,
}

impl Default for MapRemoteModelOptions {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: false,
        }
    }
}

/// Map a remote model id into a loop [`Model`].
pub fn map_remote_model(id: &str, name: Option<&str>, opts: &MapRemoteModelOptions) -> Model {
    Model {
        id: id.to_string(),
        name: name.unwrap_or(id).to_string(),
        api: API_OPENAI_COMPLETIONS.to_string(),
        provider: opts.provider.clone(),
        base_url: opts.base_url.clone(),
        reasoning: opts.reasoning,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: opts.context_window,
        max_tokens: opts.max_tokens,
        headers: None,
        compat: None,
    }
}

/// Fetch models from an OpenAI-compatible `/models` endpoint.
pub async fn list_openai_models(
    base_url: &str,
    api_key: Option<&str>,
    map: &MapRemoteModelOptions,
) -> Result<Vec<Model>, ListModelsError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = super::http::http_client();
    let mut req = client.get(&url);
    if let Some(key) = api_key {
        if !key.is_empty() {
            req = req.bearer_auth(key);
        }
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(ListModelsError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: ModelsResponse = serde_json::from_str(&body)?;
    Ok(parsed.data.iter().map(|m| m.to_model(map)).collect())
}

/// Check an API key against an authenticated endpoint (`GET {base}{path}`, bearer auth).
/// Needed where `/models` is public and so can't tell a bad key from a good one.
pub async fn verify_api_key(base_url: &str, path: &str, api_key: &str) -> Result<(), ListModelsError> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let resp = super::http::http_client()
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    Err(ListModelsError::Status {
        status: status.as_u16(),
        body: resp.text().await.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_remote_model() {
        let opts = MapRemoteModelOptions {
            provider: "soket".into(),
            base_url: "https://api.tensorstudio.ai/v1".into(),
            ..Default::default()
        };
        let m = map_remote_model("qwen3-30b", Some("Qwen 3 30B"), &opts);
        assert_eq!(m.id, "qwen3-30b");
        assert_eq!(m.provider, "soket");
        assert_eq!(m.api, API_OPENAI_COMPLETIONS);
    }

    fn opts() -> MapRemoteModelOptions {
        MapRemoteModelOptions {
            provider: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn maps_openrouter_metadata() {
        let body = r#"{"data":[{
            "id":"anthropic/claude-sonnet-4.5","name":"Anthropic: Claude Sonnet 4.5",
            "context_length":1000000,
            "top_provider":{"max_completion_tokens":64000},
            "pricing":{"prompt":"0.000003","completion":"0.000015","input_cache_read":"0.0000003","request":"0"},
            "architecture":{"input_modalities":["text","image"]},
            "supported_parameters":["tools","reasoning","max_tokens"]
        }]}"#;
        let parsed: ModelsResponse = serde_json::from_str(body).unwrap();
        let m = parsed.data[0].to_model(&opts());
        assert_eq!(m.id, "anthropic/claude-sonnet-4.5");
        assert_eq!(m.name, "Anthropic: Claude Sonnet 4.5");
        assert_eq!(m.context_window, 1_000_000);
        assert_eq!(m.max_tokens, 64_000);
        assert!((m.cost.input - 3.0).abs() < 1e-9);
        assert!((m.cost.output - 15.0).abs() < 1e-9);
        assert!((m.cost.cache_read - 0.3).abs() < 1e-9);
        assert_eq!(m.input, vec![InputModality::Text, InputModality::Image]);
        assert!(m.reasoning);
    }

    #[test]
    fn plain_openai_entries_keep_defaults() {
        let body = r#"{"data":[{"id":"gpt-4o","object":"model","owned_by":"openai"}]}"#;
        let parsed: ModelsResponse = serde_json::from_str(body).unwrap();
        let o = opts();
        let m = parsed.data[0].to_model(&o);
        assert_eq!(m.context_window, o.context_window);
        assert_eq!(m.max_tokens, o.max_tokens);
        assert_eq!(m.cost, ModelCost::default());
        assert_eq!(m.reasoning, o.reasoning);
    }

    #[test]
    fn bad_pricing_strings_become_zero() {
        let pricing = RemotePricing {
            prompt: Some("-1".into()),
            completion: Some("abc".into()),
            ..Default::default()
        };
        assert_eq!(pricing.to_cost(), ModelCost::default());
    }

    #[test]
    fn parses_openai_list_payload() {
        let body = r#"{"object":"list","data":[{"id":"a","object":"model"},{"id":"b","name":"Bee"}]}"#;
        let parsed: ModelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[1].name.as_deref(), Some("Bee"));
    }
}
