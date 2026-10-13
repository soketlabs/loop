# Releasing

This page is for maintainers.

## CI and releases

- **CI** (`.github/workflows/ci.yml`) runs on every PR and push to `main`, plus manual **Run workflow**. It builds and tests; it does **not** publish a release.
- **Release** (`.github/workflows/release.yml`) publishes multi-platform binaries only when you cut a version tag (or manually with `create_release`).

Supported release targets:

| Asset | Platform |
|-------|----------|
| `loop-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `loop-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 |
| `loop-x86_64-apple-darwin.tar.gz` | macOS Intel |
| `loop-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |
| `loop-x86_64-pc-windows-msvc.zip` | Windows x64 |

## Cut a release

1. Bump `[workspace.package] version` in the root `Cargo.toml` (must match the tag without the `v` prefix).
2. Commit, push to `main` (or your release branch).
3. Tag and push the tag:

```bash
git tag v0.3.4
git push origin v0.3.4
```

That starts the Release workflow, builds all targets, and creates a GitHub Release
with archives, `.sha256` checksums, `install.sh` and `install.ps1`.

Before tagging, require the Linux, macOS and Windows lifecycle tests in CI to
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
