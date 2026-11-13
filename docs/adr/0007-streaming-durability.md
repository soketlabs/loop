# ADR-0007: Event format, durability, the tree projection and migration

Status: accepted (final). Chains A (streaming) and B (storage) implement it.
Code references are to `staging` at `2e05437`.

After `2e05437`, `staging` divided `runtime.rs` into modules with no
behaviour change (`c4bb01d`). The functions keep their names. Where they are
now:

| At `2e05437` | At `c4bb01d` |
|---|---|
| `publish()`, `pump()`, `barrier()` in `runtime.rs:2576-2760` | `crates/loop-runtime/src/runtime/publish.rs:19-160` |
| `attachment()` in `runtime.rs:1929` | `runtime/attach.rs:177` |
| `broadcast::channel(4096)` in `runtime.rs:362` | `runtime.rs:318` |
| `SqliteSessionStore::open` in `runtime.rs:349` | `runtime.rs:305` |
| `commands: mpsc::UnboundedSender<Command>` in `runtime.rs:225` | `runtime.rs:181` |
| `logged_revision()` in `runtime.rs:2083` | `runtime.rs:1144` |

## Context

The project owner made four decisions. This ADR does not reopen them:

1. Text chunks and thought chunks go out live only. The log does not store
   them. Only complete events are durable, and they commit in batches.
2. The Event Log is the one store for truth. The Session tree becomes a
   projection of the log.
3. Each Event carries `principal` and `tenant` ([ADR-0009](0009-identity-fields-from-s4.md)).
4. Storage is behind traits. A Postgres implementation can exist outside this
   repo. Single-node SQLite behaviour for the CLI and TUI must stay correct.

What the code does today:

| Fact | Where |
|---|---|
| `publish()` appends one Event per update, every `MessageChunk` and `ThoughtChunk` included. It makes one `append_batch` call per update. | `crates/loop-runtime/src/runtime.rs:2587-2633` |
| `publish()` keeps a 256-entry tail per Session, then sends on one global `broadcast::channel(4096)`. | `runtime.rs:2623-2630`, `runtime.rs:362` |
| The pump reads the backend through a `broadcast::Receiver`. A lag fails the Session ("pump lost activity"). | `runtime.rs:2715-2717`, `crates/loop-agent/src/native_backend.rs:317` |
| A lagging Surface gets an error notice, and the server closes the connection. | `crates/core/loop-wire/src/server.rs:996-1000`, `server.rs:1259-1275` |
| Attach replays with `log.read_after(key, after_seq)`. The read is not paged. | `runtime.rs:1929-1958` |
| `transcript` does not read the log. It reads `sessions.db` through `harness.session_context()` and `protocol_events::replay()`. | `native_backend.rs:991-998`, `crates/loop-agent/src/protocol_events.rs:347` |
| Two SQLite files: `events.db` (Event Log, `synchronous=NORMAL`) and `sessions.db` (tree and leases, `synchronous=FULL`). | `crates/loop-runtime/src/log/sqlite.rs:39-48`, `crates/loop-agent/src/harness/session/sqlite/store.rs:263` |
| `log::Event { generation, session_key, turn, ts_ms, kind }`. | `crates/loop-runtime/src/log/mod.rs:52` |
| The `EventLog` trait. `From<rusqlite::Error>` is in the trait module. | `log/mod.rs:107`, `log/mod.rs:92` |
| The `SessionReader` and `SessionStore` traits. Leases, control markers and destinations are inherent methods of `SqliteSessionStore`, not trait methods. | `crates/loop-agent/src/harness/session/types.rs:306`, `types.rs:327`, `sqlite/leases.rs:145-308`, `sqlite/store.rs:1301-1441` |
| `Runtime::new` opens `SqliteSessionStore` itself. | `runtime.rs:349` |
| The SQLite writer task coalesces queued appends up to 256 events, with no intended delay. Reads go through the same task. | `log/sqlite.rs:24-33`, `sqlite.rs:153-185` |
| The harness keeps a turn's tree writes in `pending_writes`. It writes them one by one after the turn. A cancelled turn writes nothing. | `crates/loop-agent/src/harness/agent_harness.rs:1432-1450` |
| The native backend holds `TurnEnded` until the tree holds the turn (the D1 fix). | `native_backend.rs:205-252` |
| Recovery marks interrupted turns `runtime_restarted`. It also handles a `prompt_sent` with no `turn_started` (the D2 fix). | `crates/loop-runtime/src/recovery.rs:47-120` |
| The wire update kinds. | `crates/core/loop-protocol/src/update.rs:77-180` |
| Turn ids and message ids (`msg-N`) are process-local counters. They repeat after a restart. | `recovery.rs:3-4`, `protocol_events.rs:319-329` |
| `sessions.db` can be outside the agent directory, and it can serve more than one Runtime over time. `events.db` is always in the agent directory. | `crates/loop-cli/src/runtime_cmd.rs:86-97`, `docs/design/runtime.md` §4 |
| Embedded owners (the standalone `loop` path) write `sessions.db` only. They have no Event Log. | `docs/probes/e1-results.md` |

## Decision summary

1. **Live only:** text chunks, thought chunks, partial tool output and
   "tool call preparing". The Runtime sends them as **deltas** on a new
   notification. A delta has no `seq` of its own.
2. **Durable:** everything else. `message_ended` carries the complete text and
   thought. The last `tool_call_update` of a call is a full snapshot.
3. **Tree entries are their own log records** (`tree`). They hold the lossless
   `SessionTreeEntry`. The wire form cannot rebuild an `AgentMessage`: it
   loses thinking signatures, the provider, the model and the response id
   (`crates/loop-ai/src/types.rs:236-361`).
4. **One SQLite file.** The Event Log tables move into the Session database
   file. Leases, the log and the projection then commit in one transaction.
   Embedded owners write the same log. Recovery of Session records becomes
   scoped by lease, not by generation order (§2.5).
5. **The projection updates in the same transaction as the append.** A reader
   always sees its own writes.
6. **`SessionStore` stays** as the harness-facing trait. One generic
   implementation sits over the log and the projection.
7. **The traits move to a new crate**, `crates/core/loop-store`, with no
   `rusqlite` types. `loop-agent` and `loop-runtime` both depend on it.

## 1. Event kinds

### 1.1 Wire updates (`SessionUpdate`, `update.rs:77-180`)

"Barrier" means that the producer waits for the commit before it continues
(see §3.2).

