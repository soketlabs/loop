//! Configuration paths, settings, auth, and trust.

pub mod auth;
pub mod paths;
pub mod settings;
pub mod tracing;
pub mod trust;

pub use auth::{provider_has_key, FileCredentialStore};
pub use paths::*;
pub use settings::{load_settings, Settings};
pub use tracing::{resolve_tracing_credentials, store_tracing_credentials, TracingSettings};
pub use trust::TrustStore;
