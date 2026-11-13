# Workspace driver

Status: accepted for task C1 (contract and local adapters). Tasks C2 to C5 build on it.

## 1. Purpose

A **Workspace** is the place where the files and commands of one Session live. Today a Workspace is the host folder, a Seatbelt sandbox (macOS) or a Podman container (Linux). Loop web needs a fourth place: a sandbox in a remote pool that sleeps when idle and wakes from a snapshot with the same files (`docs/design/loop-web.md` on `docs/loop-vision`; Phase 1 row "Workspace driver" and Phase 2 "microVM workspace pool" in `docs/execution-plan.md`).

This note fixes one contract for all of them. Task C1 adds the contract and moves the local backends behind it. C1 does not change behaviour and does not add a remote driver.

## 2. Where the contract lives

| Crate | Content | Depends on |
|---|---|---|
| `crates/core/loop-workspace` | The contract: `FileSystem`, `Shell`, `ExecutionEnv`, `Sandbox`, `SandboxFactory`, `Workspace`, `WorkspaceDriver`, the registry, errors, capabilities, `WorkspacePath`, process and transfer types, adapters | `async-trait`, `parking_lot`, `serde`, `serde_json`, `thiserror`, `tokio-util` |
| `crates/core/loop-workspace-testkit` | The conformance suite | `loop-workspace`, `tokio`, `tokio-util` |
| `crates/loop-agent` (`harness::sandbox`) | The backends and the built-in drivers (`host`, `seatbelt`, `podman`). It re-exports every contract name under its old path | `loop-workspace` and the rest of the agent |

Reason: `loop-agent` needs `loop-ai` (model clients) and, by default, SQLite. A private driver crate must not pull them. `cargo tree -p loop-workspace -e normal` shows only the six crates above and their own dependencies (31 crates in total; no SQLite, no TUI, no HTTP client).

## 3. The contract

### 3.1 Traits

| Trait | Methods | Notes |
|---|---|---|
| `FileSystem` | `cwd`, `absolute_path`, `join_path`, `read_text_file`, `read_binary_file`, `write_file`, `append_file`, `file_info`, `list_dir`, `exists`, `create_dir`, `remove`, `canonical_path`, `create_temp_dir`, `file_identity` | Unchanged; moved from `harness/types.rs` |
| `Shell` | `sandboxed`, `removed_env_note`, `exec(command, ShellExecOptions) -> ShellOutput` | Unchanged; moved |
| `ExecutionEnv` | `FileSystem + Shell` (blanket) | Unchanged; moved |
| `Sandbox` | `kind`, `id`, `status`, `info`, `env`, `start`, `stop`, `destroy`, `outlives_process` | Unchanged; moved |
| `SandboxFactory` | `kind`, `create(SandboxConfig)` | Unchanged; moved |
| `Workspace: Sandbox` | `driver_id`, `capabilities`, `workspace_status`*, `root`*, `resolve`*, `spawn`*, `read_chunk`*, `write_chunk`*, `snapshot`*, `hibernate`*, `wake`* | New. * = default implementation |
| `WorkspaceDriver` | `id`, `kind`, `capabilities`, `supports(HostOs)`*, `check`*, `create(WorkspaceSpec)`, `attach(id)`* | New |
| `WorkspaceProcess` | `id`, `write_stdin`, `close_stdin`, `resize`, `signal`, `next_event` | New; no implementation yet |

`Workspace` extends `Sandbox`. Thus every `Arc<dyn Workspace>` is also an `Arc<dyn Sandbox>` (trait upcasting), and the harness (`SandboxMode::Enabled`) does not change.

The defaults of `spawn`, `read_chunk`, `write_chunk`, `snapshot`, `hibernate`, `wake` and `attach` return `WorkspaceError::Unsupported { driver, operation }`. C4 and C5 only add implementations.

### 3.2 Identity, kind and status

