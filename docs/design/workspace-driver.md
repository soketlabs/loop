# Workspace driver

Status: accepted for tasks C1 and C1b (contract and local adapters). C2b (the gate through the env) is done. Tasks C2, C3, C4 and C5 build on it.

## 1. Purpose

A **Workspace** is the place where the files and commands of one Session live. Today a Workspace is the host folder, a Seatbelt sandbox (macOS) or a Podman container (Linux). Loop web needs a fourth place: a sandbox in a remote pool that sleeps when idle and wakes from a snapshot with the same files (`docs/design/loop-web.md` on `docs/loop-vision`; Phase 1 row "Workspace driver" and Phase 2 "microVM workspace pool" in `docs/execution-plan.md`).

This note fixes one contract for all of them. C1 and C1b add the contract and move the local backends behind it. They do not change behaviour for local users and do not add a remote driver.

## 2. Where the contract lives

| Crate | Content | Depends on |
|---|---|---|
| `crates/core/loop-workspace` | The contract: `FileSystem`, `Shell`, `ExecutionEnv`, `Sandbox`, `SandboxFactory`, `Workspace`, `WorkspaceDriver`, `WorkspaceProcess`, the registry, errors, capabilities, `WorkspacePath`, adapters | `async-trait`, `parking_lot`, `serde`, `serde_json`, `thiserror`, `tokio-util`, `tracing` |
| `crates/core/loop-workspace-testkit` | The conformance suite | `loop-workspace`, `tokio`, `tokio-util` |
| `crates/loop-agent` (`harness::sandbox`, `harness::env`) | The backends and the built-in drivers (`host`, `seatbelt`, `podman`). It re-exports every contract name under its old path | `loop-workspace` and the rest of the agent |

Reason: `loop-agent` needs `loop-ai` (model clients) and, by default, SQLite. A private driver crate must not pull them in.

## 3. The contract

### 3.1 Traits

| Trait | Methods (* = default implementation) |
|---|---|
| `FileSystem` | `cwd`, `absolute_path`, `join_path`, `read_text_file`, `read_binary_file`, `write_file`, `append_file`, `file_info`, `list_dir`, `exists`, `create_dir`, `remove`, `canonical_path`, `create_temp_dir`, `file_identity`*, `symlink_metadata`*, `read_link`*, `resolve_for_policy`* |
| `Shell` | `sandboxed`*, `removed_env_note`*, `exec(command, ShellExecOptions)` |
| `ExecutionEnv` | `FileSystem + Shell` (blanket) |
| `Sandbox` | `kind`, `id`, `status`, `info`, `env`, `start`, `stop`, `destroy`, `outlives_process`* |
| `SandboxFactory` | `kind`, `create(SandboxConfig)` |
| `Workspace: Sandbox` | `driver_id`, `capabilities`, `workspace_status`*, `epoch`*, `root`*, `resolve`*, `spawn`*, `attach_process`*, `list_processes`*, `read_chunk`*, `begin_upload`*, `upload_write`*, `upload_committed_offset`*, `upload_commit`*, `upload_abort`*, `snapshot`*, `hibernate`*, `wake`* |
| `WorkspaceDriver` | `id`, `kind`, `capabilities`, `supports(HostOs)`*, `check`*, `create(WorkspaceSpec)`, `attach(id) -> Attached`* |
| `WorkspaceProcess` | `id`, `write_stdin`, `close_stdin`, `resize`, `signal`, `next_event` |

`Workspace` extends `Sandbox`. `SandboxMode::Enabled` and `SandboxBuilder::start` carry `Arc<dyn Workspace>`, so `spawn`, transfer and capabilities stay reachable, and callers that need only the lifecycle use it as a `Sandbox`.

The new `Workspace` and `WorkspaceDriver` methods have defaults that return `WorkspaceError::Unsupported { driver, operation }`. C4 and C5 only add implementations.

