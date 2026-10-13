# Repository structure migration

Move one responsibility at a time when it gives a clear build, test or reuse
benefit. Keep the target layout and dependency rules in this repository.

The first install/update step adds `crates/ops/loop-update`. It owns release
lookup, integrity verification, binary replacement, rollback and uninstall.
It exposes operations and typed errors. It does not depend on the agent,
app-core, protocol or CLI. The CLI owns commands and the visible update notice.
Settings and session files remain owned by their current crates.

The workspace's `default-members` now includes all existing non-desktop crates
and the updater. A plain build skips desktop. Desktop remains at its current
path and can still be built with `cargo build -p loop-desktop` and its required
nightly toolchain. Cargo can still resolve workspace dependencies, including
desktop dependencies, when the lockfile is updated.

Existing crate paths and names are retained in this step. Later P0 work can
define and extract the protocol and policy interfaces. Config, session, tools,
hooks and runtime wiring can then move separately. Desktop can move to
`apps/desktop` when its build references are updated together.

Dependency rules for new work:

- Core and operations crates do not depend on surfaces.
- A surface displays status and sends commands; distribution operations stay
  in `loop-update`.
- Updater code does not import runtime settings or perform data migrations.
- Add optional components through features and keep the default build small.
