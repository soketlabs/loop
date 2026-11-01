# S2.1 store-lease review

Reviewed only `51548ab39310f03a2fec5552fa0b080ed683601b` (`s2/store-lease`) against its parent `10f7911816eb4a7952191b59262152a7eeacf8fe`, using `git show 51548ab`, the store-lease/common briefs, and the approved Runtime design. Findings exclude inherited session-factory issues. Static review; no tests executed and no repository files edited.

## P1 — Read-only Session listing claims every Session it inspects

- **Branch:** `s2/store-lease`
- **File:line:** `crates/loop-agent/src/harness/session/sqlite/store.rs:560`
- **Issue:** Standalone `load()` now takes a lease and adds the Session to the renewal guard. Existing `loop_app_core::Runtime::list_sessions()` calls `AgentHarness::session_summaries()`, whose `list_session_summaries()` calls `store.load()` on every listed Session (`crates/loop-agent/src/harness/session/summary.rs:76`). Opening the resume picker therefore claims all unowned Sessions it inspects, including empty Sessions later excluded from the result, and renews those leases until the application's store is dropped. Another standalone process cannot resume those Sessions. Sessions already leased elsewhere silently disappear from the picker because failed loads are skipped. This violates design §5, “Read-only listing does not take a lease,” and changes standalone bootstrap/picker behavior.
- **Fix:** Keep reader loading separate from writer acquisition. Acquire at repository open/harness switch when admitting a writer, or provide a read-only load path and use it for summaries/search. Add a two-writer listing regression that checks unchanged lease rows, continued visibility of leased history, and another writer's ability to open Sessions merely inspected by the picker.

## P1 — Windows standalone crash recovery can never reclaim a lease

- **Branch:** `s2/store-lease`
- **File:line:** `crates/loop-agent/src/harness/session/sqlite/leases.rs:87`
- **Issue:** The new standalone factory integration acquires leases on every platform (`crates/loop-app-core/src/session_factory.rs:350`), but the Windows PID probe always returns `PidLiveness::Unknown`. If the process crashes or is forcibly terminated, its Drop cleanup cannot remove its persisted leases. `standalone_resume()` uses ordinary takeover, which requires both a 30 s stale heartbeat and `PidLiveness::Dead` (`leases.rs:227–229`). On Windows that second condition is unreachable, so the Session remains unresumable indefinitely. The refusal also directs the user to `loop attach`, which S2 does not support on Windows.
- **Fix:** Implement conservative Windows process-liveness detection that can prove termination, or defer standalone lease integration on unsupported platforms while preserving existing standalone behavior. Add a Windows test that abruptly terminates an owning process and verifies recovery after the stale interval. Returning Unknown as a fallback is safe against concurrent takeover, but does not provide a usable recovery path.

## P2 — Recycled PIDs block Runtime reclaim despite ownership-domain lock proof

- **Branch:** `s2/store-lease`
- **File:line:** `crates/loop-agent/src/harness/session/sqlite/leases.rs:229`
- **Issue:** `PidLiveness::Alive` unconditionally rejects takeover even when `reclaim` is true. After the previous Runtime dies, its PID can be reused by the replacement Runtime or an unrelated live process. The `kill(pid, 0)` probe only proves that the PID currently exists; it cannot prove the recorded owner incarnation is alive. Consequently startup cannot reclaim the previous Runtime's Sessions despite holding the associated `runtime.lock`, which design §5 and Decision 3 explicitly accept as proof that the previous Runtime is dead. The failure can persist for the replacement process's lifetime if it reused the old PID. The new live-owner test does not distinguish a genuinely live owner from a recycled PID.
- **Fix:** For Runtime-to-Runtime reclaim, honor the caller's explicit proof that it holds the lock for the same ownership domain, or use process-incarnation identification to distinguish recycled PIDs. Keep conservative Alive/Unknown refusal for ordinary takeover and preserve the ownership-domain restriction. Add a recovery case where the old recorded PID equals the new Runtime's PID, with a different generation and the associated lock held; also retain refusal tests without that lock proof.

Counts: P0 **0**, P1 **2**, P2 **1**, P3 **0**.
