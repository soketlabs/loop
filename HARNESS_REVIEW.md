# Harness review findings

Review date: 2026-09-21

## Validation performed

`cargo test -p loop-agent` and `cargo test --workspace --no-fail-fast` both reproduced one non-ignored failure:

```text
test krun_full_fs_via_exec ... FAILED
FileError { code: InvalidPath, message: "path escapes sandbox root: /var/.../hello.txt" }
```

The remaining offline harness, session, tool, CLI, desktop, MCP, AI, and orchestration tests passed. Live-provider and real-Podman tests are ignored by design because they need external services/runtime support.

## Findings

### 1. Absolute paths through macOS symlink aliases are rejected by sandboxing

**Severity: high**

`KrunSandbox::with_client` canonicalizes the configured workdir, while `KrunExecutionEnv::guest_path` compares a caller-supplied absolute path without canonicalizing it. On macOS, a temporary path supplied as `/var/...` commonly canonicalizes to `/private/var/...`; the lexical `starts_with` check rejects it even though it is inside the configured root. The reproduced failing test is `krun_full_fs_via_exec`.

Affected code:

- `crates/loop-agent/src/harness/sandbox/krun.rs:166`
- `crates/loop-agent/src/harness/sandbox/krun.rs:363-384`
- `crates/loop-agent/src/harness/sandbox/krun.rs:752-768` (the partial-FS jail has the same mixed canonical/non-canonical comparison pattern)

Impact: valid absolute reads/writes fail in both full and partial sandbox modes when the caller uses a symlinked spelling of the workspace path.

Suggested direction: resolve existing inputs canonically before containment checks; for paths that do not yet exist, resolve the longest existing ancestor, then append the non-existing suffix. Preserve the caller-facing path separately when its exact spelling matters.

### 2. A snapshot/setup error permanently wedges the harness in `Turn`

**Severity: high**

`AgentHarness::run_turn` changes the phase to `Turn` and records a cancellation token, then returns directly if `create_turn_state()` fails. That failure path does not clear the token or release the phase. All later calls to `prompt()` see `Turn` and return `AgentHarnessError::Busy` indefinitely. A sandbox startup failure is one direct way to trigger this.

Affected code:

- `crates/loop-agent/src/harness/agent_harness.rs:1064-1075`
- normal cleanup only occurs at `crates/loop-agent/src/harness/agent_harness.rs:1188-1194`

Suggested direction: use a single scope guard/finally-style cleanup path for every post-acquisition return, including snapshot and pre-start-hook failures.

### 3. Sandbox preflight errors report a stale `Starting` state

**Severity: medium**

`KrunSandbox::start` sets status to `Starting` before awaiting `preflight`, but uses `?` on the preflight result. Only `run` errors transition to `Failed`. Thus a missing runtime, Podman, or platform requirement yields an error while `/sandbox status` keeps showing `starting`.

Affected code:

- `crates/loop-agent/src/harness/sandbox/krun.rs:257-286`

Suggested direction: map preflight errors through the same status transition as container-start errors, ideally resetting the environment/container fields as part of the failed-state transition.

### 4. CLI sandbox changes leave approval/revert operations on the old environment

**Severity: high**

The CLI creates `ToolApprovalBridge` once with the initial `tool_env`. A `/sandbox local` or `/sandbox off` command rebuilds the agent tools with the new environment but does not rebuild or rebind the approval bridge. The bridge uses its captured environment to revert rejected file edits. After a mode change, a rejection can therefore target the host when the edit happened in a sandbox, or a destroyed sandbox when the edit happened on the host.

Affected code:

- bridge construction: `crates/loop-cli/src/app.rs:250-285`
- tool/sandbox replacement: `crates/loop-cli/src/app.rs:2523-2591`
- captured environment and revert: `crates/loop-app-core/src/tool_approval.rs:185-230`, `crates/loop-app-core/src/tool_approval.rs:628-646`

Suggested direction: make the bridge resolve the harness's current execution environment at revert time, or recreate/reinstall the bridge hooks whenever the sandbox mode changes.

### 5. The bash destructive-command blocklist is bypassable

**Severity: high if this is treated as a safety boundary**

`check_command_policy` lowercases a shell string and searches for literal substrings. Shell syntax and simple flag reordering bypass it; examples include an extra option before a target, reordered flags, tabs/multiple spaces, shell variables, or command substitution. This check should not be presented as a reliable safeguard for host execution.

Affected code:

- `crates/loop-agent/src/harness/tools/mod.rs:292-329`

Suggested direction: rely on permissions and OS/container isolation as the safety boundary. If policy enforcement is required, parse a restricted command language/argv representation and deny risky executable/argument combinations; do not try to secure arbitrary shell text with substring matches.

## Code-quality note

`cargo clippy -p loop-agent --all-features --tests -- -D warnings` currently stops on three lint violations in `loop-ai` (`unnecessary_filter_map`, `derivable_impls`, and `collapsible_match`). These do not appear to be runtime harness defects, but they prevent a warning-free lint gate.

