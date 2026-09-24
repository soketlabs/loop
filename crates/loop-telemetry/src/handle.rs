//! Process-wide telemetry state: tracer provider, export gate and destination.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider, SpanExporter,
};
use opentelemetry_sdk::Resource;
use parking_lot::Mutex;
use tracing::metadata::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::credentials::{CredentialSource, TelemetryDestination};
use crate::exporter::{otlp_exporter, ExporterSlot};
use crate::obs::SPAN_TARGET;

/// Instrumentation scope name reported to the backend.
const TRACER_NAME: &str = "loop";
/// Service name resource attribute.
const SERVICE_NAME: &str = "loop";
/// Spans per OTLP request. Generations carry the full context, so a long run's spans
/// can reach megabytes each; small batches keep requests under ingestion body limits.
const MAX_EXPORT_BATCH_SIZE: usize = 8;

/// Snapshot shown by `/tracing status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryStatus {
    /// User preference (`settings.tracing.enabled`).
    pub enabled: bool,
    /// Description of the installed destination, if any.
    pub destination: Option<String>,
    /// Where the installed credentials came from.
    pub source: Option<CredentialSource>,
    /// Error from the most recent failed export, if the last export failed.
    pub last_error: Option<String>,
}

impl TelemetryStatus {
    /// Spans are exported only when a destination exists and the user has not disabled it.
    pub fn active(&self) -> bool {
        self.enabled && self.destination.is_some()
    }
}

/// Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct TelemetryHandle {
    provider: SdkTracerProvider,
    slot: ExporterSlot,
    gate: ExportGate,
    enabled: Arc<AtomicBool>,
    destination: Arc<Mutex<Option<(String, CredentialSource)>>>,
}

impl std::fmt::Debug for TelemetryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryHandle")
            .field("status", &self.status())
            .finish()
    }
}

impl TelemetryHandle {
    /// Create the provider with no destination. `release` becomes `service.version`.
    ///
    /// Nothing is exported until [`install`](Self::install) succeeds.
    pub fn new(release: &str, enabled: bool) -> Self {
        let slot = ExporterSlot::default();
        let provider = SdkTracerProvider::builder()
            .with_resource(
                Resource::builder()
                    .with_service_name(SERVICE_NAME)
                    .with_attribute(KeyValue::new("service.version", release.to_string()))
                    .build(),
            )
            .with_span_processor(
                BatchSpanProcessor::builder(slot.sdk_exporter())
                    .with_batch_config(
                        BatchConfigBuilder::default()
                            .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
                            .build(),
                    )
                    .build(),
            )
            .build();
        Self {
            provider,
            slot,
            gate: ExportGate::default(),
            enabled: Arc::new(AtomicBool::new(enabled)),
            destination: Arc::new(Mutex::new(None)),
        }
    }

    /// The `tracing` layer that turns Loop's observation spans into OTel spans.
    pub fn layer<S>(&self) -> impl Layer<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        tracing_opentelemetry::layer()
            .with_tracer(self.provider.tracer(TRACER_NAME))
            .with_threads(false)
            .with_filter(self.gate.clone())
    }

    /// Start exporting to `destination`.
    pub fn install(&self, destination: &TelemetryDestination) -> anyhow::Result<()> {
        let exporter = otlp_exporter(destination)?;
        self.install_exporter(exporter, destination.label.clone(), destination.source);
        Ok(())
    }

    /// Export to an arbitrary exporter (tests use the in-memory exporter).
    pub fn install_exporter<E: SpanExporter + 'static>(
        &self,
        exporter: E,
        label: String,
        source: CredentialSource,
    ) {
        self.slot.install(exporter);
        *self.destination.lock() = Some((label, source));
        self.refresh_gate();
    }

    /// Set the user preference; returns the resulting status.
    pub fn set_enabled(&self, enabled: bool) -> TelemetryStatus {
        self.enabled.store(enabled, Ordering::Release);
        self.refresh_gate();
        self.status()
    }

    /// Current state.
    pub fn status(&self) -> TelemetryStatus {
        let destination = self.destination.lock().clone();
        TelemetryStatus {
            enabled: self.enabled.load(Ordering::Acquire),
            source: destination.as_ref().map(|(_, s)| *s),
            destination: destination.map(|(label, _)| label),
            last_error: self.slot.last_error(),
        }
    }

    /// Block until queued spans have been handed to the exporter.
    ///
    /// Blocking: call from `spawn_blocking` / a plain thread inside async code.
    pub fn flush(&self) {
        if let Err(err) = self.provider.force_flush() {
            tracing::warn!(target: "loop_telemetry", error = %err, "trace flush failed");
        }
    }

    /// Flush and stop exporting. Blocking, see [`flush`](Self::flush).
    pub fn shutdown(&self) {
        self.gate.set(false);
        if let Err(err) = self.provider.shutdown() {
            tracing::warn!(target: "loop_telemetry", error = %err, "trace shutdown failed");
        }
    }

    fn refresh_gate(&self) {
        self.gate.set(self.status().active());
    }
}

/// Per-layer filter: lets Loop's observation spans (and Loop warnings, as span events)
/// through while telemetry is active, and nothing else — so HTTP client internals,
/// including the exporter's own requests, are never traced.
#[derive(Clone, Default)]
pub(crate) struct ExportGate {
    open: Arc<AtomicBool>,
}

impl ExportGate {
    fn set(&self, open: bool) {
        self.open.store(open, Ordering::Release);
    }

    fn accepts(meta: &Metadata<'_>) -> bool {
        if meta.is_span() {
            meta.target() == SPAN_TARGET
        } else {
            meta.target().starts_with("loop_") && *meta.level() <= tracing::Level::WARN
        }
    }
}

impl<S> Filter<S> for ExportGate {
    fn enabled(&self, meta: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        self.open.load(Ordering::Acquire) && Self::accepts(meta)
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        // The gate can open or close at runtime, so never let the answer be cached.
        if Self::accepts(meta) {
            Interest::sometimes()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }
}
