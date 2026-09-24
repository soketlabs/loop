//! Langfuse observation spans emitted by the agent loop (faux provider, in-memory exporter).

use std::sync::Arc;

use loop_agent::{
    run_agent_loop, stream_fn_from_models, AgentContext, AgentEvent, AgentEventSink,
    AgentLoopConfig, AgentMessage, AgentTool, AgentToolResult, ToolExecutionMode,
};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use loop_telemetry::obs::keys;
use loop_telemetry::{CredentialSource, TelemetryHandle};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;

/// Tracing subscriber exporting to memory for the duration of one test.
struct Capture {
    handle: TelemetryHandle,
    exporter: InMemorySpanExporter,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Capture {
    fn new(enabled: bool) -> Self {
        let handle = TelemetryHandle::new("test", enabled);
        let exporter = InMemorySpanExporter::default();
        handle.install_exporter(exporter.clone(), "memory".into(), CredentialSource::Config);
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(handle.layer()));
        Self {
            handle,
            exporter,
            _guard: guard,
        }
    }

    fn spans(&self) -> Spans {
        self.handle.flush();
        Spans(self.exporter.get_finished_spans().unwrap())
    }
}

struct Spans(Vec<SpanData>);

impl Spans {
    fn named(&self, name: &str) -> Vec<&SpanData> {
        self.0.iter().filter(|s| s.name == name).collect()
    }

    fn one(&self, name: &str) -> &SpanData {
        let found = self.named(name);
        assert_eq!(
            found.len(),
            1,
            "expected one `{name}` span, got {:?}",
            self.names()
        );
        found[0]
    }

    fn names(&self) -> Vec<String> {
        self.0.iter().map(|s| s.name.to_string()).collect()
    }

    fn children_of(&self, parent: &SpanData) -> Vec<&SpanData> {
        self.0
            .iter()
            .filter(|s| s.parent_span_id == parent.span_context.span_id())
            .collect()
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

fn metadata(span: &SpanData, key: &str) -> Option<String> {
    attr(span, &format!("{}{key}", keys::METADATA_PREFIX))
}

fn assert_child(spans: &Spans, parent: &SpanData, child: &SpanData) {
    assert_eq!(
        child.parent_span_id,
        parent.span_context.span_id(),
        "`{}` should be a child of `{}`; spans: {:?}",
        child.name,
        parent.name,
        spans.names()
    );
}

fn tool(name: &'static str, delay_ms: u64, fails: bool) -> AgentTool {
    AgentTool::simple(
        name,
        name,
        name,
        json!({"type": "object", "properties": {}}),
        move |_id, _args, _cancel, _update| async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            if fails {
                Err(format!("{name} exploded"))
            } else {
                Ok(AgentToolResult::text(format!("{name} ok")))
            }
        },
    )
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
        thought_signature: None,
    }
}

fn noop_emit() -> AgentEventSink {
    Arc::new(|_ev: AgentEvent| Box::pin(async {}))
}

async fn run(responses: Vec<FauxResponse>, tools: Vec<AgentTool>) -> Vec<AgentMessage> {
    run_in_session(responses, tools, None).await
}

