# Working on Loop

Loop is a Rust agent harness. This file is for coding agents (Codex, Claude
Code and others) and for people starting a new session. `CLAUDE.md` points
here.

## Read first

1. [docs/roadmap.md](docs/roadmap.md): what is done, what is next, and what is
   left over. Start on the next item there.
2. [docs/vision.md](docs/vision.md), [GLOSSARY.md](GLOSSARY.md) and
   [docs/adr/](docs/adr/): the agreed direction and the reasons.
3. [docs/stress-tests.md](docs/stress-tests.md): experiments that try to break
   the load-bearing ideas before each step is built.

Design docs for built work (`docs/design/runtime.md`, `tui-client.md`,
`protocol-wire.md`, `macos-sandbox.md`) are on `staging`. This branch
(`docs/loop-vision`) does not carry them. Read one with
`git show staging:docs/design/tui-client.md`. Follow a design doc when it
exists; where a task prompt disagrees with it, the design doc wins unless the
prompt says otherwise.

## Branches and merging

- `main` is merged from `staging` by the maintainer. Do not push to `main`.
- **Do not open pull requests.** Work on a branch, push the branch, and report
  its name and last commit. The maintainer or the supervising agent merges into
  `staging`, tests it, and the maintainer tests all of it before `main`.
- One branch and one worktree per task, under `../loop-wt/`. Do not touch other
  branches or worktrees.
- Platforms: Linux and macOS only. Windows support is dropped.

## Build and test

Run these before committing. All must pass.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --exclude loop-desktop --all-targets --locked -- -D warnings
CARGO_TARGET_DIR=$CARGO_TARGET_DIR/ci199 cargo +1.99.1 clippy --workspace --exclude loop-desktop --all-targets --locked -- -D warnings
cargo test --workspace --exclude loop-desktop --locked
(cd crates/loop-desktop && cargo +nightly check)
```

- Use a shared `CARGO_TARGET_DIR` for parallel worktrees, with
  `CARGO_INCREMENTAL=0` (incremental builds filled a 163 GB disk once).
- A shared target directory can serve stale artifacts from another worktree.
  Before the final run: `git diff --name-only --diff-filter=d origin/staging | grep '\.rs$' | xargs touch`.
  The `--diff-filter=d` matters: without it the touch recreates deleted files as
  empty files.
- macOS has no `timeout`. Use `perl -e 'alarm 900; exec @ARGV' cargo test …`.
- Write tests first. Every async test that can hang wraps in `tokio::time::timeout`.
  Timing tests wait for outcomes, never for a wall-clock bound.
- Linux is not compiled locally. CI is the Linux check.

## Code style

- Match the surrounding code: comment density, names and idiom.
- User-facing messages are plain English.
- A new wire method comes with a capability flag, a schema entry and golden
  tests (see how `session/list` was added in `crates/core/loop-protocol/src/rpc.rs`,
  `crates/core/loop-wire/src/server.rs` and `client.rs`).
- The TUI talks only to `SurfaceClient`
  (`crates/loop-cli/src/surface_client.rs`). Do not reach for the harness
  directly from `app.rs`.

## Live testing

- Use a temporary agent directory:
  `LOOP_CODING_AGENT_DIR=<tmp> loop …`, with `auth.json` symlinked from the real
  one. Do not edit the real settings.
- Never print an API key.
- A model that works for live checks: `openrouter/z-ai/glm-5.3`. The Soket
  endpoint (`api.soket.ai/v1`) can time out and rejects thinking parameters.
- Check your own work in a real terminal before handing it over. Run the steps
  a tester would run and read the output.

## Working with other agents (Herdr)

- The supervising agent runs workers in Herdr panes. Do not close panes or
  tabs you did not create.
- Run at most one Codex agent at a time, on GPT-6.1-Sol at medium reasoning
  effort. Use Claude Sonnet agents in parallel for other tasks. GPT-6-Luna is
  for small tasks and only when the maintainer allows it.
- Codex has a 5-hour and a weekly usage limit. If you are close to a limit,
  commit what you have as `wip: …`, push the branch, and write a short handoff
  of what is done and what is left.
- Summaries for the maintainer are written in ASD-STE100 style when they ask for
  it: short sentences, one idea each, active voice.

## When you finish

Reply with the branch, the last commit, what changed, and the test results.
Say plainly what you did not test.