| Item | Meaning |
|---|---|
| Driver id | Unique name of a driver: `host`, `seatbelt`, `podman`, later `worktree`, `remote` |
| Kind | The sandbox mode that the driver serves: `off` (host), `local` (Seatbelt or Podman), later `remote`. `Sandbox::kind()` stays `local` for the local backends, so `/sandbox status` does not change |
| Workspace id | `Sandbox::id()`. The driver makes it. `attach(id)` takes it |
| `WorkspaceStatus` | `created`, `starting`, `ready`, `stopping`, `stopped`, `hibernating`, `hibernated`, `waking`, `failed`, `gone`. It is a superset of `SandboxStatus`. `SandboxStatus` does not change, because `agent_harness.rs` compares it with `Ready` |

### 3.3 Lifecycle

| Operation | Rule |
|---|---|
| `create` | Makes the Workspace. It does not start it. No command runs |
| `start` | Makes it `ready`. A second call is not an error |
| `stop` | Stops commands. Files stay when `persistent`. A second call is not an error |
| `snapshot` | Saves the files (and, where possible, the process state). Returns a `SnapshotId` |
| `hibernate` | `snapshot`, then release compute. Status `hibernated`. Running commands end (see 6) |
| `wake` | Restore the last snapshot. Same files. Status `ready` |
| `destroy` | Removes the Workspace. Can be called twice and before `start`. The host driver never deletes the project folder |
| `attach` | Reopens a Workspace by id after a Runtime restart |

### 3.4 Capabilities

`WorkspaceCapabilities` is data. The conformance kit checks that the data agrees with the behaviour.

| Field | Meaning |
|---|---|
| `isolation` | `none`, `partial`, `full` |
| `remote` | Files and commands are on another machine. Paths are not host paths |
| `persistent` | Files stay after `stop` and after the Runtime exits |
| `outlives_process` | Keeps running after the Runtime exits unless destroyed |
| `confines_reads`, `confines_writes` | Reads or writes outside the Workspace (and the allowed paths) fail |
| `own_environment` | Commands get the driver's minimal environment (`Shell::sandboxed`) |
| `network` | `not_enforced`, `allowed`, `denied`, `allowlist(hosts)` |
| `limits` | `cpu_millis`, `memory_mib`, `disk_mib`, `max_output_bytes`, `max_exec_timeout_ms` |
| `streaming_exec`, `pty`, `snapshot`, `chunked_transfer`, `attach` | The matching method works. When `false`, it returns `Unsupported` |

Values of the built-in drivers:

| Driver | Isolation | Reads | Writes | Own env | Network | Limits |
|---|---|---|---|---|---|---|
| `host` | none | confined only with `confineReads` (tool lock) | not confined | no | not enforced | output 1 MiB |
| `seatbelt` | partial | not confined (credentials denied) | confined | yes | `sandbox.network` (allowed or denied) | output 1 MiB |
| `podman` | full or partial | confined | confined | yes | not enforced (Linux does not enforce `sandbox.network` or `sandbox.allowWrite` yet; linux-live-check) | CPU, memory from options |

The `podman` values are from the code, not from a kit run: the kit did not run on Linux in C1.

## 4. Paths

| Question | Decision |
|---|---|
| What is a path in `FileSystem`? | A path in the **driver's namespace**. For local drivers this is a host path. For the Podman sandbox it is the same path, because the project is bind-mounted at the same place. For a remote driver it is a guest path, not valid on the Runtime host |
| What does `cwd()` return? | The Workspace root in the driver's namespace. The signature does not change. Callers must not give it to `std::fs`, `Command::current_dir` or a host child. C2 removes such uses |
| What is `WorkspacePath`? | A normalised, `/`-separated path relative to the root. No leading `/`, no `..` above the root, no NUL. It is the path type of the protocol (file list, upload, download) and of chunked transfer. `Workspace::resolve` maps it to the driver namespace. `WorkspacePath::from_native` maps back |
| Who canonicalises? | The driver, through `FileSystem::canonical_path`, in its own namespace. `WorkspacePath` is lexical only. A symbolic link inside the Workspace can still point outside; the driver must check the opened file (as `confine_reads`, `JailedHostFs` and Seatbelt do now) |
| Who checks escapes? | The driver. The kit tests plain, `..` and link escapes for each driver that says it confines reads or writes |

