//! Tracing configuration: saved settings, destination resolution and setup.
//!
//! The destination comes from the standard Langfuse env vars when all three are set,
//! otherwise from what `/tracing setup` saved for the chosen backend (URLs and public
//! identifiers in global settings, secrets in the credential store). Tracing is on
//! whenever a destination resolves, unless the user turned it off with `/tracing disable`.
//!
//! The saved settings here are plain data and always compiled, so a build without the
//! `telemetry` feature still round-trips them. Setup, resolution and `TracingControl`
//! need `loop-telemetry` and live in the feature-gated `control` module.

use serde::{Deserialize, Serialize};

#[cfg(feature = "telemetry")]
mod control;
#[cfg(feature = "telemetry")]
pub use control::*;

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
    /// JSON-lines files on this machine (default: the OS temp folder).
    File,
}

impl TracingBackend {
    /// All backends, in picker order.
    pub const ALL: [Self; 3] = [Self::Langfuse, Self::Otlp, Self::File];

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Langfuse => "Langfuse",
            Self::Otlp => "OTLP endpoint",
            Self::File => "Local file",
        }
    }

    /// One-line description for the picker.
    pub fn description(self) -> &'static str {
        match self {
            Self::Langfuse => "Langfuse cloud or self-hosted · host + public/secret keys",
            Self::Otlp => "Any OTLP/HTTP collector · Jaeger, Phoenix, Tempo, OTel Collector",
            Self::File => "JSON lines on this machine · no server needed",
        }
    }

    /// Parse a `/tracing setup <backend>` argument.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "langfuse" => Some(Self::Langfuse),
            "otlp" | "otel" => Some(Self::Otlp),
            "file" | "local" => Some(Self::File),
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
    /// Export spans without user content, toggled by `/tracing anonymous on|off`.
    #[serde(default)]
    pub anonymous: bool,
    /// Always-anonymous local usage log read by `loop dashboard`, toggled by
    /// `/tracing local on|off`. Independent of the configured backend.
    #[serde(default = "default_enabled")]
    pub local_log: bool,
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
    /// Trace file folder saved by `/tracing setup`; the OS temp folder when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_dir: Option<String>,
}

fn default_enabled() -> bool {
    true
}

impl Default for TracingSettings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            anonymous: false,
            local_log: default_enabled(),
            backend: TracingBackend::default(),
            langfuse_host: None,
            langfuse_public_key: None,
            otlp_endpoint: None,
            file_dir: None,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backend_parse_and_labels() {
        assert_eq!(
            TracingBackend::parse("Langfuse"),
            Some(TracingBackend::Langfuse)
        );
        assert_eq!(TracingBackend::parse("otel"), Some(TracingBackend::Otlp));
        assert_eq!(TracingBackend::parse("local"), Some(TracingBackend::File));
        assert_eq!(TracingBackend::parse("jaeger"), None);
        assert_eq!(TracingBackend::ALL.len(), 3);
    }

    #[test]
    fn settings_without_tracing_block_default_to_enabled_langfuse() {
        let settings: crate::config::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.tracing, TracingSettings::default());
        assert!(settings.tracing.enabled);
        assert!(!settings.tracing.anonymous);
        assert!(settings.tracing.local_log, "usage log is on by default");
        assert_eq!(settings.tracing.backend, TracingBackend::Langfuse);
    }

    #[test]
    fn tracing_settings_round_trip_through_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = crate::config::Settings {
            tracing: TracingSettings {
                enabled: false,
                anonymous: true,
                local_log: false,
                backend: TracingBackend::Otlp,
                langfuse_host: Some("https://h".into()),
                langfuse_public_key: Some("pk".into()),
                otlp_endpoint: Some("http://c:4318".into()),
                file_dir: Some("/tmp/traces".into()),
            },
            ..Default::default()
        };
        settings.save_file(&path).unwrap();
        let loaded = crate::config::Settings::load_file(&path).unwrap();
        assert_eq!(loaded.tracing, settings.tracing);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains(r#""backend": "otlp""#));
        assert!(json.contains(r#""anonymous": true"#));
        assert!(json.contains(r#""localLog": false"#));
    }

    #[test]
    fn project_settings_cannot_override_tracing() {
        let mut global = crate::config::Settings::default();
        global.tracing.langfuse_host = Some("https://global".into());
        let mut project = crate::config::Settings::default();
        project.tracing.langfuse_host = Some("https://attacker".into());
        project.tracing.backend = TracingBackend::Otlp;
        project.tracing.enabled = false;
        project.tracing.anonymous = true;
        project.tracing.local_log = false;
        global.merge_project(project);
        assert_eq!(
            global.tracing.langfuse_host.as_deref(),
            Some("https://global")
        );
        assert_eq!(global.tracing.backend, TracingBackend::Langfuse);
        assert!(global.tracing.enabled);
        assert!(!global.tracing.anonymous);
        assert!(global.tracing.local_log);
    }
}
