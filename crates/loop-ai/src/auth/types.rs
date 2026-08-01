//! Auth domain types.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Effective auth material applied to a request.
#[derive(Debug, Clone, Default)]
pub struct ModelAuth {
    /// Bearer / API key.
    pub api_key: Option<String>,
    /// Extra headers from auth.
    pub headers: HashMap<String, String>,
    /// Optional base URL override from credentials.
    pub base_url: Option<String>,
}

/// Stored credential kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Credential {
    /// Static API key.
    ApiKey {
        /// Key value.
        key: String,
    },
    /// OAuth tokens (deferred for later phases; structure reserved).
    OAuth {
        /// Access token.
        access: String,
        /// Refresh token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh: Option<String>,
        /// Expiry unix ms.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<i64>,
    },
}

impl Credential {
    /// Create an API-key credential.
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey { key: key.into() }
    }
}

/// Result of checking whether a provider is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCheck {
    /// Provider id.
    pub provider_id: String,
    /// Whether auth is configured.
    pub configured: bool,
    /// Human-readable reason when not configured.
    pub message: Option<String>,
}

/// Auth type labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    /// API key (env or stored).
    ApiKey,
    /// OAuth.
    OAuth,
}

/// Outcome of resolving ambient / stored API-key auth.
#[derive(Debug, Clone)]
pub struct AuthResult {
    /// Resolved auth material. Empty auth means keyless (local servers).
    pub auth: ModelAuth,
    /// Optional credential to persist.
    pub credential: Option<Credential>,
}

/// Async resolve function for ambient API-key auth.
pub type ApiKeyResolveFn = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<AuthResult, String>> + Send>> + Send + Sync,
>;

/// API-key auth method on a provider.
#[derive(Clone)]
pub struct ApiKeyAuth {
    /// Display name (e.g. env var hint).
    pub name: String,
    /// Ambient resolve (env vars, profiles, keyless local).
    pub resolve: ApiKeyResolveFn,
}

/// Provider auth configuration. At least one method should be present.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    /// API-key auth.
    pub api_key: Option<ApiKeyAuth>,
    // OAuth reserved for a later phase.
}

impl ProviderAuth {
    /// API-key-only auth.
    pub fn api_key(auth: ApiKeyAuth) -> Self {
        Self {
            api_key: Some(auth),
        }
    }

    /// Keyless local auth that always succeeds with empty credentials.
    pub fn keyless(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            api_key: Some(ApiKeyAuth {
                name,
                resolve: Arc::new(|| {
                    Box::pin(async {
                        Ok(AuthResult {
                            auth: ModelAuth::default(),
                            credential: None,
                        })
                    })
                }),
            }),
        }
    }
}
