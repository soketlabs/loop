# S2 progress (temporary handover note — delete when S2 lands)

Snapshot: 2026-11-01 ~14:15 IST. Plan: `/Users/anandgupta/.claude/plans/plan-the-s2-work-reactive-flurry.md`.
Design (approved): `docs/design/runtime.md` (its "Decisions" list overrides the plan where they differ).

## Merged into `staging` (pushed)

| Chunk | What | Merge |
|---|---|---|
| S2.0 | Design doc | 80277fc |
| S2.1 | IMMEDIATE store writes, atomic fork, Session leases, `--resume` refusal (standalone leases unix-only) | 821215b (wave 1) |
| S2.2 | `crates/loop-runtime` + Event Log (`events.db`, Sqlite + Memory) | 821215b |
| S2.3 | `bootstrap` split: GlobalResources / WorkspaceResources / SessionFactory | 821215b |
| S2.5-types | Protocol types, schema, goldens (`InteractParams` wraps request + optional `session`) | 821215b |
| S2.4 | Runtime core: router, pumps, limits 16 Sessions / 8 active turns, lease renewal, `serve_with_options` | 085c639 (wave 2) |
| S2.7a | serve infra: paths, RuntimeInstanceLock, socket probe, daemon log (0600), signals | 085c639 |
| S2.7b | `loop serve` command (lock on agent dir + Session DB, reclaim, accept loop, SIGINT/SIGTERM cleanup; unix only) | 8a75d55 (wave 3) |
| S2.5 | Multi-Session serving: `RuntimeControl`, per-connection filtering, `session/list/close/attach/detach`, gapless attach, `RemoteBackend` methods; `loop serve` uses `serve_with_control` | bc446bf (wave 3) |

Combined checks at each merge: fmt, clippy x2 (plain + telemetry), workspace tests (929 at wave 2, 949 at wave 3). No CI yet (CI runs only on the final staging -> main PR). Windows not exercised locally.

## On branches, not merged

Nothing. Every finished chunk is merged; all review findings so far are fixed. Last combined check (bc446bf): fmt, clippy x2, 949 tests passed, 0 failed.

## Remaining chunks

1. S2.6 Escalation routing (park until a client attaches, first answer wins, withdraw others / on cancel, log grants). Needs S2.5.
2. Wave 4 in parallel: S2.8 `loop attach` (plain-text client, `--new [--cwd]`), S2.9 `subagent` tool (child Session, `Policy::with` tightening, depth 2, no default max_turns, parent releases its turn permit while waiting).
3. S2.10 crash recovery (generation bump, `runtime_restarted` turn end, stale lease takeover, withdraw old Escalations), binary end-to-end tests, user docs `docs/runtime.md`, live GLM check in Herdr.
4. Optional: S2.11 WebSocket listener (token required even on loopback; may slip to S3); S2.1b persist model/thinking changes.
5. Land: one `staging` -> `main` PR (CI runs once; maintainer merges).

Known follow-ups:
- A Session that loses its lease is marked failed but stays listed until `close()` (S2.4 choice).
- S2.5: replay is read from the log in one go (no paging). If a connection falls behind, the server closes it with a notice (no refill from the log).
- S2.5 deviation: a v1 client that prompts a Session it did not create becomes its owner and gets its updates.
- S2.5: one attach-mid-turn test is timing-dependent and may pass without detaching mid-turn; S2.10's binary tests should cover it deterministically.
- The `attach` test once hung for over an hour (since fixed); keep a timeout on every async test.

## How the work was run

- Briefs for every chunk: `/Users/anandgupta/.claude/jobs/b3fa4ea3/tmp/w*-*.md` (job tmp; may be cleaned — copy anything needed). Pattern: common rules file + chunk brief, then a read-only review, then fixes, then a combined merge check in `../loop-wt/wave1-merge`.
- Worktrees: `../loop-wt/<chunk>`; shared build dir `CARGO_TARGET_DIR=../loop-wt/target` (the disk filled twice with per-worktree targets).
- Agents: Codex (gpt-6.1-sol) and Claude Sonnet 5.5 in Herdr panes; all stopped at handover. Suggested next: Opus 5.5 for S2.6, S2.9 and S2.10; Codex or Sonnet for reviews and smaller chunks. Check Codex `/status` and Claude `/usage` between waves.
- macOS has no `timeout`; bound test runs with `perl -e 'alarm 600; exec @ARGV' cargo test ...`.
