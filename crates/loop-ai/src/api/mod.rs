//! Wire-protocol adapters.

pub mod detect_compat;
pub mod openai_completions;
pub mod transform_messages;

pub use detect_compat::{detect_compat, resolve_compat};
pub use openai_completions::OpenAICompletionsAdapter;
pub use transform_messages::transform_messages;
