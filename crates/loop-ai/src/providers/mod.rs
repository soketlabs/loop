//! Built-in provider factories.

pub mod custom;
pub mod faux;
pub mod openai_compatible;
pub mod presets;
pub mod soket;

pub use custom::{custom_provider, CustomModelSpec, CustomProviderConfig};
pub use faux::{faux_provider, FauxResponse, FauxScript};
pub use openai_compatible::{
    openai_compatible_provider, ModelDefaults, ModelFilter, OpenAiCompatibleConfig,
};
pub use presets::{
    provider_preset, ProviderPreset, OPENAI_PROVIDER_ID, OPENROUTER_PROVIDER_ID, PROVIDER_PRESETS,
};
pub use soket::{
    soket_provider, soket_seed_models, SOKET_API_KEY_ENVS, SOKET_BASE_URL, SOKET_DEFAULT_MODEL_ID,
    SOKET_PROVIDER_ID, SOKET_PROVIDER_NAME,
};
