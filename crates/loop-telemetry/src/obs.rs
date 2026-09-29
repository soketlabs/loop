//! Observation spans and the Langfuse attributes recorded on them.
//!
//! Instrumented code creates spans only through [`agent`], [`generation`], [`tool`] and
//! [`span`], and records data only through [`ObservationExt`]. Every recorder is a no-op
//! (and skips serialization) when the span is not being exported.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{Status, TraceContextExt};
use opentelemetry::{Array, StringValue, Value};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use serde::Serialize;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// `tracing` target of every observation span; the export gate admits only this target.
pub const SPAN_TARGET: &str = "loop_telemetry";

/// Langfuse / OTel attribute keys.
pub mod keys {
    pub const OBSERVATION_TYPE: &str = "langfuse.observation.type";
    pub const INPUT: &str = "langfuse.observation.input";
    pub const OUTPUT: &str = "langfuse.observation.output";
    pub const MODEL_NAME: &str = "langfuse.observation.model.name";
    pub const MODEL_PARAMETERS: &str = "langfuse.observation.model.parameters";
    pub const USAGE_DETAILS: &str = "langfuse.observation.usage_details";
    pub const COST_DETAILS: &str = "langfuse.observation.cost_details";
    pub const COMPLETION_START_TIME: &str = "langfuse.observation.completion_start_time";
    pub const LEVEL: &str = "langfuse.observation.level";
    pub const STATUS_MESSAGE: &str = "langfuse.observation.status_message";
    pub const METADATA_PREFIX: &str = "langfuse.observation.metadata.";
    pub const SESSION_ID: &str = "langfuse.session.id";
    pub const TRACE_NAME: &str = "langfuse.trace.name";
    pub const TRACE_TAGS: &str = "langfuse.trace.tags";
    pub const TRACE_METADATA_PREFIX: &str = "langfuse.trace.metadata.";
    pub const USER_ID: &str = "langfuse.user.id";
    pub const RELEASE: &str = "langfuse.release";
    pub const ENVIRONMENT: &str = "langfuse.environment";
    pub const GEN_AI_SYSTEM: &str = "gen_ai.system";
    pub const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
    pub const GEN_AI_TOOL_CALL_ID: &str = "gen_ai.tool.call.id";
}

/// Langfuse observation types used by Loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationType {
    Agent,
    Generation,
    Tool,
    Span,
}

impl ObservationType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Generation => "generation",
            Self::Tool => "tool",
            Self::Span => "span",
        }
    }
}

/// The one place observation spans are created.
fn observation(name: &str, kind: ObservationType) -> Span {
    tracing::info_span!(
        target: SPAN_TARGET,
        "observation",
        otel.name = %name,
        langfuse.observation.type = kind.as_str(),
    )
}

/// An agent run (Langfuse `agent`).
pub fn agent(name: &str) -> Span {
    observation(name, ObservationType::Agent)
}

/// One LLM call (Langfuse `generation`).
pub fn generation(name: &str) -> Span {
    observation(name, ObservationType::Generation)
}

/// One tool execution (Langfuse `tool`).
pub fn tool(name: &str, call_id: &str) -> Span {
    let span = observation(&format!("tool {name}"), ObservationType::Tool);
    span.record_attr(keys::GEN_AI_TOOL_NAME, name);
    span.record_attr(keys::GEN_AI_TOOL_CALL_ID, call_id);
    span
}

/// A plain grouping span (Langfuse `span`).
pub fn span(name: &str) -> Span {
    observation(name, ObservationType::Span)
}

/// Trace-level attributes; set once, usually on the root span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraceAttrs {
    pub name: Option<String>,
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub tags: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub environment: Option<String>,
    pub release: Option<String>,
}

/// Recorders for Langfuse attributes on observation spans.
pub trait ObservationExt {
    /// Whether this span is exported; recorders do nothing otherwise.
    fn is_recording(&self) -> bool;
    /// Set a raw string attribute.
    fn record_attr(&self, key: &str, value: &str);
    /// `langfuse.observation.input` (JSON).
    fn record_input<T: Serialize + ?Sized>(&self, value: &T);
    /// `langfuse.observation.output` (JSON).
    fn record_output<T: Serialize + ?Sized>(&self, value: &T);
    /// `langfuse.observation.metadata.<key>`.
    fn record_metadata<T: Serialize + ?Sized>(&self, key: &str, value: &T);
    /// Model name, provider and request parameters of a generation.
    fn record_model<P: Serialize + ?Sized>(&self, model: &str, provider: &str, parameters: &P);
    /// Token usage and cost of a generation.
    fn record_usage(&self, usage: &loop_ai::Usage);
    /// Time the first output token arrived.
    fn record_completion_start(&self, at: DateTime<Utc>);
    /// Mark the observation as failed.
    fn record_error(&self, message: &str);
    /// Trace-level attributes (session, name, tags, …).
    fn record_trace(&self, attrs: &TraceAttrs);
    /// Parent this span under a W3C `traceparent`; returns whether it was accepted.
    /// Must be called before anything is recorded on the span.
    fn continue_trace(&self, traceparent: &str) -> bool;
    /// Hex trace id, when exported.
    fn trace_id(&self) -> Option<String>;
}

