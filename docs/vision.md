# Loop vision

Status: agreed direction, 2026-10-28. Terms in **bold** are defined in [GLOSSARY.md](../GLOSSARY.md). Hard-to-reverse decisions are recorded in [docs/adr](adr/).

## North star

Loop is an open-source agent harness, written in Rust, that runs AI agents **safely** on demanding work: long-range coding, security, datacenter operations and large data workloads. It works with many model sources out of the box, people can use it from a terminal, a desktop app or a browser, and other developers can build on it easily.

Four promises:

1. **Safe.** Safety means isolation, least privilege and audit. Approval prompts are not the boundary ([ADR-0002](adr/0002-safety-is-isolation-not-prompts.md)).
2. **Open.** Everything an Org needs to self-host is Apache-2.0.
3. **Capable.** Long-running Sessions and Swarms of agents that people can watch and steer.
4. **Extensible.** New tools, agents and Surfaces can be built with protocols, in any language, without recompiling Loop.

## Who it's for

| Audience | What they need | Surfaces |
|---|---|---|
| Developers | Long-range coding, any model, terminal first | TUI, Desktop |
| Government and non-technical users | Chat in a browser or desktop app, strict control | Web, Desktop |
| Integrating orgs (banking, agriculture, …) | Custom agents wired into their own systems | Desktop, Agent Definitions, Runtime API |
| Security and data-heavy teams | Swarms at scale, an isolated and audited environment | All, plus Admin |

All four stay in scope. **Priority:** developers and Web first, then Desktop and integrations, then the full Org feature set.

**Platforms:** Linux and macOS first. Windows support comes later, and the release cadence for it is not decided yet.

## Architecture: the Runtime is the hub

Every **Surface** is a client of a **Runtime**, and the Runtime is the only owner of **Sessions** ([ADR-0001](adr/0001-runtime-is-the-hub.md)). The connection can be in-process, a local socket, or WebSocket to a remote Runtime.

```
                  Surfaces: TUI · Desktop · Web · Admin · third-party
                  (people watch, Nudge, Interrupt, pause, decide)
                                     │  loop-protocol (public, versioned)
┌─────────────────────────────────── Runtime ───────────────────────────────────┐
│  Event Log — append-only; live views, audit, replay, recovery                 │
│  Sessions — a tree; a Subagent is a child Session                             │
│  Swarm coordination — Task Board · Mailboxes · Shared Artifacts · Leases      │
│  Escalations — approvals and questions routed to people                       │
│  Policy — Capabilities layered Managed → Org → Group → User → Agent → Session │
│  Workspace manager — isolation per Session (worktree / container / microVM)  │
│  Connectors — the only way out to Targets, credentials from the Secret Store │
└──────┬───────────────────────┬────────────────────────┬────────────────────────┘
  native Backend          ACP Backend                remote Runtime
  (Providers: Soket,      (ChatGPT via Codex ·       (a federated Swarm member)
   OpenAI, Anthropic,      any ACP agent)
   Gemini API, local…)
```

The Runtime is **event-sourced** ([ADR-0003](adr/0003-event-sourced-runtime.md)). Messages, tool calls, coordination, human steering and Policy decisions all go into one **Event Log**. Live views, audit, resume and recovery are all built from it.

**Scale:** the Runtime ships as one binary with embedded storage for personal and small-Org use. The same binary scales to a cluster: Postgres for the log and state, object storage for Artifacts, stateless Runtime nodes, and a pool of Workspaces on Kubernetes, Firecracker or Nomad. Storage and Workspace drivers are the two seams that make this possible without a rewrite.

## Surfaces

- **TUI.** The developer's main Surface. It runs on an in-process Runtime by default and can also connect to a remote one.
- **Desktop.** Downloads and installs **on its own**, with a built-in Runtime and no separate `loop` install needed. Individuals can point it at a **remote personal Runtime** on their own server and switch between local and remote. Org users connect only to the Runtime their admin specifies.
- **Web.** A Runtime in the cloud plus the Web Surface. Google sign-in is **identity only**: it gives no model access and no Gemini subscription. Models come from the Soket provider (billed by Soket) or the user's own API keys. Sessions start as **plain chat**. **Agent mode** gives a Session an isolated Workspace (a microVM) and agent tools. Org Policy can disable Agent mode.
- **Admin.** The Org's control panel (see [Orgs](#orgs)).
- **Third-party Surfaces.** `loop-protocol` is the public API, with SDKs: TypeScript first, then Python, plus native Rust. Anyone can build a Slack bot, an editor panel or an internal portal. Web and Admin use **only** the public API.

## Backends and Providers

- A **Backend** runs the agent loop for a Session. It is either Loop's native loop or an external agent over ACP. External agents are added as config entries, not code. Built in: ChatGPT via Codex · any ACP agent.
- **Providers** feed the native Backend. Loop ships native adapters for the major API families (OpenAI, Anthropic Messages, Gemini with API keys only, Bedrock/Vertex), the Soket provider, and local inference (vLLM, Ollama). OpenAI-compatible stays the catch-all. Native adapters matter because long-range work relies on prompt caching, reasoning controls and exact tool formats.
- Subscriptions come only through ACP agents whose terms allow it.

