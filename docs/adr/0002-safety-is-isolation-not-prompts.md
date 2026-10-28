# Safety is isolation, least privilege and audit, not approval prompts

Loop's safety boundary is the isolated Workspace each Session runs in (worktree, container or microVM by deployment), plus Capabilities that are removed rather than refused, plus an append-only audit in the Event Log. Approval prompts are a UX layer on top, not the boundary. We chose this because prompts can't contain an agent's subprocesses, MCP servers or external agents (see `docs/design/external-agents.md`), and buyers in government and security need a boundary they can audit.

## Consequences

- A Capability that is off is taken out of the agent's toolset and out of its Workspace (no network route, read-only mount, no shell) — the model never sees it.
- Where Loop can't enforce something, such as agent-managed execution or unmanaged devices, the product says so plainly instead of implying protection.
- Enforcement sits where the resources are: Org keys, data, Targets and credentials live behind the Org Runtime, not on devices.
