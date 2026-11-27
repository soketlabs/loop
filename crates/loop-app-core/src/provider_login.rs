//! `/login` and `/logout` for model and web-search providers, shared by the
//! in-process app ([`crate::Runtime`]) and the Runtime daemon.
//!
//! Each step verifies before it writes and puts back what it wrote when a
//! later step fails. The catalog and the credential store are updated in
//! place, so everything that shares them sees the change at once; callers
//! refresh what they derived from them (a selected model, tool sets, loaded
//! workspaces).

use std::path::Path;
use std::sync::Arc;

use loop_agent::harness::{verify_web_search_key, WebSearchConfig};
use loop_ai::{CredentialStore, Models};

use crate::config::auth::FileCredentialStore;
use crate::config::paths::settings_path;
use crate::config::providers::upsert_custom_provider;
use crate::config::settings::Settings;
use crate::config::ProviderLoginRequest;
use crate::runtime::{load_custom_models_json, ConnectedProvider};

/// The host's provider state that a login or logout changes.
pub struct ProviderStores<'a> {
    /// Loop's per-user state directory; its settings file is rewritten.
    pub agent_dir: &'a Path,
    /// The model catalog the provider is registered in.
    pub models: &'a Models,
    /// The credential store (`auth.json`) the key is saved in.
    pub credentials: &'a FileCredentialStore,
}

/// What a `/logout` removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnected {
    /// Display name.
    pub name: String,
    /// Only a web-search backend's own key was removed; no model provider
    /// changed.
    pub search_only: bool,
}

/// Whether `provider` (trimmed, lower case) names a web-search backend with
/// its own key, rather than one that shares a model provider's key.
pub fn is_search_only(provider: &str) -> bool {
    loop_agent::harness::web_search_backend(provider)
        .is_some_and(|backend| !backend.shares_model_provider_key)
}

