//! Provider and Models collection.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::auth::{
    check_provider_auth, resolve_provider_auth, AuthCheck, CredentialStore,
    InMemoryCredentialStore, ModelAuth, ProviderAuth,
};
use crate::stream::AssistantMessageEventStream;
use crate::types::{
    Context, Model, ModelThinkingLevel, ProviderHeaders, SimpleStreamOptions, StreamOptions,
    ThinkingLevel,
};

/// Uniform stream contract for a wire-API adapter.
pub trait ApiAdapter: Send + Sync {
    /// Provider-native stream options.
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream;

    /// Unified simple options (reasoning levels, etc.).
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream;
}

/// Shared adapter handle.
pub type SharedApiAdapter = Arc<dyn ApiAdapter>;

/// Concrete runtime provider unit.
pub struct Provider {
    /// Provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Optional default base URL.
    pub base_url: Option<String>,
    /// Optional default headers.
    pub headers: Option<HashMap<String, String>>,
    /// Auth methods.
    pub auth: ProviderAuth,
    models: RwLock<Vec<Model>>,
    /// Single adapter, or per-api map.
    adapters: AdapterSet,
}

enum AdapterSet {
    Single(SharedApiAdapter),
    ByApi(HashMap<String, SharedApiAdapter>),
}

impl Provider {
    /// Sync catalog of known models.
    pub fn get_models(&self) -> Vec<Model> {
        self.models.read().clone()
    }

    /// Lookup a model by id.
    pub fn get_model(&self, id: &str) -> Option<Model> {
        self.models.read().iter().find(|m| m.id == id).cloned()
    }

    /// Replace/extend the model list (e.g. after refresh).
    pub fn set_models(&self, models: Vec<Model>) {
        *self.models.write() = models;
    }

    fn adapter_for(&self, model: &Model) -> Option<SharedApiAdapter> {
        match &self.adapters {
            AdapterSet::Single(a) => Some(Arc::clone(a)),
            AdapterSet::ByApi(map) => map.get(&model.api).cloned(),
        }
    }

    /// Stream with provider-native options. Auth must already be resolved.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        match self.adapter_for(model) {
            Some(adapter) => adapter.stream(model, context, options, auth),
            None => error_stream(
                model,
                format!(
                    "provider {} has no API implementation for \"{}\"",
                    self.id, model.api
                ),
            ),
        }
    }

    /// Stream with simple options.
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        match self.adapter_for(model) {
            Some(adapter) => adapter.stream_simple(model, context, options, auth),
            None => error_stream(
                model,
                format!(
                    "provider {} has no API implementation for \"{}\"",
                    self.id, model.api
                ),
            ),
        }
    }
}

fn error_stream(model: &Model, message: String) -> AssistantMessageEventStream {
    use crate::stream::create_assistant_message_event_stream;
    use crate::types::{AssistantMessageEvent, StopReason};

    let stream = create_assistant_message_event_stream();
    let handle = stream.handle();
    let mut msg = crate::types::AssistantMessage::pending(model);
    msg.stop_reason = StopReason::Error;
    msg.error_message = Some(message);
    handle.push(AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: msg,
    });
    stream
}

/// Options for [`create_provider`].
pub struct CreateProviderOptions {
    /// Provider id.
    pub id: String,
    /// Display name (defaults to id).
    pub name: Option<String>,
    /// Optional base URL.
    pub base_url: Option<String>,
    /// Optional default headers.
    pub headers: Option<HashMap<String, String>>,
    /// Auth configuration.
    pub auth: ProviderAuth,
    /// Static model catalog.
    pub models: Vec<Model>,
    /// API adapter(s).
    pub api: CreateProviderApi,
}

/// How adapters are attached to a provider.
pub enum CreateProviderApi {
    /// Single adapter for all models.
    Single(SharedApiAdapter),
    /// Map of api id → adapter.
    ByApi(HashMap<String, SharedApiAdapter>),
}

impl From<SharedApiAdapter> for CreateProviderApi {
    fn from(adapter: SharedApiAdapter) -> Self {
        Self::Single(adapter)
    }
}

