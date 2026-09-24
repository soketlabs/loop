use std::collections::BTreeMap;

use loop_telemetry::obs::{self, keys};
use loop_telemetry::{CredentialSource, ObservationExt, TelemetryHandle, TraceAttrs};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use tracing::Dispatch;
use tracing_subscriber::layer::SubscriberExt;

struct Fixture {
    handle: TelemetryHandle,
    exporter: InMemorySpanExporter,
    dispatch: Dispatch,
}

impl Fixture {
    /// Provider + layer with no destination yet.
    fn uninstalled(enabled: bool) -> Self {
        let handle = TelemetryHandle::new("test-release", enabled);
        let subscriber = tracing_subscriber::registry().with(handle.layer());
        Self {
            handle,
            exporter: InMemorySpanExporter::default(),
            dispatch: Dispatch::new(subscriber),
        }
    }

    fn installed(enabled: bool) -> Self {
        let f = Self::uninstalled(enabled);
        f.install();
        f
    }

    fn install(&self) {
        self.handle.install_exporter(
            self.exporter.clone(),
            "http://memory".into(),
            CredentialSource::Config,
        );
    }

    fn run(&self, body: impl FnOnce()) {
        tracing::dispatcher::with_default(&self.dispatch, body);
    }

    fn spans(&self) -> Vec<SpanData> {
        self.handle.flush();
        self.exporter.get_finished_spans().unwrap()
    }

