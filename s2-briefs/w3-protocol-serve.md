# S2.5 server/client: multi-Session serving over loop-wire
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/protocol-serve (branch s2/protocol-serve, based on staging with waves 1-2: protocol types, loop-runtime Runtime core, serve_with_options). Read w3-common.md in this folder first, then docs/design/runtime.md §6 and §7 and Decisions 1 and 6.
Scope: crates/core/loop-wire (server.rs, client.rs) and the loop-runtime side that implements the trait. Do NOT touch loop-cli (another agent wires `loop serve`).
- A RuntimeControl trait object in loop-wire (so loop-wire does not depend on loop-runtime): list, close, attach(session, after_seq) -> (latest_seq, generation, replay iterator/stream), detach. loop_runtime::Runtime implements it.
- serve: per-connection filtering — a connection receives updates only for Sessions it created, loaded or attached. Advertise capabilities multi_session and attach when a RuntimeControl is present. Honour ClientCapabilities.interact.
- Handlers for session/list, session/close {session, force?}, session/attach {session, after_seq?} -> {latest_seq, generation} followed by replay then live updates, session/detach.
- Attach algorithm exactly as design §7: subscribe to live first, read the Event Log after the cursor, then de-duplicate by the last seq sent. No gaps or duplicates.
- RemoteBackend (client.rs): list(), attach(), detach(), close().
Tests: HEADLINE — attaching mid-turn yields contiguous session_seq with no gaps or duplicates, including re-attach from a saved cursor after the Runtime is dropped and reopened on the same events.db; two clients each see only their own Sessions; an old v1 client (no new capabilities) still works; close/detach semantics; Lane B parity and schema freshness tests still pass.
