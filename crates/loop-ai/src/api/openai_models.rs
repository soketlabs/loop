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

#[derive(Debug, Deserialize)]
struct RemoteModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
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
    Ok(parsed
        .data
        .into_iter()
        .map(|m| map_remote_model(&m.id, m.name.as_deref(), map))
        .collect())
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

    #[test]
    fn parses_openai_list_payload() {
        let body = r#"{"object":"list","data":[{"id":"a","object":"model"},{"id":"b","name":"Bee"}]}"#;
        let parsed: ModelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[1].name.as_deref(), Some("Bee"));
    }
}
