//! Anonymous mode: strip user content from spans before they are queued for export.
//!
//! The local usage log is always anonymous; the configured destination is anonymous
//! while `/tracing anonymous on` is set.
//!
//! Applied once per span as it ends, so the span tree, timings, model, token usage, cost,
//! tool names and error levels are still exported while prompts, completions, tool
//! arguments and results, error text and log events never leave the process. Whether a
//! span is redacted is decided when it ends; switching anonymous mode off later does not
//! release content from spans that were already redacted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::trace::Status;
use opentelemetry::Context;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use opentelemetry_sdk::Resource;

use crate::obs::keys;

/// Attributes that can carry user content or identity.
const REDACTED_KEYS: [&str; 4] = [
    keys::INPUT,
    keys::OUTPUT,
    keys::STATUS_MESSAGE,
    keys::USER_ID,
];

/// Remove user content from a finished span, keeping its shape and measurements.
pub(crate) fn anonymize(span: &mut SpanData) {
    span.attributes
        .retain(|kv| !REDACTED_KEYS.contains(&kv.key.as_str()));
    // Events are forwarded `loop_*` warnings, which can quote paths and messages.
    span.events = Default::default();
    if let Status::Error { .. } = span.status {
        span.status = Status::error("");
    }
}

/// One export sink: forwards finished spans to `inner` while the sink is active,
/// anonymizing them first while its anonymous switch is on.
#[derive(Debug)]
pub(crate) struct SinkProcessor<P> {
    inner: P,
    active: Arc<AtomicBool>,
    anonymous: Arc<AtomicBool>,
}

impl<P> SinkProcessor<P> {
    pub(crate) fn new(inner: P, active: Arc<AtomicBool>, anonymous: Arc<AtomicBool>) -> Self {
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
        if self.anonymous.load(Ordering::Acquire) {
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
