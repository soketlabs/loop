# Loop: execution plan to reach parity and scale

## Context

Loop has a clear vision (`docs/loop-vision` branch) and a fast foundation (`staging`, commit `1dc7ef4`).
S1 (wire protocol) and S2 (Runtime, Event Log, Subagents) are on `main`. S3 (the TUI as a Runtime client) is on `staging`.
The goal is larger than the current roadmap: parity with Claude Code, Codex and their web products, at their volume.

The research found five facts that change the plan:

1. **Parity has no measurement.** No benchmark compares Loop with Claude Code or Codex on code quality.
2. **Loop is efficient but less correct.** In the Kubernetes benchmark (run A), Loop is the fastest harness and uses the least memory. But Loop has 12 of the 16 incorrect results. Loop probably stops before it verifies its work (inference, not proven).
3. **The Runtime is a single-host design.** Limits are 16 Sessions and 8 active turns. One SQLite writer thread serves all writes and reads. Each text chunk is one durable commit. One broadcast channel carries all Sessions. No tenant or user identity exists.
4. **No stress test has a result.** E1 to E9 are plans only. `staging` has no CI. Nobody ran Loop on Linux locally.
5. **Important surfaces do not exist.** There is no Web code, no SDK, and no IDE integration. Desktop does not use the Runtime. Automatic compaction is not connected (`should_compact` has no caller).

Delivery evidence: one maintainer with coding agents built S2 in about 1.5 days and S3 in about 1.5 days. Build speed is not the limit. Review, live tests and decisions are the limit.

## Status on 2026-11-11

- **PR #107 is merged to `main`.** It brings S3 to `main`.
  - E1 passes 400 trials after the D0 to D3 fixes and the review fixes.
  - E3 enforcement shows 0 leaks for L1 to L5. L6 and L7 stay open for S4.
  - New CI: no Windows, no CI on pushes to `staging`, orchestration and desktop checks, a CI gate, and pinned actions.
- **Decision: no TLS in the Loop Runtime now.** The maintainer decided today that Phase 0 adds no `wss://` and no certificates.
  - S3.8b part 1 is replaced by SSH-tunnel documentation. Remote access is an SSH tunnel to the Runtime's loopback WebSocket listener (`docs/runtime.md`).
  - S3.8b part 2 (URL or QR remote control) moves to Phase 2 as a relay. The Runtime opens an authenticated outbound connection to the Soket edge gateway. A phone or browser pairs by QR with a one-time code. No inbound port and no certificates on user machines.
  - Optional later: `--tls-cert` and `--tls-key` for self-hosters, if users ask.
  - The security rules stay: non-loopback binds are refused, a token is required even on loopback, and host-only methods are refused for WebSocket clients.
- **Linux live check is postponed.** A Linux machine comes later.
- **Q1b analysis is done** (`docs/probes/q1b-loop-failure-analysis.md`). Fixes are in progress.
- **Current wave A tasks:**
  - split of PR #105;
  - Loop correctness fixes (block list, silent output-limit stop, permission hints, TUI running state);
  - E3 data-leak arm on GLM-5.3;
  - S4 design and ADRs;
  - PR triage;
  - parity list;
  - these documents.

## Status on 2026-11-13: Loop web

Loop web has a two-stage plan now: a beta on 20 November from the tested web app code (a chat pool and an on-demand agent pool), and then the shared cells of Phase 2. The design, the changes in the core, the cost estimate and the workstreams are in `docs/design/loop-web.md`. The S4 design lists 11 open questions (not 13).

## Principles of the plan

- **Measure first, but cheaply.** Each step has a gate with numbers. A failed gate stops the step.
- **Test budget rule.** Each test run completes in 15 to 30 minutes. Tests use the `faux` provider (no tokens) where possible. Tests that need a real model use GLM-5.3 (Soket or OpenRouter). No test needs OpenAI or Anthropic tokens. A test that cannot obey this rule is made smaller, not longer.
- **Keep the vision order, with four changes** (see "Changes to the roadmap").
- **Do not make one Runtime large. Make many Runtimes.** Scale comes from many small Runtime nodes ("cells") behind a router. Each Session has one owner node.
- **Put the expensive seams in early.** Identity fields, the storage driver and the Workspace driver cost little now and much later.
- **Inference capacity is outside Loop, but it gates everything.** The Soket endpoint has timeouts today. Treat it as a tracked dependency.

