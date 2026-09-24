//! Configuration paths, settings, auth, and trust.

pub mod auth;
pub mod paths;
pub mod providers;
pub mod settings;
pub mod tracing;
pub mod trust;

pub use auth::{provider_has_key, FileCredentialStore};
pub use paths::*;
pub use providers::{CustomProviderEntry, ProviderLoginRequest};
pub use settings::{load_settings, Settings};
pub use tracing::{
    describe_tracing_status, resolve_tracing_destination, validate_http_url, TracingBackend,
    TracingControl, TracingSettings, TracingSetupRequest,
};
pub use trust::TrustStore;
