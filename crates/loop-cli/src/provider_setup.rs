//! `/login` wizard: pick a provider, then enter its details and API key.
//!
//! Pure state machine; the app connects the resulting [`ProviderLoginRequest`].

use loop_ai::providers::{provider_preset, ProviderPreset, PROVIDER_PRESETS};
use loop_app_core::config::providers::slugify;
use loop_app_core::config::{validate_http_url, ProviderLoginRequest};

use crate::wizard::{wrap_selection, WizardView};

/// `/login custom` and the last picker row.
pub const CUSTOM_PROVIDER: &str = "custom";
const CUSTOM_LABEL: &str = "Custom";
const CUSTOM_DESCRIPTION: &str =
    "Any OpenAI-compatible API · Together, Groq, DeepSeek, vLLM, Ollama, LM Studio";

/// Which provider the key is for.
#[derive(Debug, Clone)]
pub enum LoginTarget {
    /// A built-in provider.
    Preset(&'static ProviderPreset),
    /// A custom OpenAI-compatible endpoint.
    Custom {
        /// Display name.
        name: String,
        /// Base URL.
        base_url: String,
    },
}

impl PartialEq for LoginTarget {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Preset(a), Self::Preset(b)) => a.id == b.id,
            (
                Self::Custom { name, base_url },
                Self::Custom {
                    name: other_name,
                    base_url: other_url,
                },
            ) => name == other_name && base_url == other_url,
            _ => false,
        }
    }
}

impl Eq for LoginTarget {}

/// Current step of the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSetup {
    /// Choose a preset or Custom.
    Choose {
        /// Highlighted row.
        selected: usize,
    },
    /// Custom provider name.
    CustomName,
    /// Custom provider base URL.
    CustomBaseUrl {
        /// Entered name.
        name: String,
    },
    /// API key (masked).
    ApiKey {
        /// Provider the key is for.
        target: LoginTarget,
    },
}

/// Result of submitting an answer to the login wizard.
pub type Transition = crate::wizard::Transition<ProviderSetup, ProviderLoginRequest>;

impl ProviderSetup {
    /// Start the wizard; `provider` (`/login openrouter`, `/login custom`) skips the picker.
    pub fn start(provider: Option<&str>) -> Result<Self, String> {
        match provider.map(str::trim).filter(|p| !p.is_empty()) {
            None => Ok(Self::Choose { selected: 0 }),
            Some(p) if p.eq_ignore_ascii_case(CUSTOM_PROVIDER) => Ok(Self::CustomName),
            Some(p) => provider_preset(p)
                .map(|preset| Self::ApiKey {
                    target: LoginTarget::Preset(preset),
                })
                .ok_or_else(|| format!("unknown provider `{p}`. {}", login_usage())),
        }
    }

    /// Re-ask for the key after `request` was rejected (e.g. HTTP 401).
    pub fn retry_key(request: &ProviderLoginRequest) -> Self {
        let target = match request {
            ProviderLoginRequest::Preset { id, .. } => match provider_preset(id) {
                Some(preset) => LoginTarget::Preset(preset),
                None => return Self::Choose { selected: 0 },
            },
            ProviderLoginRequest::Custom { name, base_url, .. } => LoginTarget::Custom {
                name: name.clone(),
                base_url: base_url.clone(),
            },
        };
        Self::ApiKey { target }
    }

    fn row_count() -> usize {
        PROVIDER_PRESETS.len() + 1
    }