## Changes to the roadmap

1. **Add Track Q (quality parity) now.** The roadmap has no step for agent quality. It is the largest parity risk.
2. **Move identity fields forward to S4.** Add `principal` (person or Service Identity) and `tenant` to the protocol, the Event Log and the Policy decision record in S4. OIDC, SCIM and Admin stay in Step 5.
3. **Move the TypeScript SDK forward to Step 2.** The Web Surface must use only the public protocol. Thus the Web client *is* the first SDK. Build it as a package.
4. **Add remote control by relay to Phase 2.** The Runtime gets no TLS in Phase 0. Remote access uses an SSH tunnel until the relay exists. URL or QR control comes through the relay (see Phase 2).

## Phase 0: make the base true (about 1 week)

| Task | Done when |
|---|---|
| CI on `staging` | `ci.yml` also runs on pushes to `staging`. Linux tests run on each push. |
| Linux live check | One full live session on Linux (TUI, Runtime, sandbox). Record the result. |
| E1 recovery and ownership | `faux` provider. As many trials as 20 minutes permit (target 200): 0 lost acknowledged changes, 0 duplicate effects. Result in `docs/probes/e1-results.md`. |
| E3 enforcement and data leaks | Enforcement: 100 cases with `faux` (no tokens). Data leaks: decrease from 120 attacks to 24 attacks and 6 controls on GLM-5.3. |
| E7 turn queue | `faux` provider. 1,000 schedules: 0 deadlocks, 0 lost permits. |
| Merge | Merge `staging` to `main` after E1 and E3 pass. (`docs/loop-vision` stays a separate documents branch.) |
| Triage | Decide on the 10 open PRs. Divide PR #105: keep the document tools and `ToolCallPreparing` as small PRs on `staging`; remove its `session/fork` change. Fix the settings bug from #106 (`project_overlay` in `settings.rs`). |
| S3.8b | Part 1 (TLS) is replaced by SSH-tunnel documentation: done when `docs/runtime.md` has the recipe. Part 2 (connection by URL or QR code) moves to Phase 2 as a relay. |

## Track Q: quality parity (starts in Phase 0, continues always)

**Q1. Parity checks (small and cheap).** Three parts, in this order:
- **Q1a. Feature list (no tokens, no run time).** A table in the repository: each Claude Code and Codex feature, and the Loop status (yes, partial, no). This is the main parity record. Update it in each slice.
- **Q1b. Use the data that exists (no tokens).** Read the saved trials of run A in `loop benchmarks/report/run-a`. The 12 incorrect Loop trials already show where Loop fails.
- **Q1c. Smoke set (15 to 30 minutes, GLM-5.3 only).** The 5 tasks where Loop failed (REF-01, FEAT-02, FEAT-04, FEAT-05, TOOL-01) plus 5 tasks that passed. One repetition, all trials in parallel, with the existing harness. Run it after each Track Q change. It is a regression signal, not a statistic.
- **Optional, one time only:** point Claude Code and Codex at the same GLM-5.3 endpoint (Anthropic-compatible and OpenAI-compatible URLs) and run the same 10 tasks. This compares the harnesses with no expensive tokens. If the endpoint formats do not agree in 1 hour of work, stop and do not do this part.
- Gate: Loop passes 10 of 10 smoke tasks in 3 runs one after the other.

**Q2. Fix the correctness gap.** Use the result of Q1b. Find the cause. Probable work in `crates/loop-agent` (`agent_loop.rs`, system prompt, tools):
- verify before the agent reports completion (run tests, read the diff);
- connect automatic compaction (`should_compact`) and test it with E9;
- add a background shell tool and an `apply_patch` tool;
- fix the known gap: after a cancelled tool, Loop still sends the next model request.

**Q3. Feature parity list** (order by benchmark effect and user demand):
file checkpoints and rewind · hooks that can change arguments · custom Subagent definitions · git worktrees per Session · a light Linux sandbox (Landlock and seccomp; Podman stays for strong isolation) · `--print` through the Runtime · Loop as an ACP agent (gives Zed and JetBrains support) · VS Code extension · GitHub Action and PR review · plugins with a registry · Python SDK.

