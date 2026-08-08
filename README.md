# loop

Production-grade AI harness in Rust by **Soket AI** and **Abhishek**: unified LLM API, stateful agent, and interactive coding CLI.

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

### CI vs releases

- **CI** (`.github/workflows/ci.yml`) runs on every PR and push to `main`, plus manual **Run workflow**. It builds and tests; it does **not** publish a release.
- **Release** (`.github/workflows/release.yml`) publishes multi-platform binaries only when you cut a version tag (or manually with `create_release`).

Supported release targets:

| Asset | Platform |
|-------|----------|
| `loop-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `loop-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 |
| `loop-x86_64-apple-darwin.tar.gz` | macOS Intel |
| `loop-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |
| `loop-x86_64-pc-windows-msvc.zip` | Windows x64 |

#### Cut a release

1. Bump `[workspace.package] version` in the root `Cargo.toml` (must match the tag without the `v` prefix).
2. Commit, push to `main` (or your release branch).
3. Tag and push the tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

That starts the Release workflow, builds all targets, and creates a GitHub Release with archives + `.sha256` checksums.

#### Build artifacts without releasing

In GitHub: **Actions → Release → Run workflow**, leave **create_release** unchecked. Binaries are uploaded as workflow artifacts only.

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
