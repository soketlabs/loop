//! Built-in providers offered by `/login`: OpenAI-compatible endpoints and
//! providers with their own wire API.

use std::collections::HashMap;

use super::anthropic::{
    anthropic_provider, anthropic_seed_models, ANTHROPIC_API_KEY_ENVS, ANTHROPIC_BASE_URL,
    ANTHROPIC_PROVIDER_ID, ANTHROPIC_PROVIDER_NAME,
};
use super::gemini::{
    gemini_provider, gemini_seed_models, GEMINI_API_KEY_ENVS, GEMINI_BASE_URL, GEMINI_PROVIDER_ID,
    GEMINI_PROVIDER_NAME,
};
use super::openai_compatible::{
    openai_compatible_provider, ModelDefaults, ModelFilter, ModelTraits, OpenAiCompatibleConfig,
};
use super::soket::{
    soket_seed_models, SOKET_API_KEY_ENVS, SOKET_BASE_URL, SOKET_PROVIDER_ID, SOKET_PROVIDER_NAME,
};
use crate::models::Provider;
use crate::types::{InputModality, Model, API_OPENAI_COMPLETIONS, API_OPENAI_RESPONSES};

/// OpenRouter provider id.
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
/// OpenAI provider id.
pub const OPENAI_PROVIDER_ID: &str = "openai";

/// The wire API a preset speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetWire {
    /// OpenAI Chat Completions, for any compatible server.
    OpenAiCompatible,
    /// OpenAI Responses for listed models (Chat Completions stays available for
    /// pinned models).
    OpenAiResponses,
    /// Anthropic Messages.
    Anthropic,
    /// Gemini API (API key only).
    Gemini,
}

/// A provider Loop knows how to connect to with just an API key.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    /// Provider id (settings, credentials, `provider/model` specs).
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// One-line description for the `/login` picker.
    pub description: &'static str,
    /// Wire API.
    pub wire: PresetWire,
    /// Base URL including the version path.
    pub base_url: &'static str,
    /// Env vars checked for the API key, first wins.
    pub api_key_envs: &'static [&'static str],
    /// Extra request headers.
    pub headers: &'static [(&'static str, &'static str)],
    /// What a key looks like, for prompts (e.g. `sk-or-…`).
    pub key_hint: &'static str,
    /// Lead-in shown before `key_url` in the login prompt (e.g. `get one at`).
    pub key_cta: &'static str,
    /// Where to create a key.
    pub key_url: &'static str,
    /// Defaults for listed models without metadata.
    pub defaults: ModelDefaults,
    /// Keeps chat-capable model ids.
    pub model_filter: Option<ModelFilter>,
    /// Fills in what the provider knows about listed models.
    pub model_traits: Option<ModelTraits>,
    /// Models shown until the first successful listing.
    pub fallback_models: Option<fn() -> Vec<Model>>,
    /// Authenticated endpoint (relative to `base_url`) used to verify a key, for
    /// providers whose `/models` is public.
    pub key_check_path: Option<&'static str>,
}

impl ProviderPreset {
    /// Build the provider.
    pub fn provider(&self) -> Provider {
        match self.wire {
            PresetWire::OpenAiCompatible | PresetWire::OpenAiResponses => {
                openai_compatible_provider(self.compatible_config())
            }
            PresetWire::Anthropic => anthropic_provider(self.base_url),
            PresetWire::Gemini => gemini_provider(self.base_url),
        }
    }