Gap: the Runtime's shared workspace sandbox (`runtime.rs`, `WorkspaceResources::sandbox`) is still `Arc<dyn Sandbox>`. `runtime.rs` is owned by another task; C4 changes it to `Arc<dyn Workspace>`.

### 3.2 Identity, kind, status, epoch

| Item | Meaning |
|---|---|
| Driver id | Unique name of a driver: `host`, `seatbelt`, `podman`, later `worktree`, `remote` |
| Kind | The sandbox mode the driver serves: `off` (host), `local` (Seatbelt or Podman), later `remote`. `Sandbox::kind()` stays `local` for the local backends |
| Workspace id | `Sandbox::id()`. The driver makes it |
| Idempotency key | `WorkspaceSpec::key`, from the caller. A driver with `attach` returns the same Workspace for a repeated key (a create retried after a lost answer). Local drivers make a new one each time |
| Epoch | `Workspace::epoch()`. Each `attach` gives a larger epoch. A call through an older handle fails with `Fenced`, so two Runtime cells never drive one Workspace after a failover |
| `WorkspaceStatus` | `created`, `starting`, `ready`, `stopping`, `stopped`, `hibernating`, `hibernated`, `waking`, `failed`, `gone`. A superset of `SandboxStatus`, which does not change |

### 3.3 Spec and lifecycle

`WorkspaceSpec { key, tenant, session, source, options, labels }`. `source` is `Empty`, `Snapshot(SnapshotId)` or `HostFolder(path)`. `From<SandboxConfig>` makes a `HostFolder` spec; `sandbox_config()` goes back.

| Operation | Rule |
|---|---|
| `create` | Makes the Workspace. It does not start it. No command runs |
| `start` | Makes it `ready`. A second call is not an error |
| `stop` | Stops commands. Files stay when `persistent`. A second call is not an error |
| `snapshot` | Saves the files (and, where possible, process memory). Returns a `SnapshotId` |
| `hibernate` | `snapshot`, then release compute. Running processes end with exit `Lost`. A call in flight makes it fail with `Busy` |
| `wake(snapshot)` | Restore that snapshot, or the last one with `None`. Same files. Status `ready` |
| `destroy` | Removes the Workspace. Can be called twice and before `start`. The host driver never deletes the project folder |
| `attach(id)` | Reopens a Workspace after a Runtime restart, with a new epoch |

**Idle and sleep.** The Runtime decides when a Session is idle and calls `hibernate`. A pool may also put a Workspace to sleep. Decision: a call on a sleeping Workspace (`exec`, file calls) **wakes it inside the call**. When the wake fails, the call fails with the code `Unavailable` (retry class `Safe`). Callers thus never see a "hibernated" state as an error.

**`Gone` during a turn.** The tool call fails with `Gone` (class `Unsafe`). The Runtime stops using that Workspace for the turn and ends the turn with an error that says the Workspace was lost. The next turn makes a new Workspace from the last snapshot if there is one, else empty, and the Runtime tells the model that files may be lost.

### 3.4 Capabilities

`WorkspaceCapabilities` is data. The kit checks that the data agrees with the behaviour.

**Rule: capabilities may only add restrictions or cause a refusal. They never relax the permission gate, the tool lock or a setting.**

| Field | Meaning |
|---|---|
| `isolation` | `none`, `partial`, `full` |
| `remote` | Files and commands are on another machine |
| `path_namespace` | `host` or `guest`: whether `env()` paths are Runtime host paths. Independent of `remote` |
| `persistent`, `outlives_process` | Files stay after `stop`; the Workspace runs on after the Runtime exits |
| `confines_reads`, `confines_writes` | File-tool reads or writes outside the Workspace (and allowed paths) fail |
| `confines_shell` | The same limits hold for shell commands. The host with `confineReads` (tool lock) confines only file tools: the lock removes the shell instead |
| `own_environment`, `env_allowlist` | Commands get the driver's own environment; only the allowlisted names (a `*` suffix is a prefix) can be set. `None` means any name |
| `protected_paths_enforced` | Shell commands cannot write `.loop`, git config, git hooks and pointers, and the `core.hooksPath` folder |
| `honours_allow_write` | `sandbox.allowWrite` is enforced |
| `network` | `not_enforced`, `allowed`, `denied`, `allowlist(hosts)` |
| `limits` | `cpu_millis`, `memory_mib`, `disk_mib`, `max_output_bytes`, `max_exec_timeout_ms`, `max_upload_bytes` |
| `streaming_exec`, `pty`, `snapshot`, `chunked_transfer`, `attach` | The matching methods work. `pty` needs `streaming_exec` |

