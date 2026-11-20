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

#[tokio::test(start_paused = true)]
async fn run_budget_ends_a_stalled_generation_without_user_abort() {
    let (_, model) = setup_faux(FauxScript::new());
    let mut config = AgentLoopConfig::new(model);
    config.stream_options.base.timeouts.run_budget_ms = Some(10);
    let stream_fn: loop_agent::StreamFn = Arc::new(|_, _, _| {
        Box::pin(async {
            // An unresponsive generation, with no terminal event.
            loop_ai::create_assistant_message_event_stream()
        })
    });
    let events = collect_agent_events(agent_loop(
        vec![AgentMessage::user_text("hi")],
        AgentContext::default(),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert!(events.iter().any(|event| matches!(event, AgentEvent::MessageEnd { message } if message.as_assistant().is_some_and(|m| m.stop_reason == loop_ai::StopReason::Error && m.error_message.as_deref().is_some_and(|e| e.contains("run_budget"))))));
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

/// A text response that stops on the output limit (`max_tokens`).
fn length_stop(model: &loop_ai::Model, text: &str) -> FauxResponse {
    length_stop_with(model, text, 100, None)
}

/// [`length_stop`] with `output` tokens and a provider stop reason.
fn length_stop_with(
    model: &loop_ai::Model,
    text: &str,
    output: u64,
    raw_stop_reason: Option<&str>,
) -> FauxResponse {
    use loop_ai::{AssistantContent, AssistantMessage, AssistantMessageEvent, StopReason};
    let mut message = AssistantMessage::pending(model);
    message.usage.output = output;
    message.usage.total_tokens = output;
    message.raw_stop_reason = raw_stop_reason.map(str::to_owned);
    message
        .content
        .push(AssistantContent::Text(loop_ai::TextContent {
            text: text.into(),
            text_signature: None,
        }));
    message.stop_reason = StopReason::Length;
    FauxResponse::Events(vec![
        AssistantMessageEvent::Start {
            partial: AssistantMessage::pending(model),
        },
        AssistantMessageEvent::Done {
            reason: StopReason::Length,
            message,
        },
    ])
}

async fn run_script(build: impl FnOnce(&loop_ai::Model) -> Vec<FauxResponse>) -> Vec<AgentMessage> {
    let script = FauxScript::new();
    let (models, model) = setup_faux(script.clone());
    script.extend(build(&model));
    let stream_fn = stream_fn_from_models(Arc::new(models));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: None,
    };
    let emit = Arc::new(|_ev: AgentEvent| {
        Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });
    run_agent_loop(
        vec![AgentMessage::user_text("write it all")],
        context,
        AgentLoopConfig::new(model),
        emit,
        None,
        Some(stream_fn),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_length_stop_without_tool_calls_asks_the_model_to_continue() {
    let msgs = run_script(|m| {
        vec![
            length_stop(m, "part one"),
            FauxResponse::Text("part two".into()),
        ]
    })
    .await;
    let assistants: Vec<_> = msgs.iter().filter_map(AgentMessage::as_assistant).collect();
    assert_eq!(assistants.len(), 2, "{msgs:?}");
    assert_eq!(assistants[1].stop_reason, loop_ai::StopReason::Stop);
    // One continuation prompt between the two answers.
    let users = msgs.iter().filter(|m| m.role() == "user").count();
    assert_eq!(users, 2, "the prompt and one continuation: {msgs:?}");
}

#[tokio::test]
async fn length_continuations_stop_after_three() {
    let msgs = run_script(|m| {
        (0..6)
            .map(|i| length_stop(m, &format!("part {i}")))
            .collect()
    })
    .await;
    let assistants: Vec<_> = msgs.iter().filter_map(AgentMessage::as_assistant).collect();
    assert_eq!(
        assistants.len(),
        4,
        "the first answer and three continuations"
    );
    assert_eq!(
        assistants.last().unwrap().stop_reason,
        loop_ai::StopReason::Length
    );
}

/// Every progress update of a tool reaches listeners before its end, so a
/// surface never shows a finished call as running again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_updates_arrive_before_the_tool_end() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "c1".into(),
        name: "chatty".into(),
        arguments: json!({}),
        thought_signature: None,
    }]));
    script.push(FauxResponse::Text("done".into()));
    let (models, model) = setup_faux(script);
    let stream_fn = stream_fn_from_models(Arc::new(models));
    let tool = AgentTool::simple(
        "chatty",
        "Chatty",
        "chatty",
        json!({"type":"object","properties":{}}),
        |_id, _args, _c, on_update| async move {
            if let Some(on_update) = on_update {
                for i in 0..200 {
                    on_update(AgentToolResult::text(format!("line {i}")));
                }
            }
            Ok(AgentToolResult::text("finished"))
        },
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: Some(vec![tool]),
    };
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let emit = Arc::new(move |ev: AgentEvent| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            // A listener that takes a moment per update (a broadcast, a
            // session write) must still see them all before the end.
            if ev.type_name() == "tool_execution_update" {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            sink.lock().push(ev.type_name().to_string());
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });
    run_agent_loop(
        vec![AgentMessage::user_text("go")],
        context,
        AgentLoopConfig::new(model),
        emit,
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();
    let types = events.lock().clone();
    let end = types
        .iter()
        .position(|t| t == "tool_execution_end")
        .expect("tool end");
    assert!(
        !types[end..].iter().any(|t| t == "tool_execution_update"),
        "an update came after the tool end: {types:?}"
    );
    // Only the latest waiting update is kept: a slow listener does not build
    // a queue of all 200.
    let updates = types
        .iter()
        .filter(|t| *t == "tool_execution_update")
        .count();
    assert!(
        updates < 200,
        "{updates} updates queued behind a slow listener"
    );
}