    fn names(&self) -> Vec<String> {
        self.spans().iter().map(|s| s.name.to_string()).collect()
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

#[test]
fn exports_observation_spans_with_type_and_name() {
    let f = Fixture::installed(true);
    f.run(|| {
        let _ = obs::agent("loop.run").entered();
    });
    let spans = f.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "loop.run");
    assert_eq!(
        attr(&spans[0], keys::OBSERVATION_TYPE).as_deref(),
        Some("agent")
    );
}

#[test]
fn disabled_gate_exports_nothing_and_skips_recording() {
    let f = Fixture::installed(false);
    f.run(|| {
        let span = obs::generation("llm.generate");
        assert!(!span.is_recording());
    });
    assert!(f.spans().is_empty());
}

#[test]
fn gate_toggles_at_runtime() {
    let f = Fixture::installed(true);
    f.run(|| {
        let _ = obs::span("first").entered();
        assert!(!f.handle.set_enabled(false).active());
        let _ = obs::span("while-disabled").entered();
        assert!(f.handle.set_enabled(true).active());
        let _ = obs::span("after-reenable").entered();
    });
    assert_eq!(f.names(), vec!["first", "after-reenable"]);
}

#[test]
fn nothing_exported_until_installed_then_exports() {
    let f = Fixture::uninstalled(true);
    assert!(!f.handle.status().active());
    f.run(|| {
        let _ = obs::span("before-install").entered();
    });
    f.install();
    let status = f.handle.status();
    assert!(status.active());
    assert_eq!(status.source, Some(CredentialSource::Config));
    f.run(|| {
        let _ = obs::span("after-install").entered();
    });
    assert_eq!(f.names(), vec!["after-install"]);
}

#[test]
fn non_loop_spans_are_never_exported() {
    let f = Fixture::installed(true);
    f.run(|| {
        let _ = tracing::info_span!(target: "hyper::client", "request").entered();
        let _ = tracing::info_span!(target: "loop_agent", "plain").entered();
    });
    assert!(f.spans().is_empty());
}

#[test]
fn children_nest_under_parents() {
    let f = Fixture::installed(true);
    f.run(|| {
        let run = obs::agent("loop.run");
        let _run = run.enter();
        let turn = obs::span("turn 1");
        let _turn = turn.enter();
        let _ = obs::tool("read", "call_1").entered();
    });
    let spans = f.spans();
    let by_name = |n: &str| spans.iter().find(|s| s.name == n).unwrap().clone();
    let (run, turn, tool) = (by_name("loop.run"), by_name("turn 1"), by_name("tool read"));
    assert_eq!(turn.parent_span_id, run.span_context.span_id());
    assert_eq!(tool.parent_span_id, turn.span_context.span_id());
    assert_eq!(tool.span_context.trace_id(), run.span_context.trace_id());
    assert_eq!(attr(&tool, keys::GEN_AI_TOOL_NAME).as_deref(), Some("read"));
    assert_eq!(
        attr(&tool, keys::GEN_AI_TOOL_CALL_ID).as_deref(),
        Some("call_1")
    );
    assert_eq!(attr(&tool, keys::OBSERVATION_TYPE).as_deref(), Some("tool"));
}

#[test]
fn recorders_write_langfuse_attributes() {
    let f = Fixture::installed(true);
    f.run(|| {
        let span = obs::generation("llm.generate");
        span.record_model("gpt-x", "openai", &serde_json::json!({"temperature": 0.2}));
        span.record_input(&serde_json::json!([{"role": "user", "content": "hi"}]));
        span.record_output("plain text");
        span.record_metadata("stop_reason", "stop");
        span.record_usage(&loop_ai::Usage {
            input: 10,
            output: 5,
            cache_read: 2,
            total_tokens: 15,
            reasoning: Some(3),
            cost: loop_ai::Cost {
                total: 0.5,
                ..Default::default()
            },
            ..Default::default()
        });
        span.record_completion_start(
            chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05.678Z")
                .unwrap()
                .into(),
        );
        span.record_error("boom");
    });
    let span = &f.spans()[0];
    assert_eq!(attr(span, keys::MODEL_NAME).as_deref(), Some("gpt-x"));
    assert_eq!(attr(span, keys::GEN_AI_SYSTEM).as_deref(), Some("openai"));
    assert_eq!(
        attr(span, keys::MODEL_PARAMETERS).as_deref(),
        Some(r#"{"temperature":0.2}"#)
    );
    let input: serde_json::Value = serde_json::from_str(&attr(span, keys::INPUT).unwrap()).unwrap();
    assert_eq!(
        input,
        serde_json::json!([{"role": "user", "content": "hi"}])
    );
    assert_eq!(attr(span, keys::OUTPUT).as_deref(), Some("plain text"));
    assert_eq!(
        attr(span, &format!("{}stop_reason", keys::METADATA_PREFIX)).as_deref(),
        Some("stop")
    );
    let usage: BTreeMap<String, u64> =
        serde_json::from_str(&attr(span, keys::USAGE_DETAILS).unwrap()).unwrap();
    assert_eq!(usage["input"], 10);
    assert_eq!(usage["cache_read"], 2);
    assert_eq!(usage["reasoning"], 3);
    assert_eq!(usage["total"], 15);
    let cost: BTreeMap<String, f64> =
        serde_json::from_str(&attr(span, keys::COST_DETAILS).unwrap()).unwrap();
    assert_eq!(cost["total"], 0.5);
    assert_eq!(
        attr(span, keys::COMPLETION_START_TIME).as_deref(),
        Some("2026-01-02T03:04:05.678Z")
    );
    assert_eq!(attr(span, keys::LEVEL).as_deref(), Some("ERROR"));
    assert_eq!(attr(span, keys::STATUS_MESSAGE).as_deref(), Some("boom"));
    assert!(matches!(
        span.status,
        opentelemetry::trace::Status::Error { .. }
    ));
}

#[test]
fn trace_attrs_are_recorded() {
    let f = Fixture::installed(true);
    f.run(|| {
        let span = obs::span("loop.print");
        span.record_trace(&TraceAttrs {
            name: Some("bench".into()),
            session_id: Some("sess-1".into()),
            tags: vec!["a".into(), "b".into()],
            metadata: BTreeMap::from([("task_id".into(), "t-9".into())]),
            environment: Some("benchmark".into()),
            release: Some("0.3.1".into()),
            ..Default::default()
        });
    });
    let span = &f.spans()[0];
    assert_eq!(attr(span, keys::TRACE_NAME).as_deref(), Some("bench"));
    assert_eq!(attr(span, keys::SESSION_ID).as_deref(), Some("sess-1"));
    assert_eq!(attr(span, keys::ENVIRONMENT).as_deref(), Some("benchmark"));
    assert_eq!(attr(span, keys::RELEASE).as_deref(), Some("0.3.1"));
    assert_eq!(
        attr(span, &format!("{}task_id", keys::TRACE_METADATA_PREFIX)).as_deref(),
        Some("t-9")
    );
    let tags = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == keys::TRACE_TAGS)
        .unwrap();
    assert!(matches!(
        &tags.value,
        opentelemetry::Value::Array(opentelemetry::Array::String(v)) if v.len() == 2
    ));
}

#[test]
fn continue_trace_adopts_traceparent_and_exposes_trace_id() {
    let f = Fixture::installed(true);
    let trace_hex = "4bf92f3577b34da6a3ce929d0e0e4736";
    let mut seen = None;
    f.run(|| {
        let span = obs::span("loop.print");
        assert!(span.continue_trace(&format!("00-{trace_hex}-00f067aa0ba902b7-01")));
        seen = span.trace_id();
    });
    assert_eq!(seen.as_deref(), Some(trace_hex));
    assert_eq!(f.spans()[0].span_context.trace_id().to_string(), trace_hex);
}

#[test]
fn invalid_traceparent_is_rejected() {
    let f = Fixture::installed(true);
    f.run(|| {
        let span = obs::span("loop.print");
        assert!(!span.continue_trace("garbage"));
        assert!(span.trace_id().is_some());
    });
}

/// Real OTLP/HTTP exporter against a local listener: checks path and Langfuse headers.
#[test]
fn langfuse_exporter_posts_protobuf_with_auth_headers() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap();
            }
            head.push(line.trim_end().to_string());
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n")
            .unwrap();
        (head, body)
    });

    let handle = TelemetryHandle::new("test-release", true);
    let creds = loop_telemetry::TelemetryCredentials::from_parts(
        Some(host.clone()),
        Some("pk-lf-1".into()),
        Some("sk-lf-2".into()),
        CredentialSource::Env,
    )
    .unwrap();
    handle.install(&creds).unwrap();
    let dispatch = Dispatch::new(tracing_subscriber::registry().with(handle.layer()));
    tracing::dispatcher::with_default(&dispatch, || {
        let _ = obs::agent("loop.run").entered();
    });
    handle.flush();

    let (head, body) = server.join().unwrap();
    let lower: Vec<String> = head.iter().map(|l| l.to_ascii_lowercase()).collect();
    assert_eq!(head[0], "POST /api/public/otel/v1/traces HTTP/1.1");
    assert!(lower.contains(&"authorization: basic cgstbgytmtpzay1szi0y".to_string()));
    assert!(lower.contains(&"x-langfuse-ingestion-version: 4".to_string()));
    assert!(lower.contains(&"content-type: application/x-protobuf".to_string()));
    assert!(body.windows(b"loop.run".len()).any(|w| w == b"loop.run"));
    assert_eq!(handle.status().host.as_deref(), Some(host.as_str()));
}