    /// Reject a key the provider doesn't accept, where `/models` can't tell.
    pub async fn verify_key(&self, api_key: &str) -> Result<(), String> {
        match self.key_check_path {
            Some(path) => crate::api::verify_api_key(self.base_url, path, api_key)
                .await
                .map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Settings for the OpenAI-compatible builder (OpenAI-family presets).
    pub fn compatible_config(&self) -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig {
            id: self.id.into(),
            name: self.name.into(),
            base_url: self.base_url.into(),
            api_key_env: self.api_key_envs.iter().map(|s| (*s).into()).collect(),
            headers: (!self.headers.is_empty()).then(|| {
                self.headers
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect::<HashMap<String, String>>()
            }),
            defaults: self.defaults,
            pinned: vec![],
            fallback: self.fallback_models.map(|f| f()).unwrap_or_default(),
            model_filter: self.model_filter,
            api: match self.wire {
                PresetWire::OpenAiResponses => API_OPENAI_RESPONSES,
                _ => API_OPENAI_COMPLETIONS,
            }
            .into(),
            model_traits: self.model_traits,
        }
    }
}

/// OpenAI's `/models` lists every model family; keep the ones chat completions serve.
fn openai_chat_model(id: &str) -> bool {
    const CHAT_PREFIXES: &[&str] = &["gpt-", "o1", "o3", "o4", "chatgpt-"];
    const NOT_CHAT: &[&str] = &[
        "audio",
        "realtime",
        "tts",
        "transcribe",
        "image",
        "search",
        "embedding",
        "moderation",
        "instruct",
    ];
    CHAT_PREFIXES.iter().any(|p| id.starts_with(p)) && !NOT_CHAT.iter().any(|n| id.contains(n))
}

/// Reasoning and vision by model family; OpenAI's listing doesn't say.
fn openai_model_traits(model: &mut Model) {
    let id = model.id.as_str();
    // o1, o3, o4-mini…
    let o_series = id
        .strip_prefix('o')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit());
    // gpt-5 and later.
    let gpt_major = id
        .strip_prefix("gpt-")
        .and_then(|rest| rest.split(['.', '-']).next())
        .and_then(|major| major.parse::<u32>().ok());
    model.reasoning = o_series || gpt_major.is_some_and(|major| major >= 5);
    if model.reasoning || id.starts_with("gpt-4o") || id.starts_with("gpt-4.1") {
        model.input = vec![InputModality::Text, InputModality::Image];
    }
}

/// Presets in `/login` order.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: SOKET_PROVIDER_ID,
        name: SOKET_PROVIDER_NAME,
        description: "Soket's inference Studio",
        wire: PresetWire::OpenAiCompatible,
        base_url: SOKET_BASE_URL,
        api_key_envs: SOKET_API_KEY_ENVS,
        headers: &[],
        key_hint: "your Soket API key",
        key_cta: "get your free credits at",
        key_url: "http://console.soket.ai/",
        defaults: ModelDefaults {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: true,
        },
        model_filter: None,
        model_traits: None,
        fallback_models: Some(soket_seed_models),
        key_check_path: None,
    },
    ProviderPreset {
        id: OPENROUTER_PROVIDER_ID,
        name: "OpenRouter",
        description: "Hundreds of models with one key",
        wire: PresetWire::OpenAiCompatible,
        base_url: "https://openrouter.ai/api/v1",
        api_key_envs: &["OPENROUTER_API_KEY"],
        headers: &[
            ("HTTP-Referer", "https://github.com/soketlabs/loop"),
            ("X-Title", "Loop"),
        ],
        key_hint: "sk-or-…",
        key_cta: "get one at",
        key_url: "https://openrouter.ai/keys",
        defaults: ModelDefaults {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: false,
        },
        model_filter: None,
        model_traits: None,
        fallback_models: None,
        key_check_path: Some("/key"),
    },
    ProviderPreset {
        id: OPENAI_PROVIDER_ID,
        name: "OpenAI",
        description: "GPT models on the Responses API",
        wire: PresetWire::OpenAiResponses,
        base_url: "https://api.openai.com/v1",
        api_key_envs: &["OPENAI_API_KEY"],
        headers: &[],
        key_hint: "sk-…",
        key_cta: "get one at",
        key_url: "https://platform.openai.com/api-keys",
        defaults: ModelDefaults {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: false,
        },
        model_filter: Some(openai_chat_model),
        model_traits: Some(openai_model_traits),
        fallback_models: None,
        key_check_path: None,
    },
    ProviderPreset {
        id: ANTHROPIC_PROVIDER_ID,
        name: ANTHROPIC_PROVIDER_NAME,
        description: "Claude models with prompt caching and extended thinking",
        wire: PresetWire::Anthropic,
        base_url: ANTHROPIC_BASE_URL,
        api_key_envs: ANTHROPIC_API_KEY_ENVS,
        headers: &[],
        key_hint: "sk-ant-…",
        key_cta: "get one at",
        key_url: "https://console.anthropic.com/settings/keys",
        defaults: ModelDefaults {
            context_window: 200_000,
            max_tokens: 64_000,
            reasoning: true,
        },
        model_filter: None,
        model_traits: None,
        fallback_models: Some(anthropic_seed_models),
        // Listing models needs the key, so the listing checks it.
        key_check_path: None,
    },
    ProviderPreset {
        id: GEMINI_PROVIDER_ID,
        name: GEMINI_PROVIDER_NAME,
        description: "Gemini models with thinking, by API key",
        wire: PresetWire::Gemini,
        base_url: GEMINI_BASE_URL,
        api_key_envs: GEMINI_API_KEY_ENVS,
        headers: &[],
        key_hint: "AIza…",
        key_cta: "get one at",
        key_url: "https://aistudio.google.com/apikey",
        defaults: ModelDefaults {
            context_window: 1_048_576,
            max_tokens: 65_536,
            reasoning: true,
        },
        model_filter: None,
        model_traits: None,
        fallback_models: Some(gemini_seed_models),
        // Listing models needs the key, so the listing checks it.
        key_check_path: None,
    },
];