| Driver | Isolation | Reads | Writes | Shell confined | Env | Protected paths | allowWrite | Network |
|---|---|---|---|---|---|---|---|---|
| `host` | none | only with `confineReads` (tool lock) | no | no | Runtime env | no | no | not enforced |
| `seatbelt` | partial | no (credentials denied) | yes | yes | allowlist | yes | yes | `sandbox.network` |
| `podman` | full or partial | yes | yes | yes | no caller names (`exec` passes none) | yes | no | not enforced |

The `podman` values come from the code. The kit did not run on Linux in C1 or C1b.

## 4. Paths

| Question | Decision |
|---|---|
| What is a path in `FileSystem`? | A path in the **driver's namespace** (`path_namespace`). `host`: a Runtime host path (also Podman, which mounts the project at the same path). `guest`: valid only inside the Workspace |
| The rule | **Every Runtime-side check on a Workspace path goes through `env()`, never through `std::fs` or another host API.** The trait docs say this too |
| `cwd()` | The Workspace root in the driver namespace. The signature does not change. C2 removes the uses of it as a host path |
| `WorkspacePath` | Normalised, `/`-separated, relative. It refuses a leading `/`, `..` above the root, NUL, a backslash and a drive prefix such as `C:` in any segment, on every host. It is the path type of the protocol, of `SpawnRequest::cwd` and of transfer |
| Who canonicalises | The driver, through `canonical_path` and `resolve_for_policy`, in its own namespace. `WorkspacePath` is lexical only; the driver checks the opened file |
| Gate primitives | `symlink_metadata` (lstat: kind, size, uid, nlink, mode bits), `read_link`, and `resolve_for_policy` (resolved path, exists, any link on the way, uid, nlink). The host env has real versions. The default `resolve_for_policy` is conservative (a link is assumed when the resolved path differs from the lexical one); the other two default to `Unsupported`. The gate uses `canonical_path`, `read_link` and `symlink_metadata` (section 6), not `resolve_for_policy`: that one folds `..` before links and does not resolve the existing part of a missing path, so it cannot give the gate's answers. For a guest driver, `read_link` on a path that is not a link should fail with `InvalidPath` (or `NotFound`); any other error makes the gate treat the path as unresolvable |

## 5. What runs where

| Operation | Runs in | Reason |
|---|---|---|
| `bash` tool, `!shell` commands | Workspace | Untrusted code must be in the isolation boundary |
| File tools | Workspace (`env()`) | They act on Workspace files |
| git | Workspace | It is a command on Workspace files. Credentials for push come through a Connector |
| Skill scripts | Workspace | They are commands |
| Context files and skills (reading) | Through `env()` (C2) | They are project files |
| Project hooks (`.loop/hooks/*.json`, `resources/mod.rs` ~159) | Workspace (C3) | Project code. Today `load_hooks` merges project and global files into one list; C3 must keep the origin |
| Global hooks (`<agent dir>/hooks/*.json`) | Local: Runtime host. **Hosted (shared cell): not on the host** | See section 6.1 |
| Project MCP stdio servers | Workspace (C3) | Project code |
| MCP servers that are Connectors | Runtime side | ADR 0006: credentials never enter the Workspace |
| Document reader child | Local: Runtime host, fed bytes through `env()`. **Guest drivers: in the Workspace or a jail** | See section 6.1 |
| Web fetch, web search, model calls | Runtime side | They use Runtime keys and the SSRF guard |

