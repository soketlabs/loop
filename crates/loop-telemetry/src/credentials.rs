//! Export destinations: a generic OTLP/HTTP endpoint, and Langfuse credentials that
//! resolve to one.

use base64::Engine;

/// Path of Langfuse's OTLP/HTTP trace ingestion endpoint, relative to the host.
const LANGFUSE_OTLP_TRACES_PATH: &str = "/api/public/otel/v1/traces";
/// Standard OTLP/HTTP traces path, appended to a collector base URL.
const OTLP_TRACES_PATH: &str = "/v1/traces";
/// Langfuse ingestion API version header.
const LANGFUSE_INGESTION_VERSION: (&str, &str) = ("x-langfuse-ingestion-version", "4");

/// Where a set of credentials was resolved from (shown by `/tracing status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Environment variables (`LANGFUSE_HOST` / `LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY`).
    Env,
    /// Saved by `/tracing setup` (settings + credential store).
    Config,
}

impl CredentialSource {
    /// Short label for display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Config => "config",
        }
    }
}

/// Everything needed to export traces to one Langfuse project.
#[derive(Clone, PartialEq, Eq)]
pub struct TelemetryCredentials {
    /// Base URL, e.g. `https://langfuse.example.com`.
    pub host: String,
    /// Project public key (`pk-lf-…`).
    pub public_key: String,
    /// Project secret key (`sk-lf-…`).
    pub secret_key: String,
    /// Where these came from.
    pub source: CredentialSource,
}

impl std::fmt::Debug for TelemetryCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCredentials")
            .field("host", &self.host)
            .field("public_key", &self.public_key)
            .field("secret_key", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

impl TelemetryCredentials {
    /// Build credentials when all three parts are present and non-blank.
    pub fn from_parts(
        host: Option<String>,
        public_key: Option<String>,
        secret_key: Option<String>,
        source: CredentialSource,
    ) -> Option<Self> {
        let non_blank =
            |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Some(Self {
            host: non_blank(host)?,
            public_key: non_blank(public_key)?,
            secret_key: non_blank(secret_key)?,
            source,
        })
    }

    /// Full OTLP traces endpoint for this host.
    pub fn traces_endpoint(&self) -> String {
        format!(
            "{}{LANGFUSE_OTLP_TRACES_PATH}",
            self.host.trim_end_matches('/')
        )
    }

    /// `Authorization` header value (`Basic base64(pk:sk)`).
    pub fn authorization(&self) -> String {
        let raw = format!("{}:{}", self.public_key, self.secret_key);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    /// The OTLP destination for this Langfuse project.
    pub fn destination(&self) -> TelemetryDestination {
        TelemetryDestination {
            endpoint: self.traces_endpoint(),
            headers: vec![
                ("Authorization".into(), self.authorization()),
                (
                    LANGFUSE_INGESTION_VERSION.0.into(),
                    LANGFUSE_INGESTION_VERSION.1.into(),
                ),
            ],
            label: format!("Langfuse · {}", self.host.trim_end_matches('/')),
            source: self.source,
        }
    }
}

/// Where spans are exported: an OTLP/HTTP traces endpoint plus request headers.
#[derive(Clone, PartialEq, Eq)]
pub struct TelemetryDestination {
    /// Full traces URL.
    pub endpoint: String,
    /// Request headers (may carry credentials).
    pub headers: Vec<(String, String)>,
    /// Human-readable description, e.g. `Langfuse · https://lf.example`.
    pub label: String,
    /// Where the configuration came from.
    pub source: CredentialSource,
}

impl std::fmt::Debug for TelemetryDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("TelemetryDestination")
            .field("endpoint", &self.endpoint)
            .field("headers", &header_names)
            .field("label", &self.label)
            .field("source", &self.source)
            .finish()
    }
}

