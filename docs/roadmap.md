# Roadmap and status

Where Loop stands and what is left. Read this first when you start work. The
direction is in [vision.md](vision.md), the reasons are in the [ADRs](adr/),
and the experiments that should run before each step are in
[stress-tests.md](stress-tests.md).

Last updated: 2026-11-04. Update the status columns at the end of every slice.

**Branch note.** This file lives on `docs/loop-vision`, which is not merged.
The code it describes is on `staging` (merged into `main` by the maintainer).
The `staging` design docs it names, such as `docs/design/tui-client.md`, are
not on this branch. Read them with `git show staging:docs/design/tui-client.md`.

## Where Loop is

| Step | Name | State |
|---|---|---|
| S1 | Wire protocol (`loop-protocol`, `loop-wire`) | Done, on `main` |
| S2 | Runtime, Event Log, first Subagents, `loop session` | Done, on `main` (#95) |
| S3 | The TUI is a Runtime client | Built and on `staging` (`1dc7ef4`); the maintainer is testing it |
| S3.8b | TLS and QR remote | Next |
| S4 | One Policy gate | After S3.8b |
| Step 2 | Web v1 | Planned |
| Step 3 | Swarms v1 | Planned; gated by the stress tests |
| Step 4 | Desktop and integrations | Planned |
| Step 5 | Org | Planned |

### What S3 delivered

- Plain `loop` starts a Runtime or reuses the running one, and opens a new
  Session. `--continue` and `--resume` attach to a Session, and
  `--connect <socket|ws://…>` picks a Runtime. An explicit `--connect` never
  falls back to in-process.
- The in-process path is kept for `--print`, ACP agents (ChatGPT via Codex) and
  a Runtime that fails to start. The TUI shows a warning then.
- Each Session runs tools with the cwd and environment of the shell that opened
  it. Over `ws://` only a safe list of variables is sent
  (`runtime.forwardEnv` adds more). The environment is never logged.
- An auto-started Runtime stops when idle (`runtime.idleShutdownMinutes`,
  default 10). It is not idle while a Session is busy, an approval is parked, a
  Subagent runs or a Surface is attached.
- Two TUIs can share one Session. Setters, `ConfigChanged` and approvals reach
  both.
- `/model`, the sandbox setting and `/skill` apply from the next turn when a
  turn is running.
- A failing slash command shows one line in chat and does not close the TUI.
- macOS Seatbelt sandbox, one sandbox builder, custom-provider reasoning
  (`thinkingFormat`), WebSocket listener on loopback with a required token.
- Windows support is dropped. Loop supports Linux and macOS only.

## Tasks from S3.8b

Sizes are given only for the next slices. Size the later steps when their
design docs are written, as S2 and S3 were.

### Step 1: finish the foundation

| Task | Done when | Size |
|---|---|---|
| S3.8b TLS | `wss://` with rustls. Binding beyond loopback is allowed only with TLS and a token. Certificate pinning or a trusted CA. Token rotation. | 2–3 days |
| S3.8b connect by URL or QR | `loop runtime share` prints a one-time pairing URL and QR code with a short-lived token. A phone or second laptop attaches to a Session. | 1–2 days |
| S4 Capabilities | Every switchable feature is a Capability: off, ask or on, with a scope (paths, domains, models, servers). Today's permission modes become presets. | 1–2 weeks |
| S4 one Policy gate | TUI, Runtime, Subagents and ACP agents all pass the same gate. Off means unavailable: the tool is removed and the Workspace loses the route. | with S4 |
| S4 layers and Locks | Managed → Org → Group → User → Project → Agent Definition → Session. Layers only tighten. Any Capability can be Locked. Decisions are in the Event Log. | with S4 |

### Step 2: Web v1

| Task | Done when |
|---|---|
| Cloud Runtime | The same Runtime binary runs as a hosted service, one tenant per account, behind the public protocol. |
| Web Surface | Uses only `loop-protocol`. Sessions start as plain chat. Google sign-in is identity only. |
| Agent mode | A Session can get an isolated microVM Workspace with agent tools. Org Policy can switch it off. |
| Native Provider adapters | OpenAI, Anthropic Messages, Gemini (API keys only), Bedrock/Vertex, the Soket provider, local inference. OpenAI-compatible stays the catch-all. |

### Step 3: Swarms v1

Gate: experiment E2 in [stress-tests.md](stress-tests.md) must show the
Swarm primitives earn their cost before this step is built.

| Task | Done when |
|---|---|
| Orchestration in the Runtime | `/workflow` steps become real Sessions: listed, attachable, recovered after a crash. (`/workflow` is behind the `orchestration` cargo feature today.) |
| Task Board | Tasks with dependencies, owners and status. `claim_task` is atomic. |
| Mailboxes | Per-Session inbox plus broadcast, for agents and people. Every message is logged. |
| Leases | Time-limited exclusive claims on paths or resources. A dead worker's lease expires and its task reopens. |
| Shared Artifacts | Files and notes referenced by id. |
| Workspace per agent | Each member gets its own worktree (container or microVM where Policy says), replacing one sandbox per folder. |
| Limits and steering | Token, cost and wall-clock budgets per Swarm, cut from the parent's. Pause, resume, edit the board, kill switch. |
| Every Backend joins | Swarm tools reach native agents directly and ACP agents through an MCP server the Runtime provides. |
| Event Log throughput | Batched commits across Sessions and coarser streaming events, so one Runtime carries hundreds of agents. The single writer task also serves every read today; split or page reads (see experiment E6). |

### Step 4: Desktop and integrations

| Task | Done when |
|---|---|
| Desktop on the Runtime | Installs on its own with a built-in Runtime. Switches between local and a remote personal Runtime. Checked in PR CI. |
| Agent Definitions | A manifest of instructions, Backend or model, tools, MCP servers, skills, Policy and Workspace needs. |
| Targets and Connectors | A Target is a real system. Connectors are MCP servers launched outside the Workspace with short-lived credentials from a Secret Store. |
| Relays | Connectors inside an organisation's network that dial out. No inbound ports. Works air-gapped. |
| TypeScript SDK | Third parties build Surfaces on the public protocol. |

### Step 5: Org

| Task | Done when |
|---|---|
| Identity | OIDC sign-in, SCIM Groups, agents act as the person who started them, Service Identities for unattended work. |
| Admin v1 | People and Groups, Policy with Locks, Providers and keys, Targets and Connectors, usage and cost, audit search and SIEM export, live Swarm view with kill switches. |
| Managed Policy | An OS-protected file or device-management profile that the Desktop's Runtime obeys. |
| Deployment Profiles | Personal, Team, Enterprise, Read-only Analyst, Air-gapped. |
| Cluster mode | Postgres log and state, object storage, stateless Runtime nodes, a Workspace pool. |

## Left over from S3

Small items. Any of them can be picked up on its own.

| Item | Where it stands |
|---|---|
| `loop --print --runtime` | Branch `s3/print-runtime` (off `b07a79f`) holds a task file only; no code. Planned for GPT-6-Luna. `--print` stays in-process by default. |
| Turn queue | The queue is a tokio `Semaphore` of 8 permits (`crates/loop-runtime/src/runtime.rs`). Missing: a load and chaos test, a visible "queued" state in `session list` and `runtime status`, and a mark for prompts that were still waiting when the Runtime crashed. Experiment E7 covers the test. |
| "Answered in another window" | When another TUI answers an approval, the dialog closes but chat says nothing. |
| Footer at start | A Session started with `--permission-mode plan` shows the local mode until the first mode change. |
| `/usage` over `ws://` | "Usage history hidden over WebSocket" is covered by unit tests only; not checked live (pane too small). |
| GLM thinking on Soket | Loop's side is done. `api.soket.ai/v1` rejects every thinking parameter, so the Soket server needs a change. |
| Linux | Only compiled in CI. Nothing has been built for Linux locally. The first `staging` → `main` merge is the first Linux check. |

## Before merging `staging` into `main` with the Runtime as default

Experiments E1 (recovery and ownership) and E3 (enforcement and exfiltration)
in [stress-tests.md](stress-tests.md) should pass first.