async fn run_in_session(
    responses: Vec<FauxResponse>,
    tools: Vec<AgentTool>,
    session_id: Option<&str>,
) -> Vec<AgentMessage> {
    let script = FauxScript::new();
    script.extend(responses);
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let mut config = AgentLoopConfig::new(model);
    config.tool_execution = ToolExecutionMode::Parallel;
    config.stream_options.base.session_id = session_id.map(str::to_string);
    let context = AgentContext {
        system_prompt: "be brief".into(),
        messages: vec![],
        tools: (!tools.is_empty()).then_some(tools),
    };
    run_agent_loop(
        vec![AgentMessage::user_text("hi")],
        context,
        config,
        noop_emit(),
        None,
        Some(stream_fn_from_models(Arc::new(models))),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn text_turn_produces_run_turn_generation_tree() {
    let capture = Capture::new(true);
    run(vec![FauxResponse::Text("hello".into())], vec![]).await;
    let spans = capture.spans();

    let run = spans.one("loop.run");
    let turn = spans.one("turn 1");
    let generation = spans.one("llm.generate");
    assert_child(&spans, run, turn);
    assert_child(&spans, turn, generation);

    assert_eq!(attr(run, keys::OBSERVATION_TYPE).as_deref(), Some("agent"));
    assert_eq!(metadata(run, "turns").as_deref(), Some("1"));
    assert_eq!(metadata(run, "tool_calls").as_deref(), Some("0"));
    assert_eq!(metadata(run, "stop_reason").as_deref(), Some("stop"));
    assert!(attr(run, keys::INPUT).unwrap().contains("hi"));
    assert!(metadata(run, "total_tokens").is_some());
    assert!(metadata(run, "total_cost").is_some());
    assert!(
        attr(run, keys::USAGE_DETAILS).is_none(),
        "run totals must not be usage details"
    );
    assert!(attr(run, keys::COST_DETAILS).is_none());

    assert_eq!(
        attr(generation, keys::OBSERVATION_TYPE).as_deref(),
        Some("generation")
    );
    assert_eq!(
        attr(generation, keys::MODEL_NAME).as_deref(),
        Some("faux-model")
    );
    assert_eq!(
        attr(generation, keys::GEN_AI_SYSTEM).as_deref(),
        Some("faux")
    );
    let input = attr(generation, keys::INPUT).unwrap();
    assert!(
        input.contains("be brief") && input.contains("hi"),
        "{input}"
    );
    assert!(attr(generation, keys::OUTPUT).unwrap().contains("hello"));
    assert!(attr(generation, keys::USAGE_DETAILS).is_some());
    assert!(attr(generation, keys::COST_DETAILS).is_some());
    assert!(attr(generation, keys::COMPLETION_START_TIME).is_some());
    assert!(attr(generation, keys::LEVEL).is_none());
}

#[tokio::test]
async fn parallel_tools_are_overlapping_children_of_their_turn() {
    let capture = Capture::new(true);
    run(
        vec![
            FauxResponse::ToolCalls(vec![call("a", "slow"), call("b", "fast")]),
            FauxResponse::Text("done".into()),
        ],
        vec![tool("slow", 60, false), tool("fast", 30, false)],
    )
    .await;
    let spans = capture.spans();

    let turn1 = spans.one("turn 1");
    let slow = spans.one("tool slow");
    let fast = spans.one("tool fast");
    assert_child(&spans, turn1, slow);
    assert_child(&spans, turn1, fast);
    assert_eq!(attr(slow, keys::GEN_AI_TOOL_CALL_ID).as_deref(), Some("a"));
    assert!(attr(slow, keys::OUTPUT).unwrap().contains("slow ok"));
    // Executed concurrently: each starts before the other ends.
    assert!(slow.start_time < fast.end_time && fast.start_time < slow.end_time);

    let preflights = spans.named("tool.preflight");
    assert_eq!(preflights.len(), 2);
    for preflight in preflights {
        assert_child(&spans, turn1, preflight);
    }

    let turn2 = spans.one("turn 2");
    let generations = spans.named("llm.generate");
    assert_eq!(generations.len(), 2);
    assert!(spans
        .children_of(turn2)
        .iter()
        .any(|s| s.name == "llm.generate"));
    assert_eq!(
        metadata(spans.one("loop.run"), "tool_calls").as_deref(),
        Some("2")
    );
}

#[tokio::test]
async fn generation_input_grows_across_turns() {
    let capture = Capture::new(true);
    run(
        vec![
            FauxResponse::ToolCalls(vec![call("a", "fast")]),
            FauxResponse::Text("done".into()),
        ],
        vec![tool("fast", 0, false)],
    )
    .await;
    let spans = capture.spans();
    let mut generations = spans.named("llm.generate");
    generations.sort_by_key(|s| s.start_time);
    let first = attr(generations[0], keys::INPUT).unwrap();
    let second = attr(generations[1], keys::INPUT).unwrap();
    assert!(second.len() > first.len());
    assert!(second.contains("fast ok"));
}

#[tokio::test]
async fn failing_tool_and_unknown_tool_are_marked_error() {
    let capture = Capture::new(true);
    run(
        vec![
            FauxResponse::ToolCalls(vec![call("a", "boom"), call("b", "missing")]),
            FauxResponse::Text("done".into()),
        ],
        vec![tool("boom", 0, true)],
    )
    .await;
    let spans = capture.spans();

    let boom = spans.one("tool boom");
    assert_eq!(attr(boom, keys::LEVEL).as_deref(), Some("ERROR"));
    assert_eq!(
        attr(boom, keys::STATUS_MESSAGE).as_deref(),
        Some("boom exploded")
    );

    // Unknown tools never execute; their preflight carries the error.
    assert!(spans.named("tool missing").is_empty());
    let failed_preflight = spans
        .named("tool.preflight")
        .into_iter()
        .find(|s| metadata(s, "tool").as_deref() == Some("missing"))
        .unwrap();
    assert_eq!(
        attr(failed_preflight, keys::LEVEL).as_deref(),
        Some("ERROR")
    );

    assert_eq!(
        metadata(spans.one("loop.run"), "tool_errors").as_deref(),
        Some("2")
    );
}

#[tokio::test]
async fn provider_error_marks_generation_and_run() {
    let capture = Capture::new(true);
    run(vec![FauxResponse::Error("rate limited".into())], vec![]).await;
    let spans = capture.spans();

    let generation = spans.one("llm.generate");
    assert_eq!(attr(generation, keys::LEVEL).as_deref(), Some("ERROR"));
    assert_eq!(
        attr(generation, keys::STATUS_MESSAGE).as_deref(),
        Some("rate limited")
    );
    let run = spans.one("loop.run");
    assert_eq!(attr(run, keys::LEVEL).as_deref(), Some("ERROR"));
    assert_eq!(metadata(run, "stop_reason").as_deref(), Some("error"));
}

#[tokio::test]
async fn disabled_telemetry_exports_nothing() {
    let capture = Capture::new(false);
    run(
        vec![
            FauxResponse::ToolCalls(vec![call("a", "fast")]),
            FauxResponse::Text("done".into()),
        ],
        vec![tool("fast", 0, false)],
    )
    .await;
    assert!(capture.spans().0.is_empty());
}

#[tokio::test]
async fn root_run_carries_session_id() {
    let capture = Capture::new(true);
    run_in_session(
        vec![FauxResponse::Text("x".into())],
        vec![],
        Some("sess-42"),
    )
    .await;
    let run = capture.spans();
    assert_eq!(
        attr(run.one("loop.run"), keys::SESSION_ID).as_deref(),
        Some("sess-42")
    );
}

#[tokio::test]
async fn nested_run_leaves_trace_attrs_to_enclosing_root() {
    use tracing::Instrument;

    let capture = Capture::new(true);
    let root = loop_telemetry::obs::span("loop.print");
    run_in_session(
        vec![FauxResponse::Text("x".into())],
        vec![],
        Some("sess-42"),
    )
    .instrument(root.clone())
    .await;
    drop(root);
    let spans = capture.spans();
    let (root, run) = (spans.one("loop.print"), spans.one("loop.run"));
    assert_child(&spans, root, run);
    assert!(attr(run, keys::SESSION_ID).is_none());
}

/// The real `--print` path goes through `AgentHarness::prompt`; its run must stay in the
/// caller's trace so the printed trace id points at the generations.
#[tokio::test]
async fn harness_prompt_nests_under_callers_span() {
    use loop_agent::harness::{
        create_in_memory_session_store, create_session_repository, AgentHarness,
        AgentHarnessOptions, HostExecutionEnv, SandboxMode,
    };
    use tracing::Instrument;

    let capture = Capture::new(true);
    let script = FauxScript::new();
    script.push(FauxResponse::Text("harness-ok".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, Some("h".into())).await.unwrap();
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model,
        session,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });

    let root = loop_telemetry::obs::span("loop.print");
    harness
        .prompt("hello")
        .instrument(root.clone())
        .await
        .unwrap();
    drop(root);
    harness.wait_for_idle().await;

    let spans = capture.spans();
    let (root, run) = (spans.one("loop.print"), spans.one("loop.run"));
    assert_child(&spans, root, run);
    let generation = spans.one("llm.generate");
    assert_eq!(
        generation.span_context.trace_id(),
        root.span_context.trace_id()
    );
}
