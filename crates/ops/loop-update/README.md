# loop-update

Distribution operations for Loop. This operations crate depends on no runtime
or surface crate. The CLI displays notices and invokes the public operations.

- `ReleaseClient`: redirect lookup, startup cache, checked download and bounded extraction.
- `Installation`: operation lock, binary preflight, replacement, rollback and uninstall.
- `plan`: tested version, reinstall and downgrade decisions.
- `cancellable`: ordinary signal handling with guard cleanup.
- `Error`: typed failures, including checksum and version errors.

The installation owns only its executable and reserved `.loop-update/` folder.
It never opens user settings, credentials or sessions. CLI callers use
`Installation::current()` to reject source and package-manager installations.
Embedding callers that use `Installation::at()` must establish ownership first.

Run `cargo test -p loop-update --locked` for the cross-platform lifecycle fixture.
See [Install and update](../../../docs/install-update.md) for the storage
format and recovery limits, and [Repository structure](../../../docs/repo-structure.md)
for the migration rules.
