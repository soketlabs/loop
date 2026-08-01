//! Injected stream function used by the agent loop.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use loop_ai::{AssistantMessageEventStream, Context, Model, Models, SimpleStreamOptions};
use parking_lot::RwLock;

/// Stream function used by the agent loop.
///
/// Contract:
/// - Must not panic / return a rejected future for request/model/runtime failures.
/// - Must return an [`AssistantMessageEventStream`].
/// - Failures must be encoded via stream events and a final assistant message with
///   `stop_reason` `error` or `aborted`.
pub type StreamFn = Arc<
    dyn Fn(
            Model,
            Context,
            SimpleStreamOptions,
        ) -> Pin<Box<dyn Future<Output = AssistantMessageEventStream> + Send>>
        + Send
        + Sync,
>;

static DEFAULT_STREAM_FN: RwLock<Option<StreamFn>> = RwLock::new(None);

/// Install a process-wide default stream function for callers that omit one.
pub fn set_default_stream_fn(stream_fn: StreamFn) {
    *DEFAULT_STREAM_FN.write() = Some(stream_fn);
}

/// Get the process-wide default stream function, if set.
pub fn get_default_stream_fn() -> Option<StreamFn> {
    DEFAULT_STREAM_FN.read().clone()
}

/// Clear the process-wide default stream function.
pub fn clear_default_stream_fn() {
    *DEFAULT_STREAM_FN.write() = None;
}

/// Build a [`StreamFn`] from a [`Models`] collection (`stream_simple`).
pub fn stream_fn_from_models(models: Arc<Models>) -> StreamFn {
    Arc::new(move |model, context, options| {
        let models = Arc::clone(&models);
        Box::pin(async move { models.stream_simple(&model, &context, options) })
    })
}

/// Resolve an explicit stream fn or the default; panics if neither is set.
pub(crate) fn resolve_stream_fn(stream_fn: Option<StreamFn>) -> StreamFn {
    stream_fn
        .or_else(get_default_stream_fn)
        .expect("stream_fn required: pass one or call set_default_stream_fn")
}
