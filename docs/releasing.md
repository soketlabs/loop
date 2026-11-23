# Releasing

This page is for maintainers.

## CI and releases

- **CI** (`.github/workflows/ci.yml`) runs on every PR into `main` or `staging`, on pushes to `main`, in the merge queue, and on manual **Run workflow**. Pushes to `staging` do not run CI. It lints and tests; it does **not** publish a release. See [CI](ci.md).
- **Release** (`.github/workflows/release.yml`) publishes multi-platform binaries only when you cut a version tag (or manually with `create_release`).

This repository releases the CLI only. The desktop app is developed and released from a separate repository.

Supported release targets:

| Asset | Platform |
|-------|----------|
| `loop-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `loop-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 |
| `loop-x86_64-apple-darwin.tar.gz` | macOS Intel |
| `loop-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |

## Cut a release

1. On a branch from `main` (for example `release/0.7.0`), bump `[workspace.package] version` in the root `Cargo.toml`. It must match the tag without the `v` prefix; the Release workflow stops if they differ. Run `cargo update --workspace` so `Cargo.lock` has the same version (release builds use `--locked`).
2. Open a PR into `main` and merge it. A ruleset on `main` refuses direct pushes, so the bump goes through a PR.
3. Tag the merge commit on `main` and push the tag:

```bash
git fetch origin
git tag v0.7.0 origin/main
git push origin v0.7.0
```

That starts the Release workflow, builds all targets, and creates a GitHub Release
with archives, `.sha256` checksums, and `install.sh`.

Before tagging, require the Linux and macOS lifecycle tests in CI to
pass (`cargo test -p loop-update --locked`). They check running-binary updates,
rollback, uninstall and preservation of user data. See
[Install and update](install-update.md) and the incremental
[repository structure migration](repo-structure.md).

## Publisher verification follow-up

Release SHA-256 files verify integrity against the value published on GitHub.
They do not independently verify publisher identity. Before claiming publisher
verification, choose GitHub artifact attestations or signed manifests with an
embedded verification key. Define key rotation and revocation, verify before
running the staged binary, and test rejection of an unsigned, altered or
incorrectly signed artifact on all supported platforms. Keep this separate from
the current integrity-checking updater.