## Swarms and human steering

A **Swarm** is a group of Sessions working toward one goal ([ADR-0005](adr/0005-swarms-coordinate-through-structured-primitives.md)). It has a **Lead** by default; peer Swarms use the same primitives.

**Coordination primitives:**
- **Task Board:** the shared tasks, their dependencies, owners and status.
- **Mailbox:** each Session's inbox for short messages from agents or people. It supports broadcast, and every message is logged.
- **Shared Artifacts:** files and notes referenced by id.
- **Leases:** time-limited exclusive claims on paths or resources.

**Every Backend can join.** The Runtime offers these primitives as tools (`spawn`, `send`, `claim_task`, `update_task`, `wait`, `lease`): directly to the native Backend, and through an MCP server it passes to ACP agents. A remote Runtime can join as a member for large fan-out.

**Limits** come from Policy:
- Subagent depth (default 2) and the maximum number of concurrent Sessions.
- A token, cost and wall-clock budget; each Subagent's budget comes out of its parent's.
- A Subagent never has more Capabilities than its parent.
- Only the Lead can spawn, by default.
- Every action goes through the same gate as a single Session.

Hitting a limit raises an **Escalation**.

**People steer** from any Surface:
- **watch** any Session live;
- **Nudge** (delivered at the next turn boundary, the default);
- **Interrupt**;
- **pause/resume** a Session or a Swarm;
- **edit the Task Board**;
- **answer Escalations**.

An unanswered Escalation blocks only its own branch, not the whole Swarm.

## Safety model

**Isolation.** Each Session acts in its own **Workspace**: a worktree locally, a container or microVM in the cloud and in Orgs.

**Capabilities.** Every feature that can be switched on or off is a **Capability**, with a state of off, ask or on and an optional scope (paths, domains, models, servers). The list includes:
- web search, web fetch, network egress;
- read files, write files, shell;
- each MCP server, each Provider and model, external ACP agents;
- Agent mode, Subagents and Swarms;
- file upload, export, memory, sharing, plugins;
- each Target;
- pinning to the Org Runtime.

Today's permission modes (read-only, ask, accept-edits, full-access) become named presets.

**Policy layers that only tighten** ([ADR-0004](adr/0004-policy-only-tightens.md)). The layers are **Managed Policy → Org → Group → User → Project → Agent Definition → Session**. A lower layer can only tighten what's above it, and an admin can **Lock** any Capability.

**Off means unavailable.** A Capability that is off is removed from the agent's toolset and from its Workspace (no network route, read-only mount, no shell). It isn't just refused at a prompt.

**Audit.** Every action in the Event Log traces back to a real person or an admin-owned **Service Identity**.

**Stated limits.** Loop says so in the product wherever it can't enforce something:
- execution managed by an external agent;
- secrets that already live on a Target;
- unmanaged devices.

## Real systems: Targets and Connectors

Datacenter, security and integration work acts on real systems. A **Target** is one such system: an SSH host, a cluster, a cloud account, a bank API ([ADR-0006](adr/0006-targets-behind-connectors.md)).

```
Workspace (no credentials, no route out)
   │ tool call
   ▼
Runtime: Policy gate (scope, verbs, write → Escalation) → Event Log
   │ call + acting identity
   ▼
Connector (outside the Workspace): short-lived credential from the Secret Store
   │ acts on the Target, redacts, returns output only
   ▼
Target
```

- Connectors are MCP servers, so existing MCP servers work as Connectors. The Runtime launches them outside the Workspace and passes them their secrets.
- Credentials are short-lived and tied to the acting identity: SSH certificates with a lifetime of minutes, assumed cloud roles, OAuth tokens issued on the person's behalf. The model only ever sees results.
- **Relays** are Connectors that run inside an Org's network and connect out to the Runtime. No inbound ports are needed, including on air-gapped networks.
- **Limit:** if a permitted command prints a secret stored on the Target, the agent sees it. Allowlists, write Escalations and redaction reduce this risk; they don't remove it.

## Orgs

**Deployment.** An Org runs the Runtime itself, from one binary up to a cluster, including fully air-gapped with local Providers and offline update bundles. It starts from a **Deployment Profile**: Personal, Team, Enterprise, Read-only Analyst or Air-gapped. Air-gapped is built and tested from day one.

**Identity:**
- People sign in with OIDC (Google, Microsoft Entra, Okta, Keycloak); people and **Groups** sync over SCIM.
- Agents act as the person who started the Session.
- Unattended work runs as a Service Identity.
- Credentials sit in a Secret Store (Runtime-managed, Vault or cloud KMS).

**People's work is private by default.** Each person's Sessions, Workspaces and files are private. Sharing is explicit, and Org data sources are mounted read-only by default. Admins see audit metadata. They see content only where Org Policy declares it, and users are told.

