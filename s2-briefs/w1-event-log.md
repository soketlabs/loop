# S2.2: crates/loop-runtime skeleton + Event Log
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/event-log (branch s2/event-log). Read w1-common.md in this folder first.
Scope: new crate crates/loop-runtime (you are the ONLY chunk that edits root Cargo.toml members and Cargo.lock).
- trait EventLog: append_batch, read_after(session, after_seq), read_runtime_after(global_seq), latest_seq(session).
- SqliteEventLog on <agent_dir>/events.db per design §4 and Decision 1: events(global_seq PK, generation, session_key, session_seq NULLABLE, kind, turn, ts_ms, payload) with a partial unique index on (session_key, session_seq) WHERE session_seq IS NOT NULL; runtime_generations table. Single writer task, batched IMMEDIATE transactions, WAL, synchronous=NORMAL. Migrations with include_str! + migrations table like the session store.
- MemoryEventLog for tests (same trait, same semantics).
- Event kinds enum: Update(UpdateEnvelope from loop-protocol), PromptSent, Cancelled, EscalationRaised, EscalationAnswered, EscalationWithdrawn, SessionOpened, SessionClosed, RuntimeStarted, RuntimeStopped, Unknown (forward-compatible decode). Only Update gets a session_seq.
- Reuse the shape of loop-orchestration's SqliteEventLog (crates/loop-orchestration/src/workflow/).
- Do not build the Runtime router (that is S2.4); a crate doc and the log module are enough. Do not depend on loop-app-core yet unless needed.
Tests: round trip of every kind; read_after cursor semantics; contiguous session_seq per Session with interleaved Sessions; NULL seq for non-updates; generation bump; batching under concurrent producers; reopen after drop keeps data; Unknown kind decode. Run the same suite against both impls.