## 5. What runs where

| Operation | Runs in | Reason |
|---|---|---|
| `bash` tool, `!shell` commands | Workspace | Untrusted code must be in the isolation boundary |
| File tools (read, write, edit, ls, glob, grep, find) | Workspace (`env()`) | They act on Workspace files. They already use `env` |
| git | Workspace | It is a command on Workspace files. Credentials for push come through a Connector, not the Workspace |
| Skills scripts (scripts that a skill tells the model to run) | Workspace | They are commands |
| Skill and context file *reading* (`AGENTS.md`, `SKILL.md`, `.loop/`) | Through `FileSystem` of the Workspace (C2) | The files are in the project. Today they are read with `std::fs` on the host |
| Project hooks (`.loop/hooks/*.json`, collected in `resources/mod.rs` ~159) | Workspace (C3) | A project hook is project code. On the host it escapes the sandbox. Today `load_hooks` merges project and global hook files into one list; C3 must keep the origin of each hook |
| Global hooks (`<agent dir>/hooks/*.json`) | Runtime host | The user wrote them; they may need host tools. Unchanged |
| MCP stdio servers of the project | Workspace (C3) | Project code |
| MCP servers that are Connectors (they hold credentials) | Runtime side | ADR 0006: Connectors run outside the Workspace; credentials never enter it |
| Document reader child (`loop __doc-to-text`) | Runtime host, fed bytes read through `env()` | Already so. Bytes come through the Workspace; the parser has no keys, no network use and strict limits |
| Web fetch and web search | Runtime side | They use Runtime keys and the SSRF guard. The Workspace network policy is separate |
| Model calls | Runtime side | Keys never enter a Workspace (`loop-web.md`, provider gateway) |

## 6. Policies and settings on a driver

| Item | Where it applies | Rule for drivers |
|---|---|---|
| Tool env policy (`removed_tool_env_names`, `tools.passEnv`) | `SessionExecutionEnv` in loop-agent wraps `env()` | The driver must honour `ShellExecOptions.env` and `inherit_env = false` exactly (the kit checks it). `spawn` is a new path around this wrapper: C4/C5 must add a Session wrapper that applies the same split to `SpawnRequest` before it reaches the driver. A driver with `own_environment` keeps only its allowlist, and `tools.passEnv` does not apply (as now) |
| Protected paths (`protect.rs`; Seatbelt has the same rules) | Inside the Podman and Seatbelt backends | Shell commands must not write the project `.loop`, git config, git hooks and pointers, and the `core.hooksPath` folder (Podman makes them read-only bind mounts, with placeholders for missing paths). A remote driver gets the same list and enforces it in the guest |
| Permission gate | Runtime, before the tool call | Unchanged. The gate never depends on the driver. A driver refusal is a second layer |
| Tool lock (`--tool-lock chat`) | Two layers. (1) The permission gate: `permissions::tool_lock::Confinement` (folder and lock root, resolved once at open) in `tool_gate.rs` refuses tools and paths before the call. (2) The env: sandbox off and `HostExecutionEnv::confine_reads` check the opened file | Layer 1 is Runtime side and does not depend on the driver. Layer 2 is a driver property (`confines_reads`). Unchanged in C1. The `host` driver with option `confineReads` gives the same env, and the kit runs it. C4 may route the lock through it. A remote driver under a lock must report `confines_reads` or be refused |
| `sandbox.mode` | Selects the kind: `off` → no driver (host env as now), `local` → kind `local` | C5 adds `remote` |
| `sandbox.network` | Spec option `network` | Seatbelt enforces it; Podman reports `not_enforced`. A remote driver must enforce it through the egress proxy |
| `sandbox.allowWrite`, `allowRead`, `denyHomeReads` | Spec options | Host paths. A remote driver ignores them and says so in `info()` |
| `sandbox.isolation`, `sandbox.runtime` | Spec options of `podman` | Unchanged validation in `SandboxBuilder::resolve` |

## 7. Streaming exec, PTY and transfer