| Kind | Class | Stored payload | Barrier |
|---|---|---|---|
| `branch_reset` | durable | `revision`, `branch_revision`, `view_id`, `reason`. The `view_id` is a display handle only. It expires after 300 s (`runtime/tree.rs:17-18`). The durable navigation record is the `tree` record of type `leaf` (§5). | yes (setter reply) |
| `turn_started` | durable | `turn_id` | yes (before the first tool) |
| `message_started` | durable | `message_id`, `role`. It is the anchor of the message's deltas. | no |
| `message_chunk` | **live only** | sent as a delta (§2) | — |
| `thought_chunk` | **live only** | sent as a delta (§2) | — |
| `message_ended` | durable | `message_id`, `role`, **`content: Vec<ContentBlock>`** (all text and other blocks, in order), **`thought: Vec<ContentBlock>`**, `interrupted: bool` (true only when recovery writes it) | no |
| `tool_call_preparing` | **live only** | sent as a delta | — |
| `tool_call` | durable | the full `ToolCall` (`update.rs:287`) with the complete `raw_input`: the arguments as the model sent them (`crates/loop-agent/src/agent_loop.rs:726-731`). The `policy_decision` holds the hash of the validated arguments. | **yes, before the tool runs** |
| `tool_call_update`, not terminal | **live only** | sent as a `tool_output` delta. The Runtime merges it into its open snapshot of the call. | — |
| `tool_call_update`, terminal (`status` is `completed` or `failed`) | durable | a **full snapshot**. Every field of `ToolCallUpdate` (`update.rs:313`) is set: `id`, `name`, `title`, `kind`, `status`, `content` (the full result), `locations`, `raw_input`, `raw_output`. | no |
| `plan` | durable | `entries` | no |
| `usage` | durable | `Usage` (`update.rs:486`). The mapper sends it once per assistant message (`protocol_events.rs:312-314`), not per token. | no |
| `notice` | durable | `level`, `text` | no |
| `interaction_requested` | durable | `request` | yes (before a Surface sees it) |
| `interaction_resolved` | durable | `id`, `outcome` | yes (before the agent gets the answer) |
| `turn_ended` | durable | `turn_id`, `stop`, `error` | yes (before the prompt result) |
| `config_changed` | durable | the full `ConfigChanged` | yes (setter reply) |
| `workflow_progress` | durable | `WorkflowProgress` | no |
| `unknown` | durable | the raw JSON, unchanged | no |

The Runtime pump fills `content` and `thought` of `message_ended`, and it
fills the terminal tool snapshot. It accumulates the chunks of the open
message and the updates of each open tool call. So the backends do not
change. The native mapper and the ACP mapping
(`crates/extend/loop-acp/src/mapping.rs:4-50`) still emit chunks and a bare
`message_ended`.

`content` and `thought` are `Option` on the wire type. A stored
`message_ended` without `content` is an **old-format row**: chunk rows before
it hold its text. A row with `content` (an empty list is valid) is a
new-format row. Readers apply the matching rule. No other format marker is
necessary. When recovery closes a message that has stored chunk rows (a turn
that crashed before the upgrade), it fills `content` and `thought` from those
rows. So a new-format row never hides old chunk text.

### 1.2 Log records (`EventKind`, `log/mod.rs:26-48`)

Only `update` records get a `session_seq`. All other records are ordered by
`global_seq` only.

| Kind | Class | Stored payload | Barrier |
|---|---|---|---|
| `update` | as in §1.1 | the `UpdateEnvelope` | as in §1.1 |
| `prompt_sent` | durable | the prompt input, and `turn` when known | yes (before the turn starts) |
| `prompt_settled` | durable | the reason (cancelled in the queue, busy, closing) | yes (before the reply) |
| `cancelled` | durable | the reason and `detail` | yes |
| `escalation_raised` | durable | the request, `id`, `generation` | yes |
| `escalation_answered` | durable | the answer, the grants, `answered_by` (ADR-0009) | yes (before the agent continues) |
| `escalation_withdrawn` | durable | `id`, `generation`, `reason` | yes |
| `session_opened` | durable | `cwd`, `name`, `parent`, `created_at`, **`origin`** (`create`, `fork`, `clone`, `import`, `subagent`, `legacy_import`), and for `fork` and `clone` the `source` Session and `through_entry` | yes |
| `session_closed` | durable | the reason | yes |
| `subagent_started`, `subagent_ended` | durable | as today (`docs/design/runtime.md` §4) | yes |
| `runtime_started`, `runtime_stopped` | durable | `pid` and the generation | yes |
| **`policy_decision`** (new) | durable | the S4 decision record (`docs/design/s4-policy.md` §12). The arguments are stored as a hash only. | **yes, before the tool runs.** `tool_call` commits first (§3.1). |
| **`tree`** (new) | durable | one `SessionTreeEntry` (`types.rs:41`), exactly as `sessions.db` stores it: `id`, `parent_id`, `timestamp` and the body | see §5.3 |
| **`session_deleted`** (new) | durable | `id`, `by` | yes |
| `unknown` | durable | the original kind and payload | no |

### 1.3 Two sequence spaces

- **`session_seq`** counts the durable wire `update` records of one Session.
  It is contiguous and has no gaps. Attach, replay and E8 use it. Log-only
  records, `tree` records included, never take a `session_seq`. So they
  never make a gap in the wire `seq`.
- **`global_seq`** orders all records. Recovery, audit and the projection use
  it. The projection cursor is a `global_seq`.

## 2. Deltas, sequences and attach

### 2.1 The delta

A delta goes out on a new notification, `session/delta`. It does not go out
on `session/update`. So every `session/update` still has a durable `seq`.

```text
Delta {
  session:     SessionKey,
  generation:  u64,
  anchor_seq:  u64,           // session_seq of the durable record it extends
  item:        Message(MessageId) | Tool(ToolCallId),
  part:        u32,           // 0, 1, 2 ... with no gaps, per item
  body:        DeltaBody,
}

enum DeltaBody {
  Text      { role: Role, content: ContentBlock },   // append
  Thought   { content: ContentBlock },               // append
  Tool      (ToolCallUpdate),                        // field-wise replace
  Preparing { name: String, kind: ToolKind },        // the call is being written
}
```

- For a message, `anchor_seq` is the `seq` of its `message_started`. For a
  tool call, it is the `seq` of its `tool_call`. For `Preparing`, the call has
  no durable record yet. So `anchor_seq` is the last durable `seq` of the
  Session, and `Preparing` is always part 0 of its item.
- `(anchor_seq, item)` is unique. `message_id` alone is not unique, because
  ids repeat after a restart.
- **Merge rules.** `Text` and `Thought` append. Text and thought of one
  message share one `part` counter, so a client keeps their order. `Tool`
  carries only the changed fields of the call (as `ToolCallUpdate` does,
  `update.rs:313`). Each field that is set replaces the old value. Its
  `content` is a `Vec<ToolContent>` and replaces the whole list: the native
  mapper sends the full partial result each time
  (`protocol_events.rs:111-118`).
- The Runtime gives `part` when it sends the delta, not when the backend makes
  it. So the Runtime can merge consecutive parts of one item under load
  (§3.3), and the numbers stay contiguous. Two `Text` parts merge by joining
  the text. Two `Tool` parts merge field by field, and the later value wins.

**Client rules:**

1. Order: durable updates are in `session_seq` order. The deltas of an item
   come after its anchor and before its end record.
