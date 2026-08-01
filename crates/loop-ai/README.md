# loop-ai

Unified LLM API with provider collections, automatic auth resolution, token and cost tracking, and mid-session model hand-off.

This crate is designed for agentic workflows: tool calling, thinking/reasoning blocks, and a uniform streaming event protocol. It is not a bare chat-completions client.

## Features (phase 1)

- JSON-serializable [`Context`](crate::Context) / [`Message`](crate::Message) / [`Tool`](crate::Tool)
- [`AssistantMessageEvent`](crate::AssistantMessageEvent) stream (`start` → text/thinking/toolcall → `done` | `error`)
- [`Provider`](crate::Provider) + [`Models`](crate::Models) collection with auth resolution
- OpenAI Chat Completions–compatible custom providers (Ollama, vLLM, LiteLLM, gateways, …)
- Compat auto-detect + overrides ([`OpenAICompletionsCompat`](crate::OpenAICompletionsCompat))
- [`transform_messages`](crate::transform_messages) for mid-session hand-off
- Token cost calculation, rough context estimate, overflow heuristics
- Tool argument validation against JSON Schema
- [`faux_provider`](crate::providers::faux_provider) for deterministic tests

## Quick start

```rust,no_run
use futures::StreamExt;
use loop_ai::{
    providers::{custom_provider, CustomModelSpec, CustomProviderConfig},
    Context, Message, Models, SimpleStreamOptions,
};

#[tokio::main]
async fn main() {
    let models = Models::new();
    models.set_provider(custom_provider(CustomProviderConfig {
        id: "ollama".into(),
        name: Some("Ollama".into()),
        base_url: "http://localhost:11434/v1".into(),
        api_key_env: vec![], // keyless local
        models: vec![CustomModelSpec::new("llama3.2")],
        headers: None,
    }));

    let model = models.get_model("ollama", "llama3.2").unwrap();
    let mut context = Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![Message::user_text("Hello!")],
        tools: None,
    };

    let stream = models.stream_simple(&model, &context, SimpleStreamOptions::default());
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        // handle text_delta / toolcall_* / done / error
        let _ = event;
    }
    let assistant = stream.result().await;
    context.messages.push(loop_ai::Message::Assistant(assistant));
}
```

## Adding a new wire API

1. Implement [`ApiAdapter`](crate::ApiAdapter) under `src/api/<name>.rs` (`stream` + `stream_simple`).
2. Emit the same [`AssistantMessageEvent`](crate::AssistantMessageEvent) protocol; encode failures as `error` events (do not panic the stream after invoke).
3. Call [`transform_messages`](crate::transform_messages) before building the provider payload.
4. Add a provider factory under `src/providers/` that wires models + auth + the adapter via [`create_provider`](crate::create_provider).
5. Keep `lib.rs` free of heavy provider catalogs — consumers import the factories they need.

## Auth resolution order

1. Explicit `StreamOptions.api_key`
2. Stored credential in the [`CredentialStore`](crate::CredentialStore)
3. Ambient `ProviderAuth` resolve (typically env vars, or keyless)

## Tests

```bash
# Unit + integration (offline)
cargo test -p loop-ai

# Live OpenAI-compatible endpoint (ignored by default)
LOOP_TEST_BASE_URL=http://localhost:11434/v1 \
LOOP_TEST_MODEL=llama3.2 \
cargo test -p loop-ai --test live_openai_compat -- --ignored --nocapture
```

Integration tests live under `tests/`:

| File | Coverage |
|------|----------|
| `handoff.rs` | `transform_messages` (thinking, orphans, image downgrade) |
| `cost_compat.rs` | `calculate_cost`, `detect_compat` |
| `validate_tools.rs` | JSON Schema tool arg validation / coerce |
| `faux_stream.rs` | faux provider event order + `.result()` |
| `live_openai_compat.rs` | optional live HTTP stream (`#[ignore]`) |
