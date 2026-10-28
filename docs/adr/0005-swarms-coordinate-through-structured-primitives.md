# Swarms coordinate through structured primitives, offered to every Backend

Agents in a Swarm coordinate through a Task Board, Mailboxes, Shared Artifacts and Leases, with a Lead by default, and not through a free-form group chat. People use the same primitives and can watch, Nudge, Interrupt, pause, edit the Task Board and answer Escalations. The Runtime offers the primitives to agents as tools: directly to the native Backend, and through an MCP server it passes to ACP agents, so any Backend can be a Swarm member. We chose this because free chat between agents burns tokens and is unreadable at hundreds of agents, and because tying swarms to the native loop (as `loop-orchestration` does today) would leave external agents out.

## Consequences

- Swarm limits come from Policy: Subagent depth, concurrency, a token/cost/time budget split from the parent, spawning by the Lead only by default. Hitting a limit raises an Escalation.
- Every Swarm action goes through the same permission gate as a single Session, as `/workflow` shell tasks already do.