2. Gap: the next part of an item must be `last_part + 1`. A higher part is a
   gap. The client then attaches again with its `after_seq`.
3. Drop a delta whose part is at or below the part that the client already
   has.
4. On `message_ended`, replace the partial text and thought with `content` and
   `thought`. On a terminal `tool_call_update`, replace the merged partial
   state of the call with the snapshot.

### 2.2 What `after_seq` means

`after_seq` is the highest durable `session_seq` that the client has applied.
Deltas never move it. A client that has partial text of an open message
discards that text when it attaches again, because the in-progress snapshot
replaces it. If `after_seq` is higher than the Session's `latest_seq`, the
Runtime refuses with `cursor_ahead`. The client then attaches with
`after_seq = 0`.

### 2.3 Attach in the middle of a message

The existing algorithm (`docs/design/runtime.md` §7) stays for durable
updates. Step 3 and the delta rule in step 4 are new.

1. Subscribe to the Session's live channel, and buffer it.
2. Under the Session sequencer lock, take `H` (the last durable `seq` sent)
   and the **in-progress snapshot**. Release the lock. Read the durable
   updates `(after_seq, H]` from the log in pages. Send them as replay.
3. Send `session/in_progress` with one entry per open item:
   `{ anchor_seq, item, role, parts_through, content, thought }` for a
   message, `{ anchor_seq, item, parts_through, state: ToolCallUpdate }` for a
   tool call (all `Tool` parts merged by the §2.1 rules), and
   `{ anchor_seq, item, name, kind }` for a call that is being prepared.
   `parts_through` is the last part included, or `-1` when the snapshot has no
   parts.
4. Drain the buffer. Drop durable updates with `seq <= H`. Drop deltas whose
   part is at or below the snapshot's `parts_through` for the item. Drop the
   deltas of an item whose end record has `seq <= H`. Then go live.

The snapshot and `H` come from the same lock. So the client gets each part
exactly once: in the snapshot or as a delta.

### 2.4 Crash in the middle of a message

**Lost:** the text and thought of each open message, the partial output of
each running tool, and each `preparing` delta. The durable records stay,
`message_started` and `tool_call` included.

**How recovery reports it.** For each interrupted turn of a crashed
generation, recovery appends one batch, in this order:

1. for each `message_started` with no `message_ended`: a `message_ended` with
   `content: []`, `thought: []`, `interrupted: true`;
2. for each `tool_call` with no terminal update: a terminal
   `tool_call_update` with `status: failed` and
   `raw_output: {"interrupted": "runtime_restarted"}`;
3. `turn_ended` with `stop: runtime_restarted` (as today, `recovery.rs`).

The rules for D2 and for `prompt_settled` do not change. The Session tree does
not hold the interrupted turn (§5.3). Recovery must also read
`message_started`, `message_ended`, `tool_call` and terminal tool updates of
generations that are not recovered. `recovery_records` (`log/mod.rs:124`)
adds these kinds.

### 2.5 Recovery scope when more than one owner writes the log

Today only one Runtime writes `events.db`. So recovery can treat each
generation below its own as dead (`recovery.rs:79-105`), and one global mark
(`last_recovered_generation`, `mark_recovered`) is enough. With one file
(§5.1), embedded owners write the same log while a Runtime runs. Then
"an older generation is dead" is false: a Runtime start would end a live
embedded turn, and a lower generation that dies later is never scanned.

The rule after step B8 (§10):

1. **One table gives all generations.** `runtime_generations` allocates for
   every owner kind (`owner` is `runtime` or `standalone`). A generation
   stamps records for audit. Recovery does not use generation order for
   Session records.
2. **Session records carry the lease epoch** of the writer (a column
   `lease_epoch`, from the `Fence` of the append).
3. **Recovery of a Session belongs to its next lease holder.** When an owner
   acquires or takes over the lease of Session S with epoch E, it reads the
   records of S with `lease_epoch < E` after the Session's mark
   `recovered_through(S)`. It ends the unfinished work of those records as in
   §2.4. It appends that batch with its own fence, and it moves the mark in
   the same transaction. Only then does it write anything else for S.
4. **Runtime-level records** (Escalations of a Runtime generation, `runtime_*`)
   keep the generation rule, but only over generations with
   `owner = 'runtime'`. The Session database lock
   (`crates/loop-app-core/src/runtime_instance.rs:69-74`) proves that those
   generations are dead.
5. **What an embedded owner writes.** It has no pump. A small
   `SessionRecorder` in `loop-agent`, next to `NativeBackend`, does the
   durable half of the sequencer: accumulate, fill `message_ended`, commit
   barriers. The Runtime pump wraps the same recorder and adds the live
   channel. So an embedded owner writes `prompt_sent`, `turn_started`,
   `message_started`, `message_ended`, `tool_call`, `policy_decision`, the
   terminal tool update, `turn_ended` and `tree`. It writes no deltas.

Until B8, embedded owners write only `tree` records (from B5). Those are not
recovery records, so the generation rule of today stays safe.

## 3. Batching and back-pressure

### 3.1 Commit triggers

The writer commits a group when one of these occurs:

