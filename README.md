# loop

Production-grade AI harness in Rust by **Soket AI**: unified LLM API, stateful agent, and interactive coding CLI.

## Crates

| Crate | Path | Role |
|-------|------|------|
| **loop-ai** | [`crates/loop-ai`](crates/loop-ai) | Unified LLM API, Soket provider (`/v1/models` refresh), OpenAI-compat + faux |
| **loop-agent** | [`crates/loop-agent`](crates/loop-agent) | Agent loop, AgentHarness, tools, sessions, sandbox, skills |
| **loop-cli** | [`crates/loop-cli`](crates/loop-cli) | Interactive `loop` TUI (ratatui) |

## Quick start

```bash
cargo run -p loop-cli
```

First run prompts for a Soket API key (or set `SOKET_API_KEY` / `TENSORSTUDIO_API_KEY` / `LOOP_API_KEY`). Config lives under `~/.loop/agent/`. See [`crates/loop-cli/README.md`](crates/loop-cli/README.md).

## Build / test

```bash
cargo build
cargo test -p loop-ai
cargo test -p loop-agent
cargo test -p loop-cli
```

Live OpenAI-compatible tests (ignored by default):

```bash
LOOP_TEST_BASE_URL="https://api.tensorstudio.ai/v1" \
LOOP_TEST_MODEL="qwen3-30b" \
LOOP_TEST_API_KEY_ENV="OPENAI_API_KEY" \
cargo test -p loop-ai --test live_openai_compat -- --ignored --nocapture

LOOP_TEST_BASE_URL="https://api.tensorstudio.ai/v1" \
LOOP_TEST_MODEL="qwen3-30b" \
LOOP_TEST_API_KEY_ENV="OPENAI_API_KEY" \
cargo test -p loop-agent --test live_agent -- --ignored --nocapture
```
