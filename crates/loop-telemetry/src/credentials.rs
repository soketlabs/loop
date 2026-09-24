//! Langfuse connection details and the OTLP endpoint / auth derived from them.

use base64::Engine;

/// Path of Langfuse's OTLP/HTTP trace ingestion endpoint, relative to the host.
const LANGFUSE_OTLP_TRACES_PATH: &str = "/api/public/otel/v1/traces";

/// Where a set of credentials was resolved from (shown by `/tracing status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// `LANGFUSE_HOST` / `LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY`.
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
    fn debug_redacts_secret() {
        let dbg = format!("{:?}", creds("h"));
        assert!(!dbg.contains("sk-lf-2"));
        assert!(dbg.contains("<redacted>"));
    }
}
