# Real systems are reached only through Connectors outside the Workspace

Datacenter, security and integration work means acting on real systems (Targets). An agent never reaches a Target from its Workspace; it calls a Connector — an MCP server or Relay that the Runtime runs outside the Workspace — after the Policy gate allows the call. The Connector draws a short-lived credential for the acting person or Service Identity from the Secret Store, performs the action and returns only its output. We chose this over giving Workspaces scoped credentials because the Workspace stays fully isolated, credentials never enter the model's context or the Event Log, and every action is audited at one point.

## Consequences

- Relays run inside an Org's network and connect out to the Runtime, so segmented and air-gapped networks need no inbound ports.
- Stated limit: this does not stop an agent from reading secrets that live on a Target if a permitted command prints them. Scope and verb allowlists, write Escalations and output redaction reduce this; they don't remove it.
