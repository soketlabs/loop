# loop-agent

Stateful agent with tool execution and event streaming, built on [`loop-ai`](../loop-ai).

Inspired by `@earendil-works/pi-agent-core`, implemented in pure Rust for a unified distributable binary.

## Components

| Layer | Module | Role |
|-------|--------|------|
| Loop | `agent_loop` | Turns, parallel/sequential tools, steering/follow-up |
| Agent | `agent` | Stateful transcript, awaited subscribers, queues |
| Harness | `harness::AgentHarness` | Sessions, snapshots, sandbox, resources |
| Sessions | `harness::session` | Memory, JSONL, SQLite (+ FTS), scanning search |
| Tools | `harness::tools` | `read` / `write` / `edit` / `bash` over `ExecutionEnv` |
| Sandbox | `harness::sandbox` | Pluggable env for tool side-effects (`KrunSandbox` / local krun) |

## Quick start

```rust
use std::sync::Arc;
use loop_agent::{
    stream_fn_from_models, Agent, AgentOptions, AgentState,
};
use loop_ai::{Models, providers::{faux_provider, FauxScript, FauxResponse}};

# async fn demo() {
let script = FauxScript::new();
script.push(FauxResponse::Text("hi".into()));
let models = Models::new();
models.set_provider(faux_provider(script));
let model = models.get_model("faux", "faux-model").unwrap();

let agent = Agent::new(AgentOptions::new(
    AgentState::new(model),
    stream_fn_from_models(Arc::new(models)),
));
agent.prompt("Hello").await.unwrap();
# }
```

## Sandbox

When harness sandbox mode is enabled, tool FS/shell ops use `sandbox.env()`:

```rust
use loop_agent::harness::{KrunIsolation, KrunSandbox, LocalSandboxRuntime, SandboxMode};
// SandboxMode::Enabled { sandbox: Arc::new(KrunSandbox::new(
//     KrunSandbox::config_for(workdir, KrunIsolation::Full, LocalSandboxRuntime::Runc))) }
```

`KrunSandbox` (`kind` = `local`) runs a Podman container with a selectable OCI runtime:

- **Runtimes:** `runc` (default), `crun`, `runsc` (gVisor), `krun` (microVM)
- **full** — `read` / `write` / `edit` / `bash` via `podman exec`
- **partial** — FS on the host bind-mount (path-jailed); only `bash` via `podman exec`

A future `remote` kind is reserved but not implemented.

## Sessions

- **Memory** — tests / ephemeral
- **JSONL** — one file per session, append-only tree entries (`create_jsonl_session_store`)
- **SQLite** — durable multi-session DB with migrations + optional FTS (`create_sqlite_session_store`)

## Live tests

```bash
LOOP_TEST_BASE_URL="https://api.tensorstudio.ai/v1" \
LOOP_TEST_MODEL="qwen3-30b" \
LOOP_TEST_API_KEY_ENV="OPENAI_API_KEY" \
cargo test -p loop-agent --test live_agent -- --ignored --nocapture
```

Live tests stream model text to stderr and run **3 turns** each (tool loop + harness/SQLite).

## Offline tests

```bash
cargo test -p loop-agent
```
