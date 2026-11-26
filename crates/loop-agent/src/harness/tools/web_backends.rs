//! The built-in web-search backend registry.
//!
//! Plain data with no HTTP dependency, so it compiles without the `web`
//! feature: the tool environment scrubber needs the backends' default key
//! variable names whether or not the web tools are built.

use loop_ai::providers::{SOKET_API_KEY_ENVS, SOKET_BASE_URL, SOKET_PROVIDER_ID};

/// Backend used when `webSearch.provider` is unset: Soket, with the key the
/// user already stored for the Soket model provider.
pub const DEFAULT_WEB_SEARCH_PROVIDER: &str = SOKET_PROVIDER_ID;

/// Built-in backend registry — the single source of truth for backend ids,
/// display names and defaults. `/login` validation (loop-app-core) and the
/// login wizard (loop-cli) consult it; adding a backend is one entry here.
#[derive(Debug)]
pub struct WebSearchBackendInfo {
    /// Backend id (`webSearch.provider`).
    pub id: &'static str,
    /// Human-readable name.
    pub display_name: &'static str,
    /// Env vars checked when neither `apiKeyEnv` nor a stored key resolves.
    pub default_api_key_envs: &'static [&'static str],
    /// API base URL unless `webSearch.baseUrl` overrides it.
    pub default_base_url: &'static str,
    /// The key is the one stored for the model provider of the same id, so
    /// `/login <id>` and `/logout <id>` belong to the model-provider flow and
    /// the backend gets no row of its own in the picker.
    pub shares_model_provider_key: bool,
}

impl WebSearchBackendInfo {
    /// The env var named in "set …" hints.
    pub fn primary_api_key_env(&self) -> &'static str {
        self.default_api_key_envs.first().copied().unwrap_or("")
    }
}

/// The built-in web-search backends, default first.
pub const WEB_SEARCH_BACKENDS: &[WebSearchBackendInfo] = &[
    WebSearchBackendInfo {
        id: SOKET_PROVIDER_ID,
        display_name: "Soket",
        default_api_key_envs: SOKET_API_KEY_ENVS,
        default_base_url: SOKET_BASE_URL,
        shares_model_provider_key: true,
    },
    WebSearchBackendInfo {
        id: "tavily",
        display_name: "Tavily",
        default_api_key_envs: &["TAVILY_API_KEY"],
        default_base_url: "https://api.tavily.com",
        shares_model_provider_key: false,
    },
    WebSearchBackendInfo {
        id: "tinyfish",
        display_name: "TinyFish",
        default_api_key_envs: &["TINYFISH_API_KEY"],
        default_base_url: "https://api.search.tinyfish.ai",
        shares_model_provider_key: false,
    },
    WebSearchBackendInfo {
        id: "brave",
        display_name: "Brave Search",
        default_api_key_envs: &["BRAVE_API_KEY"],
        default_base_url: "https://api.search.brave.com",
        shares_model_provider_key: false,
    },
];

/// Look up a built-in backend by id (case-insensitive, whitespace trimmed).
pub fn web_search_backend(id: &str) -> Option<&'static WebSearchBackendInfo> {
    WEB_SEARCH_BACKENDS
        .iter()
        .find(|info| info.id.eq_ignore_ascii_case(id.trim()))
}
