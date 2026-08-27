//! Wire-protocol adapters.

pub mod detect_compat;
pub mod http;
pub mod openai_completions;
pub mod openai_models;
pub mod transform_messages;

pub use detect_compat::{detect_compat, resolve_compat};
pub use http::{http_client, streaming_http_client};
pub use openai_completions::OpenAICompletionsAdapter;
pub use openai_models::{list_openai_models, map_remote_model, ListModelsError, MapRemoteModelOptions};
pub use transform_messages::transform_messages;
