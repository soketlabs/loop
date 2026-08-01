//! Agent loop tests with faux provider.

use std::sync::Arc;

use futures::StreamExt;
use loop_agent::{
    agent_loop, collect_agent_events, run_agent_loop, stream_fn_from_models, AgentContext,
    AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, AgentToolResult, ToolExecutionMode,
};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn setup_faux(script: FauxScript) -> (Models, loop_ai::Model) {
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    (models, model)
}

#[tokio::test]
async fn prompt_text_event_sequence() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("hello".into()));
    let (models, model) = setup_faux(script);
    let stream_fn = stream_fn_from_models(Arc::new(models));
    let config = AgentLoopConfig::new(model);
    let context = AgentContext {
        system_prompt: "sys".into(),
        messages: vec![],
        tools: None,
    };
    let events = collect_agent_events(agent_loop(
        vec![AgentMessage::user_text("hi")],
        context,
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    let types: Vec<_> = events.iter().map(|e| e.type_name()).collect();
    assert_eq!(types.first(), Some(&"agent_start"));
    assert!(types.contains(&"turn_start"));
    assert!(types.contains(&"message_start"));
    assert!(types.contains(&"message_end"));
    assert_eq!(types.last(), Some(&"agent_end"));
}

#[tokio::test]
async fn tool_call_loop_with_terminate() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "c1".into(),
        name: "ping".into(),
        arguments: json!({}),
        thought_signature: None,
    }]));
    // After terminate, no second LLM call expected — but if loop continues without
    // more scripted responses, faux returns default text. Use terminate on tool.
    let (models, model) = setup_faux(script);
    let stream_fn = stream_fn_from_models(Arc::new(models));

    let tool = AgentTool::simple(
        "ping",
        "Ping",
        "ping",
        json!({"type":"object","properties":{}}),
        |_id, _args, _c, _u| async move {
            let mut r = AgentToolResult::text("pong");
            r.terminate = Some(true);
            Ok(r)
        },
    );

    let mut config = AgentLoopConfig::new(model);
    config.tool_execution = ToolExecutionMode::Sequential;
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: Some(vec![tool]),
    };

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let events2 = Arc::clone(&events);
    let emit = Arc::new(move |ev: AgentEvent| {
        let events2 = Arc::clone(&events2);
        Box::pin(async move {
            events2.lock().push(ev.type_name().to_string());
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });

    let msgs = run_agent_loop(
        vec![AgentMessage::user_text("go")],
        context,
        config,
        emit,
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();

    let types = events.lock().clone();
    assert!(types.contains(&"tool_execution_start".to_string()));
    assert!(types.contains(&"tool_execution_end".to_string()));
    assert!(msgs.iter().any(|m| m.role() == "toolResult"));
}

#[tokio::test]
async fn parallel_tools_source_order_results() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![
        ToolCall {
            id: "a".into(),
            name: "slow".into(),
            arguments: json!({}),
            thought_signature: None,
        },
        ToolCall {
            id: "b".into(),
            name: "fast".into(),
            arguments: json!({}),
            thought_signature: None,
        },
    ]));
    script.push(FauxResponse::Text("done".into()));
    let (models, model) = setup_faux(script);
    let stream_fn = stream_fn_from_models(Arc::new(models));

    let slow = AgentTool::simple(
        "slow",
        "Slow",
        "slow",
        json!({"type":"object","properties":{}}),
        |_id, _a, _c, _u| async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(AgentToolResult::text("slow"))
        },
    );
    let fast = AgentTool::simple(
        "fast",
        "Fast",
        "fast",
        json!({"type":"object","properties":{}}),
        |_id, _a, _c, _u| async move { Ok(AgentToolResult::text("fast")) },
    );

    let mut config = AgentLoopConfig::new(model);
    config.tool_execution = ToolExecutionMode::Parallel;
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: Some(vec![slow, fast]),
    };

    let tool_result_names = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let names2 = Arc::clone(&tool_result_names);
    let emit = Arc::new(move |ev: AgentEvent| {
        let names2 = Arc::clone(&names2);
        Box::pin(async move {
            if let AgentEvent::MessageEnd { message } = &ev {
                if message.role() == "toolResult" {
                    if let Some(loop_ai::Message::ToolResult(tr)) = message.as_llm() {
                        names2.lock().push(tr.tool_name.clone());
                    }
                }
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });

    run_agent_loop(
        vec![AgentMessage::user_text("go")],
        context,
        config,
        emit,
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();

    let names = tool_result_names.lock().clone();
    assert_eq!(names, vec!["slow".to_string(), "fast".to_string()]);
}

#[tokio::test]
async fn abort_cancels() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("x".into()));
    let (models, model) = setup_faux(script);
    let stream_fn = stream_fn_from_models(Arc::new(models));
    let config = AgentLoopConfig::new(model);
    let context = AgentContext::default();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut stream = agent_loop(
        vec![AgentMessage::user_text("hi")],
        context,
        config,
        Some(cancel),
        Some(stream_fn),
    );
    while let Some(_ev) = stream.next().await {}
}