/// Look up a preset by id (case-insensitive).
pub fn provider_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(id.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_in_login_order_with_soket_first() {
        let ids: Vec<_> = PROVIDER_PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            ["soket", "openrouter", "openai", "anthropic", "gemini"]
        );
    }

    #[test]
    fn gemini_speaks_its_own_api_with_a_key() {
        let preset = provider_preset("gemini").unwrap();
        assert_eq!(preset.api_key_envs, ["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
        let models = preset.provider().get_models();
        assert!(models
            .iter()
            .all(|m| m.api == crate::types::API_GOOGLE_GENERATIVE_AI));
    }

    #[test]
    fn anthropic_speaks_messages_natively() {
        let provider = provider_preset("anthropic").unwrap().provider();
        assert!(provider.is_dynamic());
        let models = provider.get_models();
        assert_eq!(models[0].id, "claude-opus-5-5");
        assert!(models
            .iter()
            .all(|m| m.api == crate::types::API_ANTHROPIC_MESSAGES));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(provider_preset(" OpenRouter ").unwrap().id, "openrouter");
        assert!(provider_preset("groq").is_none());
    }

    #[test]
    fn openrouter_sends_attribution_headers() {
        let provider = provider_preset("openrouter").unwrap().provider();
        let headers = provider.headers.as_ref().unwrap();
        assert_eq!(headers["X-Title"], "Loop");
        assert!(headers.contains_key("HTTP-Referer"));
        assert!(provider.is_dynamic());
        assert!(
            provider.get_models().is_empty(),
            "no fallback models for OpenRouter"
        );
    }

    #[test]
    fn soket_keeps_its_seed_until_listed() {
        let provider = provider_preset("soket").unwrap().provider();
        assert_eq!(provider.id, SOKET_PROVIDER_ID);
        assert!(!provider.get_models().is_empty());
    }

    #[test]
    fn openai_traits_mark_reasoning_families() {
        let traits = |id: &str| {
            let mut m = crate::providers::soket_seed_models().remove(0);
            m.id = id.into();
            openai_model_traits(&mut m);
            (m.reasoning, m.supports_images())
        };
        assert_eq!(traits("gpt-5.1"), (true, true));
        assert_eq!(traits("gpt-6-mini"), (true, true));
        assert_eq!(traits("o4-mini"), (true, true));
        assert_eq!(traits("gpt-4.1"), (false, true));
        assert_eq!(traits("gpt-3.5-turbo"), (false, false));
        assert_eq!(traits("omni-x"), (false, false));
    }

    #[test]
    fn openai_filter_keeps_chat_models_only() {
        for keep in [
            "gpt-4o",
            "gpt-4.1-mini",
            "o3",
            "o4-mini",
            "chatgpt-4o-latest",
        ] {
            assert!(openai_chat_model(keep), "{keep}");
        }
        for drop in [
            "text-embedding-3-small",
            "whisper-1",
            "dall-e-3",
            "tts-1",
            "gpt-4o-realtime-preview",
            "gpt-4o-audio-preview",
            "gpt-image-1",
            "omni-moderation-latest",
            "gpt-3.5-turbo-instruct",
        ] {
            assert!(!openai_chat_model(drop), "{drop}");
        }
    }
}