Run Q1c again after each group of changes. Keep the feature list (Q1a) current.

## Phase 1: finish the foundation, ready for scale (about 3 to 4 weeks)

**S4 Policy** (as in the roadmap): Capabilities, one gate for the TUI, Runtime, Subagents, ACP and `/workflow`, layers and Locks. Build on `crates/loop-app-core/src/permissions/` and `ToolGate`. Add `principal` and `tenant` to each decision record.

**Scale seams in `crates/loop-runtime`** (single-node behaviour stays the same):

| Change | Where | Why |
|---|---|---|
| Do not commit each text chunk | `publish()` in `runtime.rs` | Send deltas live only. Commit durable events (message complete, tool call, tool result, decision) in batches. A reconnect gets the last durable state plus the live tail. |
| Separate readers from the writer | `log/sqlite.rs` | A large replay must not stop appends (E6). |
| Page all history reads | `read_after`, `logged_revision`, `last_activity_ms`, Subagent lookup | Keep a small projection table for the last revision and the last activity. |
| One channel per Session | the global `broadcast(4096)` and `forward()` | A slow client gets a refill from the log. Loop does not close the connection. |
| Bound all queues | Session command channel, wire control channels | Back-pressure in place of memory growth. |
| One store for truth | `events.db` and `sessions.db` | E1 decides: make the Session tree a projection of the Event Log. |
| Storage driver | `EventLog` trait | Add a Postgres implementation with the same test suite as SQLite. |
| Workspace driver | new trait from `Sandbox`, `SandboxFactory`, `ExecutionEnv` | Drivers: local, worktree, container, microVM. |
| Fenced leases | Session leases (`003_session_leases.sql`) | Each write carries a lease epoch. The store rejects a stale owner. |
| Hibernate and wake | the unload path of `LiveSession` | Unload an idle Session to the log. Wake it on attach. |

Gates: E6, E8, E9, and a new single-node load test: 1,000 loaded Sessions and 200 active turns with a fake provider, added delivery delay p95 below 50 ms, memory stable.

## Phase 2: Web v1 and the cloud Runtime (about 6 to 8 weeks)

**Architecture for volume:**

```
Browser / TUI / SDK ── wss ──> Edge gateway (stateless: TLS, auth, rate limits)
                                   │ finds the owner of the Session (lease table)
                                   ▼
                        Runtime nodes (cells; each owns N Sessions by fenced lease)
                          │               │                    │
                    Postgres          Object storage      Workspace pool
                 (Event Log, split    (Artifacts, old     (microVMs from snapshots,
                  by tenant/Session)   log segments)       scheduler per region)
                          │
                 Provider gateway (keys, quotas, retries, metering) ──> Soket / other models
```

- A node failure: the lease expires, a different node takes the Session and starts from the log. Interrupted turns show as uncertain (the S2 recovery rule).
- Most Sessions are idle. Hibernation makes cost follow active turns, not total users.
- The closed part (billing, quotas, abuse) stays in the gateway and the provider gateway, as the vision says.

**Product:**
- TypeScript SDK (`loop-protocol` client) first, with types from the protocol schema. The Web Surface is the `web/` UI of `loop-web-agentic-app`, moved onto the SDK (see the evaluation below). Do not write a new UI.
- Chat parity: streaming, file upload, web search, projects, share links, artifacts and previews.
- Agent mode parity (Claude Code on the web, Codex cloud): connect a GitHub repository, run a task in a microVM, show the diff, open a PR, run tasks in parallel, steer from a phone.
- Google sign-in for identity only.
- Remote control by relay (replaces S3.8b part 2). The Runtime opens an authenticated outbound connection to the Soket edge gateway. A phone or browser pairs by QR with a one-time code. No inbound port and no certificates on user machines. Until then, remote access is an SSH tunnel.

Gates: N2 to N6 in the table below pass on 3 local nodes with the `faux` provider. Then a closed beta. Real volume data comes from the beta, not from a large test cluster.

## Evaluation of `soketlabs/loop-web-agentic-app`

**Verdict: keep the web UI, change the gateway, do not host it for many users as it is.**
The direction is correct: the app uses the Runtime through `loop-wire` JSON-RPC and keeps the chat history in Loop. But it is a design for one machine, and it goes around the protocol in several places.

