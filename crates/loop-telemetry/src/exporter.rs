//! A span exporter whose destination can be installed after the tracer provider is built.
//!
//! The provider and the `tracing` layer exist for the whole process, so `/tracing setup`
//! can start exporting mid-session without rebuilding the subscriber.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use parking_lot::RwLock;

use crate::credentials::TelemetryDestination;

/// Upper bound for one export request.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);

type ExportFuture = Pin<Box<dyn Future<Output = OTelSdkResult> + Send>>;

/// Object-safe view of a [`SpanExporter`] so the slot can hold any exporter.
trait DynExporter: Send + Sync + fmt::Debug {
    fn export(self: Arc<Self>, batch: Vec<SpanData>) -> ExportFuture;
    fn force_flush(&self) -> OTelSdkResult;
    fn shutdown(&self) -> OTelSdkResult;
}

/// Adapts a concrete exporter; the SDK never calls `export` concurrently, but the
/// slot may hand out clones, so access is serialized through an async-agnostic lock.
#[derive(Debug)]
struct Adapter<E>(parking_lot::Mutex<Option<E>>);

impl<E: SpanExporter + 'static> DynExporter for Adapter<E> {
    fn export(self: Arc<Self>, batch: Vec<SpanData>) -> ExportFuture {
        Box::pin(async move {
            // Take the exporter out for the duration of the await so no lock is held across it.
            let Some(exporter) = self.0.lock().take() else {
                return Ok(());
            };
            let result = exporter.export(batch).await;
            *self.0.lock() = Some(exporter);
            result
        })
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.lock().as_ref().map_or(Ok(()), |e| e.force_flush())
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.0.lock().as_ref().map_or(Ok(()), |e| e.shutdown())
    }
}

/// Shared slot holding the current destination, if any.
#[derive(Clone, Default)]
pub(crate) struct ExporterSlot {
    inner: Arc<RwLock<SlotState>>,
}

#[derive(Default)]
struct SlotState {
    exporter: Option<Arc<dyn DynExporter>>,
    resource: Option<Resource>,
    last_error: Option<String>,
}

impl fmt::Debug for ExporterSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExporterSlot")
            .field("installed", &self.is_installed())
            .finish()
    }
}

impl ExporterSlot {
    /// Install (or replace) the destination. The provider's resource is applied to it.
    pub(crate) fn install<E: SpanExporter + 'static>(&self, mut exporter: E) {
        let mut state = self.inner.write();
        if let Some(resource) = &state.resource {
            exporter.set_resource(resource);
        }
        if let Some(previous) = state.exporter.take() {
            let _ = previous.shutdown();
        }
        state.exporter = Some(Arc::new(Adapter(parking_lot::Mutex::new(Some(exporter)))));
        state.last_error = None;
    }

    /// Error from the most recent failed export, cleared by the next success.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.inner.read().last_error.clone()
    }

    fn record_result(&self, result: &OTelSdkResult) {
        self.inner.write().last_error = result.as_ref().err().map(ToString::to_string);
    }

    /// Whether a destination is installed.
    pub(crate) fn is_installed(&self) -> bool {
        self.inner.read().exporter.is_some()
    }

    /// The exporter handed to the SDK's batch processor.
    pub(crate) fn sdk_exporter(&self) -> SlotExporter {
        SlotExporter(self.clone())
    }

    fn current(&self) -> Option<Arc<dyn DynExporter>> {
        self.inner.read().exporter.clone()
    }
}

/// Build the OTLP/HTTP (protobuf) exporter for a destination.
pub(crate) fn otlp_exporter(
    destination: &TelemetryDestination,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    let headers: HashMap<String, String> = destination.headers.iter().cloned().collect();
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(destination.endpoint.clone())
        .with_headers(headers)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
}

/// [`SpanExporter`] registered with the SDK; forwards to whatever the slot holds and
/// drops batches while nothing is installed.
#[derive(Debug)]
pub(crate) struct SlotExporter(ExporterSlot);

impl SpanExporter for SlotExporter {
    fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        let slot = self.0.clone();
        async move {
            let Some(exporter) = slot.current() else {
                return Ok(());
            };
            let result = exporter.export(batch).await;
            slot.record_result(&result);
            result
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.current().map_or(Ok(()), |e| e.force_flush())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        self.0.current().map_or(Ok(()), |e| e.shutdown())
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.inner.write().resource = Some(resource.clone());
    }
}
