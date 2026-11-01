# S2 · Runtime, Event Log and first Subagents: design

Status: Approved 2026-11-01. This document describes S2 behavior to build.
Current-code claims were checked at
`f5625a0422539ed8e8d110f75383e3471ccf9b6e` on `docs/runtime-design`.
The approved S2 plan is the scope; this is not a report that S2 is built.

Goal: one **Runtime** keeps many **Sessions** working after a **Surface**
disconnects. `loop serve` owns the Runtime. `loop attach` watches a Session,
sends prompts and answers an **Escalation**. The **Event Log** records activity
for views, replay and audit. The first **Subagent** runs as a child Session.

## 1. Scope and current foundations

S2 supports Linux and macOS. Windows reports that `serve` and `attach` are
unsupported. Unix socket code is behind `cfg(unix)`.

Not in S2:
- the TUI as a Runtime client, `--connect`, and URL or QR remote control (S3);
- ACP Sessions inside the Runtime (S3);
- model control over the wire (S3);
- one Policy gate across all execution paths (S4);
- the Task Board, Mailboxes and coordination MCP (Phase 2);
- cloud hosting and Postgres;
- changes to compaction.

The WebSocket listener is S2.11 and may slip to S3. S2's required client uses
the local Unix socket.

| Piece | Current code | Consequence for S2 |
|---|---|---|
| Application bootstrap | `crates/loop-app-core/src/runtime.rs`, `bootstrap`, `resolve_trust` | Builds one harness, gate, backend and workspace. Interactive trust can read stdin. It also sets `LOOP_CODING_AGENT` in the process environment. |
| Native backend | `crates/loop-agent/src/native_backend.rs`, `StreamState`, `check_session` | One active Session per harness, with its own stream sequence and prompt slot. Use one harness and backend per live Session. |
| Turn persistence | `crates/loop-agent/src/harness/agent_harness.rs`, `run_turn` | Pending tree writes flush after the turn. Cancellation discards those writes. The tree cannot supply a complete live audit trail. |
| Session storage | `crates/loop-agent/src/harness/session/sqlite/store.rs`, `open`, `append_entry` | Fresh connections have a 5 s busy timeout. Appends use deferred transactions with reads before writes. There is no Session lease. |
| Wire server | `crates/core/loop-wire/src/server.rs`, `run`, `forward` | One connection per `serve()` call; no Session filter. The shutdown RPC calls `backend.shutdown()`. |
| Socket | `crates/core/loop-wire/src/socket.rs`, `unix_listen` | Removes the socket path before binding. A Runtime needs a lock and live probe before reclamation. |
| CLI | `crates/loop-cli/src/main.rs`, `real_main` | Commands need an early dispatch path. Otherwise execution can reach `bootstrap_cli`. |

Terminology follows the task: Runtime, Session, Event Log, Surface,
Escalation, Capability, Policy, Lock and Subagent. A Policy caps a Capability;
a Lock prevents lower layers from changing its setting. This is distinct
from the OS file lock used for single-instance startup. The implementation
of these Policy rules is `crates/loop-app-core/src/permissions/layers.rs`.
There are no `docs/loop-vision*`, glossary or ADR files in this checkout.
ADR 0003 and ADR 0004 are references from the approved plan and code comments,
not independently inspected documents.

## 2. Three resource tiers

The new crate is `crates/loop-runtime`. Its owner type is
`loop_runtime::Runtime`. The existing `loop_app_core::Runtime` keeps its
name and gains a comment identifying it as the application bootstrap result.
It is currently defined in `crates/loop-app-core/src/runtime.rs`.

```
loop_runtime::Runtime
├── GlobalResources
├── workspaces: cwd → WorkspaceResources
├── sessions: SessionKey → LiveSession
├── EventLog + generation + limits
└── SessionFactory → {harness, native, tool_gate, approval_bridge}
```

| Tier | Owns | Extraction from today's `bootstrap` |
|---|---|---|
| `GlobalResources` | Credentials, `Models`, catalog refresh, Session store and repository | `FileCredentialStore`, `build_models`, cached/background refresh, `create_sqlite_session_store`, `create_session_repository`. No stdin. |
| `WorkspaceResources`, cached per cwd | Trust decision, effective settings, resources and hook definitions, prompt inputs, `HostExecutionEnv`, web tools, MCP connections and lazy sandbox | `load_settings`, `load_resources`, context/prompt loading, `build_system_prompt`, execution environments, `WebTools`, MCP setup. |
| `SessionFactory::open(...)` | A Session, harness, `NativeBackend`, gate, approval bridge, per-Session hook registration and grants | Repository open/create, `AgentHarness::new`, harness configuration, `NativeBackend::new`, gate installation and approver wiring. |

