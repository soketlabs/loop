# S2.1: session store concurrency + leases
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/store-lease (branch s2/store-lease). Read w1-common.md in this folder first.
Scope (crates/loop-agent, plus the --resume refusal in loop-app-core bootstrap only):
- crates/loop-agent/src/harness/session/sqlite/store.rs: append_entry, create, delete use TransactionBehavior::Immediate so busy_timeout applies.
- fork becomes ONE IMMEDIATE transaction: create session, copy selected entries, set parent_session_id; all or nothing.
- Migration 003_session_leases.sql: session_leases(session_id PK, owner, pid, generation, heartbeat_ms) following the existing migrations pattern.
- Lease API on the store (acquire, renew, release, takeover) per design §5: renew 5 s, stale 30 s; takeover needs stale heartbeat AND dead pid, except the owner "runtime" reclaiming its own leases when it holds runtime.lock (expose this as an explicit parameter; the lock itself is S2.7's job). Never take over a live owner. Make time and pid-liveness injectable for tests.
- Bootstrap `--resume` of a Session leased by a live owner is refused with a clear message (design §5 wording). Keep this change minimal in crates/loop-app-core/src/runtime.rs because another agent is refactoring that file; put logic in loop-agent and call it from one place.
- Optional S2.1b if time allows, as a separate commit: persist ModelChange and ThinkingLevelChange from AgentHarness::set_model / set_thinking_level.
Tests: concurrent appends from many tasks/connections with no SQLITE_BUSY and contiguous seqs; fork atomicity (inject failure mid-copy leaves no session); lease conflict, renew, stale takeover, live-owner refusal, runtime reclaim; migration tests still pass.
