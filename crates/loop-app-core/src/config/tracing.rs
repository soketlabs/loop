//! Langfuse tracing configuration: saved settings and credential resolution.
//!
//! Credentials come from the standard Langfuse env vars when all three are set,
//! otherwise from what `/tracing setup` saved (host + public key in global settings,
//! secret key in the credential store). Tracing is on whenever credentials resolve,
//! unless the user turned it off with `/tracing disable`.

use loop_ai::auth::{Credential, CredentialStore};
use loop_telemetry::{CredentialSource, TelemetryCredentials};
use serde::{Deserialize, Serialize};

/// Credential store id holding the Langfuse secret key.
pub const LANGFUSE_CREDENTIAL_ID: &str = "langfuse";
/// Langfuse base URL env var.
pub const ENV_LANGFUSE_HOST: &str = "LANGFUSE_HOST";
/// Langfuse public key env var.
pub const ENV_LANGFUSE_PUBLIC_KEY: &str = "LANGFUSE_PUBLIC_KEY";
/// Langfuse secret key env var.
pub const ENV_LANGFUSE_SECRET_KEY: &str = "LANGFUSE_SECRET_KEY";

/// `tracing` block of global settings. Never taken from project settings, so a
/// repository cannot redirect traces elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TracingSettings {
    /// User preference toggled by `/tracing enable|disable`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Langfuse base URL saved by `/tracing setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langfuse_host: Option<String>,
    /// Langfuse public key saved by `/tracing setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langfuse_public_key: Option<String>,
}

fn default_enabled() -> bool {
    true
}

impl Default for TracingSettings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            langfuse_host: None,
            langfuse_public_key: None,
        }
    }
}

/// Resolve credentials from the process environment, then saved config.
pub fn resolve_tracing_credentials(
    settings: &TracingSettings,
    store: &dyn CredentialStore,
) -> Option<TelemetryCredentials> {
    resolve_with_env(settings, store, |key| std::env::var(key).ok())
}

/// [`resolve_tracing_credentials`] with an injectable env lookup.
pub fn resolve_with_env(
    settings: &TracingSettings,
    store: &dyn CredentialStore,
    env: impl Fn(&str) -> Option<String>,
) -> Option<TelemetryCredentials> {
    TelemetryCredentials::from_parts(
        env(ENV_LANGFUSE_HOST),
        env(ENV_LANGFUSE_PUBLIC_KEY),
        env(ENV_LANGFUSE_SECRET_KEY),
        CredentialSource::Env,
    )
    .or_else(|| {
        let secret = match store.get(LANGFUSE_CREDENTIAL_ID) {
            Some(Credential::ApiKey { key }) => Some(key),
            _ => None,
        };
        TelemetryCredentials::from_parts(
            settings.langfuse_host.clone(),
            settings.langfuse_public_key.clone(),
            secret,
            CredentialSource::Config,
        )
    })
}

/// Save credentials entered via `/tracing setup`: host and public key into `settings`
/// (caller persists them), secret key into the credential store.
pub fn store_tracing_credentials(
    settings: &mut TracingSettings,
    store: &dyn CredentialStore,
    creds: &TelemetryCredentials,
) {
    settings.langfuse_host = Some(creds.host.clone());
    settings.langfuse_public_key = Some(creds.public_key.clone());
    store.set(
        LANGFUSE_CREDENTIAL_ID,
        Credential::ApiKey {
            key: creds.secret_key.clone(),
        },
    );
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use loop_ai::auth::InMemoryCredentialStore;

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    const FULL_ENV: &[(&str, &str)] = &[
        (ENV_LANGFUSE_HOST, "https://env.example"),
        (ENV_LANGFUSE_PUBLIC_KEY, "pk-env"),
        (ENV_LANGFUSE_SECRET_KEY, "sk-env"),
    ];

    fn saved(store: &InMemoryCredentialStore) -> TracingSettings {
        let mut settings = TracingSettings::default();
        let creds = TelemetryCredentials::from_parts(
            Some("https://cfg.example".into()),
            Some("pk-cfg".into()),
            Some("sk-cfg".into()),
            CredentialSource::Config,
        )
        .unwrap();
        store_tracing_credentials(&mut settings, store, &creds);
        settings
    }

    #[test]
    fn nothing_configured_resolves_to_none() {
        let store = InMemoryCredentialStore::default();
        assert!(resolve_with_env(&TracingSettings::default(), &store, env_of(&[])).is_none());
    }

    #[test]
    fn complete_env_wins_over_config() {
        let store = InMemoryCredentialStore::default();
        let settings = saved(&store);
        let creds = resolve_with_env(&settings, &store, env_of(FULL_ENV)).unwrap();
        assert_eq!(creds.source, CredentialSource::Env);
        assert_eq!(creds.host, "https://env.example");
        assert_eq!(creds.secret_key, "sk-env");
    }

    #[test]
    fn partial_env_falls_back_to_config() {
        let store = InMemoryCredentialStore::default();
        let settings = saved(&store);
        let creds = resolve_with_env(&settings, &store, env_of(&FULL_ENV[..2])).unwrap();
        assert_eq!(creds.source, CredentialSource::Config);
        assert_eq!(creds.public_key, "pk-cfg");
        assert_eq!(creds.secret_key, "sk-cfg");
    }

    #[test]
    fn config_without_secret_resolves_to_none() {
        let store = InMemoryCredentialStore::default();
        let settings = saved(&store);
        store.remove(LANGFUSE_CREDENTIAL_ID);
        assert!(resolve_with_env(&settings, &store, env_of(&[])).is_none());
    }

    #[test]
    fn settings_without_tracing_block_default_to_enabled() {
        let settings: crate::config::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.tracing, TracingSettings::default());
        assert!(settings.tracing.enabled);
    }

    #[test]
    fn tracing_settings_round_trip_through_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = crate::config::Settings {
            tracing: TracingSettings {
                enabled: false,
                langfuse_host: Some("https://h".into()),
                langfuse_public_key: Some("pk".into()),
            },
            ..Default::default()
        };
        settings.save_file(&path).unwrap();
        let loaded = crate::config::Settings::load_file(&path).unwrap();
        assert_eq!(loaded.tracing, settings.tracing);
    }

    #[test]
    fn project_settings_cannot_override_tracing() {
        let mut global = crate::config::Settings::default();
        global.tracing.langfuse_host = Some("https://global".into());
        let mut project = crate::config::Settings::default();
        project.tracing.langfuse_host = Some("https://attacker".into());
        project.tracing.enabled = false;
        global.merge_project(project);
        assert_eq!(
            global.tracing.langfuse_host.as_deref(),
            Some("https://global")
        );
        assert!(global.tracing.enabled);
    }

    #[test]
    fn secret_is_not_serialized_into_settings() {
        let store = InMemoryCredentialStore::default();
        let json = serde_json::to_string(&saved(&store)).unwrap();
        assert!(json.contains("pk-cfg"));
        assert!(!json.contains("sk-cfg"));
    }
}