Pass an absolute cwd throughout. Cache by a normalized workspace path; do
not call `set_current_dir` to switch workspaces. The factory sets the gate's
workspace explicitly. `ToolGate::install` already derives it from the
harness, but `ToolGate::workspace` still has a process-cwd fallback
(`crates/loop-app-core/src/tool_gate.rs`). Non-tool hook commands also fall
back to process cwd in `crates/loop-app-core/src/hooks_load.rs`, `run_command`.
They need the owning Session's cwd supplied during registration.

Share definitions and connections, not mutable Session state. Each harness
gets its own hook registry and selected tools. Prompt construction must
still reflect the tools actually offered to that Session. Model selection,
thinking level, queues, cancellation, Policy and grants stay per Session.
The `Models` injection seam lets tests use the faux provider.

`bootstrap` becomes calls into these tiers, with its existing interactive
adapter. S2.3 preserves standalone behavior. The daemon's non-interactive
trust and sandbox rules are applied by `serve`, not silently imposed on
the existing TUI bootstrap.

## 3. Runtime routing and limits

The Runtime implements `AgentBackend` from
`crates/core/loop-protocol/src/backend.rs`. It routes by
`SessionRef.instance`, then `SessionRef.key`, and validates `native_id`.
Native Sessions use instance `loop`, as `NATIVE_INSTANCE` does today.
This leaves a route for later ACP backends without adding them in S2.

- `new_session` and `load_session` reserve capacity, acquire the lease and
  build through the factory. Failure releases both lease and reservation.
- `prompt`, `cancel`, `is_busy` and `transcript` address the Session's own
  backend. Prompts and cancellation requests are recorded in the Event Log.
  A transcript query remains a read; it does not create conversation history.
- `list()` reports known Sessions and whether each is live and busy.
- `close(key)` refuses a busy Session unless forced. Forced close cancels
  and settles work, drains its pump, logs closure and releases the lease.
  Closing unloads resources; it does not delete history.
- `shutdown()` stops admission, cancels work, drains and flushes, releases
  leases and stops workspace resources.

The default cap is 16 live Sessions, including children. `--max-sessions`
sets the cap. Opening beyond it returns `BackendError::Busy`. No idle
unloading is added. A separate turn semaphore defaults to 8 concurrent
active turns, configurable with `loop serve --max-active-turns`. Saturation
queues the turn until a permit is available; it does not return Busy. The
turn count is not the live-Session count. A Session still accepts only one
active turn. A parent blocked inside the `subagent` tool releases its permit
while waiting and re-acquires it before continuing.

Each live Session has a pump subscribed before any prompt can run:

```
NativeBackend → drop Origin::Replay → stamp durable seq + generation
              → append batch and await commit → tail buffer → broadcast
```

The pump preserves Session order. It replaces the backend's transient seq
and generation; `translate` currently starts seq in memory and uses
generation 0 (`crates/loop-agent/src/native_backend.rs`). Backend history
replay never becomes new Event Log activity. Runtime replay reads the log.
The tail buffer is a cache, not the authority. A failed log write stops
publication and fails the affected work visibly. Broadcast lag at the
backend-to-pump boundary must also fail visibly; that missing activity
cannot be recovered from a log that never received it.

Keep wire prompt completion ordered after the committed and forwarded
`turn_ended`. Today's forwarder already coordinates the outcome and end
update (`crates/core/loop-wire/src/server.rs`, `Waiting`, `forward`).

## 4. Event Log

Use `<agent_dir>/events.db`, separate from the Session tree in `sessions.db`.
`agent_dir` honors `LOOP_CODING_AGENT_DIR`. The Session database also honors
`LOOP_CODING_AGENT_SESSION_DIR`; that override does not relocate `events.db`
(`crates/loop-app-core/src/config/paths.rs`).

The proposed storage shape is:

```sql
CREATE TABLE events (
  global_seq INTEGER PRIMARY KEY,
  generation INTEGER NOT NULL,
  session_key TEXT,
  session_seq INTEGER,
  kind TEXT NOT NULL,
  turn TEXT,
  ts_ms INTEGER NOT NULL,
  payload TEXT NOT NULL
);
CREATE UNIQUE INDEX events_session_seq
  ON events(session_key, session_seq) WHERE session_seq IS NOT NULL;
CREATE TABLE runtime_generations (
  generation INTEGER PRIMARY KEY,
  owner TEXT NOT NULL,
  pid INTEGER NOT NULL,
  started_ms INTEGER NOT NULL,
  stopped_ms INTEGER
);
```

