# Install and update

## Install

Linux (glibc), macOS and WSL use `install.sh`. Native Windows x64 uses
`install.ps1`. Both download a platform archive from GitHub Releases, verify
its SHA-256 checksum, and check the staged binary with `--version` before
replacing the installed program. A missing checksum, missing SHA-256 tool,
checksum mismatch, or failed binary check stops the operation.
`LOOP_ALLOW_UNVERIFIED` no longer bypasses verification.

From a clone:

```bash
./install.sh --version 0.3.4
./install.sh --no-modify-path
```

```powershell
.\install.ps1 -Version 0.3.4
.\install.ps1 -NoModifyPath
```

The release workflow also publishes these installer scripts as release assets.
This applies to releases made after these changes are merged; older releases
contain only the existing archives and checksums.

The default program directory is `~/.loop/bin` (`$HOME\.loop\bin` on Windows).
Set `LOOP_INSTALL_DIR` to use a custom directory. Keep this variable set when
you run update, rollback or uninstall. On Windows, `-InstallDir` also selects
the install directory; set `LOOP_INSTALL_DIR` to the same path for later commands.
The installers can add this directory to shell or user PATH. Use the option
above to skip PATH edits.

For a locally built executable, supply its SHA-256 hash explicitly:

```bash
./install.sh --binary target/release/loop --checksum <sha256>
```

```powershell
.\install.ps1 -Binary target\release\loop.exe -Checksum <sha256>
```

This verifies a local file against the supplied hash. The caller is responsible
for trusting that hash. Release checksums verify integrity against GitHub's
published value; they are not an independent publisher signature.

## Check and update

Each terminal chat startup, including `--resume`, checks the latest stable
GitHub release in the background. A newer version produces a chat notice:

```text
Loop 0.3.5 is available (installed: 0.3.4).
Exit this session and run `loop update` to install it.
```

The check has a five-second timeout. Offline and rate-limited checks are silent.
Set `LOOP_NO_UPDATE_CHECK=1` to disable it. `--print`, MCP server mode, config,
and version commands do not perform a startup check or alter their output.

```bash
loop update --check
loop update
loop update --version 0.3.5
loop update --version 0.3.4 --force
```

Version comparisons use semantic versions. Normal updates never downgrade.
`--force` permits an explicit downgrade or a reinstall. Exit other Loop
sessions before updating. The update command ends after replacement; the next
`loop` command starts the new binary.

The updater only replaces a binary in the installer directory. Cargo installs,
source checkouts, and package-manager installs must use their original install
method. For example, rebuild a source checkout with `cargo build -p loop-cli
--release`, then use the installer with `--binary` and `--checksum`.

## Rollback and recovery

```bash
loop rollback
```

Before replacement, Loop saves the installed binary under
`bin/.loop-update/previous-<sha256>[.exe]`. `rollback.json` records the current
hash, previous hash, previous version, and record schema. Rollback verifies both
the current and saved binary hashes and checks that the saved binary runs.
Rollback also saves the outgoing binary, so a second rollback can undo it when
both versions contain the update commands. If you restore an older release
without these commands, use the installer to install a newer release again.
Re-running either installer also records the previous binary in this format.

Rollback changes only the binary. This work does not change settings or session
schemas. Future data migrations must define compatibility and recovery before
they are included in updates.

An atomic directory lock at `bin/.loop-update/operation.lock` prevents concurrent
install, update, rollback and uninstall operations. A hard crash can leave this
lock behind. Confirm that no such operation is running, then remove this empty
lock directory and retry. Do not remove it during an active operation.

A failed download or preflight leaves the current binary intact. An interruption
after the rollback record is written can leave a record whose current hash
does not match the installed binary. Rollback fails closed in that case. Retry
the update or run the installer to restore a checked release. Temporary files
can remain after a power loss, including Windows replacement helper files.

## Uninstall

```bash
loop uninstall
```

This removes `loop` or `loop.exe` and its reserved `.loop-update` directory.
It keeps settings, credentials, session databases, session files, skills and
other files in the installation directory. On Windows, deletion of the running
binary completes after the command exits. PATH entries are kept because the
directory can hold other commands; remove them manually if no longer needed.

## Validation

`cargo test -p loop-update --locked` runs a local release fixture and checks:

- Bash or PowerShell installation of a checked local binary.
- Release lookup and checked archive download.
- Rejection of missing and incorrect checksums.
- Replacement of a running executable and version validation.
- Checked rollback, including rejection of a damaged backup.
- Operation locking and rejection of invalid candidate binaries.
- Uninstall with settings, credentials, skills and sessions preserved.

CI runs these tests on Linux, macOS and Windows. Platform support should be
confirmed from that run before release; local macOS results do not establish
Linux or Windows success.
