# Loop web: hosted design and the plan for the 20 November beta

Status: plan of 2026-11-13. Nothing in this document is deployed. This document replaces the "Phase 2: Web v1" time estimate in `docs/execution-plan.md` with a two-stage plan. The Phase 1 work in that plan does not change; it is the base of stage 2.

## Context

Loop does not run on Windows. Loop web gives Windows users (and all others) access from a browser: a simple chat window, and an agent window where files and shell commands run in a sandbox. The service must accept lakhs of registered users and some thousands of active users at the same time, on GCP. Phone apps come later. The web app and desktop app code must be private.

Decisions of 2026-11-13: two stages; a sandbox only on demand; Google sign-in; free with quotas; base repo `loop-web-agentic-app`; one private repo for the web app and the platform. Open decisions: the region and the final address.

## Facts found (they change the plan)

| Topic | Fact |
|---|---|
| `loop.soket.ai` | It is the Loop **documentation** site. It is not a web app. |
| Web apps | No web app is public. `loop-web-agentic-app` is deployed nowhere. |
| Loop Runtime | One host, one user. No user or tenant identity. Sessions and events are in local SQLite. Default 16 sessions and 8 active turns for each process. Each text chunk is a database commit. |
| Tools | Tools always run on the machine of the Runtime. No remote sandbox driver. |
| Web app (`feat/beta`, local) | Ported to Loop `staging` and made safe for one user for each gateway. No file tree, no editor, no terminal panel. The shell is one command at a time (120 s limit, no PTY). Prompts are text only. SQLite, in-memory state, no Dockerfile, no CI, no OAuth code. |
| Loop gaps for chat | Automatic compaction has no caller. No memory across sessions. |
| Desktop | `crates/loop-desktop` (about 4,700 lines) is in the **public** Apache-2.0 repo now. |
| Access | A GCP project with GKE rights, an OAuth client and a DNS record for `app.loop.soket.ai` are not available yet. |
| Risks seen | The model backend had an outage in the week of the plan. The model capacity is a release gate. |

## Part 1: the ideal design (no date limit)

### Product
- **Chat mode:** a plain chat. Web search, file upload, document reading, images. No container.
- **Agent mode:** the same chat with a right panel: Files (tree and editor), Terminal (a real PTY), Preview (sites, documents). A sandbox starts when the user opens the panel or the model needs a command or a file change. It sleeps when idle and wakes with the same files.
- Projects, chat history and search, fork, share by a read-only link, memory across chats, usage and quota page, account export and delete, admin console.
- One public API (HTTPS and WebSocket, OIDC tokens). The web app, the phone apps and the desktop app use the same API.

### Architecture (GCP)

| Layer | Design | GCP service |
|---|---|---|
| Edge | TLS, WAF, rate limits, static web app on a CDN | Global HTTPS Load Balancer, Cloud Armor, Cloud CDN and a GCS bucket |
| Identity | Google now; Microsoft, email and phone later; tokens for phone apps | Identity Platform |
| Control plane | Stateless API: users, projects, chats, quotas, files, sharing, admin. Routes each session to its cell. | GKE or Cloud Run; **Postgres** (Cloud SQL HA, later AlloyDB); Redis (Memorystore) for limits and routing |
| Agent plane | **Shared Runtime cells.** One cell holds about 1,000 sessions from many users. A session is owned through a fenced lease. If a cell fails, another cell continues from the Event Log. | GKE, autoscaled |
| Event Log | Postgres, partitioned by tenant and session. Only complete events are stored; text chunks go out live. Old segments move to object storage. | Cloud SQL, GCS |
| Sandboxes | A pool of gVisor (or microVM) sandboxes with a warm pool. The cell runs tools in the sandbox through a remote Workspace driver. Sleep and wake from a snapshot. | GKE Sandbox on spot nodes; examine GKE Agent Sandbox and pod snapshots (not verified) |
| Files | Workspace snapshots, uploads, artifacts. User content is served from a separate domain. | GCS |
| Models | A provider gateway with a key, a budget and metering for each user. Keys never enter a sandbox. | LiteLLM in HA with Postgres and Redis, or the in-house console |
| Abuse control | Sandbox egress only through an allowlist proxy; CPU, memory, disk and time limits; sign-up limits | NetworkPolicy, egress proxy |
| Operations | Metrics, logs, traces, alerts, backups, a second zone | Cloud Monitoring, Cloud Logging |

**SQLite:** it stays for personal use (CLI, TUI, desktop: one cell on one machine). The hosted service uses Postgres and GCS. SQLite is not used for hosted data.