The later terminal method needs these shapes. They are in the contract now.

| Need | API |
|---|---|
| Start | `Workspace::spawn(SpawnRequest { command, cwd, env, inherit_env, pty: Option<PtySize>, timeout_ms })` |
| Input | `WorkspaceProcess::write_stdin(&[u8])`, `close_stdin()` |
| Window size | `resize(PtySize { rows, cols })` |
| Signals | `signal(Interrupt \| Terminate \| Kill \| Hangup)`, sent to the process group |
| Output and exit | `next_event() -> Stdout(bytes) \| Stderr(bytes) \| Exited(ProcessExit { code, signal, timed_out })`. Bytes, not text: a PTY sends raw bytes |
| Download | `read_chunk(WorkspacePath, offset, max_len) -> FileChunk { offset, data, total_size, eof }` |
| Upload | `write_chunk(WorkspacePath, offset, data, last)`. Offset 0 starts a new upload in a temporary file; `last` moves it into place, so no reader sees half a file |

Methods take `&self`, so one task reads events while another writes input.

## 8. Cancellation, timeouts, restart and sleep

| Case | Rule |
|---|---|
| `ShellExecOptions.cancel` | Stop the whole process tree; return `ExecutionErrorCode::Aborted` |
| `timeout_ms` | Stop the tree; return `TimedOut`. A driver may have a lower `max_exec_timeout_ms` |
| Dropped `exec` future | The driver stops the command (host uses kill-on-drop and a tree killer) |
| Runtime restart | Local drivers: commands die with the Runtime (Podman containers are destroyed at exit, as now). Remote: the driver keeps the Workspace; the new Runtime calls `attach(id)`. Commands that were running are reported as ended with no exit code; the Runtime does not re-run them |
| Hibernate | Commands end first (`Terminate`, then `Kill` after a grace time). A PTY ends with `Exited`. `wake` restores files, not processes, unless the driver can snapshot memory and says so |
| Calls on a hibernated Workspace | The Runtime calls `wake` first. A driver gets no `exec` while `hibernated`; if it does, it returns `NotReady` |

## 9. Errors

`WorkspaceError` (`thiserror`, `#[non_exhaustive]`):

| Variant | When |
|---|---|
| `Unsupported { driver, operation }` | The method is not implemented. The capability flag is `false` |
| `UnknownKind(kind)`, `UnknownDriver(id)` | Selection found no driver |
| `PlatformUnsupported { driver, platform }` | The driver exists but not for this platform |
| `DriverUnavailable { driver, reason }` | An external tool or the remote service is missing |
| `Gone { id }` | The Workspace was destroyed, expired or lost. The Runtime must make a new one or fail the Session |
| `InvalidPath { path, reason }` | A `WorkspacePath` is not valid |
| `Sandbox(SandboxError)`, `File(FileError)`, `Exec(ExecutionError)` | Wrapped errors. Text is unchanged (`#[error(transparent)]`) |

`From<WorkspaceError> for SandboxError` keeps old texts for old callers.

## 10. Selection

| Step | Code |
|---|---|
| 1. Settings | `sandbox.mode` → `SandboxRequest.kind` (`None` = `local`) |
| 2. Registry | `platform_registry()` = built-in drivers (`podman`, `seatbelt`, `host`) + drivers from `register_workspace_driver` |
| 3. Capability check | `SandboxRegistry::select(kind, SandboxPlatform::os())` returns the first driver, in registration order, that serves the kind and `supports` the platform |
| 4. Error | No driver → `SandboxBuildError::Unavailable` with the old text ("the local sandbox is not available on … yet; tools run on the host (/sandbox off).") |

The only compile-time platform check is `HostOs::current()`. Linux selection runs in tests on macOS. Defaults do not change: Linux → `podman` (`--full --runc`), macOS → `seatbelt` (`--partial`), others → not available.

## 11. Drivers outside this repository

```rust
// In a private crate that depends only on loop-workspace:
loop_workspace::register_workspace_driver(Arc::new(MyRemoteDriver::new(config)));
```

