//! Authentication types, credential store, and resolution.

mod credential_store;
mod env;
mod resolve;
mod types;

pub use credential_store::{CredentialStore, InMemoryCredentialStore};
pub use env::env_api_key_auth;
pub use resolve::{check_provider_auth, resolve_provider_auth, ModelsError, ModelsErrorCode};
pub use types::*;