### Changes in the Loop core (public repo)
1. `Runtime::new` accepts an injected `SessionStore`, `EventLog` and sandbox factory (it hard-codes SQLite now).
2. Identity: `principal` and `tenant` on each connection, Event and store read (ADR-0009).
3. Streaming: commit complete events only; one channel for each session; bounded queues (ADR-0007).
4. A Workspace driver trait with a remote driver. `FileSystem::cwd()` must not assume a local path.
5. Fenced leases; hibernate and wake of idle sessions.
6. Protocol additions: file list, read, write, upload, download; a PTY stream; usage events for non-host clients; limits for each session.
7. Connect automatic compaction. Add memory across sessions.
8. A TypeScript SDK generated from the protocol schema.

### Repos and IP
| Repo | Visibility | Content |
|---|---|---|
| `soketlabs/loop` | Public, Apache-2.0 | Core, CLI, TUI, protocol, the seams of items 1-8 |
| `loop-web` (now `loop-web-agentic-app`) | Private | **One repo for now** (decision of 13 Nov): `web/` (UI), `gateway/`, `platform/` (router, control plane, later the Postgres Event Log, remote sandbox driver and cell binary), `infra/` (Terraform, Kubernetes). Split out `loop-cloud` when the phone apps or a second team start. |
| `loop-desktop` (new) | Private | The desktop app, moved out of the public repo. It uses the public crates by a pinned git revision. |

Limit: the desktop code that is public today stays in the public history under Apache-2.0. The move protects only future work.

### Cost at 10,000 users active at all times (24 hours, 730 hours a month)

**Not verified.** The unit prices are approximate (e2-standard-8 in Mumbai about $0.32 an hour on demand, about $0.11 spot; a search did not confirm them). The GCP pricing calculator must confirm them. Assumption: 20 % of the active users have an open sandbox (2,000 sandboxes of 0.25 vCPU and 1 GB).

| Item | Ideal design | Beta design at this size |
|---|---|---|
| Chat compute | 10-15 cells: about $2k | 8,000 chat units as processes (about 150 MB each, not measured): about $9k |
| Sandboxes (spot, gVisor) | about $6k | about $6k |
| API, WebSocket and router nodes | about $1k | about $1k |
| Postgres (Cloud SQL HA, 16 vCPU, 1 TB, one replica) | about $3k | about $1k (small: no Event Log) |
| Redis, load balancer, Cloud Armor | about $1k | about $1k |
| GCS (about 60 TB for 3 lakh users) and network egress | about $2.5k | about $2.5k |
| LiteLLM in HA, logs, monitoring | about $1.5k | about $1.5k |
| **Total for each month** | **about $15k-18k** | **about $20k-25k** |

- Sensitivity (ideal design): 10 % with a sandbox gives about $12k; 40 % gives about $24k. The sandbox share is the main cost driver.
- For comparison: one pod for each active user (the rejected design) is about $35k at this size.
- A 1-year committed-use discount lowers the non-spot part by about 25-35 %.
- **The model cost is much larger.** 10,000 active users at one turn each 2 minutes is about 80 turns a second, or some tens of thousands of output tokens a second, all day. This needs a GPU fleet (rough: 15-30 or more large GPUs for a 27B model) or a token bill that is 10 or more times the infrastructure cost. A measured model throughput is a release gate.
- A price check and a memory measurement of one chat unit are tasks before any public cost statement.

Time for the ideal design: about 8 to 10 weeks with parallel agents, because the security and failure tests cannot be shortened.

## Part 2: what is possible by 20 November (7 days, parallel agents)

Agents make the code fast. They do not shorten: GCP access, DNS, the OAuth client, the model capacity, and a security review by a person.

### Design for the beta: one "user unit", two pools
A **user unit** = the gateway in pod mode + the Loop Runtime + the user's data directory. This is the code that is tested now.

| Pool | Content | Use |
|---|---|---|
| **Chat pool** | Many user units as processes in one pod (target 100 or more). Shell, file edit, MCP, hooks and skills are locked off. | Plain chat. **No container for each user.** |
| **Agent pool** | One unit in one gVisor pod on spot nodes, with a warm pool. Idle stop after 5 to 10 minutes. | Starts only when the user opens agent mode. The unit moves (save, restore) in some seconds. |

- A new small **router** owns sign-in results, the user table, the unit location, quotas and the wait screen. Data: **Postgres (Cloud SQL)** from day one.
- User data is saved to GCS: each 60 s when changed, at stop, and at pod termination. **Loss window: up to 60 s of work** if a node stops suddenly.
- Sign-in: Google through oauth2-proxy (already integrated). Identity Platform comes in stage 2.
- Models: LiteLLM, one virtual key and budget for each user.
- Address: `app.loop.soket.ai`. The docs stay at `loop.soket.ai`.