The embedder calls this at startup, before the Runtime builds a Workspace. `platform_registry()` adds the driver after the built-in ones. Registration of an existing id replaces that driver in its place. Tests should build their own `WorkspaceDriverRegistry` and pass it to `SandboxBuilder::new` so that they do not share the global list. A crate that has an old `SandboxFactory` can wrap it with `FactoryDriver`.

## 12. Driver list

| Driver | Kind | State |
|---|---|---|
| `host` | `off` | C1: `HostWorkspaceDriver`. Not yet used by the session factory (sandbox off still builds `HostExecutionEnv` directly) |
| `seatbelt` | `local` | C1: `FactoryDriver` over `SeatbeltSandboxFactory`, macOS |
| `podman` (runc, crun, runsc, krun) | `local` | C1: `FactoryDriver` over `KrunSandboxFactory`, Linux |
| `worktree` | `local` or `off` | C4: a git worktree per Session over the host or Seatbelt env |
| `remote` (reference) | `remote` | C5: `ExecutionEnv` over a socket plus a small agent binary in the sandbox |
| microVM pool, gVisor pool | `remote` | Phase 2, private crate |

## 13. Conformance kit

`crates/core/loop-workspace-testkit`. A separate crate, not a module of `loop-store-testkit`, because that crate depends on `loop-agent` by default and a private driver crate must not. The kit depends only on the contract.

| Check | What it tests |
|---|---|
| `capabilities_match_behaviour` | Driver and Workspace flags agree; `own_environment` = `sandboxed()`; each missing capability returns `Unsupported`; each present one works |
| `lifecycle_is_idempotent` | Destroy before start, start twice, stop twice, destroy twice |
| `file_round_trips` | Text, binary (all 256 bytes), append, info, list, identity, missing file code, canonical path inside the root, remove, 4 MiB file |
| `path_escapes_are_refused` | Plain, `..` and link escapes for reads and writes, by file tools and by the shell; a non-confining driver must be able to read the outside file |
| `exec_runs_commands` | stdout, stderr, exit code, default cwd, `cwd` option, shell and file tools see the same files |
| `exec_env_is_explicit` | Overrides arrive; without `inherit_env` no Runtime variable leaks |
| `exec_timeout_and_cancel` | `TimedOut`, `Aborted`, fast stop, still usable after |
| `exec_output_is_capped` | Output above `max_output_bytes` is cut and flagged |
| `exec_runs_concurrently` | Eight commands in parallel each get their own output |

Runs in this repository: `host`, `host` with `confineReads`, the tool env policy over `host`, `seatbelt` on macOS (no external tools). `podman` needs Podman: it runs only on Linux with `LOOP_WORKSPACE_KIT_PODMAN=1` (optional `LOOP_WORKSPACE_KIT_RUNTIME`, `LOOP_WORKSPACE_KIT_ISOLATION`).

## 14. Migration

Order: C2 and C4 can start now, in parallel. C3 needs `Workspace::spawn` and the Session env wrapper for it from C4, so C3 starts after C4 lands that part (or C3 lands its code behind the `Unsupported` fallback: run on the host as now when `spawn` is unsupported). C5 starts after C4. The contract crate `crates/core/loop-workspace` is owned by C4; C2, C3 and C5 ask C4 for contract changes. Shared files: `config/settings.rs` (C4 adds `sandbox.driver`, C5 adds `mode = remote`: C5 rebases on C4), `session_factory.rs` sandbox part (C4, then C5). Protocol methods for files and terminal (`loop-protocol`) are not in C2-C5; a separate protocol task uses `WorkspacePath`.