What it is: a Rust gateway (axum, about 6,000 lines, SQLite) and a Next.js 15 web app (about 8,000 lines). The gateway starts one `loop runtime` and one rootless Podman container for each user project. It builds only with the branch of PR #105, and that PR conflicts with `staging`.

| Part | Decision | Reason |
|---|---|---|
| `web/` UI (chat, composer, approvals, previews, projects, admin, mobile) | **Keep** | It already has most of the chat parity features. Reconnect with `after_seq` is correct. |
| `key_proxy` (provider keys do not go into the sandbox) | **Keep, move into Loop** | This is the Connector idea of the vision. It becomes the provider gateway. |
| `safe_fs` and the Podman profile | **Keep for now** | A temporary Workspace driver until microVMs exist. |
| Document tools and `ToolCallPreparing` from PR #105 | **Keep, as separate small PRs on `staging`** | The remaining part of PR #105 conflicts. Its `session/fork` is not compatible with `staging`. |
| Settings through files and a Runtime restart | **Replace** | `staging` has `session/set_model`, `set_effort`, `permission_mode`, `session/rename`, `workspace/settings`. |
| `!` commands and completion through `podman exec` | **Replace** | Use `session/shell`. Add protocol methods for completion and for file upload and download. |
| The gateway owns the Runtime lifecycle, accounts and isolation | **Replace** | In the vision, the Runtime and the Workspace driver own these. The gateway becomes stateless: sign-in, routing, limits. |
| The hand-written JSON client with no types | **Replace** | Generate TypeScript types from `schema/loop-protocol.json`. This is the first SDK. |
| Each chat starts as a full agent | **Change** | The vision starts with plain chat. Agent mode is an option. |

Limits today: tens of active users for each host (one Runtime of up to 2 GB for each project, 200 Runtimes maximum). A user must always go to the same host. No failover.

Security items to correct before any public use:
1. The login token is in `localStorage`. Use an HttpOnly cookie only, and keep only a hash in the database.
2. The app shows docx and pptx previews in its own page. Show them in a sandboxed iframe.
3. The tool switches are in `.loop/settings.json`, in a folder that the agent can write. Enforce them in the Runtime (S4).
4. Containers have no egress filter. Host mode has no isolation and can show the provider key.
5. A deleted chat stays in `sessions.db`.
6. The first visitor of a new installation becomes the admin.

**Migration path (changes Phase 2):**
1. Now: move the app to the `staging` protocol (the fork call, the setters, `session/shell`). Add CI and contract tests with the protocol schema. Correct security items 1, 2 and 6. Result: an internal demo on one host.
2. With S4 and the Phase 1 seams: add `principal` and `tenant`, Policy in the Runtime, the Workspace driver (Podman first), protocol methods for files and completion.
3. Phase 2: connect to Runtime nodes by `wss://`. Make the gateway stateless (Google sign-in, Postgres, object storage). Add plain chat as the default. Then microVMs.

## Phase 3 and later (order as in the vision)

- **Swarms v1.** Only after E2 shows that the primitives earn their cost, and after E5. Build the Task Board, Mailboxes, Leases and budgets on the Runtime. Move `/workflow` steps to real Sessions.
- **Desktop and integrations.** Desktop becomes a Runtime client and enters PR CI. Agent Definitions, Targets, Connectors (after E4), Relays.
- **Org.** OIDC and SCIM, Admin v1, Managed Policy (after the H11 probe), Deployment Profiles, Air-gapped.

## Parallel work, complexity and models

**Complexity rule.** A task is **High** if it changes safety, ownership, concurrency, the protocol shape, or needs a design decision. Give it to **Opus**. All other tasks are **Low**. Give them to **Sonnet**. Opus reviews each Sonnet change in `crates/loop-runtime` and `crates/core`. Do not delegate to a Fable model.

**Conflict rule.** Two agents must not change the same large file at the same time. The files with the most risk are `crates/loop-runtime/src/runtime.rs` (2,800 lines), `crates/loop-agent/src/agent_loop.rs` and `crates/loop-cli/src/app.rs` (7,100 lines). Tasks on the same file go in one chain, one after the other.

"Build" = the task compiles the Loop Rust workspace on this machine.

### Phase 0

