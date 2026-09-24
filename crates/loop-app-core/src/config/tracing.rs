//! Tracing configuration: saved settings, destination resolution and setup.
//!
//! The destination comes from the standard Langfuse env vars when all three are set,
//! otherwise from what `/tracing setup` saved for the chosen backend (URLs and public
//! identifiers in global settings, secrets in the credential store). Tracing is on
//! whenever a destination resolves, unless the user turned it off with `/tracing disable`.

use loop_ai::auth::{Credential, CredentialStore};
use loop_telemetry::{
    CredentialSource, TelemetryCredentials, TelemetryDestination, TelemetryHandle, TelemetryStatus,
};
use serde::{Deserialize, Serialize};

/// Credential store id holding the Langfuse secret key.
pub const LANGFUSE_CREDENTIAL_ID: &str = "langfuse";
/// Credential store id holding the OTLP `Authorization` header value.
pub const OTLP_CREDENTIAL_ID: &str = "otlp";
/// Langfuse base URL env var.
pub const ENV_LANGFUSE_HOST: &str = "LANGFUSE_HOST";
/// Langfuse public key env var.
pub const ENV_LANGFUSE_PUBLIC_KEY: &str = "LANGFUSE_PUBLIC_KEY";
/// Langfuse secret key env var.
pub const ENV_LANGFUSE_SECRET_KEY: &str = "LANGFUSE_SECRET_KEY";
/// Suggested host when none has been saved.
pub const DEFAULT_LANGFUSE_HOST: &str = "https://cloud.langfuse.com";
/// Suggested collector URL when none has been saved.
pub const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4318";

/// Where traces go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TracingBackend {
    /// Langfuse (cloud or self-hosted).
    #[default]
    Langfuse,
    /// Any OTLP/HTTP collector (Jaeger, Phoenix, Tempo, OTel Collector, …).
    Otlp,
}

impl TracingBackend {
    /// All backends, in picker order.
    pub const ALL: [Self; 2] = [Self::Langfuse, Self::Otlp];

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Langfuse => "Langfuse",
            Self::Otlp => "OTLP endpoint",
        }
    }

    /// One-line description for the picker.
    pub fn description(self) -> &'static str {
        match self {
            Self::Langfuse => "Langfuse cloud or self-hosted · host + public/secret keys",
            Self::Otlp => "Any OTLP/HTTP collector · Jaeger, Phoenix, Tempo, OTel Collector",
        }
    }

    /// Parse a `/tracing setup <backend>` argument.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "langfuse" => Some(Self::Langfuse),
            "otlp" | "otel" => Some(Self::Otlp),
            _ => None,
        }
    }
}

/// `tracing` block of global settings. Never taken from project settings, so a
/// repository cannot redirect traces elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TracingSettings {
    /// User preference toggled by `/tracing enable|disable`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Backend chosen in `/tracing setup`.
    #[serde(default)]
    pub backend: TracingBackend,
    /// Langfuse base URL saved by `/tracing setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langfuse_host: Option<String>,
    /// Langfuse public key saved by `/tracing setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langfuse_public_key: Option<String>,
    /// OTLP collector URL saved by `/tracing setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
}

fn default_enabled() -> bool {
    true
}

impl Default for TracingSettings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            backend: TracingBackend::default(),
            langfuse_host: None,
            langfuse_public_key: None,
            otlp_endpoint: None,
        }
    }
}

/// Validate a user-entered base URL.
pub fn validate_http_url(what: &str, url: &str) -> anyhow::Result<()> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("{what} must start with http:// or https://"))?;
    if rest.trim_matches('/').is_empty() {
        anyhow::bail!("{what} is missing a host name");
    }
    Ok(())
}

/// What `/tracing setup` collected.
#[derive(Clone, PartialEq, Eq)]
pub enum TracingSetupRequest {
    /// Langfuse project.
    Langfuse {
        /// Base URL.
        host: String,
        /// Public key.
        public_key: String,
        /// Secret key.
        secret_key: String,
    },
    /// Generic OTLP collector.
    Otlp {
        /// Collector base URL (or full `/v1/traces` URL).
        endpoint: String,
        /// Optional `Authorization` header value.
        authorization: Option<String>,
    },
}

