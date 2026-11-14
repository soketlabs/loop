# Workspace driver

Status: accepted for tasks C1 and C1b (contract and local adapters). C2b (the gate through the env) is done. C4, first slice (the host driver's processes, PTY and transfer; contract gaps; sandbox-off Sessions through the host driver) is done; see section 14. Tasks C2, C3, the rest of C4 and C5 build on it.

## 1. Purpose

A **Workspace** is the place where the files and commands of one Session live. Today a Workspace is the host folder, a Seatbelt sandbox (macOS) or a Podman container (Linux). Loop web needs a fourth place: a sandbox in a remote pool that sleeps when idle and wakes from a snapshot with the same files (`docs/design/loop-web.md` on `docs/loop-vision`; Phase 1 row "Workspace driver" and Phase 2 "microVM workspace pool" in `docs/execution-plan.md`).

This note fixes one contract for all of them. C1 and C1b add the contract and move the local backends behind it. They do not change behaviour for local users and do not add a remote driver.

## 2. Where the contract lives

| Crate | Content | Depends on |
|---|---|---|
| `crates/core/loop-workspace` | The contract: `FileSystem`, `Shell`, `ExecutionEnv`, `Sandbox`, `SandboxFactory`, `Workspace`, `WorkspaceDriver`, `WorkspaceProcess`, the registry, errors, capabilities, `WorkspacePath`, adapters | `async-trait`, `parking_lot`, `serde`, `serde_json`, `thiserror`, `tokio-util`, `tracing` |
| `crates/core/loop-workspace-testkit` | The conformance suite | `loop-workspace`, `tokio`, `tokio-util` |
| `crates/loop-agent` (`harness::sandbox`, `harness::env`) | The backends and the built-in drivers (`host`, `seatbelt`, `podman`). It re-exports every contract name under its old path. `harness::sandbox::host_ws` holds the host driver's processes, PTY and transfer | `loop-workspace`, the rest of the agent, `sha2` (upload checksums) |

Reason: `loop-agent` needs `loop-ai` (model clients) and, by default, SQLite. A private driver crate must not pull them in.

## 3. The contract

### 3.1 Traits

| Trait | Methods (* = default implementation) |
|---|---|
| `FileSystem` | `path_namespace`*, `cwd`, `absolute_path`, `join_path`, `read_text_file`, `read_binary_file`, `write_file`, `append_file`, `file_info`, `list_dir`, `exists`, `create_dir`, `remove`, `canonical_path`, `create_temp_dir`, `file_identity`*, `symlink_metadata`*, `read_link`*, `resolve_for_policy`* |
| `Shell` | `sandboxed`*, `removed_env_note`*, `exec(command, ShellExecOptions)` |
| `ExecutionEnv` | `FileSystem + Shell` (blanket) |
| `Sandbox` | `kind`, `id`, `status`, `info`, `env`, `start`, `stop`, `destroy`, `outlives_process`* |
| `SandboxFactory` | `kind`, `create(SandboxConfig)` |
| `Workspace: Sandbox` | `driver_id`, `capabilities`, `workspace_status`*, `epoch`*, `root`*, `resolve`*, `spawn`*, `attach_process`*, `list_processes`*, `read_chunk`*, `begin_upload`*, `upload_write`*, `upload_committed_offset`*, `upload_commit`*, `upload_abort`*, `snapshot`*, `hibernate`*, `wake`* |
| `WorkspaceDriver` | `id`, `kind`, `capabilities`, `supports(HostOs)`*, `check`*, `create(WorkspaceSpec)`, `attach(id) -> Attached`* |
| `WorkspaceProcess` | `id`, `write_stdin`, `close_stdin`, `resize`, `signal`, `next_event` |

`Workspace` extends `Sandbox`. `SandboxMode::Enabled` and `SandboxBuilder::start` carry `Arc<dyn Workspace>`, so `spawn`, transfer and capabilities stay reachable, and callers that need only the lifecycle use it as a `Sandbox`.

The new `Workspace` and `WorkspaceDriver` methods have defaults that return `WorkspaceError::Unsupported { driver, operation }`. C4 and C5 only add implementations.

`FileSystem::path_namespace()` (default `host`) tells a caller of a bare env whose paths it has; every wrapper env forwards it (`SessionExecutionEnv` does).

Every Session has a `Workspace`: the Session factory makes the `host` driver's Workspace of the folder (`WorkspaceResources::host_workspace` in `loop-app-core`, with `confineReads` under the tool lock), and `WorkspaceResources::host` is its env. With the sandbox on, `SandboxMode::Enabled` carries the sandbox's `Workspace` too.

The Runtime's shared workspace sandbox (`WorkspaceState::sandbox` in `loop-runtime`) is an `Arc<dyn Workspace>`, filled from `AgentHarness::ensure_sandbox_ready`, which returns the same type (C4f, done).

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
| `streaming_exec`, `pty`, `snapshot`, `chunked_transfer`, `upload`, `attach` | The matching methods work. `streaming_exec` covers `spawn`, `attach_process` and `list_processes`; `pty` needs `streaming_exec`; `chunked_transfer` is `read_chunk` (download); `upload` is the upload sessions and needs `chunked_transfer`; `attach` is `WorkspaceDriver::attach` (reopen after a Runtime restart), not `attach_process` |

| Driver | Isolation | Reads | Writes | Shell confined | Env | Protected paths | allowWrite | Network |
|---|---|---|---|---|---|---|---|---|
| `host` | none | only with `confineReads` (tool lock) | no | no | Runtime env | no | no | not enforced |

Streaming and transfer: `host` reports `streaming_exec`, `pty`, `chunked_transfer` and `upload` (Unix), and `attach` false. With `confineReads` it reports only `chunked_transfer`: the tool lock allows no process and no write, so `spawn`, `attach_process`, `list_processes` and the upload calls return `Unsupported`, and `read_chunk` obeys the lock's read rule. `seatbelt` and `podman` report none of them yet.
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
| Permission gate | Runtime | Every path fact (canonical path, link target, `lstat` kind, owner, mode bits, hard links, `.git` for the git root) comes through `permissions::paths::GatePaths` from an env in the driver's namespace (C2b). The decision code is sync: `GatePaths::run` runs it, fetches the first fact it missed through the env, and runs it again; facts last one decision. A `host`-namespace Session uses its host env (`WorkspaceResources::host`): a Seatbelt or Podman env jails its own reads and is not a resolver. A `guest` Session uses its own env and never the host. `resolve` has three answers: resolved, gone (a link loop or `..` below a missing part; the kernel cannot open it either), or unknown (an env error other than "not there" for a guest env, `Unsupported`, `Unavailable`, or a limit). Unknown fails closed everywhere: the path is not inside the workspace, is protected, is outside the tool lock's folder, no allow rule applies, and a deny or ask path pattern that could not be compared (a resolved form, the kernel form of `link/..`, the `~` form or the pattern's own prefix) counts as matching. Limits per decision: 128 env calls, 256 components in one path, 40 links in one chain; past the first two the gate refuses the call ("the path is too deep or has too many links to check"), whatever the rules say. Host reads stay only in named Runtime-side functions (the sync API for tests and external agents, the audit folder, the tool lock's root); `tests/gate_no_host_fs.rs` enforces the list |
| Search filter | Runtime (`ToolGate::search_filter`) | `SearchFilter` is sync (`tools/search.rs`), so it cannot wait for the env. It uses host facts for a `host` Session, remembered for one search (`SearchMemo`: dropped after a 100 ms pause), and no facts for a `guest` one: a `read` or search-tool `deny` path pattern then hides every result (fail closed). Open: an async filter (C2 owns `search.rs`) |
| Tool lock (`--tool-lock chat`) | Two layers: `permissions::tool_lock::Confinement` in the gate (layer 1; per-call checks through the env), and sandbox off plus `HostExecutionEnv::confine_reads` (layer 2) | The root is admitted on the Runtime host (Runtime start and Session open). The Session factory gets layer 2 from the `host` driver with option `confineReads` (and refuses a replaced `host` driver that does not confine reads), and the kit runs it. The locked Workspace starts no process and takes no upload (`Unsupported`); its downloads open the file and check the open file: a regular file with one link inside the folder |
| **Refusal** | `SandboxBuilder::start` (`check_driver`) and Session open | A `guest`-namespace driver is refused with the tool lock (`SandboxRequest::path_policy` is `ToolLock`, or `Unknown`, the default): the lock's root and its confined reads are Runtime host paths. It is also still refused with path rules (`Rules`): see "Before guest path rules are allowed" below. It is allowed only with `None` (no path pattern rule for `read`, `write`, `edit`, `grep`, `glob` or `ls`). The Session factory sets `Rules` or `None` from the rules (a locked workspace never starts a sandbox) and refuses a guest env with the lock again at Session open. No guest driver exists yet; a test uses a fake one |
| Settings in hosted and remote kinds | `check_driver` | For kinds other than `local`, selection refuses (`SandboxBuildError::Unmet`, naming the setting) when the driver does not enforce: `sandbox.network = false`, `sandbox.allowWrite`, `sandbox.denyHomeReads`, confined writes, protected paths. `local` keeps today's behaviour |
| `sandbox.isolation`, `sandbox.runtime` | Spec options of `podman` | Unchanged validation |

**Before guest path rules are allowed** (lift the `Rules` refusal only when all are done):

| Item | State |
|---|---|
| A fact the env cannot give, and the decision limits, fail closed at every deny and ask site (review F1, F2) | Done in C2b (`tests/gate_parity_env.rs`) |
| `~` in a rule names the guest's home | Not done. Today `~` rules fail closed in a guest Session (a `~` deny or ask pattern matches every path, a `~` allow pattern none). Needs the guest env's home, or this fail-closed rule kept on purpose |
| Subagent workspace and rule bases in guest form | Not done. `crates/loop-runtime/src/runtime/subagent.rs` ~171-183 (`confine`) sets the child gate's workspace from `child.workspace.cwd` and its rule bases from `parent.workspace` and `parent.opened.workspace.cwd`, all host forms; `runtime/sessions.rs` ~297 computes `workspace_root(&opened.workspace.cwd)` on the host disk. They must come from the gate's env (`ToolGate::paths`) |
| An async search filter, so a guest search checks rules through the env | Not done (C2 owns `tools/search.rs`) |
| Tests with a real guest env (C5's reference remote driver) | Not done |

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

### 7.1 The host driver (C4, first slice)

**Processes** (`harness::sandbox::host_ws::process`).

| Item | Rule |
|---|---|
| Start | `sh -c <command>` in a new session (`setsid`): the process leads its own session and process group. With a PTY (a pair from `/dev/ptmx`, opened with `O_CLOEXEC`; only `libc` calls, no new crate) the slave is stdin, stdout, stderr and the controlling terminal. A dedicated waiter thread starts the child. Between fork and exec the child marks every descriptor from 3 up close-on-exec (`close_range(CLOSE_RANGE_CLOEXEC)` on Linux, an `fcntl` loop elsewhere), so no Runtime descriptor reaches the command. On Linux it sets `PR_SET_PDEATHSIG` (`SIGKILL`): the waiter thread lives as long as the child, so the leader dies with the Runtime. A slot is taken under the table lock before any wait (at most 64 running, `Busy` after); `stop` and `destroy` close the table under the same lock, so a start that races them is refused or killed. A dropped `spawn` call kills a child it already started |
| Environment and cwd | As buffered `exec`: no `inherit_env` clears the environment, then `env` is set, then the `remove_env` names are removed. `cwd` is a `WorkspacePath`; its resolved folder must be inside the root (a link out is `InvalidPath`). The Session's tool env policy is not applied by `spawn` itself yet (follow-up C4d) |
| Buffer | Per process, `output_buffer_bytes` clamped to 64 KiB..16 MiB (default 1 MiB); each event costs its bytes plus 64. Readers never wait for a consumer; the oldest output is evicted. A reader that missed events gets one `Dropped { bytes }` on `Control`, with the `seq` of the last evicted event, then the next `seq`. `bytes` is exact for a live handle and for `after_seq` 0, an upper bound when `after_seq` itself was evicted. `Exited` is never evicted |
| Back-pressure | Output is not back-pressure: a child waits on a full pipe only until its reader takes the bytes (a Tokio task for pipes, a thread for the PTY master). The Runtime never waits for the child. `write_stdin` waits while the pipe or PTY is full (an async pipe write; a PTY writer thread fed by a queue of 4 writes) |
| Exit | The waiter learns of the exit with `waitid(WNOWAIT)`, kills the rest of the session, then reaps; readers get 500 ms to drain; then `Exited`. Exit states: `Exited { code }`, `Signaled { signal }`, `TimedOut` (`timeout_ms`), `Cancelled` (the cancel token, `signal(Kill)`, the drop of the last handle, `stop`, `destroy`). `Lost` is never used by this driver |
| Kill tree | First the process-group signal (it needs no listing, so a missing `/proc`, `hidepid` or a failed listing still stops the group), then every live member of the session, in passes until none is left (`/proc/<pid>/stat`, read as bytes, on Linux; `proc_listallpids` and `getsid` on macOS). Each member is checked again right before its signal: through a pidfd on Linux (`pidfd_open`, check, `pidfd_send_signal`; a pid cannot be reused while its pidfd is open), with a second `getsid` elsewhere. Pid 1 and the Runtime's own pid, group and session are never signalled. Signals hold the waiter's lock and are skipped after the reap, so a recycled leader pid is never signalled; when `waitid` fails, the waiter signals nothing. `signal(Interrupt/Terminate/Hangup)` goes to the whole session too. **Out of reach:** a descendant that starts its own session (`setsid`, a daemon); the buffered `exec` cannot reach it either |
| Leader ends | The rest of its session is killed (no orphans), as a terminal hang-up would end a shell's jobs |
| Detach | `detach: false`: the drop of the last handle kills the tree. `detach: true`: it runs on without a handle, in a Runtime until the last Session of its folder is explicitly closed, its workspace is reloaded, or the Runtime shuts down (unloading a Session never stops it, and the last Session of a folder with a running process is not unloadable); `attach_process(id, after_seq)` replays and continues (`after_seq` past the newest event is `SeqAhead`); `list_processes` shows it. The 32 newest ended processes stay listed. **Unwatched output:** a detached process with no handle and a full buffer is no longer read (spec option `pauseUnwatchedOutput`, default true): it waits on its pipe or PTY instead of running on unread; a handle that attaches resumes it |
| Shutdown | `stop`, `destroy` and the drop of the Workspace kill every process (tree) and reap it. `loop_agent::harness::sandbox::kill_all_host_processes(limit)` kills every host process of the Runtime process and waits for the reaps. The Runtime ends host processes where the resources of a workspace end (C4f, done; `loop-runtime` `runtime/host_cleanup.rs`): an explicit close of the last Session of a folder `stop`s that folder's host Workspace (the next Session of the folder starts it again; a gate per folder orders the stop against a Session opening in the same folder, and a slow stop in one folder delays no other folder); a reload `destroy`s the replaced generation when its last holder releases it; at shutdown (graceful stop, `runtime/stop`, version replacement, idle stop and signals all end in `finish_shutdown`) it `destroy`s every host Workspace its own factory made (`GlobalResources::host_workspaces`, tracked weakly, so also those of closed Sessions), together, then does the existing sandbox destroy. **Unload never stops a process:** the last loaded Session of a folder whose host Workspace has a running process is not unloadable, so a full Runtime refuses the open with `Busy` instead of killing the work (open uploads cannot be seen through the Workspace trait, so they do not pin). Every wait is bounded (3 s) and a wait that runs out is logged; a process that is still ending is left to the drop of the Workspace. A Runtime's shutdown touches no other Runtime's processes, so tests of other Runtimes in one OS process are safe; `kill_all_host_processes` runs only where the daemon process exits (`loop runtime start`, 3 s, after the shutdown). On Linux the kernel kills each leader when the Runtime dies (`PR_SET_PDEATHSIG`; its descendants are killed only by the group kill of a later shutdown). Spawned processes have their own session, so a terminal hang-up does not reach them (buffered `exec` stays in Loop's session). If the Runtime is killed (not stopped), its processes die on Linux (`PR_SET_PDEATHSIG`) and stay on macOS (follow-up C4h) |
| Readers after the exit | After the 500 ms drain, pipe readers are cancelled and the PTY reader (which waits in `select` for at most 200 ms at a time) stops, so a process that left the session but holds the pipe or the slave keeps no Runtime thread or descriptor |
| PTY input | `close_stdin` sends Ctrl-D once and refuses later writes; the PTY stays open. `resize` sets the window size on the master; `Unsupported` without a PTY |

**Transfer** (`harness::sandbox::host_ws::transfer`).

| Item | Rule |
|---|---|
| Paths | Each folder on the way from the root is opened and checked by descriptor (the kernel's name of the open folder is inside the root); links inside the root are followed, a link out is `InvalidPath`, also when it is swapped in during the call. Missing folders of an upload are made (0755), each only inside a checked folder |
| Final component | Never followed: a link, a folder or a special file there is `InvalidPath` (at begin and at commit) |
| `FileTag` | `v1-<dev>-<ino>-<size>-<mtime ns>-<ctime ns>` from `fstat` of the open file. A replacement (new inode), a size change and a write with a new time stamp change it. Not seen: a rewrite in place with the same size within one clock tick of the file system, or a process that sets both time stamps back (only root can set the change time). On Linux the tick is the kernel's coarse clock (one jiffy, 1–10 ms), not a nanosecond: two writes of the same size within that time keep the tag. Clients that need more compare a checksum |
| Upload | A temporary file `.<name>.loop-upload-<id>` (`O_EXCL`, `O_NOFOLLOW`, 0600) in the destination folder; `pwrite` at the offset. A write after the committed offset is `UploadOffset` (nothing written; resume from `upload_committed_offset`); a resend of stored bytes is allowed; data past the size is `TooLarge` |
| Commit | Committed must equal the size (`UploadOffset` otherwise; the upload stays). Then: SHA-256 (any case of hex), `fsync`, `if_match` against the destination again, mode (an existing file keeps its permission bits; a new one gets `mode` or 0644, without the umask; setuid, setgid and sticky bits are always dropped), a check that the temporary name is still our file (same device and inode, not followed), `renameat` in the same folder, `fsync` of the folder, and the same check on the destination (`Changed` when something else is there). Lock order: the upload map before an upload, never the reverse; commit and abort of one upload race safely. A reader sees the old file or the new one. A failed commit (checksum, `Changed`) removes the upload and its file. POSIX has no compare-and-rename: a writer between the last check and the rename is not seen |
| `read_chunk` | Checks the file with a `stat` before the open (a device or FIFO is never opened), opens it (`O_NONBLOCK`), checks it as above (a regular file inside the root; under the tool lock, the lock's rule with one link), tag from `fstat`, `pread` of at most 8 MiB, `fstat` again: a different tag, or a file that got shorter, is `Changed` |
| Limits | 4 GiB per upload (`max_upload_bytes`), 16 open uploads per Workspace (`Busy`), idle expiry after 10 minutes (spec option `uploadIdleMs`). Expired uploads are removed, with their files, at the next transfer call; `stop`, `destroy` and the drop remove all. Temporary files of a Runtime that was killed stay |

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
| `host` | `off` | `HostWorkspaceDriver`. The Session factory makes every folder's host env with it. Processes, PTY and transfer (section 7.1) |
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
| `processes_conform` | With `streaming_exec`: order and `seq` per stream, exit codes; stdin to a pipe and a PTY, `close_stdin`, `stty size` before and after `resize`; `Terminate` (trap), `Interrupt`, `Hangup`, `Kill`; timeout, cancel and `Kill` with a background step that never writes its marker; a 4 MiB output into a 64 KiB buffer with no reader ends, gives `Dropped`, and received plus dropped is all of it; `write_stdin` waits on a full pipe; detach, list, attach with `after_seq` and from 0; kill on the drop of the last handle, on `stop` and on `destroy`; 16 processes at once (pipes and PTYs); the sentinel secret with and without a PTY; `cwd`, a missing one and a link out |
| `transfers_conform` | With `chunked_transfer` and `upload`: 0 B, 1 B and 64 MiB round trips (random resends and overlaps, refused gaps, a duplicate end); resume from `upload_committed_offset`; commit before all bytes; too large; wrong hash; `if_match` at begin, at commit and on a missing file; modes; a link at the final component; a folder link out (upload, new folders, download); a reader never sees a part; abort (twice), `stop` and expiry leave no file; `Changed` after a replace by rename |

Runs: `host`, `host` with `confineReads`, the process and transfer suites alone on `host` (`host_processes_conform`, `host_transfers_conform`, with a check that no process of a host session outlives its leader), the tool lock's download rule, the drop of a host Workspace, the env policy over `host`, `seatbelt` (macOS; `#[ignore]` elsewhere), a lying driver that the kit must reject, and `podman` (`#[ignore]`; run on Linux with `LOOP_WORKSPACE_KIT_PODMAN=1 … -- --ignored`).

## 14. Migration

Order: **C2 and C2b in parallel → C4 → C3 → C5.**

| Task | Work | Files it owns |
|---|---|---|
| **C2** Remove host-path uses in tools | `tools/mod.rs` ~229 and `documents/mod.rs` ~171, ~182 (the reader child gets `env.cwd()` as `workspace`; `documents/child.rs` uses it only to refuse a reader binary inside the Workspace; pass `None` for a guest driver), `tools/mod.rs` ~431 (`through_link`: keep through `env`), ~633 (relative `cwd` join: keep in the namespace), `search.rs` ~308 (results are made relative to `cwd`; they go back through `env`). Context files (`system_prompt.rs`) and skills (`skills.rs`, `resources/mod.rs`) through `env()` | `crates/loop-agent/src/harness/tools/**`, `crates/loop-app-core/src/system_prompt.rs`, `crates/loop-agent/src/harness/skills.rs`, `crates/loop-app-core/src/resources/**` |
| **C2b** Gate path resolution through the env (**done**) | `permissions::paths::GatePaths` over the Session env; three-state `resolve` with fail-closed deny and ask sites and decision limits; `PathMetadata::mode` added; a guest driver stays refused with path rules until the list in section 6 is done. Open: an async search filter (C2), Seatbelt and Krun envs forward the gate primitives (C4), subagent rule bases for guest Sessions (`loop-runtime` `subagent.rs` sets host paths), the diff editor for guest snapshots | `crates/loop-app-core/src/permissions/**`, the path code of `tool_gate.rs` and `tool_approval.rs`, the `path_policy` refusal in `sandbox_builder.rs` |
| **C4** Local drivers on the trait (first slice done; see below) | Owns `loop-workspace`. Implement `spawn`, PTY, process attach and upload sessions for `host`, `seatbelt`, `podman`; host transfer goes through the `confine_reads` check. The Session env wrapper for `spawn`. Forward the gate primitives in the Krun and Seatbelt envs. The `worktree` driver. Route "off" and the tool lock through the `host` driver. `WorkspaceResources::sandbox` (`runtime.rs`) to `Arc<dyn Workspace>`. `sandbox.driver` (Q1) | `crates/loop-workspace/**`, `crates/loop-agent/src/harness/sandbox/**`, `env/**`, `crates/loop-app-core/src/sandbox_builder.rs`, the sandbox part of `session_factory.rs`, `config/settings.rs` (sandbox keys) |
| **C3** Hooks and MCP stdio in the Workspace | Project hooks (`hook_command.rs` ~63) and project MCP stdio (`loop-mcp/src/client.rs` ~122) start through `Workspace::spawn`. Global hooks follow section 6.1 | `crates/loop-app-core/src/hook_command.rs`, `hooks_load.rs`, `crates/loop-mcp/**` |
| **C5** Reference remote driver | `ExecutionEnv` and `Workspace` over a socket, plus a small agent binary in the sandbox; `attach` with epochs; `mode = remote`; the kit in CI with a local container as the "remote" | New crates `crates/extend/loop-workspace-remote` and `…-agent`; `config/settings.rs` (mode value, after C4) |

A protocol task (not in this list) adds file and terminal methods with `WorkspacePath`.

**C4 status.**

| Item | Driver | State |
|---|---|---|
| `spawn` with and without PTY, `attach_process`, `list_processes` | `host` | Done (C4, first slice; Unix) |
| `read_chunk`, upload sessions | `host` | Done (C4, first slice; Unix) |
| Tool lock: no process, no upload; downloads by the lock's rule | `host` | Done |
| `path_namespace()` on `FileSystem`; the document reader passes no root for a guest env | contract | Done (the edit-diff path `text_of` still gets a guest root: C2 owns `tools/mod.rs`) |
| Gate primitives in the Seatbelt and Krun envs | `seatbelt`, `podman` | Done (the gate still uses the host env for host-namespace Sessions) |
| `list_dir` keeps going past one bad entry | `host` | Done |
| Sandbox-off Sessions through the `host` driver | app | Done (`WorkspaceResources::host_workspace`) |
| `spawn`, PTY, transfer | `seatbelt`, `podman` | Not done: they return `Unsupported` and the kit checks that |
| Windows | `host` | Not done: no streaming or transfer capability |

**C4 follow-ups.**

| Name | Work |
|---|---|
| C4a | `spawn` and PTY for `seatbelt` (in the profile, with the `process_tree` descendants for the kill) and `podman` (`podman exec -it`, kill in the container by marker) |
| C4b | Transfer for `seatbelt` (through the jailed fs) and `podman` (host side for the bind mount; guest side for full isolation) |
| C4c | The `worktree` driver |
| C4d | The Session wrapper for `spawn`: the tool env policy (`removed_tool_env_names`, `tools.passEnv`) on `SpawnRequest`, as `SessionExecutionEnv` does for `exec` |
| C4e | `sandbox.driver` (Q1) in global settings |
| C4f | Done: `WorkspaceState::sandbox` in `loop-runtime` is an `Arc<dyn Workspace>` (with `AgentHarness::ensure_sandbox_ready`); the Runtime ends host processes at shutdown, on the explicit close of the last Session of a folder, and on reload (unload pins a Session that has a running process) (`tests/host_process_cleanup.rs`) |
| C4g | Process limits as settings, and the hosted-beta resource limits below |
| C4h | Open. macOS has no parent-death signal: watch the Runtime with kqueue `NOTE_EXIT` from a small helper, so leaders die with a Runtime that was killed (Linux has `PR_SET_PDEATHSIG`). Until then a Runtime that is killed (not stopped) leaves its host processes on macOS |

The shutdown part of C4f is in place, as a protocol method that exposes `spawn` needs.

**Resource limits for the hosted beta** (review recommendation; not built yet):

| Limit | Why |
|---|---|
| A total ring budget per Runtime (all processes of all Workspaces) | 64 processes × 16 MiB per Workspace is possible today |
| A maximum lifetime for detached processes, or the pause rule above made mandatory | A detached process can run without end |
| `RLIMIT_NPROC`, cgroup `pids.max` and a memory limit per user container | A fork bomb or a large build must not reach other users |
| A disk quota per Workspace | 16 open uploads × 4 GiB is possible |

## 15. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Add `sandbox.driver` to choose a driver id? | Yes, in C4, global settings only |
| Q2 | Refuse a driver whose capabilities do not meet the settings? | Done for kinds other than `local` and for `guest` paths with the tool lock or path rules (the path-rule refusal goes when the list in section 6 is done). For `local`, show the gap in `/sandbox status` (for example Linux network) and do not refuse: local users see no change. **Owner decision:** should `local` refuse too, later? |
| Q3 | Should `FileSystem` take `WorkspacePath`? | No. Keep `&Path` in the driver namespace, with the `path_namespace` capability and the rule "checks go through `env()`". Use `WorkspacePath` at the protocol edge, in `SpawnRequest` and in transfer |
| Q4 | Project MCP servers in the Workspace? | Yes; servers that need credentials become Connectors |
| Q5 | Global registry or injection? | Both |
| Q6 | Memory snapshots? | Files only for now |
| Q7 | Remote transport? | The cell connects to the sandbox agent in the cluster; `loop-wire` framing. Separate from the ADR 0008 control link |
| Q8 | How does an external driver replace a built-in one? | Only with `replace_builtin_workspace_driver`, which is logged. A plain registration of a built-in id is refused |
| Q9 | **Owner decision:** in hosted mode, disable global hooks, or run them in the Workspace? | Disable them first; run them in the Workspace after C3 |
| Q10 | **Owner decision:** the document reader for guest drivers: in the Workspace, or a separate jail on the cell? | In the Workspace (no new isolation to build) |
