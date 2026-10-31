//! The faux provider follows the same event contract as the wire adapters.

use futures::StreamExt;
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Context, Message, Models, SimpleStreamOptions, StopReason, ToolCall};
use serde_json::json;

use crate::harness::{assert_contract, Run};

async fn run(response: FauxResponse) -> Run {
    let script = FauxScript::new();
    script.push(response);
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let context = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let mut stream = models.stream_simple(&model, &context, SimpleStreamOptions::default());
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let run = Run {
        events,
        message: stream.result().await,
    };
    assert_contract(&run, &model);
    run
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({"city": "Paris"}),
        thought_signature: None,
    }
}

#[tokio::test]
async fn streams_text() {
    let run = run(FauxResponse::Text("Hello".into())).await;
    assert_eq!(run.message.stop_reason, StopReason::Stop);
    assert_eq!(run.text(), "Hello");
}

#[tokio::test]
async fn streams_parallel_tool_calls() {
    let run = run(FauxResponse::ToolCalls(vec![
        call("call_1", "get_weather"),
        call("call_2", "get_time"),
    ]))
    .await;
    assert_eq!(run.message.stop_reason, StopReason::ToolUse);
    let ids: Vec<_> = run.tool_calls().into_iter().map(|c| c.id).collect();
    assert_eq!(ids, ["call_1", "call_2"]);
}

#[tokio::test]
async fn reports_errors() {
    let run = run(FauxResponse::Error("HTTP 429 Too Many Requests".into())).await;
    assert_eq!(run.message.stop_reason, StopReason::Error);
}