impl std::fmt::Debug for TracingSetupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Langfuse {
                host, public_key, ..
            } => f
                .debug_struct("Langfuse")
                .field("host", host)
                .field("public_key", public_key)
                .finish_non_exhaustive(),
            Self::Otlp { endpoint, .. } => f
                .debug_struct("Otlp")
                .field("endpoint", endpoint)
                .finish_non_exhaustive(),
        }
    }
}

impl TracingSetupRequest {
    /// The backend this request configures.
    pub fn backend(&self) -> TracingBackend {
        match self {
            Self::Langfuse { .. } => TracingBackend::Langfuse,
            Self::Otlp { .. } => TracingBackend::Otlp,
        }
    }

    /// Validate and build the export destination.
    pub fn destination(&self) -> anyhow::Result<TelemetryDestination> {
        match self {
            Self::Langfuse {
                host,
                public_key,
                secret_key,
            } => {
                validate_http_url("Langfuse host", host)?;
                TelemetryCredentials::from_parts(
                    Some(host.clone()),
                    Some(public_key.clone()),
                    Some(secret_key.clone()),
                    CredentialSource::Config,
                )
                .map(|creds| creds.destination())
                .ok_or_else(|| anyhow::anyhow!("host, public key and secret key are all required"))
            }
            Self::Otlp {
                endpoint,
                authorization,
            } => {
                validate_http_url("OTLP endpoint", endpoint)?;
                TelemetryDestination::otlp(
                    endpoint,
                    authorization.as_deref(),
                    CredentialSource::Config,
                )
                .ok_or_else(|| anyhow::anyhow!("OTLP endpoint is required"))
            }
        }
    }

    /// Save: URLs and public identifiers into `settings`, secrets into `store`.
    fn persist(&self, settings: &mut TracingSettings, store: &dyn CredentialStore) {
        settings.backend = self.backend();
        match self {
            Self::Langfuse {
                host,
                public_key,
                secret_key,
            } => {
                settings.langfuse_host = Some(host.trim().to_string());
                settings.langfuse_public_key = Some(public_key.trim().to_string());
                store_secret(store, LANGFUSE_CREDENTIAL_ID, Some(secret_key));
            }
            Self::Otlp {
                endpoint,
                authorization,
            } => {
                settings.otlp_endpoint = Some(endpoint.trim().to_string());
                store_secret(store, OTLP_CREDENTIAL_ID, authorization.as_deref());
            }
        }
    }
}

fn store_secret(store: &dyn CredentialStore, id: &str, secret: Option<&str>) {
    match secret.map(str::trim).filter(|s| !s.is_empty()) {
        Some(key) => store.set(id, Credential::ApiKey { key: key.into() }),
        None => store.remove(id),
    }
}

fn stored_secret(store: &dyn CredentialStore, id: &str) -> Option<String> {
    match store.get(id) {
        Some(Credential::ApiKey { key }) => Some(key),
        _ => None,
    }
}

/// Resolve the destination from the process environment, then saved config.
pub fn resolve_tracing_destination(
    settings: &TracingSettings,
    store: &dyn CredentialStore,
) -> Option<TelemetryDestination> {
    resolve_with_env(settings, store, |key| std::env::var(key).ok())
}

/// [`resolve_tracing_destination`] with an injectable env lookup.
pub fn resolve_with_env(
    settings: &TracingSettings,
    store: &dyn CredentialStore,
    env: impl Fn(&str) -> Option<String>,
) -> Option<TelemetryDestination> {
    if let Some(creds) = TelemetryCredentials::from_parts(
        env(ENV_LANGFUSE_HOST),
        env(ENV_LANGFUSE_PUBLIC_KEY),
        env(ENV_LANGFUSE_SECRET_KEY),
        CredentialSource::Env,
    ) {
        return Some(creds.destination());
    }
    match settings.backend {
        TracingBackend::Langfuse => TelemetryCredentials::from_parts(
            settings.langfuse_host.clone(),
            settings.langfuse_public_key.clone(),
            stored_secret(store, LANGFUSE_CREDENTIAL_ID),
            CredentialSource::Config,
        )
        .map(|creds| creds.destination()),
        TracingBackend::Otlp => settings.otlp_endpoint.as_deref().and_then(|endpoint| {
            TelemetryDestination::otlp(
                endpoint,
                stored_secret(store, OTLP_CREDENTIAL_ID).as_deref(),
                CredentialSource::Config,
            )
        }),
    }
}