`runtime_generations` columns beyond generation are a proposed bookkeeping
shape. Allocate generations transactionally. Every record has `global_seq`
for log-wide order. Runtime-only records have null Session columns. Session
records have a key, but only `Update(UpdateEnvelope)` records have a
`session_seq`. It counts wire updates and is contiguous per Session.
Non-update records, including prompts, Escalations and Session lifecycle,
have null `session_seq` and are ordered by `global_seq`. The partial unique
index constrains only non-null Session sequences. Sequences never reset
when a Session is reloaded or the Runtime restarts. Payload is versioned
JSON with lenient handling of unknown kinds.

| Kind | Records |
|---|---|
| `Update(UpdateEnvelope)` | Every live update, including streamed text and tool activity |
| `PromptSent`, `Cancelled` | Input and cancellation intent, with Session and turn correlation when known |
| `EscalationRaised`, `EscalationAnswered`, `EscalationWithdrawn` | Request, winning answer, grants and withdrawal reason |
| `SessionOpened`, `SessionClosed` | Session lifecycle, cwd and parent linkage where applicable |
| `RuntimeStarted`, `RuntimeStopped` | Runtime lifecycle and generation |
| `Unknown` | Future records retained without inventing their meaning |

The `EventLog` trait has `append_batch`, `read_after(session, after_seq)`,
`read_runtime_after` and `latest_seq`. `read_after(session, after_seq)` reads
update records by `session_seq`; `latest_seq` is the last update
sequence, or 0 when there are no updates. Audit and recovery reads use
`global_seq`, including records whose `session_seq` is null.
A single writer batches streamed records in bounded batches and IMMEDIATE
transactions. Use WAL and
`synchronous=NORMAL`; configure these on the actual writer connection.
Flush lifecycle and Escalation decisions before exposing their result.
Do not merge text chunks in ways that lose their order or identity.
Batch size and maximum delay are implementation choices to pin in tests.

Reuse the typed-event and memory-log shape in
`crates/loop-orchestration/src/workflow/event_log.rs` and
`sqlite_event_log.rs`, not that implementation's exact persistence behavior.
Its writer uses a mutex and individual statements, not the new batch writer.
Reuse the `include_str!` migration registry from
`crates/loop-agent/src/harness/session/sqlite/store.rs`.

Both stores remain written. The Session tree is model context, including
branch selection and compaction. The Event Log supplies views, replay and
audit, including turns whose pending tree writes were discarded on cancel.
The later direction is to make the tree a projection of the log. S2 does
not implement that change or a transaction across both databases.

“No gaps” means every committed wire update after the supplied cursor is
replayed in contiguous `session_seq` order. Non-update audit records do not
consume that cursor. Replayed delivery keeps the original update sequence
and does not allocate another one.

## 5. Session leases and concurrent writes

Add `003_session_leases.sql` beside the two existing Session migrations:

```sql
CREATE TABLE session_leases (
  session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  owner TEXT NOT NULL,
  pid INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  heartbeat_ms INTEGER NOT NULL
);
```

`owner` identifies the owner kind; Runtime leases use `owner = "runtime"`.
Fence renewals, releases and writes by the full owner, pid and generation
identity, not the owner label alone. Standalone writers and the Runtime
use the same acquire/renew/release contract. Read-only listing does not
take a lease. Acquire happens before a harness can write a Session.

| Operation | Rule inside an IMMEDIATE transaction |
|---|---|
| Acquire | Insert when absent; refuse another live owner |
| Renew | Every 5 s, update heartbeat only for the matching owner, pid and generation |
| Release | Delete only the matching owner, pid and generation row |
| Runtime startup reclaim | Holding the associated `runtime.lock` proves the previous Runtime is dead; reclaim its `owner = "runtime"` leases immediately |
| Other takeover | Replace only when heartbeat is at least 30 s old **and** the recorded pid is dead; recheck while holding the transaction |

A live owner is never taken over. Runtime startup reclamation applies only
to leases in the acquired file lock's ownership domain. Other takeover
requires both a stale heartbeat and a dead pid; unknown pid liveness is not
proof of death. A failed renewal stops new writes and cancels work; an old
owner cannot keep writing after ownership is lost. Check leases on explicit
`--resume` and on the Session later selected by `--continue` or the resume
picker.

