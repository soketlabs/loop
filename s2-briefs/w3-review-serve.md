# Review findings

## P2 — Match attach completions to their own attachment operation

- **Branch:** `s2/protocol-serve` (`3dcb9bd`)
- **Location:** `crates/core/loop-wire/src/server.rs:609-614`
- **Issue:** Request handlers run concurrently, but all attaches for one Session share one `Feed::Attaching` entry. With two overlapping attaches, the first completion consumes the second operation's entry and installs `Live`. The second completion removes that `Live` entry and returns successful `{}`, which is not an `AttachResult`. Subsequent updates are silently discarded. Detach followed by reattach while an old snapshot is pending has the same stale-completion problem. This violates §7's contiguous subscription contract.
- **Verification:** An isolated Rust reproduction using the exact branch `Feed`/`Feeds` implementation, with protocol/connection stubs, produced a first typed response, a second `{}` response, and no remaining feed; the next live update was dropped.
- **Fix:** Serialize attachment lifecycle operations per Session or give each attach an operation token and match it before removing or replacing state. Return a proper error for a cancelled/superseded operation. Test overlapping attaches and detach/reattach with an earlier snapshot still pending.

## P2 — Handle registration-buffer overflow explicitly

- **Branch:** `s2/protocol-serve` (`3dcb9bd`)
- **Location:** `crates/core/loop-wire/src/server.rs:566-569`
- **Issue:** While create/load is pending, unknown-Session updates share one 65,536-entry buffer. Once it fills, the `None => {}` branch silently drops further updates and still returns success. Unrelated busy Sessions can fill this buffer during a slow registration, causing the registering Session's own updates to disappear before `Owned` arrives. Later updates resume with sequence gaps and no lag notice. This violates §6's create/load capture requirement and §7's explicit prohibition on silent skips.
- **Verification:** The exact feed implementation accepted 65,539 sequential updates but delivered only 65,537 after ownership registration, with the final delivered sequence already at 65,539.
- **Fix:** On overflow, recover the registering Session's missing range from the Event Log, or explicitly fail/close the connection with a lag indication. Add a test where unrelated Sessions exhaust the pending buffer before the target Session is registered.

## P2 — Do not subscribe a connection through a rejected prompt

- **Branch:** `s2/protocol-serve` (`3dcb9bd`)
- **Location:** `crates/core/loop-wire/src/server.rs:431-433`
- **Issue:** `Prompted` unconditionally calls `feeds.own(session)` before the backend validates the prompt. `Rejected` only removes the waiting response at line 404 and never rolls back that subscription. A connection can obtain another Session's key from listing, prompt with a wrong instance/native id, receive `SessionNotFound`, and still receive that Session's future updates. A Busy rejection also leaves this subscription behind. Runtime reference validation rejects the operation, but filtering retains the key anyway. Design §6 limits feeds to Sessions created, loaded or attached by that connection.
- **Fix:** Keep prompt-response bookkeeping separate from subscription ownership. Require explicit attach for an otherwise unowned Session, or at minimum roll back newly acquired feed state on rejection. Test rejected prompts against a Session owned by another connection and assert no updates arrive.

## P2 — Resolve prompt completion only after its buffered end update is sent

- **Branch:** `s2/protocol-serve` (`3dcb9bd`)
- **Location:** `crates/core/loop-wire/src/server.rs:468-476`
- **Issue:** The forwarder sets `Waiting.ended = true` when it receives `TurnEnded`, before `feeds.route` decides whether to send or buffer it. During an in-progress attach, that update goes into `Feed::Attaching.buffer`. If the prompt outcome is ready, lines 506-512 nevertheless send the prompt response immediately, ahead of the attach response, replay and buffered `TurnEnded`. A client that prompts while attach is reading history can therefore observe completion before its promised updates. This breaks the §3 completion-ordering requirement preserved by the existing wire contract.
- **Fix:** Track end-update delivery, rather than receipt, when resolving prompt responses. Replay and buffer-drain delivery must participate in that bookkeeping. Test a blocked attach snapshot overlapping a prompt that finishes before the snapshot is released.

## P2 — Clear detached observation state without losing pending local prompts

- **Branch:** `s2/protocol-serve` (`3dcb9bd`)
- **Location:** `crates/core/loop-wire/src/client.rs:115-122`
- **Issue:** `detach()` stops server updates but leaves `RemoteBackend.busy` unchanged. If an observer receives `TurnStarted` and detaches before the owner finishes, it misses `TurnEnded`, which is the only update that clears that entry in `read()`. Even after the Runtime is idle, `is_busy()` stays true and `prompt()` rejects locally with Busy. Loading the Session again does not clear the stale entry either. The new detach test covers only an idle Session.
- **Fix:** Separate locally pending prompts from busy state inferred through observation, and clear/reconcile detached observation state. Add a test that detaches an observer mid-turn, lets the owner finish, then loads/prompts the Session through the former observer.

## P2 — Advertise attach only when the wire handler is installed

- **Branch:** `s2/protocol-serve` (`3dcb9bd`)
- **Location:** `crates/loop-runtime/src/runtime.rs:708`
- **Issue:** Runtime now advertises `attach: true` unconditionally. When it is served through the existing `serve_with_options` entry point, `runtime` is `None`, but initialization forwards that capability unchanged. The `session/attach` handler is guarded by `runtime.is_some()`, so the advertised operation returns method-not-found. The new v1 test constructs precisely this configuration and expects attach to fail without checking its contradictory capability advertisement. The brief ties advertisement to the presence of RuntimeControl.
- **Fix:** Make the server's advertised attach capability reflect installed RuntimeControl support, and test both serving entry points for agreement between capability flags and available methods.