| Task | Complexity | Model | Build | Wave |
|---|---|---|---|---|
| E1 recovery probe | High | Opus | Yes | 0A |
| E3 enforcement and data-leak probe | High | Opus | Yes | 0A |
| S3.8b SSH-tunnel documentation (replaces TLS) | Low | Sonnet | No | 0A |
| Q1b cause analysis of the 12 incorrect trials | High | Opus | No | 0A |
| CI on `staging` (`ci.yml`) | Low | Sonnet | No | 0A |
| Q1a feature list | Low | Sonnet | No | 0A |
| Q1c smoke set (in `loop benchmarks`) | Low | Sonnet | No | 0A |
| Web app: move to the `staging` protocol, add contract tests | Low | Sonnet | Small (gateway only) | 0A |
| Web app: token in a cookie, preview in an iframe, first-admin rule | High | Opus | Small | 0B |
| E7 turn-queue probe | Low | Sonnet | Yes | 0B |
| Settings bug from #106 (`project_overlay`) | Low | Sonnet | Yes | 0B |
| Document tools and `ToolCallPreparing` from PR #105 as small PRs | Low | Sonnet | Yes | 0B |
| S3.8b connection by URL or QR code: moved to Phase 2 (remote control by relay) | — | — | — | — |
| Linux live check | Low | Sonnet | On a Linux machine | 0B |
| PR decisions, merge of `staging` to `main` (after E1 and E3) | — | Maintainer | — | End |

Wave 0A has 3 build agents and 5 agents with no build. Wave 0B starts when a build slot becomes free.

### Track Q

| Task | Complexity | Model | Chain |
|---|---|---|---|
| Verify before completion | High | Opus | Q-loop (`agent_loop.rs`), first |
| Automatic compaction | High | Opus | Q-loop, second |
| No model request after a cancelled tool | Low | Sonnet | Q-loop, third |
| Background shell tool | Low | Sonnet | Q-tools (new files), parallel |
| `apply_patch` tool | Low | Sonnet | Q-tools, parallel |
| Hooks that change arguments | Low | Sonnet | Parallel |
| Custom Subagent definitions | Low | Sonnet | Parallel |
| `--print` through the Runtime | Low | Sonnet | Parallel |
| File checkpoints and rewind | High | Opus | After the Q-loop chain |
| Light Linux sandbox (Landlock, seccomp) | High | Opus | Parallel (sandbox files only); test on Linux |
| Loop as an ACP agent | High | Opus | Parallel (`extend/loop-acp`) |
| Plugins with a registry | High | Opus | Later |
| VS Code extension, GitHub Action, Python SDK | Low | Sonnet | No Rust build; after the SDK |

Track Q uses 1 build slot at a time.

### Phase 1

First step, alone: **divide `runtime.rs` into modules with no behaviour change** (Low, Sonnet). This decreases conflicts for all later chains. Also first: the S4 design doc and the three ADRs (High, Opus, no build).

| Chain | Tasks in order | Complexity | Model |
|---|---|---|---|
| S4 (`loop-app-core/src/permissions/`) | Capabilities model → one gate on all paths → layers and Locks | High | Opus |
| | Presets from the permission modes, `principal` and `tenant` fields, tests | Low | Sonnet |
| A: streaming (`runtime.rs`, `loop-wire/server.rs`) | No commit for each chunk → one channel for each Session → hibernate and wake | High | Opus |
| | Bound all queues | Low | Sonnet |
| B: log (`loop-runtime/src/log/`) | Separate readers → one store for truth (after E1) → Postgres driver | High | Opus |
| | Paged reads and the projection table; the shared driver test suite | Low | Sonnet |
| C: Workspace | Workspace driver trait | High | Opus |
| | Drivers: local, worktree, Podman container | Low | Sonnet |
| D: leases (`loop-agent` session store) | Fenced leases | High | Opus |
| Tests | Load test tool with `faux`; E6, E8, E9 probes | Low | Sonnet |

Run 3 chains at a time: S4, A and B first. Then C and D. Chain A and chain B must agree on the event format first: the Opus agent of chain A writes that ADR before the two chains start.

### Phase 2

