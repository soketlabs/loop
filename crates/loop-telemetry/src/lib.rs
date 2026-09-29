//! OpenTelemetry tracing for Loop, exported to Langfuse over OTLP/HTTP.
//!
//! * [`TelemetryHandle`] owns the tracer provider, the export gate and the destination.
//!   Install its [`layer`](TelemetryHandle::layer) once when building the subscriber.
//! * [`obs`] creates observation spans and records Langfuse attributes on them.
//!
//! Other crates use only this API and never depend on `opentelemetry` directly.

mod credentials;
mod exporter;
mod handle;
pub mod obs;

pub use credentials::{CredentialSource, TelemetryCredentials, TelemetryDestination};
pub use handle::{TelemetryHandle, TelemetryStatus};
pub use obs::{ObservationExt, TraceAttrs};
