//! Anonymous usage log: strip everything but names and measurements from spans before
//! they are written to the local usage log.
//!
//! Applied once per span as it ends. Only the attributes in [`KEPT_KEYS`] and
//! [`KEPT_METADATA`] survive, so the span tree, timings, model, token usage, tool and
//! skill names and run labels are kept, while prompts, completions, tool arguments and
//! results, error text, log events and any attribute added later are dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::trace::Status;
use opentelemetry::Context;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use opentelemetry_sdk::Resource;

use crate::obs::keys;

/// Attributes kept in the usage log: names and measurements, never user content.
const KEPT_KEYS: [&str; 9] = [
    keys::OBSERVATION_TYPE,
    keys::MODEL_NAME,
    keys::GEN_AI_SYSTEM,
    keys::USAGE_DETAILS,
    keys::COMPLETION_START_TIME,
    keys::LEVEL,
    keys::SESSION_ID,
    keys::RELEASE,
    keys::GEN_AI_TOOL_NAME,
];

/// `langfuse.observation.metadata.<key>` entries kept in the usage log.
const KEPT_METADATA: [&str; 13] = [
    "model",
    "provider",
    "agent",
    "skills",
    "skill",
    "session",
    "tool",
    "turn",
    "turns",
    "tool_calls",
    "tool_errors",
    "total_tokens",
    "stop_reason",
];

fn is_kept(key: &str) -> bool {
    KEPT_KEYS.contains(&key)
        || key
            .strip_prefix(keys::METADATA_PREFIX)
            .is_some_and(|name| KEPT_METADATA.contains(&name))
}

/// Remove user content from a finished span, keeping its shape and measurements.
pub(crate) fn anonymize(span: &mut SpanData) {
    span.attributes.retain(|kv| is_kept(kv.key.as_str()));
    // Events are forwarded `loop_*` warnings, which can quote paths and messages.
    span.events = Default::default();
    if let Status::Error { .. } = span.status {
        span.status = Status::error("");
    }
}

/// One export sink: forwards finished spans to `inner` while the sink is active,
/// anonymizing them first when it is the usage log.
#[derive(Debug)]
pub(crate) struct SinkProcessor<P> {
    inner: P,
    active: Arc<AtomicBool>,
    anonymous: bool,
}

impl<P> SinkProcessor<P> {
    pub(crate) fn new(inner: P, active: Arc<AtomicBool>, anonymous: bool) -> Self {
        Self {
            inner,
            active,
            anonymous,
        }
    }
}

impl<P: SpanProcessor> SpanProcessor for SinkProcessor<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        if self.anonymous {
            anonymize(&mut span);
        }
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_listed_keys() {
        assert!(is_kept(keys::MODEL_NAME));
        assert!(is_kept("langfuse.observation.metadata.skill"));
        assert!(!is_kept(keys::INPUT));
        assert!(!is_kept(keys::TRACE_NAME));
        assert!(!is_kept(keys::GEN_AI_TOOL_CALL_ID));
        assert!(!is_kept("langfuse.observation.metadata.response_id"));
        assert!(!is_kept("langfuse.trace.metadata.ticket"));
        assert!(!is_kept("code.filepath"));
        assert!(!is_kept("some.future.attribute"));
    }
}
