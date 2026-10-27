//! Tracing setup, destination resolution and live control (`telemetry` feature).

use std::path::PathBuf;

use loop_ai::auth::{Credential, CredentialStore};
use loop_telemetry::{
    default_trace_dir, CredentialSource, TelemetryCredentials, TelemetryDestination,
    TelemetryHandle, TelemetryStatus,
};

use super::*;

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
    /// JSON-lines files in a local folder.
    File {
        /// Folder; blank means the default under the OS temp folder.
        dir: String,
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
            Self::File { dir } => f.debug_struct("File").field("dir", dir).finish(),
        }
    }
}

impl TracingSetupRequest {
    /// The backend this request configures.
    pub fn backend(&self) -> TracingBackend {
        match self {
            Self::Langfuse { .. } => TracingBackend::Langfuse,
            Self::Otlp { .. } => TracingBackend::Otlp,
            Self::File { .. } => TracingBackend::File,
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
            Self::File { dir } => Ok(file_destination(
                Some(dir.as_str()),
                CredentialSource::Config,
            )),
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
            Self::File { dir } => {
                settings.file_dir = Some(dir.trim().to_string()).filter(|d| !d.is_empty());
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

/// Default folder of the `file` backend: beside the usage log, not in it, so
/// `loop dashboard` does not count those spans twice.
pub fn default_file_backend_dir() -> PathBuf {
    default_trace_dir().join("full")
}

/// Trace file destination in `dir`, or the default folder when blank or unset.
fn file_destination(dir: Option<&str>, source: CredentialSource) -> TelemetryDestination {
    let dir = dir
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map_or_else(default_file_backend_dir, PathBuf::from);
    TelemetryDestination::file(&dir, source)
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
        TracingBackend::File => Some(file_destination(
            settings.file_dir.as_deref(),
            CredentialSource::Config,
        )),
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
        if self.settings.local_log {
            self.handle.install_local(&default_trace_dir());
        }
        if let Some(destination) = destination {
            self.handle.install(destination)?;
        }
        self.handle.set_anonymous(self.settings.anonymous);
        Ok(self.handle.set_enabled(self.settings.enabled))
    }

    /// `/tracing enable|disable`.
    pub fn set_enabled(&mut self, enabled: bool) -> TelemetryStatus {
        self.settings.enabled = enabled;
        self.handle.set_enabled(enabled)
    }

    /// `/tracing local on|off`: the anonymous usage log behind `loop dashboard`.
    pub fn set_local_log(&mut self, on: bool) -> TelemetryStatus {
        self.settings.local_log = on;
        let running = self.handle.status().local_log.is_some();
        match (on, running) {
            (true, false) => {
                self.handle.install_local(&default_trace_dir());
            }
            (false, true) => {
                self.handle.remove_local();
            }
            _ => {}
        }
        self.handle.status()
    }

    /// `/tracing anonymous on|off`.
    pub fn set_anonymous(&mut self, anonymous: bool) -> TelemetryStatus {
        self.settings.anonymous = anonymous;
        self.handle.set_anonymous(anonymous)
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
    let usage = match &status.local_log {
        Some(path) => format!("usage log: on · anonymous, local only · {path} · loop dashboard"),
        None => "usage log: off (/tracing local on)".into(),
    };
    format!("{}\n{usage}", describe_destination(status))
}

fn describe_destination(status: &TelemetryStatus) -> String {
    let Some(destination) = &status.destination else {
        return "tracing: not configured · run /tracing setup (Langfuse, any OTLP endpoint \
                or a local file), \
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
    if status.anonymous {
        line.push_str(" · anonymous (no prompts, outputs or tool data)");
    }
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
    fn local_log_starts_by_default_and_toggles() {
        let (mut settings, store, handle) = fresh();
        let mut control = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        };
        let status = control.apply_destination(None).unwrap();
        let path = status.local_log.clone().expect("usage log on by default");
        assert!(std::path::Path::new(&path).starts_with(default_trace_dir()));
        assert!(!status.active());
        let text = describe_tracing_status(&status);
        assert!(
            text.contains("not configured") && text.contains("usage log: on"),
            "{text}"
        );

        let status = control.set_local_log(false);
        assert_eq!(status.local_log, None);
        assert!(describe_tracing_status(&status).contains("usage log: off"));
        assert!(!settings.local_log);

        let (mut settings, store, handle) = fresh();
        settings.local_log = false;
        let status = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        }
        .apply_destination(None)
        .unwrap();
        assert_eq!(status.local_log, None, "saved opt-out is honoured");
    }

    #[test]
    fn apply_honours_saved_anonymous_preference() {
        let (mut settings, store, handle) = fresh();
        settings.anonymous = true;
        let status = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        }
        .apply_destination(None)
        .unwrap();
        assert!(status.anonymous);
        assert!(handle.status().anonymous);
    }

    #[test]
    fn set_anonymous_updates_settings_handle_and_status_line() {
        let (mut settings, store) = saved(&langfuse("https://lf.example", "sk-1"));
        let handle = TelemetryHandle::new("t", true);
        let destination = resolve_with_env(&settings, &store, env_of(&[]));
        let mut control = TracingControl {
            settings: &mut settings,
            store: &store,
            handle: &handle,
        };
        control.apply_destination(destination.as_ref()).unwrap();
        let status = control.set_anonymous(true);
        assert!(status.anonymous && status.active());
        // First line describes the destination; the second is the always-anonymous usage log.
        let destination_line = |s: &TelemetryStatus| {
            describe_tracing_status(s)
                .lines()
                .next()
                .unwrap()
                .to_string()
        };
        assert!(destination_line(&status).contains("anonymous"));
        let status = control.set_anonymous(false);
        assert!(!status.anonymous);
        assert!(!destination_line(&status).contains("anonymous"));
        assert!(!settings.anonymous);
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
    fn request_debug_hides_secrets() {
        let text = format!(
            "{:?} {:?}",
            langfuse("https://h", "sk-secret"),
            otlp("http://c", Some("Bearer tok"))
        );
        assert!(!text.contains("sk-secret") && !text.contains("tok"));
    }

    #[test]
    fn file_setup_defaults_to_temp_folder_and_saves_custom_dir() {
        let (settings, store) = saved(&TracingSetupRequest::File { dir: "  ".into() });
        assert_eq!(settings.backend, TracingBackend::File);
        assert_eq!(settings.file_dir, None);
        let destination = resolve_with_env(&settings, &store, env_of(&[])).unwrap();
        assert!(std::path::Path::new(&destination.endpoint).starts_with(default_file_backend_dir()));

        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().display().to_string();
        let (settings, store) = saved(&TracingSetupRequest::File {
            dir: custom.clone(),
        });
        assert_eq!(settings.file_dir.as_deref(), Some(custom.as_str()));
        let destination = resolve_with_env(&settings, &store, env_of(&[])).unwrap();
        assert!(std::path::Path::new(&destination.endpoint).starts_with(dir.path()));
        assert!(destination.label.starts_with("file · "));
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
    fn secrets_are_not_serialized_into_settings() {
        let (settings, _) = saved(&langfuse("https://h", "sk-cfg"));
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("pk-cfg"));
        assert!(!json.contains("sk-cfg"));
        let (settings, _) = saved(&otlp("http://c", Some("Bearer tok")));
        assert!(!serde_json::to_string(&settings).unwrap().contains("tok"));
    }
}
