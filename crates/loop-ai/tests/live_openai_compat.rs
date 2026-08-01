//! Live integration test against an OpenAI-compatible endpoint.
//!
//! Always compiled; marked `#[ignore]` so normal `cargo test` stays offline.
//!
//! ```bash
//! LOOP_TEST_BASE_URL=https://api.example.com/v1 \
//! LOOP_TEST_MODEL=my-model \
//! LOOP_TEST_API_KEY_ENV=OPENAI_API_KEY \
//! LOOP_TEST_PRINT=1 \
//! cargo test -p loop-ai --test live_openai_compat -- --ignored --nocapture
//! ```
//!
//! Set `LOOP_TEST_PRINT=1` (or `true`) to print streamed model output.

use futures::StreamExt;
use loop_ai::{
    providers::{custom_provider, CustomModelSpec, CustomProviderConfig},
    AssistantContent, AssistantMessageEvent, Context, Message, Models, SimpleStreamOptions,
    StopReason,
};

fn print_enabled() -> bool {
    matches!(
        std::env::var("LOOP_TEST_PRINT").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

#[tokio::test]
#[ignore = "requires LOOP_TEST_BASE_URL and a reachable OpenAI-compatible API"]
async fn live_openai_compatible_stream() {
    let base_url = std::env::var("LOOP_TEST_BASE_URL")
        .expect("set LOOP_TEST_BASE_URL to run this ignored live test");
    let model_id = std::env::var("LOOP_TEST_MODEL").unwrap_or_else(|_| "llama3.2".into());
    let api_key_env = std::env::var("LOOP_TEST_API_KEY_ENV").ok();
    let print = print_enabled();

    let models = Models::new();
    models.set_provider(custom_provider(CustomProviderConfig {
        id: "live".into(),
        name: Some("Live".into()),
        base_url,
        api_key_env: api_key_env.into_iter().collect(),
        models: vec![CustomModelSpec::new(model_id.clone())],
        headers: None,
    }));

    let model = models.get_model("live", &model_id).expect("model registered");
    let context = Context {
        messages: vec![Message::user_text("Reply with the single word: pong")],
        ..Default::default()
    };

    if print {
        eprintln!("model={model_id} prompt=Reply with the single word: pong");
        eprint!("assistant: ");
    }

    let stream = models.stream_simple(&model, &context, SimpleStreamOptions::default());
    let mut stream = stream;
    let mut text = String::new();
    let mut thinking = String::new();
    let mut saw_error = None;

    while let Some(ev) = stream.next().await {
        match ev {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                if print {
                    eprint!("{delta}");
                }
                text.push_str(&delta);
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                if print {
                    eprint!("{delta}");
                }
                thinking.push_str(&delta);
            }
            AssistantMessageEvent::Error { error, .. } => {
                saw_error = Some(error.error_message.unwrap_or_default());
            }
            _ => {}
        }
    }

    if print {
        eprintln!();
    }

    let result = stream.result().await;
    if let Some(err) = saw_error {
        panic!("stream error: {err}");
    }
    assert!(
        matches!(
            result.stop_reason,
            StopReason::Stop | StopReason::Length | StopReason::ToolUse
        ),
        "unexpected stop: {:?} err={:?}",
        result.stop_reason,
        result.error_message
    );
    assert!(
        !text.is_empty() || !result.content.is_empty(),
        "expected some assistant content"
    );

    if print {
        if text.is_empty() {
            // Fall back to assembled content if deltas were empty.
            let assembled: String = result
                .content
                .iter()
                .filter_map(|b| match b {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            eprintln!("assistant (final): {assembled}");
        }
        if !thinking.is_empty() {
            eprintln!("thinking: {thinking}");
        }
        eprintln!(
            "stop={:?} usage_in={} usage_out={} total={}",
            result.stop_reason,
            result.usage.input,
            result.usage.output,
            result.usage.total_tokens
        );
    }
}
