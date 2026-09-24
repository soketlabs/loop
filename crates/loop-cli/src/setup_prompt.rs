//! Guided input in the TUI setup box: provider API keys and the tracing wizard.

use crate::tracing_setup::TracingSetup;

/// What the setup box is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupPrompt {
    /// API key for a model provider (`/login`, first-run setup).
    ProviderKey(String),
    /// `/tracing setup` wizard.
    Tracing(TracingSetup),
}

impl SetupPrompt {
    /// Heading of the setup box.
    pub fn title(&self) -> String {
        match self {
            Self::ProviderKey(provider) => format!("Connect to {provider}"),
            Self::Tracing(step) => step.title(),
        }
    }

    /// What to enter.
    pub fn instructions(&self) -> String {
        match self {
            Self::ProviderKey(_) => {
                "Paste your API key and press enter — input stays hidden".into()
            }
            Self::Tracing(step) => step.instructions(),
        }
    }

    /// Env var alternative to typing the value, if any.
    pub fn env_hint(&self) -> Option<String> {
        match self {
            Self::ProviderKey(provider) if provider == "soket" => {
                Some("SOKET_API_KEY / TENSORSTUDIO_API_KEY / LOOP_API_KEY".into())
            }
            Self::ProviderKey(provider) => Some(format!(
                "{}_API_KEY",
                provider.to_uppercase().replace('-', "_")
            )),
            Self::Tracing(step) => step.env_hint(),
        }
    }

    /// Picker rows `(label, description)` and the highlighted row, when choosing.
    pub fn options(&self) -> Option<(Vec<(&'static str, &'static str)>, usize)> {
        match self {
            Self::ProviderKey(_) => None,
            Self::Tracing(step) => step.options(),
        }
    }

    /// Whether the input line is hidden.
    pub fn masked(&self) -> bool {
        match self {
            Self::ProviderKey(_) => true,
            Self::Tracing(step) => step.masked(),
        }
    }

    /// Placeholder while the input line is empty.
    pub fn placeholder(&self) -> &'static str {
        match self {
            Self::ProviderKey(_) => " paste your API key",
            Self::Tracing(step) => step.placeholder(),
        }
    }

    /// Footer / status-line hint.
    pub fn status_hint(&self) -> &'static str {
        match self {
            Self::ProviderKey(_) => "setup · paste your API key · enter save",
            Self::Tracing(step) if step.options().is_some() => {
                "tracing setup · ↑↓ choose · enter continue · esc cancel"
            }
            Self::Tracing(_) => "tracing setup · enter next · esc cancel",
        }
    }

    /// Status line after esc.
    pub fn cancelled_message(&self) -> &'static str {
        match self {
            Self::ProviderKey(_) => "login cancelled",
            Self::Tracing(_) => "tracing setup cancelled",
        }
    }

    /// Whether esc quits the app (first-run provider setup) instead of cancelling.
    pub fn esc_quits(&self, first_run: bool) -> bool {
        first_run && matches!(self, Self::ProviderKey(_))
    }
}

#[cfg(test)]
mod tests {
    use loop_app_core::config::TracingSettings;

    use super::*;

    #[test]
    fn provider_prompt_text() {
        let prompt = SetupPrompt::ProviderKey("open-router".into());
        assert_eq!(prompt.title(), "Connect to open-router");
        assert_eq!(prompt.env_hint().as_deref(), Some("OPEN_ROUTER_API_KEY"));
        assert!(prompt.masked());
        assert!(prompt.options().is_none());
        assert!(prompt.esc_quits(true));
        assert!(!prompt.esc_quits(false));
    }

    #[test]
    fn tracing_prompt_delegates_to_wizard() {
        let (step, _) = TracingSetup::start(None, &TracingSettings::default());
        let prompt = SetupPrompt::Tracing(step);
        assert_eq!(prompt.title(), "Set up tracing");
        assert!(prompt.options().is_some());
        assert!(!prompt.masked());
        assert!(!prompt.esc_quits(true));
        assert_eq!(prompt.cancelled_message(), "tracing setup cancelled");
        assert!(prompt.status_hint().contains("↑↓"));
    }
}