    /// Submit the input line at the current step.
    pub fn submit(self, input: &str) -> Transition {
        let value = input.trim().to_string();
        match self {
            Self::Choose { selected } => {
                let step = match PROVIDER_PRESETS.get(selected) {
                    Some(preset) => Self::ApiKey {
                        target: LoginTarget::Preset(preset),
                    },
                    None => Self::CustomName,
                };
                Transition::Next {
                    step,
                    prefill: String::new(),
                }
            }
            Self::CustomName => {
                let id = slugify(&value);
                if id.is_empty() {
                    Self::CustomName.retry("name must contain letters or digits")
                } else if provider_preset(&id).is_some() {
                    Self::CustomName.retry(&format!(
                        "`{id}` is a built-in provider — pick it from /login instead"
                    ))
                } else {
                    Transition::Next {
                        step: Self::CustomBaseUrl { name: value },
                        prefill: String::new(),
                    }
                }
            }
            Self::CustomBaseUrl { name } => match validate_http_url("Base URL", &value) {
                Ok(()) => Transition::Next {
                    step: Self::ApiKey {
                        target: LoginTarget::Custom {
                            name,
                            base_url: value,
                        },
                    },
                    prefill: String::new(),
                },
                Err(err) => Self::CustomBaseUrl { name }.retry(&err.to_string()),
            },
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } if value.is_empty() => Self::ApiKey {
                target: LoginTarget::Preset(preset),
            }
            .retry(&format!("{} needs an API key", preset.name)),
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => Transition::Done(ProviderLoginRequest::Preset {
                id: preset.id.into(),
                api_key: value,
            }),
            Self::ApiKey {
                target: LoginTarget::Custom { name, base_url },
            } => Transition::Done(ProviderLoginRequest::Custom {
                name,
                base_url,
                api_key: (!value.is_empty()).then_some(value),
            }),
        }
    }

    fn retry(self, error: &str) -> Transition {
        Transition::Retry {
            step: self,
            error: error.into(),
        }
    }
}

/// Usage line for `/login`.
pub fn login_usage() -> String {
    let ids: Vec<&str> = PROVIDER_PRESETS
        .iter()
        .map(|p| p.id)
        .chain([CUSTOM_PROVIDER])
        .collect();
    format!("Usage: /login [{}]", ids.join("|"))
}

impl WizardView for ProviderSetup {
    fn title(&self) -> String {
        match self {
            Self::Choose { .. } => "Connect a model provider".into(),
            Self::CustomName => "Connect a custom provider · step 1 of 3".into(),
            Self::CustomBaseUrl { .. } => "Connect a custom provider · step 2 of 3".into(),
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => format!("Connect {}", preset.name),
            Self::ApiKey {
                target: LoginTarget::Custom { name, .. },
            } => format!("Connect {name} · step 3 of 3"),
        }
    }

    fn instructions(&self) -> String {
        match self {
            Self::Choose { .. } => "Choose where Loop sends model requests".into(),
            Self::CustomName => "A name for this provider, e.g. Together, Groq or LM Studio".into(),
            Self::CustomBaseUrl { name } => {
                format!("Base URL of {name}'s OpenAI-compatible API, including the version path")
            }
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => format!(
                "Paste your {} API key ({}) — input stays hidden · get one at {}",
                preset.name, preset.key_hint, preset.key_url
            ),
            Self::ApiKey {
                target: LoginTarget::Custom { name, .. },
            } => {
                format!("API key for {name} — input stays hidden · enter to skip for local servers")
            }
        }
    }

