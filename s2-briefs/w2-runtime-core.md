# S2.4: Runtime core
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/runtime-core (branch s2/runtime-core, based on staging which now has wave 1: leases, Event Log crate loop-runtime, SessionFactory tiers, protocol types). Read w2-common.md in this folder first, then docs/design/runtime.md §3, §4, §5 and Decisions.
Scope: crates/loop-runtime (+ a small change in crates/core/loop-wire/src/server.rs).
- loop_runtime::Runtime holds GlobalResources, a per-cwd WorkspaceResources cache, HashMap<SessionKey, LiveSession>, the EventLog, the generation, limits (max 16 live Sessions; max 8 active turns via a semaphore — saturation waits; expose a way for a caller to release/reacquire its permit, which the S2.9 subagent tool will use).
- impl AgentBackend as a router keyed by SessionRef.instance: new_session/load_session take the Session lease (owner "runtime") then build via SessionFactory; prompt/cancel/is_busy/transcript route to the Session's own NativeBackend and log PromptSent/Cancelled.
- Per-Session pump task: subscribe to the NativeBackend, DROP Origin::Replay envelopes, re-stamp durable session_seq (contiguous, from the Event Log's latest_seq on load) and generation, append to the Event Log and an in-memory tail buffer, then broadcast. Log SessionOpened/SessionClosed, RuntimeStarted/RuntimeStopped.
- Inherent methods: list() -> SessionInfo (per protocol types), close(key, force) (refuse while busy unless forced; release the lease), shutdown().
- Lease renewal every 5 s for live Sessions.
- loop-wire: add ServeOptions { owns_backend } to serve(); with owns_backend=false the shutdown RPC ends only that connection; true keeps today's behaviour (default for existing callers/tests).
- No socket listener, no CLI, no attach/Escalation routing (later chunks).
Tests (faux provider via the Models test seam): two Sessions run at once with correct tags and contiguous seqs; the log has both streams in order; replay envelopes not logged; close releases the lease; one connection's shutdown doesn't stop other Sessions; the 17th Session gets Busy; active-turn limit waits; Lane B parity tests still pass.