**Go/no-go on 15 November:** measure the memory of one chat unit and the lock of the tools.
- Pass: the beta has on-demand sandboxes as above.
- Fail: fallback = agent pool only (one small pod for each active user) with a hard limit of about 300 users at a time and a wait screen.

**Go/no-go on 16 November:** GCP access. If there is no cluster, the fallback is one large VM with Podman and `runsc` (the gateway supports this now), for a small invite beta.

### Workstreams (maximum 3 agents that build Rust at the same time)

| # | Work | Model | Days |
|---|---|---|---|
| W1 | Push `feat/beta` (remove `web/tsconfig.tsbuildinfo` first); add `platform/` and `infra/` in the same repo; CI | Sonnet | 14 |
| W2 | Router and unit supervisor: start, stop, move, idle, wait screen, user table in Postgres | Opus | 14-17 |
| W3 | GCS save and restore with a safe SQLite checkpoint; termination handler | Sonnet, Opus review | 14-16 |
| W4 | One image (gateway, `staging` Loop, `web/out`, tools; non-root, read-only root); Terraform and Kubernetes files: GKE Standard, gVisor spot pool, chat pool on standard nodes, NetworkPolicy, egress allowlist | Opus | 14-17 |
| W5 | Web UI: chat mode and agent mode; right panel with Files (list, view, edit), command runner (one-shot shell for the beta), Preview; image input; quota and wait screens | Sonnet build, Opus review | 14-18 |
| W6 | Loop `staging`: tool lock for chat units; connect automatic compaction; gateway file endpoints | Opus | 14-16 |
| W7 | Quotas and metering: LiteLLM virtual keys, messages for each day, sandbox minutes, storage limit | Sonnet | 16-18 |
| W8 | Independent security review; isolation tests between units; load test with the `faux` model (300 agent units, 2,000 chat units); node-loss test | Opus | 17-19 |
| W9 | Monitoring, alerts, backup of Postgres, runbook | Sonnet | 18-19 |
| - | Deploy, smoke test in a real browser, release to a small group, then open | - | 19-20 |

### Not in the beta (stage 2)
Shared cells and the Postgres Event Log; real PTY terminal; share links; memory; Microsoft and email sign-in; payments; the move of the desktop code; the app on `loop.soket.ai`.

### No data loss
Beta data moves to stage 2 with `session/export` and import for the sessions, and a copy of `gateway.db` rows to Postgres. A migration test is part of stage 2.

## Needed from the project owner (the critical path)

| Item | Date |
|---|---|
| A GCP project or service account with rights for GKE, Cloud SQL, GCS and Artifact Registry | 14-15 Nov |
| Region. Recommendation: Mumbai (`asia-south1`); `us-central1` if the model gateway must be near. | 14 Nov |
| DNS record `app.loop.soket.ai` and a Google OAuth client | 16 Nov |
| Model capacity for the beta, and a LiteLLM that I can make user keys on | 16 Nov |
| Approval to push to `loop-web-agentic-app` | 14 Nov |
| Terms of use and privacy text | 19 Nov |
| A person for the security review result | 19 Nov |

## Verification
- Unit: gateway tests (56 now), web tests, new router and save/restore tests.
- Isolation: a hostile unit cannot read another unit's files, sockets, keys or network.
- Chat pool: a prompt-injection set shows that no command or file write runs.
- Save and restore: stop a node during a turn; the user returns with at most 60 s lost.
- Load: `faux` model, 2,000 chat units and 300 agent units; sign-in to first reply p95 under 5 s with the warm pool.
- End to end in a real browser on `app.loop.soket.ai`: Google sign-in, chat, agent mode, upload, preview, quota limit, wait screen.

## Work already done (2026-11-13)

- `loop-web-agentic-app`, local branch `feat/beta` (not pushed): port to the `staging` protocol (fork and clone, setters with no Runtime restart, `session/shell`, trust, new update kinds, generated TypeScript types); pod mode (identity from the sign-in proxy header with a shared secret); cookie-only sessions; read-only provider; sandboxed document previews; strict CSP; rate limits; short private socket paths. 56 gateway tests, 5 web tests, the end-to-end script and a browser smoke test pass.
- Loop `staging`: the Linux sandbox fixes and the removal of provider keys from the tool environment (`tools.passEnv`) are merged. See `docs/probes/linux-live-check.md` on `staging`.
- A page that explains the deployment exists as a private artifact; its content is in this document.
