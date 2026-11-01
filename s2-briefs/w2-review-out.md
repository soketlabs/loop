# S2 wave 2 review

Branches: `s2/runtime-core` (e88835e), `s2/serve-infra` (a208bd8).
Method: read `git diff staging..<branch>` against `docs/design/runtime.md` §3–5, §9 and the Decisions list.
Ran `cargo test -p loop-runtime -p loop-wire` on runtime-core: all pass (loop-runtime 1+18+10, loop-wire 9+10).
Ran `cargo check --tests` on serve-infra (loop-wire, loop-app-core) on macOS: clean.
Could not check the Windows target here (`aws-lc-sys` needs `windows.h`). By reading, every new unix-only item is behind `cfg(unix)` and the non-unix arms return `Unsupported`, so I found no Windows breakage.

No P0 or P1 found.

## s2/runtime-core

### P2-1: Shutdown and forced close can hang forever, and they block the admission lock
- File: `crates/loop-runtime/src/runtime.rs:463-465`, `:443-444`, `:499`
- Issue: `close_inner` waits for `busy` to clear with a 1 ms poll and no timeout. This applies even to a forced close. A turn stuck in a tool or provider call that ignores cancellation never clears `busy`.
- `close()` and `shutdown()` both hold `self.admission` for the whole wait. One stuck Session therefore blocks every `new_session`, `load_session`, `close` and `shutdown`. `shutdown()` also closes Sessions one after another.
- §9.7 says SIGTERM triggers Runtime shutdown. A hung turn would stall that shutdown indefinitely and leave leases and the sandbox held until SIGKILL.
- Fix:
  - Bound the settle wait (for example a 5–10 s grace after cancel), then abort the turn task and drop through to lease release.
  - Cancel all Sessions first, then settle them concurrently (`join_all`).
  - Do not hold `admission` while waiting. Mark the Session `closing` under the lock, then release it.

### P2-2: One transient lease-renewal error permanently kills a Session
- File: `crates/loop-runtime/src/runtime.rs:717-727`
- Issue: any `Err` from `renew_lease` calls `fail()` and returns. That includes `SessionError::Storage` from SQLite busy or locked after the 5 s `busy_timeout`, or `spawn_blocking` failure. Only `lost()` means ownership is gone.
- A busy `sessions.db` (a standalone writer, a checkpoint) cancels the turn and rejects all later prompts. The Session must then be closed and reopened, although the lease is still valid for another ~25 s (stale after 30 s).
- Fix: treat only the lease-lost variant as fatal. Retry storage errors on the next ticks. Fail once no renewal has succeeded for about 20–25 s. Add a test with an injected transient error.

### P2-3: `list()` reads the whole Event Log on every call
- File: `crates/loop-runtime/src/runtime.rs:378`
- Issue: `read_runtime_after(0)` returns every `LoggedEvent` since the beginning. That includes every streamed `Update` payload for every Session. It is cloned into memory just to find each Session's last timestamp, then scanned in reverse once per Session (O(sessions × events)). It is called under load by attach/list.
- Fix: add an indexed `last_activity(session)` / `MAX(ts_ms) GROUP BY session_key` query to `EventLog`. Alternatively keep a `last_active_ms` in `LiveSession` and use `read_runtime_after` only for non-live Sessions.

### P3-1: Cancel reaches the backend only through the pump queue
- File: `crates/loop-runtime/src/runtime.rs:643-647`, `:739-744`
- Issue: `cancel()` only enqueues `Command::Cancel`. The pump logs `Cancelled` first (a log commit await) and only then bumps `live.cancel` and calls `native.cancel`.
- If the pump is blocked in `publish` behind a slow or contended log write (the `live_leases_renew…` test holds one for ~6 s), cancellation, including cancellation of a turn queued for a permit, is delayed by the same amount.
- If the pump has already exited, `cancel()` is a silent no-op. In that state `fail()` has already cancelled the backend once.
- Fix: call `live.cancel.send_modify` and `native.cancel` directly in `cancel()`. Let the pump (or a spawned task) only log the intent.