For example, the Runtime is killed by OOM. With no Runtime restart,
`loop --resume X` takes over once the last heartbeat is 30 s old and the
recorded Runtime pid is dead. Before that threshold it refuses. A restarting
Runtime that acquires the associated `runtime.lock` can reclaim its Runtime
leases immediately, without waiting 30 s.

The refusal should be actionable:

```text
Session <id> is in use by Runtime <pid>. Use `loop attach <id>` instead.
```

Change `create`, `append_entry` and `delete` from `.transaction()` to
`TransactionBehavior::Immediate`. This moves writer contention before the
read and avoids the deferred read-to-write upgrade race. The existing busy
timeout still bounds waiting; IMMEDIATE does not guarantee no error under
arbitrary contention.

`fork` needs special care. Today it loads history, calls `create`, appends
each entry separately, then updates the parent id outside those transactions
(`crates/loop-agent/src/harness/session/sqlite/store.rs`, `fork`). There is
no single fork transaction to switch. In S2.1, fork becomes one IMMEDIATE
transaction: read the source snapshot, create the destination, copy entries
and set the parent id. It commits all of that or rolls it all back.

## 6. Protocol additions

These are additive changes. `PROTOCOL_VERSION` stays 1. Advertise new
`Capabilities.multi_session` and `Capabilities.attach`, defaulting to false
when absent. Existing fields use that pattern in
`crates/core/loop-protocol/src/capabilities.rs`. Keep old v1 clients working.
An unknown method still returns `-32601`.

| Method | Direction | Kind | Params → result |
|---|---|---|---|
| `session/list` | c → s | request | `{}` → `[SessionInfo{ref,cwd,name,parent,live,busy,created_ms,last_active_ms}]` |
| `session/close` | c → s | request | `{session,force?}` → `{}`; force defaults false |
| `session/attach` | c → s | request | `{session,after_seq?}` → `{latest_seq,generation}`; cursor defaults 0 |
| `session/detach` | c → s | request | `{session}` → `{}` |
| `host/interact` | s → c | request | Existing `InteractionRequest` plus optional `session` → existing `InteractionOutcome` |
| `host/interact_withdrawn` | s → c | notification | `{session,id}` |

`session` is a full `SessionRef`, not a display title. Its existing fields
are `instance`, `key` and `native_id`
(`crates/core/loop-protocol/src/backend.rs`). Examples of proposed messages:

```json
{"jsonrpc":"2.0","id":10,"method":"session/list","params":{}}
{"jsonrpc":"2.0","id":10,"result":[{"ref":{"instance":"loop","key":"s1","native_id":"s1"},"cwd":"/work/repo","name":"Review","parent":null,"live":true,"busy":true,"created_ms":1793400000000,"last_active_ms":1793400001000}]}
{"jsonrpc":"2.0","id":11,"method":"session/attach","params":{"session":{"instance":"loop","key":"s1","native_id":"s1"},"after_seq":40}}
{"jsonrpc":"2.0","id":11,"result":{"latest_seq":42,"generation":3}}
{"jsonrpc":"2.0","method":"session/update","params":{"v":1,"instance":"loop","generation":3,"session":"s1","turn":"t1","seq":41,"origin":"replay","update":{"type":"message_started","message_id":"m1","role":"agent"}}}
{"jsonrpc":"2.0","method":"session/update","params":{"v":1,"instance":"loop","generation":3,"session":"s1","turn":"t1","seq":42,"origin":"replay","update":{"type":"message_chunk","message_id":"m1","role":"agent","content":{"type":"text","text":"Checking files."}}}}
{"jsonrpc":"2.0","id":12,"method":"session/detach","params":{"session":{"instance":"loop","key":"s1","native_id":"s1"}}}
{"jsonrpc":"2.0","id":12,"result":{}}
{"jsonrpc":"2.0","id":13,"method":"session/close","params":{"session":{"instance":"loop","key":"s1","native_id":"s1"},"force":true}}
{"jsonrpc":"2.0","id":13,"result":{}}
```

The response to attach precedes its replay, followed by live updates.
`latest_seq` is the snapshot's last update `session_seq`, not its last
`global_seq` or the last seq the client has rendered. A client persists only
its own processed update cursor. `SessionInfo.parent` identifies a parent
Session when present; `last_active_ms` supplies the list's last activity.