**Controlling Desktops** happens at two layers:
- **Device layer.** The Desktop's built-in Runtime honours an OS-protected **Managed Policy** set through the Org's device management: a configuration profile on macOS, or a root-owned file on Linux. It can pin the Desktop to the Org Runtime, turn off the local Runtime, personal Providers and keys, and ACP agents, and lock all of these.
- **Server layer (the real boundary).** Org keys, data, Targets, credentials and audit exist only behind the Org Runtime. The Org Runtime can refuse unmanaged devices. A personal copy of Loop can't reach any of it.
- **Stated limit.** Controlling unmanaged devices is the job of the Org's endpoint and network tooling, not Loop's.

**Admin v1** covers:
- people and Groups;
- Policy with Locks, and Deployment Profiles;
- Providers, models and keys;
- Targets and Connectors;
- available Agent Definitions;
- usage and cost per person, Group and Swarm;
- audit search and SIEM export;
- a live Swarm view with **kill switches** for any Session, Swarm or Group.

## Extending Loop

Protocols come first, so extensions can be written in any language with no recompiling:

- **MCP** for tools and Connectors.
- **ACP** for whole external agents.
- **`loop-protocol`** and the SDKs for new Surfaces and automation.
- **Agent Definitions**: a manifest of instructions, Backend or model, tools and MCP servers, skills, Policy and Workspace needs. Publish one to a Runtime and run it from any Surface or call it over the API. "Custom Loop agent" means this.
- **Plugins**: bundles of skills, hooks, MCP servers and Agent Definitions.

In-process Rust crates (new Backends, Providers, Workspace or storage drivers) are the second tier, for core contributors and embedders.

## Open source and closed source

The rule: something is closed only if it exists to run Soket's own public service.

| Open source (Apache-2.0) | Closed (Soket) |
|---|---|
| Runtime server, `loop-protocol`, `loop-agent`, `loop-ai`, `loop-acp`, `loop-mcp` | Hosted multi-tenant control plane for the public Web: billing, quotas, metering |
| Swarms / orchestration | Abuse, fraud and rate-limit systems for the public service |
| TUI, Desktop, Web Surface, Admin | Soket models and inference; the backends behind `/v1/search` and `/v1/fetch` |
| OIDC/SSO, SCIM, RBAC, Policy and Locks, audit export, multi-Group tenancy | Operations for Soket's hosted Workspace fleet |
| Workspace drivers (local, container, microVM), Connectors and Relays, plugin format, SDKs | Compliance and accreditation packages, paid support, SLAs |

## Roadmap

1. **Foundation.** The Runtime becomes the only owner of Sessions. `loop-protocol` runs over a local socket and WebSocket. The Capability and Policy engine is built. The TUI moves onto the Runtime.
2. **Web v1.** Cloud Runtime, Web Surface, Google sign-in, plain chat plus Agent mode on microVM Workspaces, Soket provider and bring-your-own keys, native Provider adapters.
3. **Swarms v1.** Orchestration moves under the Runtime on `AgentBackend`, with the Task Board, Mailboxes, Leases, limits and steering, available to every Backend.
4. **Desktop and integrations.** Desktop becomes a standalone Runtime client with remote mode, checked in PR CI. Agent Definitions, Targets and Connectors, the TypeScript SDK.
5. **Org.** OIDC/SCIM, Admin v1, Managed Policy, Policy layers with Locks, Deployment Profiles including Air-gapped, cluster mode.

Nothing user-facing jumps ahead of step 1.

## Where the code stands (v0.5.0)

| Today | Gap to the vision |
|---|---|
| `loop-protocol` has a serializable event model; `AgentBackend` is an in-process trait over a tokio broadcast | Needs a wire transport and versioning (step 1) |
| Sessions are stored locally (JSONL, SQLite) and owned by each process | The Runtime becomes the only owner |
| Desktop (GPUI) links `loop-app-core` and `loop-agent` directly; PR CI excludes it, and only the release workflow builds it | Move it onto `loop-protocol`; check it in PR CI |
| Permission modes and `toolPermissions`; project settings can only tighten | Generalise into Capabilities and layered Policy |
| `loop-orchestration` is feature-gated and runs on its own `StreamFn`, so ACP agents can't take part | Move it onto `AgentBackend` under the Runtime, keeping one gate for everything |
| `loop-ai`: OpenAI-compatible, Soket, custom | Add native adapters for the major API families and local inference |

The architecture-review candidates from 2026-10-28 line up with step 1. The permission-policy module (candidate 1) becomes the start of the Policy engine. The surface session module and "every surface behind `AgentBackend`" (candidate 2, W1) become the move onto the Runtime.

## Open questions

- The Windows support timeline and release cadence, and whether today's Windows CLI builds (`install.ps1`, Windows CI) continue in the meantime.
- The `loop-protocol` wire format and transport details, and the first SDK API.
- The microVM technology for hosted Workspaces, and the Workspace driver interface.
- How Mailbox delivery and Lease timeouts behave under failure.
