//! OpenTelemetry tracing for Loop, exported to Langfuse over OTLP/HTTP.
//!
//! * [`TelemetryHandle`] owns the tracer provider, the export gate and the destination.
//!   Install its [`layer`](TelemetryHandle::layer) once when building the subscriber.
//! * [`obs`] creates observation spans and records Langfuse attributes on them.
//!
//! Other crates use only this API and never depend on `opentelemetry` directly.

mod anonymize;
mod credentials;
mod exporter;
mod file_exporter;
mod handle;
pub mod obs;

pub use credentials::{CredentialSource, TelemetryCredentials, TelemetryDestination};
pub use file_exporter::{is_trace_file, TRACE_RETENTION, USAGE_LOG_VERSION};
pub use handle::{TelemetryHandle, TelemetryStatus, UsageLog};
pub use obs::{ObservationExt, TraceAttrs};
