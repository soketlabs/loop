# Review findings

## P1 — Reclaim only leases covered by the acquired Runtime lock

- **Branch:** `s2/serve-cmd` (`8d4e106`)
- **Location:** `crates/loop-cli/src/serve_cmd.rs:133-139`
- **Issue:** Startup passes `runtime_lock_held = true` to `takeover_lease` for every Runtime lease in the Session database. Two different `LOOP_CODING_AGENT_DIR` values can share that database through `LOOP_CODING_AGENT_SESSION_DIR`, while each process holds a different `runtime.lock`. Starting the second Runtime therefore replaces and deletes the first, live Runtime's leases. In `sqlite/leases.rs:225-230`, this flag bypasses both heartbeat age and pid liveness checks. The original Runtime loses its write fence and its work can fail or be cancelled. Design §5 and its Decisions permit immediate reclamation only within the associated lock's ownership domain.
- **Fix:** Associate persisted leases with their Runtime lock domain and reclaim only matching leases, or serialize Runtime ownership of a shared Session database with a common lock. For leases outside that proven domain, retain stale-heartbeat and dead-pid checks. Add a regression test with two agent directories sharing one Session database and a live lease. Isolate or remove the inherited `LOOP_CODING_AGENT_SESSION_DIR` in the new binary tests as well, so they cannot reclaim real external Session leases.

## P2 — Preserve shutdown received during the initial signal poll

- **Branch:** `s2/serve-cmd` (`8d4e106`)
- **Location:** `crates/loop-cli/src/serve_cmd.rs:74-76`
- **Issue:** The registration poll handles `Ready(Err(_))` but discards `Ready(Ok(()))`. If SIGINT or SIGTERM arrives while this first poll registers and polls the signal receivers, `shutdown_signal()` can complete successfully. Startup then continues and the accept loop at line 172 polls the completed async future again, causing a panic instead of the required clean shutdown. This skips the explicit socket cleanup and Runtime shutdown path.
- **Fix:** Preserve an already-completed shutdown result and enter cleanup without polling that future again, or register concrete signal receivers separately from the future that awaits them. Add coverage for shutdown already ready during the registration poll.