    fn placeholder(&self) -> &'static str {
        match self {
            Self::Choose { .. } => " ↑↓ to choose, enter to continue",
            Self::CustomName => " e.g. Together",
            Self::CustomBaseUrl { .. } => {
                " https://api.together.xyz/v1 or http://localhost:11434/v1"
            }
            Self::ApiKey {
                target: LoginTarget::Preset(_),
            } => " paste your API key",
            Self::ApiKey {
                target: LoginTarget::Custom { .. },
            } => " API key (optional)",
        }
    }

    fn options(&self) -> Option<(Vec<(&'static str, &'static str)>, usize)> {
        match self {
            Self::Choose { selected } => Some((
                PROVIDER_PRESETS
                    .iter()
                    .map(|p| (p.name, p.description))
                    .chain([(CUSTOM_LABEL, CUSTOM_DESCRIPTION)])
                    .collect(),
                *selected,
            )),
            _ => None,
        }
    }

    fn masked(&self) -> bool {
        matches!(self, Self::ApiKey { .. })
    }

    fn env_hint(&self) -> Option<String> {
        match self {
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => Some(preset.api_key_envs.join(" / ")),
            _ => None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if let Self::Choose { selected } = self {
            *selected = wrap_selection(*selected, delta, Self::row_count());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(t: Transition) -> ProviderSetup {
        match t {
            Transition::Next { step, .. } => step,
            other => panic!("expected Next, got {other:?}"),
        }
    }

    fn retry_error(t: Transition) -> String {
        match t {
            Transition::Retry { error, .. } => error,
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn picker_lists_presets_then_custom_with_soket_first() {
        let step = ProviderSetup::start(None).unwrap();
        let (rows, selected) = step.options().unwrap();
        let labels: Vec<_> = rows.iter().map(|r| r.0).collect();
        assert_eq!(labels, ["Soket", "OpenRouter", "OpenAI", "Custom"]);
        assert_eq!(selected, 0);
        assert!(!step.masked());
    }

    #[test]
    fn selection_wraps_over_all_rows() {
        let mut step = ProviderSetup::Choose { selected: 0 };
        step.move_selection(-1);
        assert_eq!(step, ProviderSetup::Choose { selected: 3 });
    }

    #[test]
    fn openrouter_flow() {
        let mut step = ProviderSetup::start(None).unwrap();
        step.move_selection(1);
        let key = next(step.submit(""));
        assert!(key.masked());
        assert_eq!(key.title(), "Connect OpenRouter");
        assert_eq!(key.env_hint().as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(key.instructions().contains("openrouter.ai/keys"));
        assert!(retry_error(key.clone().submit("  ")).contains("needs an API key"));
        assert_eq!(
            key.submit(" sk-or-1 "),
            Transition::Done(ProviderLoginRequest::Preset {
                id: "openrouter".into(),
                api_key: "sk-or-1".into(),
            })
        );
    }

    #[test]
    fn custom_flow_allows_empty_key_and_validates() {
        let step = ProviderSetup::start(Some("custom")).unwrap();
        assert_eq!(step, ProviderSetup::CustomName);
        assert!(retry_error(ProviderSetup::CustomName.submit("!!")).contains("letters"));
        assert!(retry_error(ProviderSetup::CustomName.submit("OpenAI")).contains("built-in"));
        let url = next(step.submit("LM Studio"));
        assert!(retry_error(url.clone().submit("localhost:1234")).contains("http://"));
        let key = next(url.submit("http://localhost:1234/v1"));
        assert!(key.masked());
        assert_eq!(key.title(), "Connect LM Studio · step 3 of 3");
        assert_eq!(
            key.submit(""),
            Transition::Done(ProviderLoginRequest::Custom {
                name: "LM Studio".into(),
                base_url: "http://localhost:1234/v1".into(),
                api_key: None,
            })
        );
    }

    #[test]
    fn start_with_argument_skips_the_picker() {
        let step = ProviderSetup::start(Some("OpenAI")).unwrap();
        assert_eq!(step.title(), "Connect OpenAI");
        let err = ProviderSetup::start(Some("groq")).unwrap_err();
        assert!(
            err.contains("Usage: /login [soket|openrouter|openai|custom]"),
            "{err}"
        );
    }

    #[test]
    fn retry_key_returns_to_the_key_step() {
        let preset = ProviderLoginRequest::Preset {
            id: "openai".into(),
            api_key: "bad".into(),
        };
        assert_eq!(ProviderSetup::retry_key(&preset).title(), "Connect OpenAI");
        let custom = ProviderLoginRequest::Custom {
            name: "G".into(),
            base_url: "http://g/v1".into(),
            api_key: None,
        };
        assert!(matches!(
            ProviderSetup::retry_key(&custom),
            ProviderSetup::ApiKey {
                target: LoginTarget::Custom { .. }
            }
        ));
    }
}