#[tokio::test]
async fn a_context_overflow_is_not_continued() {
    // No output at all: the prompt filled the context window.
    let msgs = run_script(|m| {
        vec![
            length_stop_with(m, "", 0, None),
            FauxResponse::Text("no".into()),
        ]
    })
    .await;
    assert_eq!(
        msgs.iter().filter_map(AgentMessage::as_assistant).count(),
        1,
        "{msgs:?}"
    );
    // The provider says the context window is full.
    let msgs = run_script(|m| {
        vec![
            length_stop_with(m, "part", 100, Some("model_context_window_exceeded")),
            FauxResponse::Text("no".into()),
        ]
    })
    .await;
    assert_eq!(
        msgs.iter().filter_map(AgentMessage::as_assistant).count(),
        1,
        "{msgs:?}"
    );
}

// ---- overflow recovery and compaction notices ----

const OVERFLOW: &str = "This model's maximum context length is 1000 tokens";

fn user_context(text: &str) -> AgentContext {
    AgentContext {
        system_prompt: "sys".into(),
        messages: vec![AgentMessage::user_text(text)],
        tools: None,
    }
}

fn assistant_errors(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| match e {
            AgentEvent::MessageEnd { message } => {
                message.role() == "assistant"
                    && matches!(
                        message.as_llm(),
                        Some(loop_ai::Message::Assistant(a))
                            if a.stop_reason == loop_ai::StopReason::Error
                    )
            }
            _ => false,
        })
        .count()
}

fn progress(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

async fn run_with(
    script: FauxScript,
    config_fn: impl FnOnce(&mut AgentLoopConfig),
) -> Vec<AgentEvent> {
    let (models, model) = setup_faux(script);
    let stream_fn = stream_fn_from_models(Arc::new(models));
    let mut config = AgentLoopConfig::new(model);
    config_fn(&mut config);
    collect_agent_events(agent_loop(
        vec![AgentMessage::user_text("a very long conversation")],
        user_context("earlier"),
        config,
        None,
        Some(stream_fn),
    ))
    .await
}

#[tokio::test]
async fn overflow_error_is_compacted_and_retried_once() {
    let script = FauxScript::new();
    script.push(FauxResponse::Error(OVERFLOW.into()));
    script.push(FauxResponse::Text("recovered".into()));
    let events = run_with(script, |config| {
        config.recover_from_overflow = Some(Arc::new(|_ctx| {
            Box::pin(async { Some((user_context("short"), "compacted it".to_string())) })
        }));
    })
    .await;
    assert_eq!(
        assistant_errors(&events),
        0,
        "failed attempt must leave no trace"
    );
    assert_eq!(progress(&events), vec!["compacted it".to_string()]);
    let last = events.last().unwrap();
    let AgentEvent::AgentEnd { messages } = last else {
        panic!("run did not end cleanly: {last:?}");
    };
    assert!(messages.iter().any(|m| matches!(
        m.as_llm(),
        Some(loop_ai::Message::Assistant(a)) if a.stop_reason != loop_ai::StopReason::Error
    )));
}

#[tokio::test]
async fn overflow_without_a_hook_still_fails_as_before() {
    let script = FauxScript::new();
    script.push(FauxResponse::Error(OVERFLOW.into()));
    let events = run_with(script, |_| {}).await;
    assert_eq!(assistant_errors(&events), 1);
    assert!(progress(&events).is_empty());
}

#[tokio::test]
async fn overflow_when_nothing_can_be_shrunk_surfaces_the_error() {
    let script = FauxScript::new();
    script.push(FauxResponse::Error(OVERFLOW.into()));
    let events = run_with(script, |config| {
        config.recover_from_overflow = Some(Arc::new(|_ctx| Box::pin(async { None })));
    })
    .await;
    assert_eq!(assistant_errors(&events), 1);
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

#[tokio::test]
async fn overflow_recovery_is_attempted_only_once_per_turn() {
    let script = FauxScript::new();
    script.push(FauxResponse::Error(OVERFLOW.into()));
    script.push(FauxResponse::Error(OVERFLOW.into()));
    script.push(FauxResponse::Text("never reached".into()));
    let events = run_with(script, |config| {
        config.recover_from_overflow = Some(Arc::new(|_ctx| {
            Box::pin(async { Some((user_context("short"), "compacted".to_string())) })
        }));
    })
    .await;
    assert_eq!(progress(&events).len(), 1);
    assert_eq!(assistant_errors(&events), 1, "second failure is reported");
}

#[tokio::test]
async fn other_errors_do_not_trigger_recovery() {
    let script = FauxScript::new();
    script.push(FauxResponse::Error("HTTP 500 upstream exploded".into()));
    let events = run_with(script, |config| {
        config.recover_from_overflow = Some(Arc::new(|_ctx| {
            Box::pin(async { Some((user_context("short"), "compacted".to_string())) })
        }));
    })
    .await;
    assert!(progress(&events).is_empty());
    assert_eq!(assistant_errors(&events), 1);
}

#[tokio::test]
async fn turn_update_notice_is_shown_to_the_user() {
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "c1".into(),
        name: "ping".into(),
        arguments: json!({}),
        thought_signature: None,
    }]));
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
    config.prepare_next_turn = Some(Arc::new(|_turn| {
        Box::pin(async {
            Some(loop_agent::AgentLoopTurnUpdate {
                notice: Some("Context compacted automatically: 118,092 → 22,990 tokens".into()),
                ..Default::default()
            })
        })
    }));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: Some(vec![tool]),
    };
    let events = collect_agent_events(agent_loop(
        vec![AgentMessage::user_text("go")],
        context,
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(
        progress(&events),
        vec!["Context compacted automatically: 118,092 → 22,990 tokens".to_string()]
    );
}
