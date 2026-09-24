//! Masked single-line secret entry in the TUI (provider API keys, Langfuse secret key).

use loop_app_core::config::tracing::ENV_LANGFUSE_SECRET_KEY;

/// What the masked input is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretPrompt {
    /// API key for a model provider (`/login`, first-run setup).
    ProviderKey(String),
    /// Langfuse secret key, after `/tracing setup <host> <public-key>`.
    LangfuseSecret {
        /// Langfuse base URL.
        host: String,
        /// Project public key.
        public_key: String,
    },
}

impl SecretPrompt {
    /// Heading of the setup box.
    pub fn title(&self) -> String {
        match self {
            Self::ProviderKey(provider) => format!("Connect to {provider}"),
            Self::LangfuseSecret { host, .. } => format!("Connect Langfuse tracing ({host})"),
        }
    }

    /// What to paste.
    pub fn instructions(&self) -> &'static str {
        match self {
            Self::ProviderKey(_) => "Paste your API key and press enter — input stays hidden",
            Self::LangfuseSecret { .. } => {
                "Paste your Langfuse secret key (sk-lf-…) and press enter — input stays hidden"
            }
        }
    }

    /// Env var alternative to typing the secret.
    pub fn env_hint(&self) -> String {
        match self {
            Self::ProviderKey(provider) if provider == "soket" => {
                "SOKET_API_KEY / TENSORSTUDIO_API_KEY / LOOP_API_KEY".into()
            }
            Self::ProviderKey(provider) => {
                format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"))
            }
            Self::LangfuseSecret { .. } => ENV_LANGFUSE_SECRET_KEY.into(),
        }
    }

    /// Footer / status-line hint.
    pub fn status_hint(&self) -> &'static str {
        match self {
            Self::ProviderKey(_) => "setup · paste your API key · enter save",
            Self::LangfuseSecret { .. } => "tracing setup · paste your secret key · enter save",
        }
    }

    /// Whether esc quits the app (first-run provider setup) instead of cancelling.
    pub fn esc_quits(&self, first_run: bool) -> bool {
        first_run && matches!(self, Self::ProviderKey(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prompt_text() {
        let prompt = SecretPrompt::ProviderKey("open-router".into());
        assert_eq!(prompt.title(), "Connect to open-router");
        assert_eq!(prompt.env_hint(), "OPEN_ROUTER_API_KEY");
        assert!(prompt.esc_quits(true));
        assert!(!prompt.esc_quits(false));
    }

    #[test]
    fn langfuse_prompt_text() {
        let prompt = SecretPrompt::LangfuseSecret {
            host: "https://lf.example".into(),
            public_key: "pk".into(),
        };
        assert_eq!(
            prompt.title(),
            "Connect Langfuse tracing (https://lf.example)"
        );
        assert_eq!(prompt.env_hint(), "LANGFUSE_SECRET_KEY");
        assert!(!prompt.esc_quits(true));
    }
}