```json
{"jsonrpc":"2.0","id":90,"method":"host/interact","params":{"kind":"permission","id":"approval-7","session":{"instance":"loop","key":"s1","native_id":"s1"},"options":[{"native_id":"accept","label":"Allow once","kind_hint":"allow_once"},{"native_id":"reject","label":"Reject","kind_hint":"reject_once"}]}}
{"jsonrpc":"2.0","id":90,"result":{"outcome":"selected","native_id":"accept"}}
{"jsonrpc":"2.0","method":"host/interact_withdrawn","params":{"session":{"instance":"loop","key":"s1","native_id":"s1"},"id":"approval-7"}}
```

The last notification goes to other clients, or all pending recipients on
cancel. JSON-RPC request ids are per connection; the interaction id names
the shared Escalation. Add `session` to every relevant request variant in
`crates/core/loop-protocol/src/interaction.rs`, with a default for old peers.

Introduce a `RuntimeControl` trait at the wire boundary for list, close,
attach, detach and connection registration. `loop-wire` must not depend on
`loop-runtime`. One connection tracks only Sessions it created, loaded or
attached. Listing does not subscribe to every Session. Detach removes that
subscription and its interaction recipient; it does not cancel the turn.
The registration path must capture updates emitted during create/load.

Honor `ClientCapabilities.interact`: observers receive updates, but no
requests to answer. Today's server parses that capability but does not use
it (`crates/core/loop-wire/src/server.rs`, `run`).

Add `ServeOptions { owns_backend }`. Runtime connections use false:
`shutdown` returns `{}` and ends that connection only. Tests and in-process
owners can use true to retain backend shutdown. Losing a connection also
leaves Runtime work running; unresolved remote prompt handles remain
uncertain, as the existing contract requires.

S2.5 owns `rpc.rs` method constants and `json_schema()`, `interaction.rs`,
`capabilities.rs`, regenerated `schema/loop-protocol.json`, wire goldens in
`tests/rpc_wire.rs`, and server/client methods. `RemoteBackend` gains
`list`, `attach`, `detach` and `close`.

## 7. Attach without gaps or duplicates

Use one ordered sender per connection and one cursor per attached Session.
The cursor is a durable, contiguous update `session_seq`, independent of
generation. Non-update records are not part of attach replay; they remain
available for audit and recovery through `global_seq`.

```mermaid
sequenceDiagram
    participant C as Surface
    participant S as Connection forwarder
    participant R as Runtime live stream
    participant L as Event Log
    C->>S: session/attach(after_seq = c)
    S->>R: Subscribe and buffer live updates
    S->>L: Read updates after c through update high-water H
    L-->>S: Ordered updates, latest_seq H
    S-->>C: Result {latest_seq: H, generation: G}
    S-->>C: Replay updates after c through H
    Note over S: Advance last_sent; discard buffered seq <= last_sent
    R-->>S: Committed live updates
    S-->>C: Forward only seq > last_sent
```

1. Register the live subscription before reading history. Buffer while
   replay is sent; never interleave live messages ahead of replay.
2. Read updates after the supplied `session_seq` through a fixed update
   high-water mark H (`latest_seq`). Page large histories without moving
   that boundary. Non-update records do not advance H.
3. Send replay in contiguous update sequence order. Keep `last_sent` for
   that Session, initialized to the supplied cursor.
4. Drain the buffered live stream in order, dropping seqs at or below
   `last_sent`. Continue with live delivery through the same sender.
5. On lag, refill from the Event Log after `last_sent`, or close with an
   explicit notice and let the client reattach. Never silently skip.

An update committed before subscription is in the snapshot. A later update
is in the subscription, the snapshot, or both. Commit-before-broadcast and
the fixed replay boundary ensure coverage. Dedupe removes the overlap.
Pending Escalations are also reconciled against current Runtime state;
an old raised audit event must not reopen an answered request.

On restart the database retains Session seqs and allocates a higher
generation. Reattach reads after the client's update cursor, including
committed updates never delivered before the crash. Historical stored generations
remain audit data. Replay delivery uses `origin: replay` and the current
generation, so readers that discard older generations still see history.
Do not append these delivery envelopes back to the log.

This guarantees no duplicate seqs within an attachment and replay after a
processed cursor. It does not guarantee exactly-once terminal display if
the client crashes between rendering text and saving its cursor. Nor does
it recover an uncommitted batch or prove a crashed tool did not run.
WAL with NORMAL synchronization is not a promise against power-loss data
loss. Recovery records uncertainty instead of retrying prompts.

## 8. Escalation routing

