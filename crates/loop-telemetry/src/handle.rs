//! Process-wide telemetry state: tracer provider, export gate and the two sinks.
//!
//! * The **usage log** is an always-on, always-anonymous JSON-lines file per process,
//!   read by `/usage` and `loop usage`. It never leaves the machine.
//! * The **destination** is what `/tracing setup` configured (Langfuse or OTLP), with
//!   its own on/off switch.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider, SpanExporter,
};
use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
use opentelemetry_sdk::Resource;
use parking_lot::Mutex;
use tracing::metadata::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::anonymize::SinkProcessor;
use crate::credentials::{CredentialSource, TelemetryDestination};
use crate::exporter::{otlp_exporter, ExporterSlot};
use crate::file_exporter::{self, FileSpanExporter, TRACE_RETENTION};
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
    /// Spans go to the configured destination only when one exists and the user has
    /// not disabled it. The usage log is independent of this.
    pub fn active(&self) -> bool {
        self.enabled && self.destination.is_some()
    }
}

/// Where the usage log is written, shown by `/usage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLog {
    /// This process's usage log file.
    pub path: PathBuf,
    /// Error from the most recent failed write, cleared by the next success.
    pub last_error: Option<String>,
}

/// Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct TelemetryHandle {
    provider: SdkTracerProvider,
    slot: ExporterSlot,
    gate: ExportGate,
    enabled: Arc<AtomicBool>,
    /// Destination installed and enabled.
    remote_active: Arc<AtomicBool>,
    destination: Arc<Mutex<Option<(String, CredentialSource)>>>,
    local_slot: ExporterSlot,
    /// The usage log's batch processor, kept so it can be flushed on its own.
    local_processor: Arc<BatchSpanProcessor>,
    local_active: Arc<AtomicBool>,
    local_log: Arc<Mutex<Option<PathBuf>>>,
}

impl std::fmt::Debug for TelemetryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryHandle")
            .field("status", &self.status())
            .finish()
    }
}

impl TelemetryHandle {
    /// Create the provider with no destination and no usage log. `release` becomes
    /// `service.version`.
    ///
    /// Nothing is exported until [`install`](Self::install) or
    /// [`install_local`](Self::install_local) succeeds.
    pub fn new(release: &str, enabled: bool) -> Self {
        let slot = ExporterSlot::default();
        let local_slot = ExporterSlot::default();
        let remote_active = Arc::new(AtomicBool::new(false));
        let local_active = Arc::new(AtomicBool::new(false));
        let local_processor = Arc::new(batch_processor(&local_slot));
        let provider = SdkTracerProvider::builder()
            .with_resource(
                Resource::builder()
                    .with_service_name(SERVICE_NAME)
                    .with_attribute(KeyValue::new("service.version", release.to_string()))
                    .build(),
            )
            .with_span_processor(SinkProcessor::new(
                batch_processor(&slot),
                remote_active.clone(),
                false,
            ))
            .with_span_processor(SinkProcessor::new(
                SharedProcessor(local_processor.clone(), local_slot.clone()),
                local_active.clone(),
                true,
            ))
            .build();
        Self {
            provider,
            slot,
            gate: ExportGate::default(),
            enabled: Arc::new(AtomicBool::new(enabled)),
            remote_active,
            destination: Arc::new(Mutex::new(None)),
            local_slot,
            local_processor,
            local_active,
            local_log: Arc::new(Mutex::new(None)),
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

    /// Start the anonymous usage log: this process's file in `dir`, after deleting
    /// files there older than [`TRACE_RETENTION`]. Returns the file path.
    pub fn install_local(&self, dir: &Path) -> PathBuf {
        file_exporter::prune_old_traces(dir, TRACE_RETENTION);
        let path = file_exporter::trace_file_path(dir);
        self.install_local_exporter(FileSpanExporter::new(path.clone()), path.clone());
        path
    }

    /// Usage log to an arbitrary exporter (tests use the in-memory exporter); `path` is
    /// what [`usage_log`](Self::usage_log) reports.
    pub fn install_local_exporter<E: SpanExporter + 'static>(&self, exporter: E, path: PathBuf) {
        self.local_slot.install(exporter);
        *self.local_log.lock() = Some(path);
        self.local_active.store(true, Ordering::Release);
        self.refresh_gate();
    }

    /// The usage log, once installed.
    pub fn usage_log(&self) -> Option<UsageLog> {
        let path = self.local_log.lock().clone()?;
        Some(UsageLog {
            path,
            last_error: self.local_slot.last_error(),
        })
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

    /// Block until spans queued for the usage log are written. Unlike
    /// [`flush`](Self::flush) it never waits on the network.
    ///
    /// Blocking: call from `spawn_blocking` / a plain thread inside async code.
    pub fn flush_local(&self) {
        if let Err(err) = self.local_processor.force_flush() {
            tracing::warn!(target: "loop_telemetry", error = %err, "usage log flush failed");
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
        let remote = self.status().active();
        self.remote_active.store(remote, Ordering::Release);
        self.gate
            .set(remote || self.local_active.load(Ordering::Acquire));
    }
}

/// A batch processor shared with the handle, so the usage log can be flushed without
/// flushing the configured destination.
#[derive(Debug)]
struct SharedProcessor(Arc<BatchSpanProcessor>, ExporterSlot);

impl SpanProcessor for SharedProcessor {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
        self.0.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        self.0.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        // The processor is shared, so hand the resource to its exporter's slot directly.
        self.1.set_resource(resource);
    }
}

fn batch_processor(slot: &ExporterSlot) -> BatchSpanProcessor {
    BatchSpanProcessor::builder(slot.sdk_exporter())
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
                .build(),
        )
        .build()
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
