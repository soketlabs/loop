# Every Surface is a client of a Runtime

The terminal UI, Desktop, Web, Admin and other agents in a Swarm all talk to a Runtime over `loop-protocol`, and the Runtime is the only owner of Sessions. Running in-process is just one transport; a local socket and WebSocket are others. We chose this over letting each Surface link the agent crates directly (as Desktop does today) because one hub lets "remote deployment", "web chat", "org deployment" and "swarm member on another machine" all be the same thing: a Runtime somewhere else.

## Consequences

- `loop-protocol` becomes a public, versioned wire API (semver plus capability negotiation), with SDKs. Our own Web and Admin are built only on that public API.
- Desktop ships with a built-in Runtime so it installs on its own, and can also connect to a remote personal Runtime or, for Org users, only to the Runtime the admin names.