## 6. Policies and settings on a driver

| Item | Where it applies | Rule |
|---|---|---|
| Tool env policy (`removed_tool_env_names`, `tools.passEnv`) | `SessionExecutionEnv` | It sends the removed names in `ShellExecOptions::remove_env` (an explicit override of the caller still applies). The driver drops them even from its own base environment. `SpawnRequest` has the same fields; C4 adds the Session wrapper for `spawn` |
| Protected paths (`protect.rs`; Seatbelt has the same rules) | Podman and Seatbelt backends | `protected_paths_enforced`. A remote driver gets the list and enforces it in the guest |
| Permission gate | Runtime | Every path fact (canonical path, link target, `lstat` kind, owner, mode bits, hard links, `.git` for the git root) comes through `permissions::paths::GatePaths` from an env in the driver's namespace (C2b). The decision code is sync: `GatePaths::run` runs it, fetches the first fact it missed through the env, and runs it again; facts last one decision. A `host`-namespace Session uses its host env (`WorkspaceResources::host`): a Seatbelt or Podman env jails its own reads and is not a resolver. A `guest` Session uses its own env and never the host. An answer the env cannot give (an error other than "not there" for a guest env, `Unsupported`, `Unavailable`) fails closed: the path is unresolvable, so deny and ask rules apply, allow rules and the `accept-edits` workspace do not, and the tool lock refuses it. Host reads stay only in named Runtime-side functions (the sync API for tests, external agents, the audit folder, the tool lock's root); `tests/gate_no_host_fs.rs` enforces the list |
| Search filter | Runtime (`ToolGate::search_filter`) | `SearchFilter` is sync (`tools/search.rs`), so it cannot wait for the env. It uses host facts for a `host` Session and no facts for a `guest` one: a `read` or search-tool `deny`/`ask` path pattern then hides every result (fail closed). Open: an async filter (C2 owns `search.rs`) |
| Tool lock (`--tool-lock chat`) | Two layers: `permissions::tool_lock::Confinement` in the gate (layer 1; per-call checks through the env), and sandbox off plus `HostExecutionEnv::confine_reads` (layer 2) | The root is admitted on the Runtime host (Runtime start and Session open). The `host` driver with option `confineReads` gives layer 2, and the kit runs it |
| **Refusal (after C2b)** | `SandboxBuilder::start` (`check_driver`) and Session open | A `guest`-namespace driver is refused with the tool lock: `SandboxRequest::path_policy` is `ToolLock`, or `Unknown` (the default). The lock's root and its confined reads are Runtime host paths. Path rules are allowed (`Rules` or `None`): the gate checks them through the env. The Session factory sets `Rules` (a locked workspace never starts a sandbox) and refuses a guest env with the lock again at Session open. No guest driver exists yet; a test uses a fake one |
| Settings in hosted and remote kinds | `check_driver` | For kinds other than `local`, selection refuses (`SandboxBuildError::Unmet`, naming the setting) when the driver does not enforce: `sandbox.network = false`, `sandbox.allowWrite`, `sandbox.denyHomeReads`, confined writes, protected paths. `local` keeps today's behaviour |
| `sandbox.isolation`, `sandbox.runtime` | Spec options of `podman` | Unchanged validation |

### 6.1 Hosted-mode security rules

| Rule | Reason |
|---|---|
| Global hooks do not run on the host of a shared cell. Disable them in hosted mode, or run them in the Workspace | The host of a shared cell serves many users |
| The document reader child parses untrusted bytes. For a `guest` driver it runs in the Workspace or in a jail, not on the cell host | A parser bug must not reach the cell |
| Any output that the Runtime uses as a path (search results, tool output, link targets) goes back through `env()` | The Runtime host disk is not the Workspace |

## 7. Streaming exec, PTY and transfer

