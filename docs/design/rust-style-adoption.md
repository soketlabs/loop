# Adopting the Google Rust style guide

Source: https://google.github.io/styleguide/rustguide.html

Goal: follow the parts of the guide that prevent real bugs or confusion. Skip the
parts that add noise without value. Loop is application code: the crates are
consumed inside this workspace, not by outside teams. The guide treats API design
rules as recommendations for application code, so this plan does too.

## How this was measured

- `cargo clippy --workspace --exclude loop-desktop` on library and binary targets
  (test code excluded), with extra lints that map to guide rules.
- Greps for the rules clippy cannot check.
- Not linted: `loop-desktop` (needs nightly) and test targets. The guide allows
  panics and asserts in tests.

## Already compliant

| Guide rule | Status |
|---|---|
| rustfmt, clippy in CI | `cargo fmt --check` and `clippy -D warnings` run in `ci.yml` |
| `&str`/`&[T]`/`&T` params, `FromStr` over custom `from_str`, `as_/to_/into_` naming, `is_empty` with `len` | Enforced by default clippy lints (`ptr_arg`, `borrowed_box`, `should_implement_trait`, `wrong_self_convention`, `len_without_is_empty`) |
| `unsafe` needs a `SAFETY:` comment | 4 `unsafe` sites, 4 `SAFETY:` comments |
| `expect` over `unwrap` | Production code has 13 `expect` calls and only 8 `unwrap` calls |
| No `catch_unwind`, `for_each`, `#[rustfmt::skip]`, `ctor`, or proc macros | 0 of each |
| Glob imports | Nearly all are allowed exceptions: `use super::*` in tests, `pub use types::*` re-exports, `use Enum::*` in enum-focused functions |
| Public API docs | `#![deny(missing_docs)]` already set in `loop-ai`, `loop-agent`, `loop-protocol`, `loop-acp` |
| Pattern matching over `is_some()` + `unwrap()` | Enforced by default clippy (`unnecessary_unwrap`) |

## Changes worth making

### 1. Stop reading through poisoned mutexes (cleanup)

The guide says never to read a poisoned mutex's data. These sites do, with
`lock().unwrap_or_else(|p| p.into_inner())`. The data at these sites is
replaced in a single assignment or push, so a panic can't leave it
half-written. This is consistency and less boilerplate, not a bug fix:

- `crates/loop-app-core/src/agents.rs` (fields `host` and `gate`, 4 sites)
- `crates/loop-ai/src/api/openai_completions.rs:930` (`REASONING_REJECTED`)
- `crates/loop-cli/src/agent_question.rs` (`reply`)
- `crates/loop-app-core/src/extensions/mod.rs:66` (`lock().unwrap()`)

Fix: switch these to `parking_lot::Mutex`. It is already a workspace dependency,
has no poisoning, and `lock()` returns the guard directly. Also review the other
`std::sync::Mutex`/`RwLock` uses in production code. For example,
`harness/mcp/mod.rs:49` calls `.expect("tool lock poisoned")`. Use `parking_lot`
for all of them so the question doesn't come up again.

### 2. Replace the 8 production `unwrap()` calls

| Site | Fix |
|---|---|
| `loop-agent/src/harness/prompt_templates.rs:120` | `Regex::new(..).unwrap()` runs on every call. Move it into a `static LazyLock<Regex>` with `expect` |
| `loop-agent/src/agent.rs:391` | `last_mut().unwrap()`: use `if let Some(last) = ...` or `expect("<invariant>")` |
| `loop-agent/src/harness/tools/file_mutation_queue.rs:24` | `guard.as_mut().unwrap()`: use `get_or_insert_with` |
| `loop-orchestration/src/planner/task_graph.rs:122` | `in_degree.get_mut(..).unwrap()`: `expect("every node has an in-degree entry")` |
| `loop-app-core/src/extensions/mod.rs:66` | covered by item 1 |
| `loop-test-support/src/lib.rs` (3) | test helper crate; switch to `expect` or allow `unwrap_used` there |

Add `# Panics` doc sections where clippy flags them (9 sites).

### 3. Small mechanical fixes

- `#[must_use]` on 11 builder methods that return `Self`
  (`return_self_not_must_use`). Dropping a builder's result is always a bug.
- 5 `redundant_clone`, 2 `string_add` (use `format!`), 2 `&Option<T>` params
  (`ref_option`, use `Option<&T>`).