Each Session gets an `EscalationHost` through
`ToolApprovalBridge::with_host` and `ToolGate::set_approver`
(`crates/loop-app-core/src/tool_approval.rs` and `tool_gate.rs`). The host
keeps pending Escalations and one answer channel per request. Reuse the
request/oneshot shape of `InteractionQueue` in
`crates/loop-app-core/src/interaction.rs`; today that queue targets one
Surface and rejects when it has no receiver. Parking is new Runtime behavior.

- Log the raised Escalation before dispatch. If no interactive client is
  attached, park it. The turn waits with no default timeout.
- Send it to every interactive client attached to that Session. A newly
  attached client receives still-pending requests after replay setup.
- Validate the native option id and atomically claim the first valid
  answer. Log it before resuming the tool. Other answers have no effect.
- Send `host/interact_withdrawn` to the other recipients and resolve their
  pending request bookkeeping. Withdrawal is not a second tool decision.
- Cancel withdraws pending Escalations and unblocks their waiting tasks.
  Disconnect removes a recipient; it does not choose rejection for everyone.
- Record Session grants, including their scope, in the answered event.
  Grants stay per Session and do not bypass Policy or a Lock. They are not
  automatically restored as permissions after a Runtime crash.

Routing and cancel must share one decision state so an answer/cancel race
has one winner. Reusing process-local `approval-N` ids after restart must
not let a stale answer resolve a new request; include generation in identity
or reject responses outside their original connection/request context.

## 9. `loop serve` lifecycle

Add `Commands::Serve { socket, max_sessions, max_active_turns,
permission_mode }` and dispatch it before interactive bootstrap in `crates/loop-cli/src/main.rs`.
It never reads stdin or starts a TUI. The invoking process owns the service;
backgrounding it does not require an additional daemon-fork protocol.

1. Resolve agent paths. Default socket: `~/.loop/agent/run/runtime.sock`.
   Add `run_dir()` and `runtime_socket_path()` to
   `crates/loop-app-core/src/config/paths.rs`. Respect `--socket`. When the
   default exceeds the platform's Unix socket path limit, use
   `/tmp/loop-<uid>/runtime.sock`. An explicit invalid path gets an error.
2. Create/verify the runtime directory with mode 0700 and current ownership;
   create the socket with mode 0600. Check an existing fallback directory
   too. Today's `unix_listen` only sets 0700 when creating the directory.
3. Hold `flock` on `runtime.lock` for the service lifetime and write a pid
   file. Keep the lock identity tied to the agent directory even when the
   socket path changes. A pid file is diagnostic, not the ownership proof.
4. Probe an existing socket. A live Runtime produces an “already running”
   error. Reclaim only a socket proven stale while holding the lock. An
   ambiguous probe failure is an error, not permission to unlink. Replace
   the unconditional unlink in `crates/core/loop-wire/src/socket.rs`.
5. Initialize daemon tracing to `<agent_dir>/logs/runtime.log`. Extend
   `crates/loop-cli/src/debug_log.rs`, `init_tracing`; its current file mode
   writes under the cwd's `target/debug/logs`.
6. Open stores, bump generation and recover (§12), then accept connections.
   Each gets its own `serve()` call with `owns_backend: false`.
7. SIGINT or SIGTERM stops admission and triggers Runtime shutdown. Flush
   the Event Log, release leases, stop sandboxes, remove owned socket/pid
   files and release the file lock. Do not unlink the lock inode while held.

Permission modes use per-workspace settings. `full-access` requires the
explicit `loop serve --permission-mode full-access` flag; a settings value
alone falls back to `ask` with a notice. Keep deny rules, hooks and Policy
ceilings. This extends the existing non-interactive rule in
`crates/loop-app-core/src/runtime.rs`, `startup_permission_mode`.

A folder without recorded trust gets global settings and a notice, with no
prompt. Do not implicitly trust it through `default_project_trust: always`.
Today's `resolve_trust` can do that; the daemon needs the stricter rule.
Do not load project hooks or executable resources as trusted input.

The sandbox belongs to the cwd and starts lazily. Sessions in that cwd
share it; different cwds do not. Refuse a Session if its required sandbox
fails. Today's bootstrap starts it eagerly and falls back to host execution
on failure; preserve that standalone behavior during S2.3 extraction.

## 10. `loop attach` UX

`loop attach` connects to the local Runtime using `RemoteBackend`; it does
not bootstrap an agent. With no id, list folder, title, busy state and last
activity. With `<id|prefix>`, require an unambiguous match, attach from the
chosen cursor, replay output and follow live updates. Show parent links for
children. Reuse the text rendering in `crates/loop-cli/src/print_mode.rs`.

