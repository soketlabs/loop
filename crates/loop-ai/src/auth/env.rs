//! Environment-variable API key auth helper.

use std::sync::Arc;

use super::types::{ApiKeyAuth, AuthResult, ModelAuth};

/// Build API-key auth that reads the first present environment variable.
///
/// Stored credentials (handled by resolution) still take precedence over env.
pub fn env_api_key_auth(name: impl Into<String>, env_vars: &[&str]) -> ApiKeyAuth {
    let name = name.into();
    let env_vars: Vec<String> = env_vars.iter().map(|s| (*s).to_string()).collect();
    ApiKeyAuth {
        name,
        resolve: Arc::new(move || {
            let env_vars = env_vars.clone();
            Box::pin(async move {
                for var in &env_vars {
                    if let Ok(val) = std::env::var(var) {
                        let trimmed = val.trim();
                        if !trimmed.is_empty() {
                            return Ok(AuthResult {
                                auth: ModelAuth {
                                    api_key: Some(trimmed.to_string()),
                                    headers: Default::default(),
                                    base_url: None,
                                },
                                credential: None,
                            });
                        }
                    }
                }
                Err(format!(
                    "missing API key; set one of: {}",
                    env_vars.join(", ")
                ))
            })
        }),
    }
}
