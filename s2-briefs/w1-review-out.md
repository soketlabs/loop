# S2 wave 1 review

Reviewed `git diff staging..<branch>` and implementation against the approved `docs/design/runtime.md` and wave 1 briefs. Base: `80277fca9fef453138636d200754740d348b5e88`. Heads: `s2/event-log` at `075b36225e7d485b2c862b992df558d158a859e2`, `s2/session-factory` at `10f7911816eb4a7952191b59262152a7eeacf8fe`, and `s2/protocol-types` at `89d2455222a3139592fbb3ea22b7f2c1b375b7b6`. Skipped `s2/store-lease`.

Static review only. No source files were edited, branches switched, or commits made. No tests were executed; Windows behavior below follows the path representations used by the code.

## Findings

### P2 — Standalone bootstrap changes ancestor instruction discovery

- **Branch:** `s2/session-factory`
- **File:line:** `crates/loop-app-core/src/runtime.rs:1298`
- **Issue:** Canonicalizing `opts.cwd` before context discovery violates the brief's requirement that bootstrap keep its exact TUI/`--print`/desktop behavior. For example, `loop --cwd /work/project-link`, where the link targets `/srv/project`, previously loaded `/work/AGENTS.md`; it now loads `/srv/AGENTS.md` instead. CLI passes `--cwd` directly (`crates/loop-cli/src/main.rs:228`), and `load_context_files` walks lexical parents with `PathBuf::pop()` (`crates/loop-app-core/src/system_prompt.rs:214` on this branch). This can remove required user instructions and introduce different instructions into the model prompt. `WorkspaceResources::load_resolved` also canonicalizes the path before loading context, so removing only the bootstrap assignment does not fix it.
- **Fix:** Preserve the standalone caller's path for ancestor context discovery while using a separate normalized path for workspace identity and execution. Carry that distinction through `load_resolved`. Add a Unix symlink/ancestor characterization test that pins the pre-refactor context and system prompt.

### P2 — New bootstrap characterization fails on Windows path spelling

- **Branch:** `s2/session-factory`
- **File:line:** `crates/loop-app-core/tests/bootstrap_characterization.rs:69`
- **Issue:** The test compares `runtime.harness.cwd()` with the unnormalized `std::env::current_dir()` value. The harness receives a canonical path, and `HostExecutionEnv::new` also canonicalizes it (`crates/loop-agent/src/harness/env/host.rs:28`). On Windows, canonicalization returns a verbatim path such as `\\?\C:\...`, while `current_dir()` normally returns `C:\...`. Lexical `PathBuf` equality fails for those representations. The child test therefore fails, followed by the parent test's status assertion, contrary to the brief's requirement that tests not break on Windows.
- **Fix:** Compare against `cwd.canonicalize().unwrap()` or normalize both sides equivalently. Keep this cross-platform assertion enabled.

## Requested InteractParams assessment

`crates/core/loop-protocol/src/rpc.rs:176–182` uses an optional `session` alongside a flattened `InteractionRequest`. This produces exactly the design's top-level wire JSON, `{kind, id, session, ...}`, without a nested `request` property. The added goldens cover all five interaction variants, old messages without `session`, extra fields, and decoding routed messages with the original `InteractionRequest` reader. This differs from the design's literal per-variant Rust representation, but is not a wire compatibility defect.

It is a sound boundary for S2.6: a Session-bound `EscalationHost` can capture the authoritative `SessionRef` and wrap each request in `InteractParams` when dispatching to attached interactive connections. Arbitration must use Session plus interaction identity, retain per-connection JSON-RPC request ids, and scope withdrawal to the original recipients. The later server/client chunk must adopt the wrapper; the current single-Session wire server still serializes bare requests, which is expected in this types-only chunk. No actionable finding arises from the wrapper itself.

Findings: P0 **0**, P1 **0**, P2 **2**, P3 **0**.