- Show parked and new approvals as numbered native choices. Keep prompt
  entry distinct from approval answers when both are available.
- A typed prompt line starts a turn on that Session. Report busy explicitly.
- Ctrl+C cancels the current turn. A second Ctrl+C, or Ctrl+D, detaches.
  Detaching and connection loss do not stop the agent.
- Reattach after the last processed cursor to continue the view. A fresh
  view can replay from zero. Explain connection loss and uncertain outcomes.

`loop attach --new [--cwd DIR]` creates a Session through `session/new`,
then attaches to it. Without `--cwd`, use the invoking client's cwd. No-id
listing and Session creation are distinct modes. The list uses
`SessionInfo.parent` and `last_active_ms` for child links and last activity.

## 11. `subagent` tool

Offer `subagent {task, cwd?, max_turns?}` only inside the Runtime. The
standalone TUI does not expose it before S3. It opens a child Session,
defaults cwd to the parent and submits the task there. The parent receives
the child's final answer, or an explicit failure/cancellation result.

Derive the child Policy with `parent.policy().with(tightening_layer)` from
`crates/loop-app-core/src/permissions/layers.rs`. `Policy::with` returns
the new Policy and notices. Set it on the child's gate. A child cannot gain
a Capability the parent lacks; preserve any Lock and apply additional
workspace restrictions when cwd differs. Do not inherit grants as a way
to loosen that ceiling. Policy inheritance does not mean the single S4
Policy gate is already complete.

Default depth limit: 2, with the root at depth 0. Children at depth 2 may
work but cannot spawn another child. Several calls can run in parallel
within Session and turn limits. Cancelling a parent cascades through its
descendants, including parked Escalations. While blocked inside the tool,
the parent releases its active-turn permit and re-acquires it before
continuing (§3). `max_turns` counts model calls in the child, not tool calls.
It defaults to no limit; a parent may set it for bounded jobs. Children
remain visible and closeable in `loop attach`. Once the parent has the final answer, the finished child
closes automatically and releases its live-Session slot. Its log stays.

Persist the parent id and log child lifecycle. The existing
`SessionMetadata.parent_session_id` and SQLite `sessions` column already
support linkage (`crates/loop-agent/src/harness/session/types.rs` and
`sqlite/migrations/001_initial.sql`). Creation currently sets it to null;
only `fork` sets it later. A fresh child creation path is needed. A Subagent
is not implicitly a fork of all parent context. Route child Escalations
through the same Runtime machinery and expose children in the attach list.

## 12. Crash recovery

Before accepting clients on start:

1. Allocate and persist a new generation and `RuntimeStarted`.
2. Read committed activity. For each `turn_started` without `turn_ended`,
   append the existing turn-end update with reason `runtime_restarted`
   (`stop: "runtime_restarted"` on the wire),
   displayed as “uncertain — Runtime restarted”.
   Do not rerun its prompt or assume its tools had no effect.
3. With the associated `runtime.lock` held, reclaim the previous Runtime
   leases (`owner = "runtime"`) immediately. Other takeovers require a
   heartbeat at least 30 s old and a dead pid. Never take over a live owner.
   Reopening a Session still goes through the factory and capacity checks;
   recovery need not load all history.
4. Withdraw old pending Escalations and record why. Do not replay a stale
   approval into a new turn or automatically restore a Session grant.
5. Begin normal serving only after recovery records commit. Repeated
   startup must not append a second end for an already recovered turn.

There is no `StopReason::Uncertain` today:
`crates/core/loop-protocol/src/update.rs` has `Error`, while
`backend.rs` has `TurnOutcome::Uncertain` and `rpc.rs` has
`PromptResult.uncertain`. S2 adds the reason value `runtime_restarted` to
the existing turn-end update, pinned in schema and golden tests in
S2.5-types. This is additive; `PROTOCOL_VERSION` remains 1. It denotes an
interrupted, uncertain turn, not successful completion. Historical recovery
does not resolve a handle in a dead client process. The tree may lack an interrupted turn that the log
shows. That is expected until the later projection work.

## 13. Chunks, verification and decisions

