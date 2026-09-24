//! Built-in Soket / TensorStudio provider with dynamic model catalog.

use crate::models::Provider;
use crate::types::{InputModality, Model, ModelCost, API_OPENAI_COMPLETIONS};

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

/// Build the built-in Soket provider (dynamic catalog via `/v1/models`).
pub fn soket_provider() -> Provider {
    super::presets::provider_preset(SOKET_PROVIDER_ID)
        .expect("Soket preset")
        .provider()
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