/// Applies tracing settings and credentials to a live [`TelemetryHandle`].
///
/// Callers persist `settings` afterwards (see `Runtime::save_settings`).
pub struct TracingControl<'a> {
    /// Global tracing settings.
    pub settings: &'a mut TracingSettings,
    /// Credential store holding tracing secrets.
    pub store: &'a dyn CredentialStore,
    /// Process telemetry.
    pub handle: &'a TelemetryHandle,
}

impl TracingControl<'_> {
    /// Startup: install the resolved destination (if any) and apply the saved preference.
    pub fn apply(&mut self) -> anyhow::Result<TelemetryStatus> {
        let destination = resolve_tracing_destination(self.settings, self.store);
        self.apply_destination(destination.as_ref())
    }

    fn apply_destination(
        &mut self,
        destination: Option<&TelemetryDestination>,
    ) -> anyhow::Result<TelemetryStatus> {
        if let Some(destination) = destination {
            self.handle.install(destination)?;
        }
        Ok(self.handle.set_enabled(self.settings.enabled))
    }

    /// `/tracing enable|disable`.
    pub fn set_enabled(&mut self, enabled: bool) -> TelemetryStatus {
        self.settings.enabled = enabled;
        self.handle.set_enabled(enabled)
    }

    /// `/tracing setup`: save the configuration and start exporting with it now.
    pub fn setup(&mut self, request: &TracingSetupRequest) -> anyhow::Result<TelemetryStatus> {
        let destination = request.destination()?;
        self.handle.install(&destination)?;
        request.persist(self.settings, self.store);
        Ok(self.set_enabled(true))
    }
}

