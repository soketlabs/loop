//! Timed check of the agent loop's per-round cost with a long history.
//!
//! Run with `cargo test -p loop-agent --test compaction_overhead -- --ignored --nocapture`.
//! It prints the mean time of one tool round with 50 and 500 messages in the
//! Session. The context stays far under the window, so no compaction runs:
//! the numbers show only what the threshold check adds to each round.

use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    HostExecutionEnv, SandboxMode,
};
use loop_agent::{AgentMessage, AgentTool, AgentToolResult};
use loop_ai::providers::{faux_provider_fn, FauxResponse};
use loop_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Models, StopReason, TextContent,
    ToolCall,
};
use serde_json::json;

const ROUNDS: usize = 40;
const RUNS: usize = 5;

fn tool_round(model: &loop_ai::Model, n: usize) -> FauxResponse {
    let mut message = AssistantMessage::pending(model);
    message.content.push(AssistantContent::ToolCall(ToolCall {
        id: format!("call-{n}"),
        name: "ping".into(),
        arguments: json!({}),
        thought_signature: None,
    }));
    message.usage.input = 2_000;
    message.usage.output = 20;
    message.usage.total_tokens = 2_020;
    message.stop_reason = StopReason::ToolUse;
    FauxResponse::Events(vec![
        AssistantMessageEvent::Start {
            partial: AssistantMessage::pending(model),
        },
        AssistantMessageEvent::Done {
            reason: StopReason::ToolUse,
            message,
        },
    ])
}

async fn harness_with_history(
    messages: usize,
) -> (Arc<AgentHarness>, Arc<std::sync::Mutex<usize>>) {
    let calls = Arc::new(std::sync::Mutex::new(0usize));
    let counter = Arc::clone(&calls);
    let model_slot: Arc<std::sync::Mutex<Option<loop_ai::Model>>> = Default::default();
    let slot = Arc::clone(&model_slot);
    let provider = faux_provider_fn(Arc::new(move |_ctx| {
        let mut n = counter.lock().unwrap();
        *n += 1;
        let current = *n;
        let model = slot.lock().unwrap().clone();
        Box::pin(async move {
            let model = model.expect("model");
            if current.is_multiple_of(ROUNDS + 1) {
                FauxResponse::Text("done".into())
            } else {
                tool_round(&model, current)
            }
        })
    }));
    let models = Arc::new(Models::new());
    models.set_provider(provider);
    let mut model = models.get_model("faux", "faux-model").unwrap();
    model.context_window = 1_000_000;
    *model_slot.lock().unwrap() = Some(model.clone());

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, None).await.unwrap();
    let host = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let ping = AgentTool::simple(
        "ping",
        "Ping",
        "ping",
        json!({"type":"object","properties":{}}),
        |_id, _args, _c, _u| async move { Ok(AgentToolResult::text("pong")) },
    );
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model.clone()),
        session,
        host_env: host,
        tools: vec![ping],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    for i in 0..messages {
        let message = if i % 2 == 0 {
            AgentMessage::user_text(format!("question {i} {}", "x".repeat(400)))
        } else {
            let mut a = AssistantMessage::pending(&model);
            a.content.push(AssistantContent::Text(TextContent {
                text: format!("answer {i} {}", "y".repeat(400)),
                text_signature: None,
            }));
            a.usage.input = 1_000 + i as u64 * 100;
            a.usage.output = 100;
            a.usage.total_tokens = a.usage.input + 100;
            a.stop_reason = StopReason::Stop;
            AgentMessage::assistant(a)
        };
        harness.append_message(message).await.unwrap();
    }
    (harness, calls)
}

async fn mean_round(messages: usize) -> Duration {
    let mut times = Vec::new();
    for _ in 0..RUNS {
        let (harness, _calls) = harness_with_history(messages).await;
        let start = Instant::now();
        harness.prompt("go").await.unwrap();
        times.push(start.elapsed() / ROUNDS as u32);
    }
    times.sort();
    times[RUNS / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "timing; run by hand"]
async fn per_round_time_with_long_history() {
    for messages in [50usize, 500] {
        let round = mean_round(messages).await;
        println!("history={messages} messages: median per-round time {round:?}");
    }
}

/// The threshold check alone, as the loop runs it before each request:
/// with usage on the newest response (one tool round after it), and with no
/// usage at all (every message estimated).
#[test]
#[ignore = "timing; run by hand"]
fn threshold_check_time() {
    use loop_agent::harness::compaction::context_tokens;
    let model = loop_ai::providers::faux_provider(loop_ai::providers::FauxScript::new())
        .get_model("faux-model")
        .unwrap();
    for messages in [50usize, 500] {
        for with_usage in [true, false] {
            let mut history: Vec<AgentMessage> = (0..messages)
                .map(|i| {
                    if i % 2 == 0 {
                        AgentMessage::user_text(format!("question {i} {}", "x".repeat(400)))
                    } else {
                        let mut a = AssistantMessage::pending(&model);
                        a.content.push(AssistantContent::Text(TextContent {
                            text: format!("answer {i} {}", "y".repeat(400)),
                            text_signature: None,
                        }));
                        if with_usage {
                            a.usage.total_tokens = 1_000 + i as u64;
                        }
                        a.stop_reason = StopReason::Stop;
                        AgentMessage::assistant(a)
                    }
                })
                .collect();
            history.push(AgentMessage::user_text("tool output ".repeat(100)));
            let runs = 2_000u32;
            let start = Instant::now();
            let mut sum = 0u64;
            for _ in 0..runs {
                sum = sum.wrapping_add(context_tokens(std::hint::black_box(&history), 0, "sys"));
            }
            let each = start.elapsed() / runs;
            println!(
                "check: history={messages} messages, usage={with_usage}: {each:?} per check ({sum})"
            );
        }
    }
}