| Trigger | Rule |
|---|---|
| Size | 256 events (`MAX_BATCH_EVENTS`, `log/mod.rs:17`) or 1 MiB of payload. The first limit that is reached applies. One atomic request can be larger than this (a turn's tree commit, §5.3). It is then a transaction of its own. |
| Time | No fixed window. The writer commits what is queued, at once. Events that arrive while a commit runs go into the next group. This is the present behaviour (`sqlite.rs:153-185`). It adapts to the load and adds no delay at low load. |
| Barrier | A producer waits for each event that §1 marks as a barrier. |
| Turn boundary | `turn_started` and `turn_ended` are barriers. |
| Side effect | `tool_call` and its `policy_decision` both commit before the tool runs, `tool_call` first. The mechanism: the agent loop awaits the emit of `ToolExecutionStart` before it calls the `before_tool_call` hook, where the gate decides. This is true on the sequential and the parallel path (`agent_loop.rs:724-740`, `agent_loop.rs:813-829`, `agent_loop.rs:1002-1031`). The Runtime's `DecisionSink` first runs `barrier()` on the Session (`runtime.rs:2576`). That commits every update before it, `tool_call` included. Then the sink appends `policy_decision` through the same sequencer and awaits the commit. The gate returns only after that. `escalation_raised` commits before a Surface sees the request. `escalation_answered` commits before the agent gets the answer. |
| Acknowledgement | A prompt result, a setter reply, an Escalation answer and a lease release go out only after their batch commits. |
| Lifecycle | `session_opened`, `session_closed`, unload, hibernate and lease release commit before they take effect. |

**Failure isolation in a group.** Today one transaction error fails all
coalesced requests (`sqlite.rs:24-33`). After the change, the writer puts each
request in a savepoint. A request that fails its own checks (`Fenced`,
`Invalid`, `Conflict`) rolls back to its savepoint and gets the error. The
other requests of the group still commit. Only a storage error (disk full,
I/O) fails the whole group.

### 3.2 Commit before send, and order

1. A durable record reaches Surfaces only after its commit. This rule does
   not change.
2. Each Session has one **sequencer** in its pump. Items enter in the order
   that the backend makes them. A durable item stops the queue until its
   commit. A delta goes out at once only when no durable item waits before
   it.
3. So the wire order of a Session is the order that the backend made. A delta
   waits at most one commit after a durable record. Inside one message there
   are no durable records, so text streams with no waits.
4. A durable record that is not a barrier does not stop the producer. The
   pump sends it to the writer and continues. The writer still commits it in
   order.
5. Within a Session, records commit in the order that they were made.
   Batching never reorders them. The store gives `session_seq` inside the
   transaction (`sqlite.rs:325-338`). So this is also true when more than one
   process writes.

### 3.3 Back-pressure from a slow writer

A slow writer slows the turn. It never loses data, and it never fails the
Session.

1. The backend-to-pump channel becomes a bounded `mpsc` (1,024 items) for each
   Session. It replaces the `broadcast` (`native_backend.rs:317`). When the
   channel is full, the backend waits. The model stream then stops being
   read, and the provider connection applies TCP back-pressure.
2. The writer queue stays bounded (`WRITER_QUEUE_CAPACITY = 64` requests,
   `log/mod.rs:19`). A full queue makes the pump wait.
3. Each sequencer has a limit: 1,024 durable items or 8 MiB that wait for
   commit. Above the limit, the pump stops reading from the backend.
4. Waiting deltas have a limit of 4 MiB per Session. Above it, the sequencer
   merges consecutive parts of the same item into one part, with the §2.1
   merge rules. No text is lost, and the merged state of a tool call is the
   same.
5. The Session command channel becomes bounded. Today it is
   `mpsc::UnboundedSender<Command>` (`runtime.rs:225`).

### 3.4 Slow consumers

A slow Surface never slows a turn, and the Runtime never disconnects it.

1. Each Session has its own live channel (a ring of 1,024 items). It replaces
   the global `broadcast::channel(4096)` (`runtime.rs:362`).
2. Each connection keeps `last_sent` for each attached Session.
3. When a subscriber lags, it does a **refill**. It reads the durable updates
   after `last_sent` from the log in pages. Then it takes a new in-progress
   snapshot and continues live with the §2.3 rules. The Runtime does not send
   a notice and does not close the connection. This replaces `lagged()`
   (`server.rs:1259-1275`).
4. The socket send queue of a connection is bounded. When it is full, the
   forwarder stops reading its channels. The forwarder then lags and refills
   later. The turn does not wait.
5. A refill uses a reader connection (§8), never the writer.

## 4. Old clients and the protocol

- `ClientCapabilities` (`crates/core/loop-protocol/src/rpc.rs:185`) gets
  `live_deltas: bool`, in the same way as `branch_reset`.
- `PROTOCOL_VERSION` (`crates/core/loop-protocol/src/lib.rs:46`) changes from
  1 to 2.
- The Runtime refuses a client that does not offer `live_deltas`. The error is
  `upgrade_required`. All known clients (TUI, Desktop, the web gateway) are
  Loop's own, and they ship with the Runtime. See Q2.
- The N8 golden wire tests cover `session/delta`, `session/in_progress`,
  `message_ended.content` and the refusal.

## 5. The Session tree as a projection

### 5.1 One file and one transaction

The Event Log tables move into the Session database file (`sessions.db`, at
its present path, with `LOOP_CODING_AGENT_SESSION_DIR`). The reasons:

- The leases are already there. A fenced append checks the lease in the same
  transaction (§8).
- Embedded owners already open this file. So they can write the log with no
  new path.
- In WAL mode, SQLite does not make a transaction atomic across two attached
  files. One file is the only way to commit a tree change and its log record
  together.
- The file keeps `synchronous=FULL` (`store.rs:263`). With batches, this costs
  one fsync per group, not one per chunk.

After the migration, nothing writes to `events.db` (§6).

### 5.2 Which records define the tree

| Tree concept | Defined by |
|---|---|
| A Session exists | `session_opened` (its `origin` tells how) |
| A Session is gone | `session_deleted` |
| An entry (message, model change, thinking level, active tools, label, session info, custom, compaction, branch summary) | one `tree` record with that `SessionTreeEntry` |
| Head | the last applied entry, or the target of the last `leaf` entry |
| Branches and branch tips | derived from `parent_id` (the present `branch_entries` and `branch_tips` tables) |
| Fork | `session_opened { origin: fork, source, through_entry }`, then `tree` records that **copy** the selected entries, all in one batch |
| Clone | as fork, with `origin: clone` and the whole active branch |
| Import | `session_opened { origin: import }`, then one `tree` record per JSONL line, in one batch |
| Compaction | a `compaction` entry. `read_path_to_root_or_compaction` stops there. It is a pure function of the entries. |
| Navigation | a `leaf` entry and the `branch_reset` update, **in one batch** |
| Rename | a `session_info` entry and `config_changed`, in one batch |
| Config choices (permission mode, sandbox, timeout) | `custom` entries as today (`types.rs:294-302`) and `config_changed`, in one batch |

A fork copies entries. It does not refer to the source. So each Session can be
deleted, exported and partitioned by tenant on its own.

The navigate, compact and rename paths use `control_mutation` and
`acknowledge_control` markers (`runtime/tree.rs:466-626`) to bridge two
files. With one batch, the markers are not necessary. Step B6 removes them.

### 5.3 Turns and cancelled turns

The harness keeps its present behaviour. It buffers the writes of a turn in
`pending_writes`. A cancelled turn writes nothing.

- At the end of a turn that was not cancelled, the harness calls
  `SessionStore::commit_turn(session, turn, writes)`. The store appends all
  `tree` records of the turn in **one atomic request**. This replaces the
  loop of `append_entry` calls (`agent_harness.rs:1441`).
- A crash before that commit leaves no tree entries for the turn. Today a
  crash in the middle of the loop can leave part of a turn (E1: 319 partial
  turns of embedded owners in set A). This is a fix, not a regression.
- The held `TurnEnded` (D1) stays. So `turn_ended` always has a higher
  `global_seq` than the tree records of its turn. A later step can put both in
  one batch. Correctness does not need that.
- The wire records of a cancelled turn stay in the log for display and audit.
  The projection never sees them, because it reads only `tree`,
  `session_opened` and `session_deleted`.

### 5.4 How the reads are served

The projection tables are the present `sessions.db` tables: `sessions`,
`session_entries`, `session_sequences`, `branch_entries`, `branch_tips`,
`session_materialized` and `entry_materialized`. Each gets a `tenant` column.

| `SessionReader` / `SessionStore` method | Served by |
|---|---|
| `read_head` | `sessions.active_leaf_id` |
| `read_entry` | `session_entries` by `(tenant, session_id, entry_id)` |
| `read_entries(after_seq)` | `session_entries` by `entry_seq`, paged |
| `read_path_to_root_or_compaction` | the `branch_entries` cache, as today (`sqlite/branch_cache.rs`) |
| `list(cwd)` | `sessions`, paged, with `session_deleted` applied |
| `fork`, `create`, `create_surface_destination` | one append batch (§5.2). The projection applies it in the same transaction. |
| `append_entry` | one append of one `tree` record (for writes outside turns) |
| `delete` | `session_deleted`, then a purge (§6.4) |

`entry_seq` is the order of the `tree` records of one Session. The projection
gives it, as `session_sequences` does today.

### 5.5 Same-transaction projection, idempotency and rebuild

1. The store applies `tree`, `session_opened` and `session_deleted` to the
   projection tables **in the same transaction** as their append. So
   `build_context` always sees the turn that was just committed.
2. The table `projection_state(name, applied_global_seq)` records the last
   record that was applied.
3. Applying is idempotent. The store skips a record at or below
   `applied_global_seq`. An entry insert uses
   `(tenant, session_id, entry_id)` as its key. An insert that conflicts with
   an equal row is a no-op. An insert that conflicts with a different row is a
   `Conflict` error.
4. **Rebuild:** delete the projection rows (of one tenant, or of all), set
   `applied_global_seq` to 0, and apply all records in `global_seq` order. The
   command is `loop store rebuild`. A test proves that the rebuilt rows are
   equal to the live rows (§9, C9).
5. A Postgres implementation can use the same rule. It can also project
   asynchronously. Then each read must wait until the projection has reached
   the last write of that reader.

### 5.6 `SessionStore` stays a trait

**Decision:** keep `SessionStore` and `SessionReader` as the harness-facing
traits. Add one generic implementation, `LogSessionStore`, over the
`loop-store` traits. Do not fold `SessionStore` into the log API.

The reasons:

- The harness, fork, transfer, search and stats code use these traits
  (`harness/session/*.rs`, about 4,000 lines). They do not change.
- The memory and JSONL stores (`memory_store.rs`, `jsonl_store.rs`) stay for
  tests and for tools that do not need a log.
- The log API stays small. It knows records, cursors and pages. It does not
  know tree semantics.
- A Postgres crate implements the `loop-store` traits only. It then gets
  `LogSessionStore` with no more work.

`SessionStore` gets one new method, `commit_turn`. Its default calls
`append_entry` in a loop, so the memory and JSONL stores keep working.

## 6. Migration

### 6.1 `events.db` into the Session database

At the first start of the new version, under the Session database lock
(`crates/loop-app-core/src/runtime_instance.rs:74`):

1. Copy `sessions.db` to `sessions.db.pre-0007` (a SQLite backup, not a file
   copy).
2. Run the new migrations in `sessions.db`: the `events` and
   `runtime_generations` tables, the identity columns, `projection_state` and
   the `tenant` columns.
3. If `<agent_dir>/events.db` exists, copy its rows. Keep `session_seq`, so
   that client cursors stay valid. Give new `global_seq` values in the old
   order. Keep the generations. Old chunk rows stay. Old `message_ended` rows
   stay in the old format (§1.1).
4. Write the `migrations` row `0007_import_events:<path>`. Leave `events.db`
   in place and do not write it again. A later version can delete it.

Edge case: two agent directories share one `sessions.db`. The second
`events.db` can have rows for a Session that already has rows. Then the
import appends those rows with new `session_seq` values after the existing
ones. It also writes a `notice` that tells clients to attach again from 0.

### 6.2 The tree into the log

The import is **eager, one time and resumable**:

1. For each Session in `sessions` (in `created_at` order), in one
   transaction: append `session_opened { origin: legacy_import }`, then one
   `tree` record per `session_entries` row in `entry_seq` order.
2. Mark the Session as imported (`legacy_import(session_id)`). After a crash,
   the import continues with the next Session that has no mark.
3. The existing projection rows stay. The import moves `applied_global_seq`
   to the end of the import. It does not apply the records again.
4. Then `loop store verify` rebuilds into temporary tables and compares them
   with the existing rows. A difference stops the start with a clear error.

We reject a lazy import. With a lazy import, the log is not the one truth
until each Session was opened, and a Postgres export would miss old
Sessions.

### 6.3 Rollback and compatibility

- The migration is **forward only**. A downgrade is not supported. To roll
  back, restore `sessions.db.pre-0007` and use the old binary. Sessions made
  after the upgrade are then lost.
- We do not test an old binary on the new file. The new columns have
  defaults, but the new keys include `tenant`, so old statements can fail.
- From this version on, the file has a `store_format` row. A binary refuses
  to open a file with a higher format than it knows. This protects later
  downgrades. It cannot protect against binaries that are already released.
- If a user runs an old binary on the new file anyway, the next start of the
  new version tries to adopt its writes:
  - an **adopt pass** turns each `session_entries` row with no matching `tree`
    record into a `tree` record;
  - rows that the old binary wrote to `events.db` continue `session_seq` from
    the old maximum, so they can collide with rows already in the log. The
    import uses the renumber rule of §6.1 for them, with the notice to attach
    again from 0.

### 6.4 Delete

The log is append only, with one exception. `delete` appends
`session_deleted`. Then, in the same transaction, it removes the other rows of
the Session from `events` and from the projection. Only the tombstone stays.
This keeps the present meaning of delete (the SQLite store removes the rows,
`store.rs:899`) and the privacy rule of ADR-0009.

### 6.5 Export and import (JSONL)

- `session/export` stays the `loop_session` v1 file
  (`harness/session/transfer.rs:1-20`): a header and the active branch, root
  first. It reads the projection. Nothing changes for users.
- Import appends `session_opened { origin: import }` and the `tree` records in
  one batch.
- A full event export (`loop_events` v1: all records of one Session, wire
  updates included) is new. The move of beta data to Postgres needs it
  (`docs/design/loop-web.md:135`). See Q11.

## 7. Identity fields

### 7.1 In the stored record

```text
Event {
  generation, session_key, turn, ts_ms, kind,
  tenant:    Tenant,               // required, opaque string
  principal: Principal,            // { kind: person | service, issuer, id }
  via:       Option<String>,       // token, device:<id>
  actor:     Option<String>,       // the Session or agent that acted
}
```

- SQLite columns: `tenant TEXT NOT NULL DEFAULT 'local'`, `principal_kind`,
  `principal_issuer`, `principal_id`, `via` and `actor`. Old rows read as
  `local` (ADR-0009).
- The projection tables get `tenant`. Each key starts with `tenant`.
- Runtime records (`runtime_started` and similar) use the Runtime's own
  service principal. On a single node, their tenant is `local`. In a shared
  cell, it is the reserved tenant `system`.
- The Runtime stamps the fields from the authenticated connection. The store
  does not take them from a request.

### 7.2 In reads

- Each read method takes `&Tenant` as its first argument. There is no default
  value. A query without a tenant does not compile.
- Recovery, audit and the projection rebuild read across tenants. They use a
  `SystemScope` value. Only the Runtime startup code can make one.
- The store checks the tenant only. The principal check (who can attach to a
  Session) is in the Runtime, above the store.
- The wire `UpdateEnvelope` and `Delta` do not carry `tenant`. The client gets
  it from `initialize`. This answers the second open question of ADR-0009.

## 8. Trait surface

The traits go in a new crate, `crates/core/loop-store`. It has no `rusqlite`
dependency. `loop-store-sqlite` holds the SQLite implementation and the
`From<rusqlite::Error>` conversion. `loop-agent` and `loop-runtime` depend on
`loop-store`.

```rust
// ---- types ----
pub struct Tenant(String);
pub struct Principal { kind: PrincipalKind, issuer: String, id: String }
pub struct SystemScope(());                    // made only by Runtime startup
pub struct Generation(u64);
pub struct Fence { tenant: Tenant, session: SessionKey, epoch: u64 } // from the lease store
pub struct Cursor(u64);                        // a session_seq or a global_seq

pub struct Event { generation, session_key, turn, ts_ms, kind: EventKind,
                   tenant: Tenant, principal: Principal,
                   via: Option<String>, actor: Option<String> }
pub struct Committed { global_seq: u64, session_seq: Option<u64>, event: Event }
pub struct Page<T> { items: Vec<T>, next: Option<Cursor> }

pub struct Append {
    events: Vec<Event>,        // one Session, or Runtime records only
    fence: Option<Fence>,      // required for Session records of a leased Session;
                               // its tenant must equal each event's tenant;
                               // its epoch is stored as lease_epoch (§2.5)
    atomic: bool,              // true: one transaction, no size cap (tree commit)
    recovered_through: Option<u64>, // moves the Session's recovery mark (§2.5)
}

#[non_exhaustive]
pub enum StoreError {
    NotFound(String),
    Invalid(String),
    Conflict(String),                // id or seq clash, different row on apply
    Fenced { session: SessionKey },  // stale lease epoch
    Busy,                            // a retry is safe
    TooLarge(usize),
    Storage(String),                 // the driver's text, no driver types
    Stopped,
}
type Result<T> = std::result::Result<T, StoreError>;

// ---- write side ----
#[async_trait]
pub trait EventWriter: Send + Sync {
    /// Returns after commit. Gives global_seq and session_seq.
    /// Applies the projection in the same transaction.
    async fn append(&self, req: Append) -> Result<Vec<Committed>>;
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> Result<Generation>;
    async fn finish_generation(&self, g: Generation, stopped_ms: i64) -> Result<()>;
    async fn mark_recovered(&self, g: Generation, at_ms: i64) -> Result<()>;
}

// ---- read side (reader connections, never the writer) ----
#[async_trait]
pub trait EventReader: Send + Sync {
    async fn read_session(&self, t: &Tenant, s: &SessionKey,
                          after_seq: u64, through_seq: Option<u64>, limit: u32)
        -> Result<Page<Committed>>;
    async fn latest_seq(&self, t: &Tenant, s: &SessionKey) -> Result<u64>;
    /// Small per-Session summary that the projection keeps:
    /// last revision, last activity, prompts sent, turn count, parent.
    async fn session_summary(&self, t: &Tenant, s: &SessionKey) -> Result<Option<SessionSummary>>;
    async fn summaries(&self, t: &Tenant, after: Option<SessionKey>, limit: u32)
        -> Result<Page<SessionSummary>>;
    /// Session-scoped recovery (§2.5): records of earlier lease epochs
    /// after the Session's recovery mark.
    async fn session_recovery_records(&self, t: &Tenant, s: &SessionKey,
                                      below_epoch: u64, after_global: u64, limit: u32)
        -> Result<Page<Committed>>;
    async fn recovered_through(&self, t: &Tenant, s: &SessionKey) -> Result<u64>;
    // across tenants
    async fn read_global(&self, scope: &SystemScope, after_global: u64, limit: u32)
        -> Result<Page<Committed>>;
    /// Runtime-level recovery: generations with owner = 'runtime' only.
    async fn recovery_records(&self, scope: &SystemScope, from: Generation,
                              after_global: u64, limit: u32) -> Result<Page<Committed>>;
    async fn last_recovered_generation(&self, scope: &SystemScope) -> Result<Generation>;
}

#[async_trait]
pub trait TreeReader: Send + Sync {
    async fn session(&self, t: &Tenant, id: &str) -> Result<Option<SessionMetadata>>;
    async fn list(&self, t: &Tenant, cwd: Option<&str>, after: Option<&str>, limit: u32)
        -> Result<Page<SessionMetadata>>;
    async fn head(&self, t: &Tenant, id: &str) -> Result<Option<String>>;
    async fn entry(&self, t: &Tenant, id: &str, entry: &str) -> Result<Option<SessionTreeEntry>>;
    async fn entries(&self, t: &Tenant, id: &str, after_entry_seq: u64, limit: u32)
        -> Result<Page<SessionTreeEntry>>;
    async fn path_to_root_or_compaction(&self, t: &Tenant, id: &str, leaf: Option<&str>)
        -> Result<Vec<SessionTreeEntry>>;
}

#[async_trait]
pub trait LeaseStore: Send + Sync {     // chain D owns the details
    async fn acquire(&self, t: &Tenant, s: &SessionKey, owner: &OwnerInfo) -> Result<Fence>;
    async fn takeover(&self, t: &Tenant, s: &SessionKey, owner: &OwnerInfo) -> Result<Fence>;
    async fn renew(&self, f: &Fence) -> Result<()>;
    async fn release(&self, f: &Fence) -> Result<()>;
    async fn sweep_stale(&self, scope: &SystemScope) -> Result<usize>;
}

#[async_trait]
pub trait ProjectionAdmin: Send + Sync {
    async fn rebuild(&self, scope: &SystemScope, t: Option<&Tenant>) -> Result<RebuildReport>;
    async fn verify(&self, scope: &SystemScope) -> Result<VerifyReport>;
}

pub trait Store: EventWriter + EventReader + TreeReader + LeaseStore + ProjectionAdmin {}
```

- `SessionTreeEntry` and `SessionMetadata` move from `loop-agent` to
  `loop-store`. Their serde forms do not change. In `loop-store`, the entry
  body keeps `AgentMessage` as `serde_json::Value`. `loop-agent` converts it.
- `Runtime::new` takes `Arc<dyn Store>` in place of `Arc<dyn EventLog>`. It
  does not open SQLite itself (`runtime.rs:349`).
- `SessionStore` (in `loop-agent`) gets
  `async fn commit_turn(&self, id: &str, turn: &TurnId, writes: Vec<PendingSessionWrite>) -> Result<Vec<SessionTreeEntry>, SessionError>`.
  `LogSessionStore` changes `StoreError::Fenced` to `SessionError::InUse`.
- `SessionStore` methods take no tenant. So a `LogSessionStore` is bound to
  one tenant and one principal when it is made:
  `LogSessionStore::new(store, tenant, principal, lease)`. The Runtime makes
  one per tenant. On a single node there is one, for `local`.
- The SQLite implementation keeps one writer task. It adds a pool of read
  connections (WAL). `latest_seq` reads the `events_session_seq` index.

## 9. Rules that keep E1 true

E1 (`crates/loop-runtime/tests/e1_recovery.rs`, `docs/probes/e1-results.md`)
must pass again with 0 failures on seeds 20261111 and 424242 after each step
that §10 marks "E1". Testers turn each rule into a test.

1. **No lost acknowledged mutation.** A reply that says "done" (a prompt
   result, a setter reply, an Escalation answer, a fork result) goes out only
   after the batch with its records commits.
2. **Commit before send.** No Surface gets a durable record before its commit.
3. **Write-ahead for side effects.** A tool that Loop runs starts only after
   its `tool_call` and its `policy_decision` commit, `tool_call` first. The
   mechanism is in §3.1: the S4 `DecisionSink`, which the gate awaits
   (`docs/design/s4-policy.md` §12), runs the Runtime `barrier()`
   (`runtime.rs:2576`) and then appends the decision. Loop records the tools
   that an ACP agent runs itself when the permission request arrives, or
   after the fact.
4. **Turn start before tools.** `prompt_sent` and `turn_started` commit before
   the first tool of the turn runs.
5. **D1.** The `turn_ended` of a prompted turn has a higher `global_seq` than
   the tree commit of the turn. If the tree commit fails, `turn_ended`
   reports `stop: error` (D3).
6. **Atomic turn in the tree.** The `tree` records of a turn commit in one
   transaction. After any crash, the projection holds all of a turn or none
   of it.
7. **Cancelled turns stay out of the tree.** A cancelled turn has wire records
   in the log and no `tree` records.
8. **Order.** Within a Session, records commit in the order that they were
   made. `session_seq` is contiguous from 1, with no gaps and no repeats.
9. **Live only means not stored.** After the change, the log gets no
   `message_chunk`, `thought_chunk`, `tool_call_preparing` or non-terminal
   `tool_call_update`. An `origin: replay` envelope is never appended
   (`log/mod.rs:248`).
10. **Each interrupted turn is marked.** Recovery ends it with
    `runtime_restarted`. It ends each open message with `interrupted: true`
    and fails each open tool call (§2.4).
11. **Recovery is idempotent and scoped.** A second pass writes nothing.
    Synthetic ids are deterministic (`recovered-prompt-<global_seq>`). After
    B8, recovery never ends a turn of a Session whose lease a live owner holds,
    and each dead owner's open work is ended by the next lease holder of that
    Session (§2.5).
12. **Settled prompts are not uncertain.** Recovery does not recover a prompt
    with `prompt_settled` (`e1_defects.rs:410`, `e1_defects.rs:456`).
13. **Fencing.** An append with a stale lease epoch fails with `Fenced` and
    writes nothing. Nothing takes over a live owner.
14. **Embedded owners (after B8).** An embedded owner writes the durable
    records of §2.5 point 5 to the same log. The next owner of a Session runs
    Session-scoped recovery before its first write. E1 with the daemon and two
    embedded owners: 0 live embedded turns ended by a Runtime start, and 0
    interrupted embedded turns left without a mark.
15. **Projection equals rebuild.** After each trial, a rebuild gives the same
    projection rows.

### New conformance tests (`loop-store-conformance`)

The kit is a crate of async test functions that are generic over `Store`.
SQLite runs it in CI. The Postgres driver runs it outside this repo.

| Id | Test |
|---|---|
| C1 | `session_seq` is contiguous per Session with 8 concurrent writers in 2 processes (SQLite) or 2 connections (Postgres). |
| C2 | Log-only records never take a `session_seq`. |
| C3 | An atomic append of 5,000 `tree` records commits all or none (a fault is injected in the middle). |
| C4 | A stale `Fence` gets `Fenced`. Nothing of that request is written. Other requests in the same group commit. |
| C5 | A read with one tenant returns nothing of another tenant. Two tenants with the same Session key do not see each other. |
| C6 | Paged `read_session` gives the same records as one unpaged read, with a fixed `through_seq`, while appends continue. |
| C7 | A long read does not delay an append by more than 50 ms (reader/writer split, E6). |
| C8 | The projection is visible in the same transaction (read-your-writes after `append`). |
| C9 | A rebuild gives equal projection rows after a random workload of 10,000 records with forks, navigation, compaction and deletes. |
| C10 | Applying a record twice is a no-op. A different row with the same key is `Conflict`. |
| C11 | `session_deleted` leaves only the tombstone. `list` and every read omit the Session. |
| C12 | Old-format rows (chunks, and `message_ended` without `content`) and new rows replay to the same display. |
| C13 | The legacy import of a `sessions.db` fixture continues correctly after a kill at each Session. `verify` passes. |
| C14 | Recovery records page across generations. `last_recovered_generation` never decreases. |

### New Runtime tests (chain A)

| Id | Test |
|---|---|
| R1 | Attach in the middle of a message: replay, snapshot and live tail give each part exactly once (a property test over random cut points). |
| R2 | A Surface that pauses for 10 s during a turn refills and converges. It is not disconnected. The turn time does not change. |
| R3 | A writer that is slow by 50 ms per batch slows the turn. Nothing is lost, and the Session does not fail. |
| R4 | `message_ended.content` equals the joined chunks, for native and ACP Sessions. |
| R5 | The terminal `tool_call_update` equals the field-wise merge of `tool_call` and all updates of the call. A client that merges the `Tool` deltas gets the same state as the in-progress snapshot. |
| R6 | A client without `live_deltas` gets `upgrade_required`. |
| R7 | N2: kill the node during streaming, 200 trials. The client gets a correct state and 0 duplicates. |

## 10. Implementation order

"Neutral" means no change that a user or a test can see. "E1" means: run E1
again after the step. The module split of `runtime.rs`
(`docs/execution-plan.md`, Phase 1) is done (`c4bb01d`). Both chains start
from it.

### Chain B (storage)

| Step | Work | Depends on | Neutral | E1 |
|---|---|---|---|---|
| B0 | New crate `loop-store`: `Event`, `EventKind`, `StoreError`, and the present `EventLog` trait moved in. It also defines every new type of this ADR, unused for now: `policy_decision`, `tree`, `session_deleted`, the identity fields, `Fence`, `Delta`, and `content`, `thought`, `interrupted` on `message_ended` (the last three in `loop-protocol`). `From<rusqlite::Error>` moves to the SQLite crate. | — | yes | — |
| B1 | Inject the stores. `Runtime::new` takes the store. The pump takes a trait object, not `SqliteSessionStore` (this changes the `pump` signature, `runtime/publish.rs:82`). | B0 | yes | — |
| B2 | Reader/writer split in SQLite. Paged `read_session`. The `session_summary` table. Replace the full scans (`runtime.rs:2085`, `last_activity_ms`, `subagent_sessions`, `turn_ids`). | B0 | yes | yes |
| B3 | Identity columns, and `&Tenant` on each read (with the S4 chain, which owns `Principal`). | B2, S4 types | yes (all `local`) | — |
| B4 | One file: the event tables in the Session database, the `events.db` import (§6.1), `store_format`, `Fence` on `append` and the `lease_epoch` column (with chain D), savepoint isolation in a group (§3.1). | B2, D (lease epoch) | no (file layout) | yes |
| B5 | `tree` records, the same-transaction projection, `LogSessionStore`, `commit_turn`, the legacy tree import (§6.2), the adopt pass, `verify`. Embedded owners now write `tree` records through `LogSessionStore`. | B4 | no (no more partial turns) | yes |
| B6 | Remove the `control_mutation` markers: navigation, rename and compaction in one batch. `rebuild`. Delete with purge. | B5 | no | yes |
| B7 | The `loop-store-conformance` crate. The Postgres driver (outside this repo) runs it. | B5 | yes | — |
| B8 | `SessionRecorder` in `loop-agent` (the durable half of A0's sequencer). Embedded owners record turns. Session-scoped recovery at lease acquire and takeover (§2.5). | A0, A5, B5, D | no | **yes, with embedded owners** |

### Chain A (streaming)

| Step | Work | Depends on | Neutral | E1 |
|---|---|---|---|---|
| A0 | Per-Session sequencer and accumulators in the pump. It still commits each update. Keep the durable half free of wire types, so B8 can move it to `loop-agent`. | B0, B1 | yes | yes |
| A1 | Per-Session live channels. Lag causes a refill from the log, not a disconnect (`server.rs:996`, `server.rs:1259`). | A0; B2 for the paged refill (A1 can start with `read_after`) | no (no disconnect) | — |
| A2 | `message_ended.content` and `thought`; the terminal tool snapshot. The fields are optional, so old clients ignore them. | A0 | yes (additive) | — |
| A3 | The `live_deltas` capability, protocol version 2, `session/delta`, `session/in_progress`, and the refusal of old clients. | A2 | no | — |
| A4 | Stop the commit of chunks. Deltas are live only. Durable records that are not barriers are pipelined. In-progress snapshot on attach and refill. Recovery closes open messages and tool calls (§2.4). | A1, A3 | no | **yes** |
| A5 | Write-ahead barrier: `tool_call` and `policy_decision` in one batch before the tool runs. | A4, S4 `DecisionSink` | no (timing) | **yes** |
| A6 | Bounded queues: the backend-to-pump `mpsc`, Session commands, wire control channels, delta merge. | A4 | no (back-pressure) | yes |
| A7 | Hibernate and wake from the projection. | A6, B5 | no | yes |

### What blocks what across the chains

| Blocker | Blocks | Why |
|---|---|---|
| B0 | A0, A2, A3, A4, A5 | A0 writes against the moved `Event` type. A2 to A5 use the new kinds and fields that B0 defines. |
| B1 | A0 | B1 changes the `pump` signature. A0 rewrites the pump body. |
| B2 | A1 (final form), A4 | A refill must page and must use a reader connection. B2 and A4 both change `log/sqlite.rs` and commit timing. |
| A0, A5 | B8 | B8 moves the durable half of the sequencer, with the write-ahead barrier, into `loop-agent`. |
| B5 | A7 | Wake reads the projection. |
| B4, A4 | each other's E1 run | Both change commit timing. Run E1 after each. Do not merge them in the same E1 window. |

After B0 and B1, chains A and B can run in parallel. The files that they share
are `log/sqlite.rs` (B2, B4) and the pump in `runtime/publish.rs` (A0, A4,
A6). B2 must merge before A4 starts.

## 11. Open questions

Each question has a recommendation. Answer yes or no.

| # | Question | Recommendation |
|---|---|---|
| Q1 | Move the Event Log tables into the Session database file, and stop writes to `events.db`? | **Yes.** It is the only way to commit the fence, the log and the projection in one SQLite transaction. It also keeps the `LOOP_CODING_AGENT_SESSION_DIR` layout. |
| Q2 | Refuse clients that do not offer `live_deltas`, with no compatibility mode? | **Yes.** All clients are Loop's own. A compatibility mode needs its own `seq` space per connection. |
| Q3 | No fixed batch window (commit what is queued, at once)? | **Yes.** No added delay at low load. Groups get larger when commits are slow. Measure again in E6. |
| Q4 | No durable checkpoint for long messages? | **Yes, none for now.** Add `message_checkpoint` only if N2 or users show a need. |
| Q5 | Keep `synchronous=FULL` for the single file? | **Yes.** The tree has it today. After batching, one fsync per group is cheap. |
| Q6 | Keep `usage` durable each time it is sent? | **Yes.** It comes once per model call, not per token. |
| Q7 | Store the text twice in phase 1 (`message_ended.content` for display, `tree` for model context)? | **Yes.** Today the text is also stored twice (chunk rows and `sessions.db`). Remove the copy later only if N4 shows that size matters. |
| Q8 | In step B6, let the projection keep a reference (`global_seq`) and not a copy of the payload? | **Yes.** It halves the file size. Defer it if B6 is late. |
| Q9 | An eager, resumable import of the old tree, with a backup, forward only? | **Yes.** |
| Q10 | Delete purges the rows of the Session and keeps only a tombstone? | **Yes.** It keeps the present meaning of delete and the privacy rule. |
| Q11 | Add a full event export (`loop_events` v1) in Phase 2, and keep `session/export` as it is? | **Yes.** |
| Q12 | Recovery closes open messages (`interrupted: true`) and open tool calls (`failed`)? | **Yes.** Without it, a log replay shows messages and tools that never end. |
| Q13 | Embedded owners record their turns in the log (B8), and recovery of Session records becomes scoped by lease, not by generation order (§2.5)? | **Yes.** It closes the E1 gap "a side effect with no record" for embedded owners. Without B8, embedded owners write only `tree` records and stay as unprotected as today. |
| Q14 | Runtime records in a shared cell use the reserved tenant `system`? | **Yes.** |
| Q15 | Fork and clone copy entries, with no references to the source? | **Yes.** Delete, export and tenant partitions stay simple. |

## Consequences

- The writer cost follows messages and tool calls, not tokens.
- A crash loses the partial text of open messages and the partial output of
  running tools. Recovery records this in the log.
- One file holds the truth, the leases and the projection. After B8, embedded
  owners get recovery marks for the first time.
- The protocol changes to version 2. Old clients are refused.
- After a crash, the tree can no longer hold part of a turn.
- The `control_mutation` marker protocol goes away in B6.
- Gates: E1 (same seeds) after each step marked E1; E6 and E8 before and after
  A4; N2 after A4; C1 to C14 in CI.
