//! Faux provider stream protocol tests.

use futures::StreamExt;
use loop_ai::{
    providers::{faux_provider, FauxResponse, FauxScript},
    AssistantContent, AssistantMessageEvent, Context, Message, Models, SimpleStreamOptions,
    StopReason, ToolCall,
};
use serde_json::json;

#[tokio::test]
async fn faux_text_event_order_and_result() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("hello".into()));

    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let ctx = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };

    let stream = models.stream_simple(&model, &ctx, SimpleStreamOptions::default());
    let mut stream = stream;
    let mut types = Vec::new();
    while let Some(ev) = stream.next().await {
        types.push(match &ev {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { delta, .. } => {
                assert_eq!(delta, "hello");
                "text_delta"
            }
            AssistantMessageEvent::TextEnd { content, .. } => {
                assert_eq!(content, "hello");
                "text_end"
            }
            AssistantMessageEvent::Done { reason, .. } => {
                assert_eq!(*reason, StopReason::Stop);
                "done"
            }
            other => panic!("unexpected event: {other:?}"),
        });
    }

    assert_eq!(
        types,
        ["start", "text_start", "text_delta", "text_end", "done"]
    );

    let result = stream.result().await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(matches!(
        &result.content[0],
        AssistantContent::Text(t) if t.text == "hello"
    ));
}

#[tokio::test]
async fn faux_tool_calls_and_complete_simple() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "1".into(),
        name: "get_time".into(),
        arguments: json!({"timezone": "UTC"}),
        thought_signature: None,
    }]));

    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let result = models
        .complete_simple(&model, &Context::default(), SimpleStreamOptions::default())
        .await;

    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert!(matches!(
        &result.content[0],
        AssistantContent::ToolCall(tc) if tc.name == "get_time"
    ));
}

#[tokio::test]
async fn faux_error_is_stream_encoded() {
    let script = FauxScript::new();
    script.push(FauxResponse::Error("provider down".into()));

    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let result = models
        .complete_simple(&model, &Context::default(), SimpleStreamOptions::default())
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("provider down"));
}