### P3-2: Cancel can be lost between permit acquisition and the native turn starting
- File: `crates/loop-runtime/src/runtime.rs:616-620`
- Issue: after the `has_changed()` check, a cancel that arrives before `native.prompt` registers its turn sets the watch and calls `native.cancel` on an idle backend. The turn then runs uncancelled. The window is small.
- Fix: re-check `cancelled.has_changed()` after `native.prompt` returns and cancel again if set.

### P3-3: Orphan empty Session row on lease or open failure for a new Session
- File: `crates/loop-runtime/src/runtime.rs:277-292`, `:353-366`
- Issue: for `new_session` the repository row is created before the lease is acquired and the factory runs. If either fails (the `failed_factory_releases_lease_and_capacity` path), the lease and capacity are released but the created Session row stays. It then appears in `list()` as a non-live, empty Session on every failed open.
- Fix: delete the row on failure when this call created it.

### P3-4: `close_inner` can leave a lease held and the Session mapped if the pump task panics
- File: `crates/loop-runtime/src/runtime.rs:470-472`
- Issue: `task.await.map_err(failed)?` returns early on a `JoinError`. Neither `SessionClosed` is logged nor `release_lease` called, and the entry is not removed. During `shutdown` this only prints to stderr and the lease is left to expire.
- Fix: capture the join error, continue with logging and release, then return the error afterwards (the same pattern already used for `logged` and `released`).

### P3-5: Missing tests listed in the brief and design
- `Runtime::shutdown()` is only called as cleanup. Nothing asserts that Session leases are released, `RuntimeStopped` is logged, the generation is finished, or that a second `shutdown()` is a no-op.
- Nothing covers forced close or shutdown while a turn ignores cancellation (see P2-1).
- Nothing covers a transient renewal error (see P2-2).

## s2/serve-infra

### P2-4: Daemon log file and `logs/` directory are created with default umask
- File: `crates/loop-cli/src/debug_log.rs` (`TracingMode::Daemon` arm, the `create_dir_all` and `OpenOptions` calls)
- Issue: the log file is created via `OpenOptions::new().create(true).append(true)`, so it gets the default mode (typically 0644), and `logs/` is created with the default 0755. The rest of the feature creates the run dir 0700 and the lock/pid files 0600 with owner and symlink checks.
- A multi-user host can read `runtime.log` (paths, session ids, provider errors, anything logged at `info`). It also follows a symlink planted at `logs/runtime.log` (no `O_NOFOLLOW`).
- Fix: create `logs/` as 0700 and the file with `.mode(0o600)` and `custom_flags(O_NOFOLLOW)` under `cfg(unix)`. Verify ownership as `private_file` does.

### P3-6: `/tmp/loop-<uid>` fallback socket is shared by every agent dir that needs the fallback
- File: `crates/loop-app-core/src/config/paths.rs` (`runtime_socket_path` fallback)
- Issue: this follows the design text, but the socket name is keyed only on the uid while the lock is keyed on the agent dir. Two different agent dirs with long paths (for example a long `LOOP_CODING_AGENT_DIR` plus a test or CI setup) hold different locks and both choose `/tmp/loop-<uid>/runtime.sock`.
- The second Runtime then fails the probe with "Runtime already listening". A client resolving the socket for the second agent dir would connect to the first Runtime and see the wrong Sessions.
- Fix: include a short hash of the canonical agent dir in the fallback path, for example `/tmp/loop-<uid>/<hash8>.sock`.

### P3-7: `unix_listen` still depends on the caller holding the lock, and nothing enforces it
- File: `crates/core/loop-wire/src/socket.rs`, `unix_listen` (probe, then `remove_file`, then `bind`)
- Issue: the probe, unlink and bind are not atomic, and the lock is only a doc-comment requirement. The existing caller `crates/core/loop-wire/tests/parity.rs:94` is a test, so nothing wrong today.
- Two starters that skip the lock can both see the socket as stale, and one can unlink the other's freshly bound socket. This needs to be wired correctly in the later `loop serve` chunk.
- Fix: for the serve chunk, take `RuntimeInstanceLock` before `unix_listen`. Consider making `unix_listen` accept `&RuntimeInstanceLock` (or a marker) so the compiler enforces it.

## Counts
- P0: 0
- P1: 0
- P2: 4 (runtime-core 3, serve-infra 1)
- P3: 7 (runtime-core 5, serve-infra 2)