| Task | Work | Files it owns |
|---|---|---|
| **C2** Remove local-path assumptions | Stop using `env.cwd()` as a host path: `tools/mod.rs` ~229 and `documents/mod.rs` ~171, ~182 (the reader child gets `env.cwd()` as `workspace`; `documents/child.rs` uses it only to refuse a reader binary that lies inside the Workspace, and runs the child in the host temp folder. For a remote driver pass `None`: the reader cannot be inside a remote Workspace) and ~633 (relative `cwd` join is fine; keep it in the namespace), `tools/mod.rs` ~431 `through_link` (uses `canonical_path` through `env`: keep, document), `search.rs` ~308 (relative paths of results are made from `cwd`; keep it in the driver namespace and never walk it with host APIs). Read context files (`system_prompt.rs` ~187-275) and skills (`skills.rs` `load_skills`, `loop-app-core/src/resources/mod.rs` ~105) through `FileSystem`. Add `WorkspacePath` where the protocol names files | `crates/loop-agent/src/harness/tools/**`, `crates/loop-app-core/src/system_prompt.rs`, `crates/loop-agent/src/harness/skills.rs`, `crates/loop-app-core/src/resources/**` |
| **C3** Hooks and MCP stdio in the Workspace | Project hooks (`loop-app-core/src/hook_command.rs` ~63 spawns on the host) and project MCP stdio servers (`loop-mcp/src/client.rs` ~122) start through `Workspace::spawn` when a Workspace is on. Global hooks and Connector MCP servers stay on the Runtime side | `crates/loop-app-core/src/hook_command.rs`, `hooks_load.rs`, `crates/loop-mcp/**` |
| **C4** Local drivers on the trait | Implement `spawn` (and PTY) and chunked transfer for `host`, `seatbelt`, `podman`. Host `read_chunk` must go through the `confine_reads` check. `TaggedSandbox` does not implement `Workspace` yet; add it. Fix the Podman timeout and cancel defects D2 and D3 of `docs/probes/linux-live-check.md`, which the kit check `exec_timeout_and_cancel` will find. Add a Session wrapper that applies the tool env policy to `spawn`. Add the `worktree` driver. Route sandbox-off through the `host` driver, the tool lock through `host` with `confineReads`. Add `sandbox.driver` if chosen (Q1) | `crates/loop-agent/src/harness/sandbox/**`, `env/**`, `crates/loop-app-core/src/sandbox_builder.rs`, `session_factory.rs` (sandbox part) |
| **C5** Reference remote driver | A `remote` driver: `ExecutionEnv` and `Workspace` over a socket, plus a small agent binary in the sandbox (`exec`, files, spawn, transfer). `attach` after restart. `sandbox.mode = remote`. Run the kit against it in CI with a local container as the "remote" | New crates `crates/extend/loop-workspace-remote` and `…-agent`, `crates/loop-app-core/src/config/settings.rs` (mode value) |

## 15. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Add `sandbox.driver` to choose a driver id directly? | Yes, in C4, global settings only. A project must not choose a weaker driver. Keep `sandbox.mode` as the main switch |
| Q2 | Should the Runtime refuse a driver whose capabilities do not meet the settings (for example `sandbox.network = false` but `not_enforced`)? | Not in C1 (it would change Linux behaviour). Show it in `/sandbox status` first; refuse for `remote` from C5 |
| Q3 | Should `FileSystem` take `WorkspacePath` instead of `&Path`? | No, not now. Keep `&Path` in the driver namespace; use `WorkspacePath` at the protocol edge. Revisit after C2 if host-path bugs stay |
| Q4 | Do project MCP servers run in the Workspace even when they need the network? | Yes; they get the Workspace network policy. A server that needs credentials must be a Connector |
| Q5 | Global registry or explicit injection for external drivers? | Both: `register_workspace_driver` for binaries, `SandboxBuilder::new(platform, registry)` for tests and embedders |
| Q6 | Memory snapshots (processes survive sleep)? | Files only for Loop web stage 1. Add a capability flag when a driver can do it |
| Q7 | `remote` driver transport? | In the cluster, the Runtime cell connects to the sandbox agent (a socket inside the pod network; the sandbox has no route out except the egress proxy). This is separate from the ADR 0008 control link, where the Runtime dials out to the Edge Gateway. Reuse `loop-wire` framing |
| Q8 | How does an external `local` driver replace the built-in one? | Today selection takes the first `local` driver, so an external one wins only if it reuses the id `podman` or `seatbelt`. With Q1, `sandbox.driver` names it directly |
