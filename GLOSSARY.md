# Loop

Loop is an open-source agent harness: it runs AI agents safely on real work and lets people watch, steer and extend them from many surfaces.

## Language

### Running agents

**Runtime**:
The process that owns Sessions and runs agents for them; every Surface is a client of a Runtime, local or remote.
_Avoid_: server, deployment, "a loop", harness (when meaning the process)

**Session**:
One conversation-and-work thread between people and an agent, with its own history, Backend and Workspace.
_Avoid_: chat, conversation, thread

**Backend**:
What runs the agent loop for a Session: Loop's native loop, or an external agent reached over ACP.
_Avoid_: agent (alone), engine

**Provider**:
A source of model access that the native Backend calls, such as an API or a local inference server.
_Avoid_: backend, vendor, model host

**Workspace**:
The isolated files and machine an agent acts on for a Session.
_Avoid_: sandbox (it is one kind of isolation), environment, project

**Event Log**:
The append-only record of everything that happens in a Runtime; live views, audit and recovery are all read from it.
_Avoid_: history, transcript (a transcript is one Session's slice of it)

**Agent Definition**:
A packaged, reusable description of an agent: its instructions, Backend or model, tools, skills, Policy and Workspace needs.
_Avoid_: custom agent, persona, profile, bot

### Swarms

**Swarm**:
A group of Sessions working toward one goal under shared coordination.
_Avoid_: team, crew, fleet, multi-agent

**Subagent**:
A Session started by another Session, which becomes its parent.
_Avoid_: child agent, worker (when meaning any subagent)

**Lead**:
The Session that plans and directs a Swarm by default.
_Avoid_: orchestrator, manager, supervisor

**Task Board**:
A Swarm's shared list of tasks, with their dependencies, owners and status.
_Avoid_: plan, todo list, queue

**Mailbox**:
A Session's inbox for short messages from other agents or people, read at its turn boundaries.
_Avoid_: channel, chat, inbox queue

**Lease**:
A time-limited exclusive claim by one Session on a path or resource, so two writers don't collide.
_Avoid_: lock

**Shared Artifact**:
A file or note a Swarm member publishes for others to reference by id.
_Avoid_: blackboard, shared memory

### Safety and control

**Capability**:
One thing an agent or person may be allowed to do, such as web search, shell or a given MCP server, with a state of off, ask or on and an optional scope.
_Avoid_: permission, feature flag, toggle

**Policy**:
The Capability settings in force for a Session, layered from Managed Policy and Org down to Session, where each layer can only tighten the one above.
_Avoid_: settings (Policy is the safety subset), rules

**Managed Policy**:
The top Policy layer, set on a device by the Org through its device management, which the device's user can't change.
_Avoid_: MDM settings, enterprise config

**Lock**:
An admin's mark on a Capability that stops every lower Policy layer from changing it.
_Avoid_: pin, force

**Deployment Profile**:
A ready-made Policy bundle (Personal, Team, Enterprise, Read-only Analyst, Air-gapped) that an Org starts from.
_Avoid_: template, preset (presets are the smaller permission modes)

**Agent mode**:
The switch that gives a Session a Workspace and agent tools; without it a Session is plain chat.
_Avoid_: runtime mode, `/runtime`

### Real systems

**Target**:
An external system an agent may act on, such as a server, a cluster, a cloud account or an internal API.
_Avoid_: host, environment, integration

**Connector**:
The bridge, outside the Workspace, through which a Runtime acts on a Target using short-lived credentials; the agent sees only results.
_Avoid_: plugin, adapter, integration

**Relay**:
A Connector that runs inside an Org's own network and connects out to the Runtime.
_Avoid_: agent (on-prem agent), gateway, runner

**Secret Store**:
Where an Org's credentials live; Connectors draw short-lived credentials from it and the model never sees them.
_Avoid_: vault (one product), keychain, credentials file

**Service Identity**:
A named, admin-owned identity that unattended Sessions and Swarms act as.
_Avoid_: bot account, system user

### Organisations

**Org**:
An organisation that runs or uses a Runtime, with its own people, Groups and top Policy layer.
_Avoid_: tenant, company, account

**Group**:
A set of people inside an Org, such as a department, that shares a Policy layer.
_Avoid_: team, department, role

### People-facing

**Surface**:
Anything a person uses to talk to a Runtime: the terminal UI, Desktop, Web or Admin.
_Avoid_: client app, frontend, "Loop" (when meaning one app)

**Nudge**:
A message from a person to a running agent, delivered at its next turn boundary without stopping its work.
_Avoid_: steer, inject

**Interrupt**:
Stopping an agent's current turn so a person's message is handled at once.
_Avoid_: cancel (cancel ends the turn with no message)

**Escalation**:
A request from an agent that a person must answer before that branch of work can continue.
_Avoid_: approval (one kind of escalation), prompt, ask