impl ProviderStores<'_> {
    fn save(&self, settings: &Settings) -> anyhow::Result<()> {
        settings.save_file(&settings_path(self.agent_dir))
    }

    /// Save a model provider's key (and custom entry in `settings`), register
    /// it in the catalog and list its models. Nothing is kept if the key is
    /// rejected or the listing fails.
    pub async fn connect_model_provider(
        &self,
        settings: &mut Settings,
        request: &ProviderLoginRequest,
    ) -> anyhow::Result<ConnectedProvider> {
        request.validate()?;
        let (id, name) = (request.provider_id(), request.display_name());
        if let (Some(preset), Some(key)) = (request.preset(), request.api_key()) {
            preset
                .verify_key(key)
                .await
                .map_err(|e| anyhow::anyhow!("{name} rejected the key: {e}"))?;
        }
        let mut entry = request.custom_entry();
        if let Some(entry) = &mut entry {
            if entry.models.is_none() {
                entry.models = settings
                    .providers
                    .iter()
                    .find(|saved| saved.id == entry.id)
                    .and_then(|saved| saved.models.clone());
            }
        }
        let provider = match &entry {
            Some(entry) => {
                let candidate = Models::new();
                candidate.set_provider(entry.provider());
                load_custom_models_json(self.agent_dir, &candidate, std::slice::from_ref(entry))?;
                candidate.provider(&id).expect("registered custom provider")
            }
            None => self
                .models
                .provider(&id)
                .ok_or_else(|| anyhow::anyhow!("unknown provider {id}"))?,
        };
        let previous_models = provider.models();
        // Validate without saving a candidate key or lifting explicit logout.
        provider
            .refresh_models(loop_ai::RefreshModelsContext {
                api_key: request.api_key().map(str::to_owned),
                store: Some(Arc::clone(self.models.models_store())),
                allow_network: true,
                force: true,
            })
            .await
            .map_err(|e| anyhow::anyhow!("could not list {name} models: {e}"))?;
        let model_count = provider.models().len();
        if model_count == 0 {
            anyhow::bail!("could not list {name} models: no models were listed");
        }
        let previous_custom = settings.providers.clone();
        if let Some(entry) = &entry {
            upsert_custom_provider(&mut settings.providers, entry.clone());
            if let Err(error) = self.save(settings) {
                settings.providers = previous_custom;
                provider.set_models(previous_models);
                return Err(error);
            }
        }
        if let Err(error) = self
            .credentials
            .connect(&id, request.api_key().map(loop_ai::Credential::api_key))
        {
            provider.set_models(previous_models);
            if entry.is_some() {
                settings.providers = previous_custom;
                self.save(settings)?;
            }
            return Err(error.context("could not save login"));
        }
        for model in provider.models() {
            let previous = self.models.model(&id, &model.id);
            if previous.as_ref().is_none_or(|previous| {
                previous.base_url != model.base_url
                    || loop_ai::resolve_compat(&previous.base_url, previous.compat.as_ref())
                        .thinking_format
                        != loop_ai::resolve_compat(&model.base_url, model.compat.as_ref())
                            .thinking_format
            }) {
                loop_ai::clear_reasoning_rejection(&model);
            }
        }
        self.models.set_provider(provider);
        Ok(ConnectedProvider {
            id,
            name,
            model_count,
        })
    }

    /// Verify a web-search backend's key with a probe search, select and
    /// enable that backend in `settings` and save the key. Nothing is kept if
    /// the key is rejected. Returns the display name.
    pub async fn connect_web_search(
        &self,
        settings: &mut Settings,
        provider: &str,
        api_key: &str,
    ) -> anyhow::Result<String> {
        let provider = provider.trim().to_ascii_lowercase();
        let name = crate::config::web_search_display_name(&provider);
        if !crate::config::is_web_search_provider(&provider) {
            anyhow::bail!("unknown web-search provider '{provider}'");
        }
        if api_key.trim().is_empty() {
            anyhow::bail!("{name} needs an API key");
        }
        // Keep a same-backend baseUrl / private-address opt-in when re-logging in.
        let mut config = match &settings.web_search {
            Some(existing) if existing.provider_id() == provider => existing.clone(),
            _ => WebSearchConfig::default(),
        };
        config.provider = Some(provider.clone());
        config.enabled = true;
        verify_web_search_key(&config, api_key)
            .await
            .map_err(|e| anyhow::anyhow!("{name} rejected the key: {e}"))?;

        let previous_config = settings.web_search.replace(config);
        if let Err(error) = self.save(settings) {
            settings.web_search = previous_config;
            return Err(error);
        }
        if let Err(error) = self
            .credentials
            .connect(&provider, Some(loop_ai::Credential::api_key(api_key)))
        {
            settings.web_search = previous_config;
            self.save(settings)?;
            return Err(error.context("could not save login"));
        }
        Ok(name)
    }

    /// Log out of a model or search provider: a search backend that shares a
    /// model provider's key logs out that model provider.
    pub fn disconnect(
        &self,
        settings: &mut Settings,
        provider: &str,
    ) -> anyhow::Result<Disconnected> {
        let provider = provider.trim().to_ascii_lowercase();
        if is_search_only(&provider) {
            Ok(Disconnected {
                name: self.disconnect_web_search(&provider)?,
                search_only: true,
            })
        } else {
            Ok(Disconnected {
                name: self.disconnect_model_provider(settings, &provider)?,
                search_only: false,
            })
        }
    }

    /// Block a search backend until login. The selection stays, so a logout
    /// never silently switches search providers.
    pub fn disconnect_web_search(&self, provider: &str) -> anyhow::Result<String> {
        let provider = provider.trim().to_ascii_lowercase();
        let name = crate::config::web_search_display_name(&provider);
        if !crate::config::is_web_search_provider(&provider) {
            anyhow::bail!("unknown search provider {provider}");
        }
        self.credentials.logout(&provider)?;
        Ok(name)
    }

    /// Forget a model provider's key; a custom provider is removed from
    /// `settings` and the catalog. Returns the display name.
    pub fn disconnect_model_provider(
        &self,
        settings: &mut Settings,
        id: &str,
    ) -> anyhow::Result<String> {
        let id = id.trim().to_ascii_lowercase();
        let custom_index = settings.providers.iter().position(|p| p.id == id);
        let preset = loop_ai::providers::provider_preset(&id);
        if custom_index.is_none()
            && preset.is_none()
            && self.models.provider(&id).is_none()
            && self.credentials.get(&id).is_none()
        {
            anyhow::bail!("{id} is not connected");
        }
        self.credentials.logout(&id)?;
        Ok(match custom_index {
            Some(index) => {
                let entry = settings.providers.remove(index);
                self.models.remove_provider(&id);
                self.save(settings)?;
                entry.name
            }
            None => preset.map_or_else(|| id.clone(), |p| p.name.to_string()),
        })
    }

    /// Ids of the providers usable now: presets with a key (saved, or in this
    /// process's environment), then custom providers.
    pub fn connected(&self, settings: &Settings) -> Vec<String> {
        crate::config::providers::connected_providers(self.credentials, &settings.providers, |k| {
            std::env::var(k).ok()
        })
    }
}
