//! Built-in provider factories.

pub mod custom;
pub mod faux;

pub use custom::{custom_provider, CustomModelSpec, CustomProviderConfig};
pub use faux::{faux_provider, FauxResponse, FauxScript};