| Chunk | Work | Dependencies |
|---|---|---|
| S2.1 | IMMEDIATE writes and atomic fork, migration 003, lease API and resume refusal; optional S2.1b persists model/effort changes | Approved S2.0 |
| S2.2 | `loop-runtime` skeleton, Event Log trait, SQLite writer and memory implementation | Approved S2.0; owns root Cargo member/lock changes |
| S2.3 | Extract global/workspace/factory tiers, preserve standalone bootstrap, inject Models | Approved S2.0; owns app-core `runtime.rs` refactor |
| S2.4 | Router, pumps, limits, close and shutdown ownership | S2.1–S2.3 |
| S2.5 | Protocol types/schema/goldens, including `runtime_restarted` in S2.5-types; RuntimeControl, connection filters and RemoteBackend methods | Types can start after S2.0; server/client need S2.4 |
| S2.6 | EscalationHost, parking, first-answer arbitration, withdrawal and grant events | S2.5 server/client |
| S2.7 | CLI serve, paths, file lock/probe, logging and signals | Paths/lock/logging can accompany S2.4; command wiring needs serving support |
| S2.8 | Plain-text attach client | S2.5–S2.7 |
| S2.9 | Runtime-only Subagent tool, child Policy, depth and cancellation | S2.4 and S2.6; can accompany S2.8 |
| S2.10 | Recovery, binary end-to-end coverage, user docs and live check | S2.8 and S2.9 |
| S2.11 | Optional authenticated WebSocket listener | S2.10; may slip to S3 |

Each implementation chunk is test-first. Characterize bootstrap before
extraction. Test concurrent appends, lease conflicts/takeover, Event Log
round trips, parallel Sessions, Replay dropping, close/release, capacity,
connection shutdown isolation, old-v1 compatibility and schema freshness.
The headline attach test must overlap replay with live production and
restart from a saved cursor, checking contiguous update sequences, coverage
and deduplication. Also test queued turn admission, parent permit release,
atomic fork rollback, immediate Runtime lease reclaim, the 5 s/30 s lease
intervals, child model-call limits and automatic child closure.

Test parked Escalation → attach → answer, two clients racing to answer,
cancel/answer races, grants, child Policy tightening, depth and parent
cancellation. Binary tests use isolated `LOOP_CODING_AGENT_DIR`, the
`loop_test_support::FakeHttpServer` SSE fixture, timeouts and kill-on-drop.
Include `kill -9` then restart; never infer crash safety from clean shutdown.
Retain Lane B parity and the schema freshness test.

The plan requires each implementation commit to pass formatting, workspace
clippy excluding desktop (also with agent/app-core/CLI telemetry enabled),
and workspace tests excluding desktop. App-core public API changes also
require `cargo +nightly check -p loop-desktop`. S2.0 changes docs only.
The later live check uses GLM in Herdr: two Sessions, detach/reattach mid-turn,
a parked approval and two parallel Subagents. These checks have not been
run for this design document.

Decisions (maintainer, 2026-11-01):

- `session_seq` counts only wire updates and is contiguous per Session.
  Every record has `global_seq`. Non-update records have null `session_seq`
  and use `global_seq` order. A partial unique index constrains non-null
  Session sequences. `latest_seq` is the last update's `session_seq`.
- The turn semaphore defaults to 8 concurrent active turns, configurable
  with `loop serve --max-active-turns`. Saturation queues rather than
  returning Busy. A parent blocked inside `subagent` releases its permit
  while waiting and re-acquires it before continuing.
- Leases renew every 5 s and become stale after 30 s. On startup, holding
  the associated `runtime.lock` proves the previous Runtime is dead and
  permits immediate reclaim of its `owner = "runtime"` leases. All other
  takeovers require both a stale heartbeat and dead pid. Never take over a
  live owner. After an OOM kill, `loop --resume X` can take over once the
  last heartbeat is 30 s old and the Runtime pid is dead.
- S2.1 makes fork one IMMEDIATE transaction: create, copy entries and set
  parent, all or nothing.
- Crash recovery uses the existing turn-end update with new reason
  `runtime_restarted`. S2.5-types pins it in schema and golden tests.
- `SessionInfo` includes `parent` and `last_active_ms`.
  `loop attach --new [--cwd DIR]` creates a Session through `session/new`.
- Subagent `max_turns` counts child model calls and defaults to no limit.
  A parent may bound a job. Parent cancel cascades; depth is limited to 2;
  children are visible and closeable in `loop attach`. A finished child
  closes once the parent has its answer, freeing capacity while keeping
  its log.
- The WebSocket listener requires a bearer token even on loopback,
  matching `loop-wire/src/socket.rs`, not the tokenless-loopback HTTP rule
  in `loop-cli/src/mcp_serve.rs`. It stays last and may slip to S3.