| Task | Complexity | Model | Build | Parallel |
|---|---|---|---|---|
| TypeScript types from the schema, SDK client | Low | Sonnet | No | First |
| Web UI onto the SDK (chat, composer, files, admin: one agent each) | Low | Sonnet | No | After the SDK, 4 in parallel |
| Plain chat as the default, Google sign-in, sandboxed previews | Low | Sonnet | No | Parallel |
| Protocol methods for files and completion | Low | Sonnet | Yes | Parallel |
| Stateless gateway and Session routing | High | Opus | Yes | Parallel |
| Remote control by relay (outbound Runtime connection to the edge gateway, QR pairing with a one-time code) | High | Opus | Yes | After the gateway |
| Provider gateway (from `key_proxy`) | High | Opus | Yes | Parallel |
| microVM Workspace driver | High | Opus | On Linux | Parallel |
| Repository connection, diff view, PR flow | High | Opus | Small | After the gateway |
| N2 and N3 failure tests | High | Opus | Yes | After the gateway |
| N4, N5, N6 measurements | Low | Sonnet | Yes | After the drivers |

Most of Phase 2 has no Rust build. Thus 6 to 8 agents can work at the same time.

### Phase 3 and later

Size these tasks when their design docs exist. Design docs, the Task Board logic, Connectors and identity are High. UI pages, drivers from an existing pattern, and tests are Low.

## One build folder for all agents

Facts about this machine (2026-11-11): 103 GB free disk, 24 GB memory, 12 cores. The shared folder `/Users/anandgupta/Soket/Code/LOOP/loop-wt/target` already has 46 GB (40 GB in `debug`). One more full build folder is about 40 GB. Thus a build folder for each agent is not possible.

**Yes, all agents can and must use the same folder.** Rules:

1. Each agent sets `CARGO_TARGET_DIR=/Users/anandgupta/Soket/Code/LOOP/loop-wt/target` and `CARGO_INCREMENTAL=0` (as in `AGENTS.md`). No agent makes a `target/` folder in its worktree.
2. Cargo locks the folder during a build. A second build waits. This is correct: it also keeps memory use low. But it means build agents take turns. **Use 3 build agents at most at the same time.** Agents with no build have no such limit.
3. Dependencies are compiled one time for all worktrees. Cargo keeps separate results of the Loop crates for each worktree path, so the folder grows with each new worktree. Check this with the first 2 agents (assumption N11).
4. **Clean rule.** Before Phase 0 starts, clean the folder one time (`cargo clean` on the shared folder) to get back about 40 GB. Clean again when free disk is below 40 GB, at a time when no agent builds. Remove each worktree when its branch is merged (`git worktree remove`).
5. The clippy folder (`ci199`) and the desktop check stay in the shared folder as today. Only one agent at a time runs the desktop check (nightly toolchain, large).
6. The web gateway uses the same `CARGO_TARGET_DIR`. The web UI uses one `pnpm` store for all checkouts.
7. Each agent uses its own small `LOOP_CODING_AGENT_DIR=<temp folder>` for live tests (sockets and databases must not be shared). The agent deletes it at the end.
8. Do not run a load test or a timing probe (E6, E7, N4, N5) during a build of a different agent. The numbers become wrong. The supervisor gives these tests a quiet slot.
9. Benchmarks with containers run on the cluster, not on this machine.
10. Set the test thread count to 4 (`cargo test -- --test-threads=4`) when 2 or more agents run tests.

## Where the documents are

The `docs/loop-vision` branch is the scratchpad and the store for documents. This plan is `docs/execution-plan.md` on that branch. Probe results (`docs/probes/`), the feature list and the ADR drafts also go there. Do not merge this branch into `staging`. Copy a document to `staging` only when a code step needs it.

## Assumptions and their tests