impl ObservationExt for Span {
    fn is_recording(&self) -> bool {
        !self.is_disabled() && self.context().span().span_context().is_valid()
    }

    fn record_attr(&self, key: &str, value: &str) {
        if self.is_recording() {
            self.set_attribute(key.to_string(), value.to_string());
        }
    }

    fn record_input<T: Serialize + ?Sized>(&self, value: &T) {
        self.record_json(keys::INPUT, value);
    }

    fn record_output<T: Serialize + ?Sized>(&self, value: &T) {
        self.record_json(keys::OUTPUT, value);
    }

    fn record_metadata<T: Serialize + ?Sized>(&self, key: &str, value: &T) {
        self.record_json(&format!("{}{key}", keys::METADATA_PREFIX), value);
    }

    fn record_model<P: Serialize + ?Sized>(&self, model: &str, provider: &str, parameters: &P) {
        self.record_attr(keys::MODEL_NAME, model);
        self.record_attr(keys::GEN_AI_SYSTEM, provider);
        self.record_json(keys::MODEL_PARAMETERS, parameters);
    }

    fn record_usage(&self, usage: &loop_ai::Usage) {
        if !self.is_recording() {
            return;
        }
        let mut details = BTreeMap::from([
            ("input", usage.input),
            ("output", usage.output),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
            ("total", usage.total_tokens),
        ]);
        if let Some(reasoning) = usage.reasoning {
            details.insert("reasoning", reasoning);
        }
        let cost = &usage.cost;
        let cost_details = BTreeMap::from([
            ("input", cost.input),
            ("output", cost.output),
            ("cache_read", cost.cache_read),
            ("cache_write", cost.cache_write),
            ("total", cost.total),
        ]);
        self.record_json(keys::USAGE_DETAILS, &details);
        self.record_json(keys::COST_DETAILS, &cost_details);
    }

    fn record_completion_start(&self, at: DateTime<Utc>) {
        self.record_attr(
            keys::COMPLETION_START_TIME,
            &at.to_rfc3339_opts(SecondsFormat::Millis, true),
        );
    }

    fn record_error(&self, message: &str) {
        if !self.is_recording() {
            return;
        }
        self.record_attr(keys::LEVEL, "ERROR");
        self.record_attr(keys::STATUS_MESSAGE, message);
        self.set_status(Status::error(message.to_string()));
    }

    fn record_trace(&self, attrs: &TraceAttrs) {
        if !self.is_recording() {
            return;
        }
        let optional = [
            (keys::TRACE_NAME, &attrs.name),
            (keys::SESSION_ID, &attrs.session_id),
            (keys::USER_ID, &attrs.user_id),
            (keys::ENVIRONMENT, &attrs.environment),
            (keys::RELEASE, &attrs.release),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                self.record_attr(key, value);
            }
        }
        if !attrs.tags.is_empty() {
            let tags: Vec<StringValue> = attrs.tags.iter().cloned().map(Into::into).collect();
            self.set_attribute(keys::TRACE_TAGS, Value::Array(Array::String(tags)));
        }
        for (key, value) in &attrs.metadata {
            self.record_attr(&format!("{}{key}", keys::TRACE_METADATA_PREFIX), value);
        }
    }

    fn continue_trace(&self, traceparent: &str) -> bool {
        let carrier = std::collections::HashMap::from([(
            "traceparent".to_string(),
            traceparent.trim().to_string(),
        )]);
        let parent = TraceContextPropagator::new().extract(&carrier);
        if !parent.span().span_context().is_valid() {
            return false;
        }
        self.set_parent(parent).is_ok()
    }

    fn trace_id(&self) -> Option<String> {
        let cx = self.context();
        let span_context = cx.span().span_context().clone();
        span_context
            .is_valid()
            .then(|| span_context.trace_id().to_string())
    }
}

/// Serialization shared by every JSON-valued recorder.
trait RecordJson {
    fn record_json<T: Serialize + ?Sized>(&self, key: &str, value: &T);
}

impl RecordJson for Span {
    fn record_json<T: Serialize + ?Sized>(&self, key: &str, value: &T) {
        if !self.is_recording() {
            return;
        }
        // Plain strings are stored as-is; everything else as JSON text.
        let text = match serde_json::to_string(value) {
            Ok(json) if json.starts_with('"') => {
                serde_json::from_str::<String>(&json).unwrap_or(json)
            }
            Ok(json) => json,
            Err(err) => format!("<unserializable: {err}>"),
        };
        self.set_attribute(key.to_string(), text);
    }
}