impl TelemetryDestination {
    /// Generic OTLP/HTTP collector (Jaeger, Phoenix, Tempo, an OTel Collector, …).
    ///
    /// `/v1/traces` is appended to `base_url` unless it is already there;
    /// `authorization`, when given, is sent as the `Authorization` header.
    pub fn otlp(
        base_url: &str,
        authorization: Option<&str>,
        source: CredentialSource,
    ) -> Option<Self> {
        let base = base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        let endpoint = if base.ends_with(OTLP_TRACES_PATH) {
            base.to_string()
        } else {
            format!("{base}{OTLP_TRACES_PATH}")
        };
        let headers = authorization
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| vec![("Authorization".to_string(), v.to_string())])
            .unwrap_or_default();
        Some(Self {
            label: format!("OTLP · {endpoint}"),
            endpoint,
            headers,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(host: &str) -> TelemetryCredentials {
        TelemetryCredentials::from_parts(
            Some(host.into()),
            Some("pk-lf-1".into()),
            Some("sk-lf-2".into()),
            CredentialSource::Env,
        )
        .unwrap()
    }

    #[test]
    fn endpoint_appends_otlp_path_once() {
        assert_eq!(
            creds("https://lf.example.com").traces_endpoint(),
            "https://lf.example.com/api/public/otel/v1/traces"
        );
        assert_eq!(
            creds("https://lf.example.com//").traces_endpoint(),
            "https://lf.example.com/api/public/otel/v1/traces"
        );
    }

    #[test]
    fn authorization_is_basic_base64_of_keys() {
        // echo -n "pk-lf-1:sk-lf-2" | base64
        assert_eq!(creds("h").authorization(), "Basic cGstbGYtMTpzay1sZi0y");
    }

    #[test]
    fn missing_or_blank_parts_yield_none() {
        let src = CredentialSource::Config;
        assert!(
            TelemetryCredentials::from_parts(None, Some("a".into()), Some("b".into()), src)
                .is_none()
        );
        assert!(TelemetryCredentials::from_parts(
            Some("h".into()),
            Some("  ".into()),
            Some("b".into()),
            src
        )
        .is_none());
        assert!(
            TelemetryCredentials::from_parts(Some("h".into()), Some("a".into()), None, src)
                .is_none()
        );
    }

    #[test]
    fn langfuse_destination_has_auth_and_ingestion_headers() {
        let dest = creds("https://lf.example/").destination();
        assert_eq!(
            dest.endpoint,
            "https://lf.example/api/public/otel/v1/traces"
        );
        assert_eq!(dest.label, "Langfuse · https://lf.example");
        assert!(dest
            .headers
            .contains(&("Authorization".into(), "Basic cGstbGYtMTpzay1sZi0y".into())));
        assert!(dest
            .headers
            .contains(&("x-langfuse-ingestion-version".into(), "4".into())));
    }

    #[test]
    fn otlp_destination_appends_traces_path_once() {
        let src = CredentialSource::Config;
        let base = TelemetryDestination::otlp("http://localhost:4318/", None, src).unwrap();
        assert_eq!(base.endpoint, "http://localhost:4318/v1/traces");
        assert!(base.headers.is_empty());
        let full = TelemetryDestination::otlp("http://c:4318/v1/traces", Some(" "), src).unwrap();
        assert_eq!(full.endpoint, "http://c:4318/v1/traces");
        assert!(full.headers.is_empty());
        assert!(TelemetryDestination::otlp("  ", None, src).is_none());
    }

    #[test]
    fn otlp_destination_sends_authorization_and_redacts_it() {
        let dest =
            TelemetryDestination::otlp("https://c", Some("Bearer tok"), CredentialSource::Config)
                .unwrap();
        assert_eq!(
            dest.headers,
            vec![("Authorization".into(), "Bearer tok".into())]
        );
        assert!(!format!("{dest:?}").contains("tok"));
    }

    #[test]
    fn debug_redacts_secret() {
        let dbg = format!("{:?}", creds("h"));
        assert!(!dbg.contains("sk-lf-2"));
        assert!(dbg.contains("<redacted>"));
    }
}
