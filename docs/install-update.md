# Install and update

## Install

Loop supports Linux (glibc) and macOS only; on Windows, use WSL. `install.sh`
downloads a platform archive from GitHub Releases, verify
its SHA-256 checksum, and check the staged binary with `--version` before
replacing the installed program. A missing checksum, missing SHA-256 tool,
checksum mismatch, or failed binary check stops the operation.
`LOOP_ALLOW_UNVERIFIED` no longer bypasses verification.

From a clone:

```bash
./install.sh --version 0.7.1
./install.sh --no-modify-path
```

The release workflow also publishes this installer script as release assets.
This applies to releases made after these changes are merged; older releases
contain only the existing archives and checksums.

The default program directory is `~/.loop/bin`.
Set `LOOP_INSTALL_DIR` to use a custom directory. Later commands recognize custom installs through
the real `.loop-update` directory beside the running executable. The variable
does not need to stay set. The default path is also recognized for legacy installs.
The installer can add this directory to your shell PATH. Use the option
above to skip PATH edits.

For a locally built executable, supply its SHA-256 hash explicitly:

```bash
./install.sh --binary target/release/loop --checksum <sha256>
```

This verifies a local file against the supplied hash. The caller is responsible
for trusting that hash. Release checksums verify integrity against GitHub's
published value; they are not an independent publisher signature.

## Check and update

Installer-owned terminal chat startup, including `--resume`, checks the latest
stable GitHub release in the background. Source and package-manager launches
skip it. A newer version produces a chat notice:

```text
Loop 0.7.1 is available (installed: 0.7.0).
Exit this session and run `loop update` to install it.
```

The check follows GitHub's latest-release redirect, without using the API quota.
Startup caches a successful result for 24 hours in `.loop-update/update-check.json`.
Explicit update commands always check again. Background checks have a five-second
timeout; failed and offline checks are silent. Set `LOOP_NO_UPDATE_CHECK=1`,
`true`, `yes` or `on` to disable them. Empty values, `0`, and `false` keep checks
enabled. `--print`, MCP server mode, config,
and version commands do not perform a startup check or alter their output.

```bash
loop update --check
loop update
loop update --version 0.7.1
loop update --version 0.5.0 --force
```

Version comparisons use semantic versions. Normal updates never downgrade.
`--force` permits an explicit downgrade or a reinstall. Exit other Loop
sessions before updating. The update command ends after replacement; the next
`loop` command starts the new binary.

The updater only replaces an installer-owned binary. Cargo installs,
source checkouts, and package-manager installs must use their original install
method. For example, rebuild a source checkout with `cargo build -p loop-cli
--release`, then use the installer with `--binary` and `--checksum`.

## Rollback and recovery

```bash
loop rollback
```

Before replacement, Loop saves the installed binary under
`bin/.loop-update/previous-<sha256>`. `rollback.json` records the current
hash, previous hash, previous version, and record schema. Rollback verifies both
the current and saved binary hashes and checks that the saved binary runs.
Rollback also saves the outgoing binary, so a second rollback can undo it when
both versions contain the update commands. If you restore an older release
without these commands, use the installer to install a newer release again.
Re-running the installer also records a usable previous binary in this format.
If the old binary cannot run, the installer repairs it after checking the new
binary and removes the unusable rollback record. A failed final check restores
the previous working binary when one is available.

Rollback changes only the binary. This work does not change settings or session
schemas. Future data migrations must define compatibility and recovery before
they are included in updates.

An atomic directory lock at `bin/.loop-update/operation.lock` prevents concurrent
install, update, rollback and uninstall operations. A hard crash can leave this
lock behind. Downloads do not hold the lock. Ctrl-C, SIGHUP and SIGTERM cancel
the Rust update operation and clean up staging files and locks. An uncatchable kill or power loss can leave the lock behind.
Confirm that no such operation is running, then remove this empty
lock directory and retry. Do not remove it during an active operation.

A failed download or preflight leaves the current binary intact. An interruption
after the rollback record is written can leave a record whose current hash
does not match the installed binary. Rollback fails closed in that case. Retry
the update or run the installer to restore a checked release. Temporary files
can remain after a power loss.

## Uninstall

```bash
loop uninstall
```

This removes `loop` and its reserved `.loop-update` directory.
It keeps settings, credentials, session databases, session files, skills and
other files in the installation directory. PATH entries are kept because the
directory can hold other commands; remove them manually if no longer needed.

## Validation

`cargo test -p loop-update --locked` runs a local release fixture and checks:

- Bash installation and repair of a broken binary.
- Release lookup and checked archive download.
- Rejection of missing and incorrect checksums.
- Replacement of a running executable and version validation.
- Checked rollback, including rejection of a damaged backup.
- Operation locking and rejection of invalid candidate binaries.
- Cancellation during a stalled download and while holding a lock (Unix signals).
- Redirect lookup and reuse, expiration and repair of the startup cache.
- Uninstall with settings, credentials, skills and sessions preserved.

CI runs these tests on Linux and macOS. Platform support should be
confirmed from that run before release; local macOS results do not establish
Linux success.