| Need | API |
|---|---|
| Start | `spawn(SpawnRequest { command, cwd: Option<WorkspacePath>, env, inherit_env, remove_env, pty: Option<PtySize>, timeout_ms, cancel, detach, output_buffer_bytes })` |
| Events | `next_event() -> ProcessEvent { seq, stream: Stdout/Stderr/Pty/Control, kind: Output(bytes) / Dropped { bytes } / Exited(ExitState) }`. `seq` starts at 1 and grows by one |
| Exit | `ExitState::Exited { code }`, `Signaled { signal }`, `TimedOut`, `Cancelled`, `Lost` (the driver cannot know) |
| Input, size, signals | `write_stdin`, `close_stdin`, `resize(PtySize)`, `signal(Interrupt/Terminate/Kill/Hangup)` to the whole tree |
| Reconnect | `attach_process(id, after_seq)`, `list_processes()` |
| Ring buffer | Bounded (default 1 MiB of output). Evicted events become one `Dropped { bytes }`. Output never blocks the process; `write_stdin` waits when the pipe is full |
| Kill tree | Timeout, cancel and `Kill` stop every descendant, also one that called `setsid` (PID namespace, cgroup, container or a tracked tree) |
| Handle dropped | `detach = false`: kill the tree. `detach = true`: it runs on; `attach_process` finds it |
| Download | `read_chunk(path, offset, max_len, if_match) -> FileChunk { offset, data, total_size, eof, tag }`. With `if_match`, a changed file fails with `Changed` |
| Upload | `begin_upload(UploadRequest { path, size, sha256, if_match, mode }) -> UploadId`, `upload_write(id, offset, data) -> committed offset`, `upload_committed_offset(id)`, `upload_commit(id) -> FileTag` (size and checksum check, then one rename of a temporary file; a new file gets `mode` or 0644; an existing file keeps its mode), `upload_abort(id)`. `max_upload_bytes` limits the size |
| Later | Directories and archives (tar upload and download) |

## 8. Cancellation, timeouts, restart

| Case | Rule |
|---|---|
| Cancel | Stop the whole tree; code `Aborted` |
| Timeout | Stop the tree; code `TimedOut`. A driver may have a lower `max_exec_timeout_ms` |
| Dropped `exec` future | The driver stops the command |
| Runtime restart | Local: commands die with the Runtime (Podman containers are removed at exit). Remote: the new Runtime calls `attach(id)` and gets a new epoch; processes it cannot account for show `Lost` |

## 9. Errors and retries

`WorkspaceError` (non-exhaustive): `Unsupported`, `UnknownKind`, `UnknownDriver`, `BuiltinId`, `PlatformUnsupported`, `Unmet`, `DriverUnavailable`, `Gone`, `Busy`, `Fenced`, `OutcomeUnknown`, `Changed`, `TooLarge`, `UnknownUpload`, `ChecksumMismatch`, `UnknownProcess`, `InvalidPath`, and the transparent `Sandbox`, `File`, `Exec`. `FileErrorCode` and `ExecutionErrorCode` are non-exhaustive and add `Unavailable`, `Gone`, `OutcomeUnknown` (and `FileErrorCode::Unsupported`).

| Retry class | Errors | Agent loop |
|---|---|---|
| `Safe` | `DriverUnavailable`, `Busy`, `Changed`, `SandboxError::NotReady`, code `Unavailable`, exec `SpawnFailed` | Nothing happened. Retry the same call with back-off |
| `Unknown` | `OutcomeUnknown`, exec `TimedOut`, `Aborted`, `OutcomeUnknown` | The effect is unknown. Do not retry blindly; check the state or tell the model |
| `Unsafe` | All others, including `Gone` and `Fenced` | Do not retry |

`WorkspaceError::retry()`, `is_retryable()`, `FileErrorCode::retry()` and `ExecutionErrorCode::retry()` give the class. `From<WorkspaceError> for SandboxError` keeps old texts.

## 10. Selection

