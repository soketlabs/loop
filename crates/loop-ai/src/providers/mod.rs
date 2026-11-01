//! Built-in provider factories.

pub mod anthropic;
pub mod custom;
pub mod faux;
pub mod gemini;
pub mod native;
pub mod openai_compatible;
pub mod presets;
pub mod soket;

pub use anthropic::{
    anthropic_models, anthropic_provider, anthropic_seed_models, list_anthropic_models,
    ANTHROPIC_API_KEY_ENVS, ANTHROPIC_BASE_URL, ANTHROPIC_PROVIDER_ID,
};
pub use custom::{custom_provider, CustomModelSpec, CustomProviderConfig};
pub use faux::{faux_provider, faux_provider_fn, FauxResponder, FauxResponse, FauxScript};
pub use gemini::{
    gemini_models, gemini_provider, gemini_seed_models, list_gemini_models, GEMINI_API_KEY_ENVS,
    GEMINI_BASE_URL, GEMINI_PROVIDER_ID,
};
pub use native::{native_provider, ListModelsFn, NativeProviderConfig};
pub use openai_compatible::{
    openai_compatible_provider, ModelDefaults, ModelFilter, OpenAiCompatibleConfig,
};
pub use presets::{
    provider_preset, PresetWire, ProviderPreset, OPENAI_PROVIDER_ID, OPENROUTER_PROVIDER_ID,
    PROVIDER_PRESETS,
};
pub use soket::{
    soket_provider, soket_seed_models, SOKET_API_KEY_ENVS, SOKET_BASE_URL, SOKET_DEFAULT_MODEL_ID,
    SOKET_PROVIDER_ID, SOKET_PROVIDER_NAME,
};
