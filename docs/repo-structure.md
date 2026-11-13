# Repository structure migration

Move one responsibility at a time when it gives a clear build, test or reuse
benefit. Keep the target layout and dependency rules in this repository.

The first install/update step adds `crates/ops/loop-update`. It owns release
lookup, integrity verification, binary replacement, rollback and uninstall.
It exposes operations and typed errors. It does not depend on the agent,
app-core, protocol or CLI. The CLI owns commands and the visible update notice.
Settings and session files remain owned by their current crates.

The change to workspace default members is reviewed separately. This updater
change keeps the existing default build selection.

Existing crate paths and names are retained in this step. Later P0 work can
define and extract the protocol and policy interfaces. Config, session, tools,
hooks and runtime wiring can then move separately.

Dependency rules for new work:

- Core and operations crates do not depend on surfaces.
- A surface displays status and sends commands; distribution operations stay
  in `loop-update`.
- Updater code does not import runtime settings or perform data migrations.
- Add optional components through features and keep the default build small.

## Current layout

Crates are grouped by the dependency rules above:

- `crates/core/`: `loop-protocol` (the versioned event model and backend interface) and `loop-wire` (JSON-RPC and the transports that carry it).
- `crates/extend/`: optional surfaces and integrations, `loop-acp` and `loop-dashboard`.
- `crates/ops/`: distribution operations, `loop-update`.
- `crates/`: the remaining crates keep their original paths: `loop-agent`, `loop-ai`, `loop-app-core`, `loop-cli`, `loop-mcp`, `loop-orchestration`, `loop-runtime` (the shared Runtime and its durable Session Event Log), `loop-telemetry` and `loop-test-support` (test helpers).

The desktop app is developed in a separate repository. It uses the public crates here as git dependencies.