| Step | Code |
|---|---|
| 1. Settings | `sandbox.mode` → `SandboxRequest.kind` (`None` = `local`) |
| 2. Registry | `platform_registry()`: built-in drivers (`register_builtin`), then the drivers from `register_workspace_driver` |
| 3. Platform | `select(kind, os)`: the first driver that serves the kind and `supports` the platform |
| 4. Capabilities | `check_driver`: the refusals of section 6 |
| 5. Errors | No driver: `Unavailable` with the old text. A refusal: `Unmet` |

Defaults do not change: Linux `podman` (`--full --runc`), macOS `seatbelt` (`--partial`), others not available.

## 11. Drivers outside this repository

```rust
loop_workspace::register_workspace_driver(Arc::new(MyRemoteDriver::new(config)));
// Only on purpose, and logged:
loop_workspace::replace_builtin_workspace_driver(Arc::new(MyPodman::new()));
```

A plain registration that takes a built-in id is refused and logged. `reset_registered_workspace_drivers()` clears the list for tests; tests should prefer their own `WorkspaceDriverRegistry` with `SandboxBuilder::new`. `kinds()` lists the kinds usable on this platform (old meaning); `all_kinds()` lists every kind. `SandboxWorkspace::info()` adds one new last row, `Driver`, to the `/sandbox status` card; the other rows do not change.

## 12. Driver list

| Driver | Kind | State |
|---|---|---|
| `host` | `off` | `HostWorkspaceDriver`. Not yet used by the session factory |
| `seatbelt` | `local` | `FactoryDriver` over `SeatbeltSandboxFactory`, macOS |
| `podman` | `local` | `FactoryDriver` over `KrunSandboxFactory`, Linux. `KrunSandbox` also implements `Workspace` |
| `worktree` | `local` or `off` | C4 |
| `remote` (reference) | `remote` | C5 |
| microVM or gVisor pool | `remote` | Phase 2, private crate |

## 13. Conformance kit

`crates/core/loop-workspace-testkit` (a separate crate, because `loop-store-testkit` depends on `loop-agent`).

| Check | What it tests |
|---|---|
| `capabilities_match_behaviour` | Flags agree with behaviour; `pty` needs `streaming_exec`; each missing capability returns `Unsupported` (spawn, process, transfer, snapshot, wake, attach); present ones work (event seq, upload, tag, `Changed`) |
| `lifecycle_is_idempotent` | Destroy before start, start twice, stop twice, destroy twice |
| `file_round_trips` | Text, binary, append, info, list, identity, missing file code, canonical path, remove, 4 MiB |
| `path_escapes_are_refused` | Absolute and `..` reads, link reads, shell reads under `confines_shell`, plain, `..`, link and shell writes |
| `exec_runs_commands` | Output, exit code, cwd, shared files |
| `exec_env_is_explicit` | A sentinel secret in the Runtime environment never arrives, for both `inherit_env` values and every driver; an allowlisted override arrives exactly |
| `exec_timeout_and_cancel` | Typed codes, fast stop, and `sleep 2; touch marker` never writes the marker |
| `exec_output_is_capped` | `max_output_bytes` |
| `exec_runs_concurrently` | Eight commands in parallel |

Runs: `host`, `host` with `confineReads`, the env policy over `host`, `seatbelt` (macOS; `#[ignore]` elsewhere), a lying driver that the kit must reject, and `podman` (`#[ignore]`; run on Linux with `LOOP_WORKSPACE_KIT_PODMAN=1 … -- --ignored`).

## 14. Migration

Order: **C2 and C2b in parallel → C4 → C3 → C5.**