- `crates/ops/loop-update/src/control.rs:1`: split the nested `use std::{..}`.
- `missing_docs` in `loop-update` (33), `loop-telemetry` (32), `loop-mcp` (6),
  `loop-orchestration` (5) and `loop-app-core` (1). Add the docs, then
  `#![deny(missing_docs)]` to match the other library crates. Skip `loop-cli`,
  which already opts out as a binary.

### 4. Lock it in with lints, not reviews

Rules a tool can check go in `Cargo.toml`. CI's `-D warnings` then makes them
fail the build:

```toml
[workspace.lints.clippy]
unwrap_used = "warn"
undocumented_unsafe_blocks = "warn"
return_self_not_must_use = "warn"
redundant_clone = "warn"
string_add = "warn"
ref_option = "warn"
missing_panics_doc = "warn"
```

Add `[lints] workspace = true` to each crate's `Cargo.toml`. Then add a
`clippy.toml` at the root:

```toml
allow-unwrap-in-tests = true
```

Add `rustfmt.toml` with `max_width = 100` (this matches the current default)
and `use_field_init_shorthand = true`. Skip the nightly-only options.

### 5. A short `CODING_STANDARDS.md` for the judgement-call rules

`/code-review` reads this file. It applies to **new diffs only**, with no
retrofitting. Keep it to the rules below:

- Use an `enum` instead of a `bool` parameter when the call site would read
  `foo(true)` and the meaning isn't obvious. Simple setters such as
  `set_enabled(bool)` are fine.
- Use a newtype over a type alias when values can be mixed up (IDs, paths,
  units).
- Use named fields over tuples, except single-field newtypes.
- Keep iterator chains short. Name the chain before a `for` loop instead of
  building it in the header.
- Give `assert!` and `expect` a message that says *why* the condition holds.
- Add explicit types on `.parse()`, `.into()`, `.collect()` when not obvious.
- Use `Option` for "not found", `Result` for recoverable failures, and `panic`
  only for broken invariants.
- No `lock()` recovery from poison. Use `parking_lot`.

## Deliberately skipped

| Guide rule | Why skip |
|---|---|
| `[chrome]` and `[Fuchsia]` rules (no async, no threads, flat layout, `chromium::import!`) | Only apply to those codebases. Loop is tokio-async by design |
| Avoid `Rc`/`Arc` | Most `Arc` use is shared service state across tokio tasks (`Arc<dyn ExecutionEnv>`, `Arc<dyn SessionStore>`). That is the idiomatic pattern here |
| Generics over `dyn Trait` | A library rule. Dynamic services are the right fit for runtime-selected providers and stores |
| Return iterators over `Vec` | The guide allows `Vec` in application code |
| `#[non_exhaustive]` on public enums and structs (289 candidates) | All consumers are in this workspace and can be updated in one change. That is the guide's own exception |
| `#[must_use]` on every candidate (365) | Too noisy. Only builders get it (item 3), plus `Result` which already has it |
| `# Errors` doc sections (176) | Little value for internal APIs |
| Qualify function calls with their module, one-module-per-`use` rewrite | Style churn across about 73k lines with no bug-prevention value. Apply to new code through rustfmt defaults only |
| rustfmt `imports_granularity`, `hex_literal_case`, `format_macro_matchers` | Nightly-only. The guide allows omitting them on stable |
| Splitting non-leaf modules (`loop-cli/src/tui/mod.rs`, 2133 lines; `loop-cli/src/app.rs`, 5496 lines) | A real smell, but an architecture task, not a style rule. Handle separately if wanted |
| `format_push_string` (57) | Minor allocation win. Fix only when touching that code |

## Status

Implemented on `chore/rust-style-guide`, one commit per item. Notes:

- `loop-cli/src/debug_log.rs` keeps `std::sync::Mutex`. tracing-subscriber's
  `MakeWriter` requires it, and that file already returns an `io::Error` on
  poison instead of reading through it.
- `loop-test-support` keeps `std::sync::Mutex` with `expect`, so the test
  helper crate doesn't need a new dependency.
- `task_graph.rs` uses `if let` instead of `expect`. The edges are validated
  just above, so the entry always exists.
- clippy's `allow-unwrap-in-tests` covers only `#[test]` functions, so 20
  integration-test files allow `unwrap_used` at file level for their
  helpers.
- The `keys` constants in `loop-telemetry` allow `missing_docs`, because each
  key's string value is its documentation.