| # | Assumption | Test | Stops what, if false |
|---|---|---|---|
| A1 | The Event Log can be the one source of truth after a crash | E1 | Merge to `main` |
| A2 | Policy can enforce "off means unavailable" on all paths | E3 | S4 design, safety claims |
| A3 | The hub adds little delay | E6 | Runtime as default |
| A4 | The turn queue has no deadlock | E7 | Runtime as default |
| A5 | Two Surfaces always converge | E8 | Web live views |
| A6 | Long Sessions stay correct and memory stays stable | E9, made smaller: 10,000 scripted turns with `faux`, plus one real task of 20 minutes on GLM-5.3. No 2-hour tasks. | Long-task claims |
| A7 | Swarm primitives give better results per cost | E2, made smaller: 6 issues × 3 arms × 1 seed on GLM-5.3, in parallel (18 attempts, not 72). A clear loss stops Swarms. A small difference means: build only Subagent steering. | Swarms v1 |
| A8 | Native and ACP agents can share one coordination layer | E5 | "Every Backend joins" |
| A9 | Connectors keep credentials out of the Workspace | E4 | Targets |
| N1 | The Loop harness is as good as Claude Code and Codex | Q1a feature list and Q1c smoke set (GLM-5.3, 15 to 30 minutes) | The parity claim |
| N2 | Live deltas without a commit do not lose state | Kill a node during streaming, 200 trials: the client gets a correct state, 0 duplicates | The streaming change |
| N3 | Fenced leases prevent two owners | Pause and partition tests on 3 nodes, 1,000 schedules: 0 writes from a stale owner | Cluster mode |
| N4 | Postgres carries the log volume | Load test with `faux` and a local Postgres, 10 minutes: 2,000 active Sessions, commit p99 below 100 ms. Calculate the limit for each node from this. | Storage design (else log segments per Session) |
| N5 | Wake from hibernation is fast | Attach to an idle Session: p95 below 300 ms without a Workspace, below 3 s with a microVM from a snapshot | Cost model |
| N6 | microVM cost per Session is acceptable | Measure start time, memory per idle VM, density per host for 2 technologies (Firecracker, krun) | Agent mode price |
| N7 | Model capacity can serve the volume | A 15-minute burst on the Soket endpoint with short prompts: error rate below 1%, stable first-token time | Public launch |
| N8 | The protocol is stable enough for an SDK | Golden wire tests; the Web Surface builds with only the SDK; an old client works with a new Runtime | SDK release |
| N9 | One maintainer with agents can review this volume | Track lead time and escaped defects for each slice in Phase 0 and 1 | The schedule; add people or reduce scope |
| N11 | One shared build folder works for parallel worktrees | The first 2 build agents record: the folder growth for each worktree, and the wait time for the lock (10 minutes) | The limit of 3 build agents |
| N10 | Users accept Web chat as equal to the competitors | One person does a fixed list of 20 tasks in Loop Web and records each result (30 minutes) | Web v1 launch |

Scale targets are proposals until N4 to N7 give data: 100,000 concurrent Sessions per region, linear growth with nodes, 0 lost acknowledged events.

## Process

- Keep the rules in `AGENTS.md`: one branch and one worktree for each task, tests first, a design doc before each step, results in `docs/probes/`.
- Each slice ends with: tests, a live check, an update of `docs/roadmap.md`.
- Record each large decision as an ADR. New ADRs: streaming durability, cell architecture, identity fields in S4.
- Delegation rule: give delegated work to Opus or Sonnet agents only. Do not delegate to a Fable model. High complexity goes to Opus, low complexity to Sonnet.
- Parallel limits: see "Parallel work, complexity and models" and "One build folder for all agents". Review capacity of the maintainer is the second limit.

## First actions after approval

0. Clean the shared build folder one time. Set `CARGO_TARGET_DIR` for all agents.
1. Add `staging` to `ci.yml`. Run the Linux live check.
2. Start wave 0A: probes E1 and E3 and the TLS task in separate worktrees (`probe/e1-…`), plus the tasks with no build.
3. Write the feature list (Q1a). Read the 12 incorrect benchmark trials and write the cause analysis (Q1b).
4. Make the 10-task smoke set (Q1c) from the existing harness.
5. Write the three new ADRs and update `docs/roadmap.md` and `docs/stress-tests.md` with Track Q and N1 to N10.

## Verification of this plan

- Each probe writes `docs/probes/<id>-results.md` with the gate, numbers, seed, commit and decision.
- The feature list (Q1a) is in the repository. The smoke set (Q1c) runs again after each Track Q group.
- Each test obeys the test budget rule: 15 to 30 minutes, `faux` or GLM-5.3 only.
- The repository checks stay green: `cargo fmt --all -- --check`, clippy with `-D warnings`, `cargo test --workspace --exclude loop-desktop --locked`, and the desktop check.
- The load tests (single node, then 3 local nodes) use `faux` and run for 10 minutes. Run them before each merge to `main`, not each night.
