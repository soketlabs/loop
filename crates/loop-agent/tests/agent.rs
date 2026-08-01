//! Stateful Agent tests.

use std::sync::Arc;

use loop_agent::{
    stream_fn_from_models, Agent, AgentOptions, AgentState, AgentTool, AgentToolResult,
};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use serde_json::json;

#[tokio::test]
async fn agent_prompt_updates_state() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("hi there".into()));
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let agent = Agent::new(AgentOptions::new(
        AgentState::new(model),
        stream_fn_from_models(Arc::new(models)),
    ));
    agent.set_system_prompt("you are helpful").await;
    agent.prompt("hello").await.unwrap();
    let state = agent.state().await;
    assert!(state.messages().len() >= 2);
    assert!(!state.is_streaming);
}

#[tokio::test]
async fn agent_tool_roundtrip() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "1".into(),
        name: "add".into(),
        arguments: json!({"a": 1, "b": 2}),
        thought_signature: None,
    }]));
    script.push(FauxResponse::Text("3".into()));
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let tool = AgentTool::simple(
        "add",
        "Add",
        "add numbers",
        json!({
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"]
        }),
        |_id, args, _c, _u| async move {
            let a = args.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let b = args.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Ok(AgentToolResult::text((a + b).to_string()))
        },
    );
    let mut state = AgentState::new(model);
    state.set_tools(vec![tool]);
    let agent = Agent::new(AgentOptions::new(
        state,
        stream_fn_from_models(Arc::new(models)),
    ));
    agent.prompt("add").await.unwrap();
    let state = agent.state().await;
    assert!(state.messages().iter().any(|m| m.role() == "toolResult"));
}

#[tokio::test]
async fn busy_reject() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "1".into(),
        name: "sleep".into(),
        arguments: json!({}),
        thought_signature: None,
    }]));
    script.push(FauxResponse::Text("done".into()));
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let tool = AgentTool::simple(
        "sleep",
        "Sleep",
        "sleep",
        json!({"type":"object","properties":{}}),
        |_id, _a, _c, _u| async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(AgentToolResult::text("ok"))
        },
    );
    let mut state = AgentState::new(model);
    state.set_tools(vec![tool]);
    let agent = Arc::new(Agent::new(AgentOptions::new(
        state,
        stream_fn_from_models(Arc::new(models)),
    )));
    let a = Arc::clone(&agent);
    let h = tokio::spawn(async move { a.prompt("one").await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let err = agent.prompt("two").await;
    assert!(err.is_err(), "expected busy");
    let _ = h.await;
}
