//! Live agent e2e against OpenAI-compatible endpoint (`#[ignore]`).
//!
//! ```bash
//! LOOP_TEST_BASE_URL="https://api.tensorstudio.ai/v1" \
//! LOOP_TEST_MODEL="qwen3-30b" \
//! LOOP_TEST_API_KEY_ENV="OPENAI_API_KEY" \
//! cargo test -p loop-agent --test live_agent -- --ignored --nocapture
//! ```

use std::sync::Arc;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_search, create_sqlite_session_store,
    AgentHarness, AgentHarnessOptions, HostExecutionEnv, SandboxMode,
};
use loop_agent::{
    stream_fn_from_models, Agent, AgentEvent, AgentMessage, AgentOptions, AgentState, AgentTool,
    AgentToolResult,
};
use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};
use loop_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Model, Models,
    ToolResultContent, UserContent, UserMessageContent,
};

fn live_models() -> (Arc<Models>, Model) {
    let base_url = std::env::var("LOOP_TEST_BASE_URL").expect("LOOP_TEST_BASE_URL");
    let model_id = std::env::var("LOOP_TEST_MODEL").unwrap_or_else(|_| "qwen3-30b".into());
    let api_key_env = std::env::var("LOOP_TEST_API_KEY_ENV").ok();

    let models = Models::new();
    models.set_provider(custom_provider(CustomProviderConfig {
        id: "live".into(),
        name: Some("Live".into()),
        base_url,
        api_key_env: api_key_env.into_iter().collect(),
        models: vec![CustomModelSpec::new(model_id.clone())],
        headers: None,
    }));
    let model = models.get_model("live", &model_id).unwrap();
    (Arc::new(models), model)
}

fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn format_agent_message(msg: &AgentMessage) -> String {
    match msg {
        AgentMessage::Llm(Message::User(u)) => match &u.content {
            UserMessageContent::Text(s) => s.clone(),
            UserMessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        },
        AgentMessage::Llm(Message::Assistant(a)) => assistant_text(a),
        AgentMessage::Llm(Message::ToolResult(t)) => {
            let body = t
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            format!("tool={} is_error={} {}", t.tool_name, t.is_error, body)
        }
        other => format!("{other:?}"),
    }
}

fn print_turn(turn: usize, prompt: &str, reply: &AgentMessage) {
    eprintln!("── turn {turn} ──");
    eprintln!("user: {prompt}");
    eprintln!("[{}] {}", reply.role(), format_agent_message(reply));
}

fn attach_stream_printer(agent: &Agent) {
    agent.subscribe(|event| async move {
        match event {
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                eprint!("{delta}");
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingDelta { delta, .. },
                ..
            } => {
                eprint!("{delta}");
            }
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                eprintln!("\n[tool start] {tool_name}");
            }
            AgentEvent::ToolExecutionEnd { tool_name, .. } => {
                eprintln!("[tool end] {tool_name}");
            }
            AgentEvent::MessageEnd { message } if message.role() == "assistant" => {
                eprintln!();
            }
            _ => {}
        }
    });
}

#[tokio::test]
#[ignore = "requires LOOP_TEST_BASE_URL and a reachable OpenAI-compatible API"]
async fn live_agent_tool_loop() {
    let (models, model) = live_models();

    let tool = AgentTool::simple(
        "get_time",
        "Get Time",
        "Returns the current UTC time as an ISO-ish string. No arguments.",
        serde_json::json!({"type":"object","properties":{}}),
        |_id, _args, _c, _u| async move {
            Ok(AgentToolResult::text(chrono::Utc::now().to_rfc3339()))
        },
    );

    let mut state = AgentState::new(model);
    state.set_tools(vec![tool]);
    state.system_prompt = "You are a helpful assistant. Keep replies short. When asked the time, call get_time.".into();

    let agent = Agent::new(AgentOptions::new(
        state,
        stream_fn_from_models(Arc::clone(&models)),
    ));
    attach_stream_printer(&agent);

    let turns = [
        "What time is it? Use the get_time tool.",
        "Thanks. In one short sentence, what city timezone abbreviation would UTC be if it were London?",
        "Reply with exactly three words confirming you remember we checked the time.",
    ];

    for (i, prompt) in turns.iter().enumerate() {
        let turn = i + 1;
        eprintln!("\n=== agent turn {turn}/3 ===");
        eprintln!("user: {prompt}");
        eprint!("assistant: ");
        agent.prompt(*prompt).await.expect("prompt");

        let state = agent.state().await;
        let reply = state
            .messages()
            .iter()
            .rev()
            .find(|m| m.role() == "assistant")
            .cloned()
            .expect("assistant reply");
        print_turn(turn, prompt, &reply);
    }

    let state = agent.state().await;
    let roles: Vec<_> = state.messages().iter().map(|m| m.role()).collect();
    assert!(
        roles.iter().any(|r| *r == "toolResult") || roles.iter().any(|r| *r == "assistant"),
        "expected tool or assistant messages, got {roles:?}"
    );
    eprintln!("\n=== full transcript ({} messages) ===", state.messages().len());
    for m in state.messages() {
        eprintln!("[{}] {}", m.role(), format_agent_message(m));
    }
}

#[tokio::test]
#[ignore = "requires LOOP_TEST_BASE_URL and a reachable OpenAI-compatible API"]
async fn live_harness_sqlite_persist_and_search() {
    let (models, model) = live_models();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("live-sessions.sqlite");
    let store = create_sqlite_session_store(&db_path).unwrap();
    let search = create_sqlite_session_search(&db_path, Arc::clone(&store));
    let repo = create_session_repository(Arc::clone(&store), Some(search));
    let session = repo
        .create(Some("/live-proj".into()), Some("live".into()))
        .await
        .unwrap();
    let session_id = session.metadata().id.clone();

    let host = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(&models),
        model: model.clone(),
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "You are a helpful assistant. Keep replies short. Prefer including the word tensorstudio when greeting.".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });
    harness.subscribe(|event| async move {
        if let AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } = event
        {
            eprint!("{delta}");
        }
    });

    let turns = [
        "Say hello and mention tensorstudio once.",
        "What single word did I ask you to mention?",
        "Reply with a short farewell that still includes tensorstudio.",
    ];

    for (i, prompt) in turns.iter().enumerate() {
        let turn = i + 1;
        eprintln!("\n=== harness turn {turn}/3 ===");
        eprintln!("user: {prompt}");
        eprint!("assistant: ");
        let msg = harness.prompt(*prompt).await.expect("harness prompt");
        assert_eq!(msg.role(), "assistant");
        eprintln!();
        print_turn(turn, prompt, &msg);
        harness.wait_for_idle().await;
    }

    let reopened = repo.open(&session_id).await.expect("reopen session");
    let ctx = reopened.build_context().await.expect("build context");
    assert!(
        !ctx.messages.is_empty(),
        "expected persisted messages after reopen"
    );

    let hits = repo
        .search("tensorstudio", Some("/live-proj"), 10)
        .await
        .expect("fts/scanning search");
    assert!(
        !hits.is_empty(),
        "expected search hits for persisted transcript text"
    );

    eprintln!("\n=== reopened context ({} messages) ===", ctx.messages.len());
    for m in &ctx.messages {
        eprintln!("[{}] {}", m.role(), format_agent_message(m));
    }
    eprintln!("search hits: {}", hits.len());
}
