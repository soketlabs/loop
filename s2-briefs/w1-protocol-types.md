# S2.5-types: protocol types, schema and goldens only
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/protocol-types (branch s2/protocol-types). Read w1-common.md in this folder first.
Scope: crates/core/loop-protocol only (+ regenerated schema/loop-protocol.json and protocol golden/schema tests). No server/client logic in loop-wire (that is a later chunk), except what is needed to keep it compiling.
Per design §6 and Decisions:
- Method constants + param/result types: session/list -> SessionInfo{ref, cwd, name, live, busy, created_ms, last_active_ms, parent}; session/close {session, force?}; session/attach {session, after_seq?} -> {latest_seq, generation}; session/detach.
- host/interact params gain optional `session`; new notification host/interact_withdrawn.
- Capabilities multi_session and attach (default false; old peers unaffected). PROTOCOL_VERSION stays 1.
- Turn-end reason value `runtime_restarted` on the existing turn-end update (additive).
- Add all to the json_schema() map; regenerate schema/loop-protocol.json (the workspace uses serde_json preserve_order; the schema test compares JSON values).
Tests: serde round trips and goldens for each new type; old v1 messages without the new fields still decode; unknown fields tolerated as today; schema freshness test passes.