/// Build a [`Provider`].
pub fn create_provider(input: CreateProviderOptions) -> Provider {
    let adapters = match input.api {
        CreateProviderApi::Single(a) => AdapterSet::Single(a),
        CreateProviderApi::ByApi(m) => AdapterSet::ByApi(m),
    };
    Provider {
        id: input.id.clone(),
        name: input.name.unwrap_or(input.id),
        base_url: input.base_url,
        headers: input.headers,
        auth: input.auth,
        models: RwLock::new(input.models),
        adapters,
    }
}

/// Options when constructing a [`Models`] collection.
pub struct CreateModelsOptions {
    /// Credential store (defaults to in-memory).
    pub credentials: Option<Arc<dyn CredentialStore>>,
}

impl Default for CreateModelsOptions {
    fn default() -> Self {
        Self { credentials: None }
    }
}

/// Runtime collection of providers plus auth application and stream convenience.
pub struct Models {
    providers: RwLock<HashMap<String, Arc<Provider>>>,
    credentials: Arc<dyn CredentialStore>,
}

impl Models {
    /// Create an empty collection with an in-memory credential store.
    pub fn new() -> Self {
        Self::create(CreateModelsOptions::default())
    }

    /// Create with options.
    pub fn create(options: CreateModelsOptions) -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            credentials: options
                .credentials
                .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
        }
    }

    /// Credential store handle.
    pub fn credentials(&self) -> &Arc<dyn CredentialStore> {
        &self.credentials
    }

    /// Register or replace a provider.
    pub fn set_provider(&self, provider: Provider) {
        let id = provider.id.clone();
        self.providers.write().insert(id, Arc::new(provider));
    }

    /// All providers.
    pub fn get_providers(&self) -> Vec<Arc<Provider>> {
        self.providers.read().values().cloned().collect()
    }

    /// Lookup provider by id.
    pub fn get_provider(&self, id: &str) -> Option<Arc<Provider>> {
        self.providers.read().get(id).cloned()
    }

    /// Sync models from one provider or all.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let providers = self.providers.read();
        match provider {
            Some(id) => providers
                .get(id)
                .map(|p| p.get_models())
                .unwrap_or_default(),
            None => providers.values().flat_map(|p| p.get_models()).collect(),
        }
    }

    /// Sync model lookup.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.providers.read().get(provider)?.get_model(id)
    }

    /// Check auth without network OAuth refresh.
    pub async fn check_auth(&self, provider_id: &str) -> Option<AuthCheck> {
        let provider = self.get_provider(provider_id)?;
        Some(check_provider_auth(provider_id, &provider.auth, self.credentials.as_ref()).await)
    }

    /// Models whose providers are configured.
    pub async fn get_available(&self) -> Vec<Model> {
        let providers = self.get_providers();
        let mut out = Vec::new();
        for provider in providers {
            let check =
                check_provider_auth(&provider.id, &provider.auth, self.credentials.as_ref()).await;
            if check.configured {
                out.extend(provider.get_models());
            }
        }
        out
    }

    fn merge_headers(
        provider: &Provider,
        model: &Model,
        auth: &ModelAuth,
        options: &StreamOptions,
    ) -> ProviderHeaders {
        let mut headers: ProviderHeaders = HashMap::new();
        for (k, v) in &auth.headers {
            headers.insert(k.clone(), Some(v.clone()));
        }
        if let Some(ph) = &provider.headers {
            for (k, v) in ph {
                headers.insert(k.clone(), Some(v.clone()));
            }
        }
        if let Some(mh) = &model.headers {
            for (k, v) in mh {
                headers.insert(k.clone(), Some(v.clone()));
            }
        }
        if let Some(oh) = &options.headers {
            for (k, v) in oh {
                headers.insert(k.clone(), v.clone());
            }
        }
        headers
    }

    /// Stream with auth resolution. Failures after invoke are stream-encoded.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let Some(provider) = self.get_provider(&model.provider) else {
            return error_stream(
                model,
                format!("provider not found: {}", model.provider),
            );
        };
        self.stream_with_provider(provider, model, context, options)
    }

    fn stream_with_provider(
        &self,
        provider: Arc<Provider>,
        model: &Model,
        context: &Context,
        mut options: StreamOptions,
    ) -> AssistantMessageEventStream {
        use crate::stream::create_assistant_message_event_stream;
        use crate::types::{AssistantMessageEvent, StopReason};
        use futures::StreamExt;

        let stream = create_assistant_message_event_stream();
        let handle = stream.handle();
        let model = model.clone();
        let context = context.clone();
        let credentials = Arc::clone(&self.credentials);
        let provider_id = provider.id.clone();
        let provider_auth = provider.auth.clone();

        tokio::spawn(async move {
            let auth = match resolve_provider_auth(
                &provider_id,
                &provider_auth,
                credentials.as_ref(),
                options.api_key.as_deref(),
            )
            .await
            {
                Ok(a) => a,
                Err(e) => {
                    let mut msg = crate::types::AssistantMessage::pending(&model);
                    msg.stop_reason = StopReason::Error;
                    msg.error_message = Some(e.to_string());
                    handle.push(AssistantMessageEvent::Error {
                        reason: StopReason::Error,
                        error: msg,
                    });
                    return;
                }
            };

            let merged = Self::merge_headers(&provider, &model, &auth, &options);
            options.headers = Some(merged);

            let mut inner = provider.stream(&model, &context, options, auth);
            while let Some(ev) = inner.next().await {
                let terminal = ev.is_terminal();
                handle.push(ev);
                if terminal {
                    break;
                }
            }
        });

        stream
    }

    /// Non-streaming completion (collects the stream result).
    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> crate::types::AssistantMessage {
        let stream = self.stream(model, context, options);
        stream.result().await
    }

    /// Stream with unified simple options.
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let Some(provider) = self.get_provider(&model.provider) else {
            return error_stream(
                model,
                format!("provider not found: {}", model.provider),
            );
        };

        use crate::stream::create_assistant_message_event_stream;
        use crate::types::{AssistantMessageEvent, StopReason};
        use futures::StreamExt;

        let stream = create_assistant_message_event_stream();
        let handle = stream.handle();
        let model = model.clone();
        let context = context.clone();
        let credentials = Arc::clone(&self.credentials);
        let provider_id = provider.id.clone();
        let provider_auth = provider.auth.clone();

        tokio::spawn(async move {
            let auth = match resolve_provider_auth(
                &provider_id,
                &provider_auth,
                credentials.as_ref(),
                options.base.api_key.as_deref(),
            )
            .await
            {
                Ok(a) => a,
                Err(e) => {
                    let mut msg = crate::types::AssistantMessage::pending(&model);
                    msg.stop_reason = StopReason::Error;
                    msg.error_message = Some(e.to_string());
                    handle.push(AssistantMessageEvent::Error {
                        reason: StopReason::Error,
                        error: msg,
                    });
                    return;
                }
            };

            let mut options = options;
            let merged = Self::merge_headers(&provider, &model, &auth, &options.base);
            options.base.headers = Some(merged);

            let inner = provider.stream_simple(&model, &context, options, auth);
            let mut inner = inner;
            while let Some(ev) = inner.next().await {
                let terminal = ev.is_terminal();
                handle.push(ev);
                if terminal {
                    break;
                }
            }
        });

        stream
    }

    /// Non-streaming simple completion.
    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> crate::types::AssistantMessage {
        let stream = self.stream_simple(model, context, options);
        stream.result().await
    }
}

impl Default for Models {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether two models refer to the same endpoint identity.
pub fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.provider == b.provider && a.id == b.id && a.api == b.api && a.base_url == b.base_url
}

/// Clamp a thinking level against a model's map (unsupported → next lower / off).
pub fn clamp_thinking_level(model: &Model, level: ThinkingLevel) -> Option<ThinkingLevel> {
    if !model.reasoning {
        return None;
    }
    let Some(map) = &model.thinking_level_map else {
        return Some(level);
    };
    let model_level = ModelThinkingLevel::from(level);
    if let Some(entry) = map.get(&model_level) {
        if entry.is_none() {
            return None;
        }
        return Some(level);
    }
    Some(level)
}

pub use crate::utils::calculate_cost;
