# Text streams live; only complete events are durable, and they commit in batches

Today the Runtime commits every Session update to the Event Log on its own, one
SQLite transaction each, before it broadcasts it (`publish()` in
`crates/loop-runtime/src/runtime.rs`, staging `2bffae3`). Every text chunk and
every thought chunk is one durable commit. We change this so that **deltas are
live only** and **durable events commit in batches**.

**Live only (transient):**

- `message_chunk`;
- `thought_chunk`;
- partial tool output.

These are broadcast at once and are never written to the log.

**Durable:**

- `prompt_sent`, `prompt_settled`;
- `turn_started`, `turn_ended`;
- `message_started`;
- `message_ended`, which now carries the complete content;
- `tool_call`;
- the final `tool_call_update`. `ToolCallUpdate` today carries only changed
  fields. The final durable one must be a full snapshot of the call (status,
  content, locations, raw input and output), so that replay and the tree
  projection lose nothing;
- `plan`, `usage`, `notice`;
- `config_changed`, `branch_reset`;
- Escalations;
- Policy decision records;
- Session and Runtime records.

The single writer groups durable events from all Sessions into one transaction.
A batch closes when one of these occurs:

- 256 events (`MAX_BATCH_EVENTS`);
- a few milliseconds pass;
- an event needs an acknowledgement.

A durable event still reaches Surfaces only after its commit.

**Reconnect.** A reconnect gets the last durable state, then the live tail. The
tail is the text of the open message so far, and then new deltas.

We chose this over a commit per chunk for two reasons. First, the commit rate
grows with tokens, not with messages, and one writer task serves every commit
and every read. E6 will measure where this stops; it has not run yet. Second,
the log fills with text that `message_ended` repeats. We also chose it over
periodic durable chunk batches (for example every 100 ms), because those keep
most of the cost and still store the text twice.

## Rules that keep E1 true

E1 passed only after two ordering fixes (`staging:docs/probes/e1-results.md`).
Batching must not undo them:

- **Write-ahead for side effects.** A tool that Loop runs starts only after
  its `tool_call` and its Policy decision record are committed. The two
  commit in the same batch. This applies to native tools, `mcp serve` and
  `/workflow` tasks. It cannot apply to tools that an ACP agent runs itself.
  For those, Loop logs the agent's permission decision before it answers.
  Calls without a request are logged after the fact. For tools that Loop
  runs, this closes the E1 D2 class (a tool ran, and the log has no turn) by
  construction. Recovery still reads `prompt_sent` as a second line of
  defence.
- **D1 stays.** `turn_ended` commits only after the Session tree holds the
  turn (the held `TurnEnded` in `native_backend.rs`).
- **Acknowledgement after commit.** A prompt result, a setter reply or an
  Escalation answer is sent only after its batch commits.
- **Order.** Within a Session, durable events commit in the order they were
  made. Batching never reorders them.

## Sequences

- `seq` stays a **durable** per-Session counter, and it has no gaps. Attach,
  replay and E8 depend on this.
- A live delta does not get its own `seq`. It carries the `seq` of the last
  durable update of its Session, plus `delta: { message_id, offset }`, where
  `offset` is the character offset in that message.
- A client drops a delta whose offset is below the text it already has. On
  `message_ended`, the client replaces the partial text with the full
  content.
- On attach, the Runtime sends replay up to the high-water mark H, then a
  `message_snapshot` for each open message (the text so far, its offset and
  the `seq` it follows), then live updates. A refill after lag does the same.
  The existing attach algorithm (`staging:docs/design/runtime.md` §4) is
  unchanged for durable updates.
- **Old clients.** `seq` is a required field of `UpdateEnvelope` today, and an
  old client does not read the content of `message_ended`. Such a client
  would drop the deltas or show empty messages. So the protocol version
  increases. A client that does not offer `transient_deltas` in `initialize`
  is refused with an "upgrade" error. All known clients are Loop's own (TUI,
  Desktop and the web app gateway that is moving to the SDK).

## Relation to "the Session tree as a projection of the Event Log"

E1 recommends one store for truth in Phase 1. This ADR is the event format
that makes the projection possible:

- `message_ended` carries the full content;
- the final `tool_call_update` carries the result.

So the tree can be rebuilt from durable events alone. Chain A (streaming) and
chain B (log) both build on this format. When the tree becomes a projection,
D1 is no longer needed: there is one commit point. Embedded owners can then
write the same log and get recovery marks, which they lack today.

## Consequences

- Fewer commits and a smaller log. The writer cost follows the number of
  messages and tool calls, not the number of tokens.
- A crash loses the partial text of an open message. Recovery still marks the
  turn uncertain (the S2 rule). Replay shows the completed messages, not the
  original chunk boundaries.
- A protocol version change with the capability `transient_deltas`, checked
  by the N8 golden wire tests. Old clients are refused at `initialize`.
- Old logs keep their chunk rows. Replay of old rows works unchanged.
- The writer needs a group-commit loop and a per-batch acknowledgement fan-out.
  This is the same work as "batched commits across Sessions" in Swarms v1.
- Gates:
  - **N2:** kill a node during streaming, 200 trials. The client gets a
    correct state, with 0 duplicates.
  - E1 again with the same seeds.
  - E6 numbers before and after.
  - E8 for convergence.

## Open questions

- The batch window: fixed (for example 5 ms), or adaptive to the load?
- Should very long messages get a durable `message_checkpoint` (for example
  every 2 s or 16 KB), so that a crash keeps most of the text?
- `events.db` uses `synchronous=NORMAL`, so power loss can drop the last
  commits. Should acknowledged events need `FULL`, and what does that cost
  per batch? E1 tested SIGKILL only.
- Is refusing old clients acceptable? The other choice is a compatibility
  mode that sends old clients only durable updates, plus one synthetic chunk
  with the full text before each `message_ended`. That needs its own `seq`
  space for each connection.
- Usage updates are frequent. Should they be durable on each change, or only
  at the end of a turn?
