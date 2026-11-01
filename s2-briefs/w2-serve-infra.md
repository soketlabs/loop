# S2.7a: `loop serve` infrastructure (paths, single-instance lock, daemon logging) — no command wiring yet
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/serve-infra (branch s2/serve-infra, based on staging with wave 1 merged). Read w2-common.md in this folder first, then docs/design/runtime.md §9.
Scope (do NOT touch crates/loop-runtime or loop-wire/src/server.rs; another agent owns them):
- crates/loop-app-core/src/config/paths.rs: run_dir() (<agent_dir>/run, mode 0700) and runtime_socket_path() with the sun_path length fallback to /tmp/loop-<uid>/ (0700) when too long; runtime_lock_path(), runtime_pid_path(), runtime_log_path() (<agent_dir>/logs/runtime.log).
- A RuntimeInstanceLock (put it where it fits, e.g. loop-app-core): flock on runtime.lock (non-blocking), writes the pid file, released on drop; returns a clear "a Runtime is already running (pid N)" error. cfg(unix); elsewhere returns an Unsupported error.
- crates/core/loop-wire/src/socket.rs: replace the unconditional unlink in unix_listen with a probe: if a peer answers on the socket, refuse ("Runtime already listening"); if connection is refused/stale, remove and bind. Socket mode 0600.
- init_tracing in loop-cli gains a daemon mode writing to runtime.log (no stderr spam), keep current mode unchanged.
- Signal helper: a future that resolves on SIGINT or SIGTERM (tokio::signal::unix), for the serve command to use later.
Tests: path fallback for long agent dirs; permissions 0700/0600; second lock fails while first held and succeeds after drop; probe refuses a live listener and reclaims a stale socket file; daemon logging writes to the file. All unix tests behind cfg(unix).