/// One-line summary for `/tracing status` and startup messages.
pub fn describe_tracing_status(status: &TelemetryStatus) -> String {
    let Some(destination) = &status.destination else {
        return "tracing: not configured · run /tracing setup (Langfuse or any OTLP endpoint), \
                or set LANGFUSE_HOST, LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY"
            .into();
    };
    let state = if status.enabled {
        "on"
    } else {
        "off (/tracing enable)"
    };
    let source = status
        .source
        .map(CredentialSource::as_str)
        .unwrap_or("unknown");
    let mut line = format!("tracing: {state} · {destination} · configured from {source}");
    if let Some(err) = &status.last_error {
        line.push_str(&format!(" · last export failed: {err}"));
    }
    line
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

    fn langfuse(host: &str, secret: &str) -> TracingSetupRequest {
        TracingSetupRequest::Langfuse {
            host: host.into(),
            public_key: "pk-cfg".into(),
            secret_key: secret.into(),
        }
    }

    fn otlp(endpoint: &str, authorization: Option<&str>) -> TracingSetupRequest {
        TracingSetupRequest::Otlp {
            endpoint: endpoint.into(),
            authorization: authorization.map(Into::into),
        }
    }

    fn fresh() -> (TracingSettings, InMemoryCredentialStore, TelemetryHandle) {
        (
            TracingSettings::default(),
            InMemoryCredentialStore::default(),
            TelemetryHandle::new("t", true),
        )
    }

    fn setup(
        settings: &mut TracingSettings,
        store: &InMemoryCredentialStore,
        handle: &TelemetryHandle,
        request: &TracingSetupRequest,
    ) -> anyhow::Result<TelemetryStatus> {
        TracingControl {
            settings,
            store,
            handle,
        }
        .setup(request)
    }

    /// Settings + store as `/tracing setup` would leave them for `request`.
    fn saved(request: &TracingSetupRequest) -> (TracingSettings, InMemoryCredentialStore) {
        let (mut settings, store, handle) = fresh();
        setup(&mut settings, &store, &handle, request).unwrap();
        (settings, store)
    }

    #[test]
    fn apply_without_destination_is_inactive() {
        let (mut settings, store, handle) = fresh();
        let status = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        }
        .apply_destination(None)
        .unwrap();
        assert!(!status.active());
        assert!(describe_tracing_status(&status).contains("not configured"));
    }

    #[test]
    fn apply_saved_destination_honours_disabled_preference() {
        let (mut settings, store) = saved(&langfuse("https://cfg.example", "sk-cfg"));
        settings.enabled = false;
        let handle = TelemetryHandle::new("t", true);
        let destination = resolve_with_env(&settings, &store, env_of(&[]));
        let status = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        }
        .apply_destination(destination.as_ref())
        .unwrap();
        assert_eq!(
            status.destination.as_deref(),
            Some("Langfuse · https://cfg.example")
        );
        assert!(!status.enabled && !status.active());
        assert!(describe_tracing_status(&status).contains("off"));
    }

    #[test]
    fn set_enabled_updates_settings_and_handle() {
        let (mut settings, store, handle) = fresh();
        let status = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        }
        .set_enabled(false);
        assert!(!status.enabled);
        assert!(!settings.enabled);
        assert!(!handle.status().enabled);
    }

    #[test]
    fn langfuse_setup_stores_keys_and_activates() {
        let (mut settings, store, handle) = fresh();
        settings.enabled = false;
        let status = setup(
            &mut settings,
            &store,
            &handle,
            &langfuse(" https://lf.example/ ", "sk-1"),
        )
        .unwrap();
        assert!(status.active());
        assert_eq!(status.source, Some(CredentialSource::Config));
        assert!(settings.enabled);
        assert_eq!(settings.backend, TracingBackend::Langfuse);
        assert_eq!(
            settings.langfuse_host.as_deref(),
            Some("https://lf.example/")
        );
        assert_eq!(settings.langfuse_public_key.as_deref(), Some("pk-cfg"));
        assert_eq!(
            stored_secret(&store, LANGFUSE_CREDENTIAL_ID).as_deref(),
            Some("sk-1")
        );
        assert!(describe_tracing_status(&status)
            .contains("tracing: on · Langfuse · https://lf.example"));
    }

    #[test]
    fn otlp_setup_stores_endpoint_and_auth_and_switches_backend() {
        let (mut settings, store) = saved(&langfuse("https://lf.example", "sk-1"));
        let handle = TelemetryHandle::new("t", true);
        let status = setup(
            &mut settings,
            &store,
            &handle,
            &otlp("http://collector:4318", Some("Bearer tok")),
        )
        .unwrap();
        assert!(status.active());
        assert_eq!(settings.backend, TracingBackend::Otlp);
        assert_eq!(
            settings.otlp_endpoint.as_deref(),
            Some("http://collector:4318")
        );
        assert_eq!(
            stored_secret(&store, OTLP_CREDENTIAL_ID).as_deref(),
            Some("Bearer tok")
        );
        // Langfuse config is kept so switching back needs no re-entry of keys.
        assert_eq!(
            stored_secret(&store, LANGFUSE_CREDENTIAL_ID).as_deref(),
            Some("sk-1")
        );
        let destination = resolve_with_env(&settings, &store, env_of(&[])).unwrap();
        assert_eq!(destination.endpoint, "http://collector:4318/v1/traces");
        assert_eq!(
            destination.headers,
            vec![("Authorization".into(), "Bearer tok".into())]
        );
    }

    #[test]
    fn otlp_setup_without_auth_clears_stored_header() {
        let (mut settings, store) = saved(&otlp("http://c:4318", Some("Bearer old")));
        let handle = TelemetryHandle::new("t", true);
        setup(&mut settings, &store, &handle, &otlp("http://c:4318", None)).unwrap();
        assert!(stored_secret(&store, OTLP_CREDENTIAL_ID).is_none());
        let destination = resolve_with_env(&settings, &store, env_of(&[])).unwrap();
        assert!(destination.headers.is_empty());
    }

    #[test]
    fn setup_rejects_invalid_input_without_saving() {
        let (mut settings, store, handle) = fresh();
        for bad in [
            langfuse("https://h", " "),
            langfuse("lf.example", "sk"),
            otlp("localhost:4318", None),
            otlp("https://", None),
        ] {
            assert!(
                setup(&mut settings, &store, &handle, &bad).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(settings, TracingSettings::default());
        assert!(store.list().is_empty());
        assert!(!handle.status().active());
    }

    #[test]
    fn validate_http_url_messages() {
        assert!(validate_http_url("Langfuse host", "https://lf.example").is_ok());
        let err = validate_http_url("Langfuse host", "lf.example").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Langfuse host must start with http:// or https://"
        );
        assert!(validate_http_url("OTLP endpoint", "http:///").is_err());
    }

    #[test]
    fn request_debug_hides_secrets() {
        let text = format!(
            "{:?} {:?}",
            langfuse("https://h", "sk-secret"),
            otlp("http://c", Some("Bearer tok"))
        );
        assert!(!text.contains("sk-secret") && !text.contains("tok"));
    }

    #[test]
    fn backend_parse_and_labels() {
        assert_eq!(
            TracingBackend::parse("Langfuse"),
            Some(TracingBackend::Langfuse)
        );
        assert_eq!(TracingBackend::parse("otel"), Some(TracingBackend::Otlp));
        assert_eq!(TracingBackend::parse("jaeger"), None);
        assert_eq!(TracingBackend::ALL.len(), 2);
    }

    #[test]
    fn nothing_configured_resolves_to_none() {
        let store = InMemoryCredentialStore::default();
        assert!(resolve_with_env(&TracingSettings::default(), &store, env_of(&[])).is_none());
    }

    #[test]
    fn complete_langfuse_env_wins_over_any_config() {
        let (settings, store) = saved(&otlp("http://c:4318", None));
        let destination = resolve_with_env(&settings, &store, env_of(FULL_ENV)).unwrap();
        assert_eq!(destination.source, CredentialSource::Env);
        assert_eq!(destination.label, "Langfuse · https://env.example");
    }

    #[test]
    fn partial_env_falls_back_to_config() {
        let (settings, store) = saved(&langfuse("https://cfg.example", "sk-cfg"));
        let destination = resolve_with_env(&settings, &store, env_of(&FULL_ENV[..2])).unwrap();
        assert_eq!(destination.source, CredentialSource::Config);
        assert_eq!(destination.label, "Langfuse · https://cfg.example");
    }

    #[test]
    fn langfuse_config_without_secret_resolves_to_none() {
        let (settings, store) = saved(&langfuse("https://cfg.example", "sk-cfg"));
        store.remove(LANGFUSE_CREDENTIAL_ID);
        assert!(resolve_with_env(&settings, &store, env_of(&[])).is_none());
    }

    #[test]
    fn settings_without_tracing_block_default_to_enabled_langfuse() {
        let settings: crate::config::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.tracing, TracingSettings::default());
        assert!(settings.tracing.enabled);
        assert_eq!(settings.tracing.backend, TracingBackend::Langfuse);
    }

    #[test]
    fn tracing_settings_round_trip_through_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = crate::config::Settings {
            tracing: TracingSettings {
                enabled: false,
                backend: TracingBackend::Otlp,
                langfuse_host: Some("https://h".into()),
                langfuse_public_key: Some("pk".into()),
                otlp_endpoint: Some("http://c:4318".into()),
            },
            ..Default::default()
        };
        settings.save_file(&path).unwrap();
        let loaded = crate::config::Settings::load_file(&path).unwrap();
        assert_eq!(loaded.tracing, settings.tracing);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains(r#""backend": "otlp""#));
    }

    #[test]
    fn project_settings_cannot_override_tracing() {
        let mut global = crate::config::Settings::default();
        global.tracing.langfuse_host = Some("https://global".into());
        let mut project = crate::config::Settings::default();
        project.tracing.langfuse_host = Some("https://attacker".into());
        project.tracing.backend = TracingBackend::Otlp;
        project.tracing.enabled = false;
        global.merge_project(project);
        assert_eq!(
            global.tracing.langfuse_host.as_deref(),
            Some("https://global")
        );
        assert_eq!(global.tracing.backend, TracingBackend::Langfuse);
        assert!(global.tracing.enabled);
    }

    #[test]
    fn secrets_are_not_serialized_into_settings() {
        let (settings, _) = saved(&langfuse("https://h", "sk-cfg"));
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("pk-cfg"));
        assert!(!json.contains("sk-cfg"));
        let (settings, _) = saved(&otlp("http://c", Some("Bearer tok")));
        assert!(!serde_json::to_string(&settings).unwrap().contains("tok"));
    }
}
