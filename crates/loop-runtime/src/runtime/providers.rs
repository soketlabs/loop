//! `runtime/providers`: the host's provider logins (host-only; the wire
//! refuses it without host-local authority).
//!
//! Keys go to the Runtime's credential store and providers to its model
//! catalog, both shared by every Session: a Session's model request reads
//! its key when it is sent, so new and running Sessions use a changed key
//! from their next request. A change that alters what loaded workspaces
//! cached (the global settings' web search or default model, or the Soket
//! key that powers the default web tools) reloads them, as
//! `runtime/defaults` does.

use std::sync::atomic::Ordering;

use loop_app_core::config::paths;
use loop_app_core::config::{ProviderLoginRequest, Settings};
use loop_app_core::provider_login::ProviderStores;
use loop_protocol::host::{HostAction, ProviderInfo, ProvidersParams, ProvidersResult};
use loop_protocol::{BackendError, BackendResult, ErrorCategory};

use super::Runtime;

/// A refused or failed login, with its cause chain.
fn refused(error: impl std::fmt::Display) -> BackendError {
    BackendError::failed(ErrorCategory::Other, format!("{error:#}"))
}

fn malformed(message: impl Into<String>) -> BackendError {
    BackendError::failed(ErrorCategory::Protocol, message)
}

impl Runtime {
    /// List, connect or disconnect a model or web-search provider. Writes
    /// run under the admin lock, so they never interleave with
    /// `runtime/defaults` or a workspace change.
    pub(super) async fn manage_providers(
        &self,
        params: ProvidersParams,
    ) -> BackendResult<ProvidersResult> {
        if params.action != HostAction::List {
            self.refuse_if_locked("runtime/providers")?;
        }
        let _admin = self.admin.lock().await;
        let globals = &self.factory.globals;
        let stores = ProviderStores {
            agent_dir: &globals.agent_dir,
            models: &globals.models,
            credentials: &globals.credentials,
        };
        let path = paths::settings_path(&globals.agent_dir);
        let mut settings = Settings::load_file(&path).map_err(refused)?;
        let mut result = ProvidersResult::default();
        let reload = match params.action {
            HostAction::List => {
                result.providers = stores
                    .connected(&settings)
                    .into_iter()
                    .map(|id| ProviderInfo {
                        name: id.clone(),
                        id,
                        connected: true,
                        model_count: None,
                    })
                    .collect();
                false
            }
            HostAction::Connect => {
                let login: ProviderLoginRequest = serde_json::from_value(
                    params
                        .login
                        .ok_or_else(|| malformed("missing provider login"))?,
                )
                .map_err(|e| malformed(format!("invalid provider login: {e}")))?;
                let (row, reload) = match &login {
                    ProviderLoginRequest::WebSearch { id, api_key } => {
                        let name = stores
                            .connect_web_search(&mut settings, id, api_key)
                            .await
                            .map_err(refused)?;
                        let row = ProviderInfo {
                            id: login.provider_id(),
                            name,
                            connected: true,
                            model_count: None,
                        };
                        (row, true)
                    }
                    _ => {
                        let connected = stores
                            .connect_model_provider(&mut settings, &login)
                            .await
                            .map_err(refused)?;
                        let soket = connected.id == loop_ai::providers::SOKET_PROVIDER_ID;
                        let row = ProviderInfo {
                            id: connected.id,
                            name: connected.name,
                            connected: true,
                            model_count: Some(connected.model_count),
                        };
                        (row, soket)
                    }
                };
                result.providers.push(row);
                reload
            }
            HostAction::Disconnect => {
                let id = params
                    .provider
                    .ok_or_else(|| malformed("missing provider name"))?
                    .trim()
                    .to_ascii_lowercase();
                let gone = stores.disconnect(&mut settings, &id).map_err(refused)?;
                result.notices.push(if gone.search_only {
                    format!(
                        "this Runtime's {} search is blocked until /login",
                        gone.name
                    )
                } else {
                    "this Runtime's Sessions stop using it from their next request".into()
                });
                // A default model of a logged-out provider is cleared, as
                // the in-process `/logout` does.
                let cleared = !gone.search_only
                    && settings
                        .selected_model()
                        .is_some_and(|(provider, _)| provider == id);
                if cleared {
                    settings.clear_selected_model();
                    settings.save_file(&path).map_err(refused)?;
                    self.defaults_revision.fetch_add(1, Ordering::AcqRel);
                    result.notices.push(format!(
                        "the default model was from {}; it is cleared",
                        gone.name
                    ));
                }
                let soket = id == loop_ai::providers::SOKET_PROVIDER_ID;
                result.providers.push(ProviderInfo {
                    id,
                    name: gone.name,
                    connected: false,
                    model_count: None,
                });
                gone.search_only || cleared || soket
            }
            _ => return Err(malformed("use list, connect or disconnect")),
        };
        if reload {
            self.reload_loaded_workspaces().await?;
        }
        Ok(result)
    }
}