| Task | Work | Files it owns |
|---|---|---|
| **C2** Remove host-path uses in tools | `tools/mod.rs` ~229 and `documents/mod.rs` ~171, ~182 (the reader child gets `env.cwd()` as `workspace`; `documents/child.rs` uses it only to refuse a reader binary inside the Workspace; pass `None` for a guest driver), `tools/mod.rs` ~431 (`through_link`: keep through `env`), ~633 (relative `cwd` join: keep in the namespace), `search.rs` ~308 (results are made relative to `cwd`; they go back through `env`). Context files (`system_prompt.rs`) and skills (`skills.rs`, `resources/mod.rs`) through `env()` | `crates/loop-agent/src/harness/tools/**`, `crates/loop-app-core/src/system_prompt.rs`, `crates/loop-agent/src/harness/skills.rs`, `crates/loop-app-core/src/resources/**` |
| **C2b** Gate path resolution through the env (**done**) | `permissions::paths::GatePaths` over the Session env; `PathMetadata::mode` added; the guest refusal now covers only the tool lock. Open: an async search filter (C2), Seatbelt and Krun envs forward the gate primitives (C4), subagent rule bases for guest Sessions (`loop-runtime` `subagent.rs` sets host paths), the diff editor for guest snapshots | `crates/loop-app-core/src/permissions/**`, the path code of `tool_gate.rs` and `tool_approval.rs`, the `path_policy` refusal in `sandbox_builder.rs` |
| **C4** Local drivers on the trait | Owns `loop-workspace`. Implement `spawn`, PTY, process attach and upload sessions for `host`, `seatbelt`, `podman`; host transfer goes through the `confine_reads` check. The Session env wrapper for `spawn`. Forward the gate primitives in the Krun and Seatbelt envs. The `worktree` driver. Route "off" and the tool lock through the `host` driver. `WorkspaceResources::sandbox` (`runtime.rs`) to `Arc<dyn Workspace>`. `sandbox.driver` (Q1) | `crates/loop-workspace/**`, `crates/loop-agent/src/harness/sandbox/**`, `env/**`, `crates/loop-app-core/src/sandbox_builder.rs`, the sandbox part of `session_factory.rs`, `config/settings.rs` (sandbox keys) |
| **C3** Hooks and MCP stdio in the Workspace | Project hooks (`hook_command.rs` ~63) and project MCP stdio (`loop-mcp/src/client.rs` ~122) start through `Workspace::spawn`. Global hooks follow section 6.1 | `crates/loop-app-core/src/hook_command.rs`, `hooks_load.rs`, `crates/loop-mcp/**` |
| **C5** Reference remote driver | `ExecutionEnv` and `Workspace` over a socket, plus a small agent binary in the sandbox; `attach` with epochs; `mode = remote`; the kit in CI with a local container as the "remote" | New crates `crates/extend/loop-workspace-remote` and `…-agent`; `config/settings.rs` (mode value, after C4) |

A protocol task (not in this list) adds file and terminal methods with `WorkspacePath`.

## 15. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Add `sandbox.driver` to choose a driver id? | Yes, in C4, global settings only |
| Q2 | Refuse a driver whose capabilities do not meet the settings? | Done for kinds other than `local` and for `guest` paths with the tool lock (path rules with `guest` paths are allowed since C2b). For `local`, show the gap in `/sandbox status` (for example Linux network) and do not refuse: local users see no change. **Owner decision:** should `local` refuse too, later? |
| Q3 | Should `FileSystem` take `WorkspacePath`? | No. Keep `&Path` in the driver namespace, with the `path_namespace` capability and the rule "checks go through `env()`". Use `WorkspacePath` at the protocol edge, in `SpawnRequest` and in transfer |
| Q4 | Project MCP servers in the Workspace? | Yes; servers that need credentials become Connectors |
| Q5 | Global registry or injection? | Both |
| Q6 | Memory snapshots? | Files only for now |
| Q7 | Remote transport? | The cell connects to the sandbox agent in the cluster; `loop-wire` framing. Separate from the ADR 0008 control link |
| Q8 | How does an external driver replace a built-in one? | Only with `replace_builtin_workspace_driver`, which is logged. A plain registration of a built-in id is refused |
| Q9 | **Owner decision:** in hosted mode, disable global hooks, or run them in the Workspace? | Disable them first; run them in the Workspace after C3 |
| Q10 | **Owner decision:** the document reader for guest drivers: in the Workspace, or a separate jail on the cell? | In the Workspace (no new isolation to build) |
