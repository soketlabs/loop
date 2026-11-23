# Linux live check (2026-11-12)

Loop commit: `8180235` (`staging`). Two runs:

- **Kube:** Kubernetes pods, restricted pod security, egress only to the model server, no internet. Model `qwen3-8-27b` (GLM-5.3 was down). Image `ghcr.io/anandgupta-soket/loop-bench-runner@sha256:e997ed7cf37363fbc8b0fcf5f92934d928aa93b05c4a5717812ac7fdef28ef2f`. Full results: `harness-benchmarking` branch `feat/linux-check-kube`, `linux-check/kube/RESULTS.md`.
- **Sandbox:** GitHub Actions `ubuntu-latest` (Ubuntu 26.04.1, Podman 5.7.0, runc 1.5.2, crun 1.21), rootless Podman, fake model server. Full results: branch `feat/linux-check-sandbox`, `linux-check/sandbox/RESULTS.md`, run https://github.com/anandgupta-soket/harness-benchmarking/actions/runs/43156635674.

## Result

Loop works on Linux from start to end. All checks pass. Six defects were found; two are serious (sandbox timeout and cancel).

| Check | Result | Evidence |
|---|---|---|
| E1 crash recovery | PASS | 1,400 trials (seeds 20261111, 424242, 777): 0 lost acknowledged mutations, 0 duplicate effects, 0 live takeovers, 0 interrupted turns not marked uncertain |
| E3 enforcement | PASS, as documented | 248 cases; 14 LEAK, all known L6/L7; 0 control failures; `leaks::` 8/8. Case 079 is UNREPORTED on Linux (case-sensitive file system), as expected |
| E7 turn queue | PASS | 1,000/1,000 schedules, 0 failed; permit probes exact |
| Runtime live, `kill -9` mid-turn | PASS (17/17 steps) | Turn recovered as uncertain; tool effect ran once; one `runtime_restarted` record per kill |
| TUI against the Runtime (PTY, no human) | PASS | `runtime_e2e` 15/15, `lease_exit_paths` 16/16, scripted session 5/5 |
| Document tools | PASS with D1 | Child `/proc/self/exe __doc-to-text`, empty environment, rlimits CPU 31 s / FSIZE 0 / CORE 0 / AS 2 GiB, at most 3 children |
| `--print` mode | PASS (15/15) | Exit codes 0/1/2/130; empty answer exits 1; 3 length continuations then exit 1 |
| Sandbox (Podman, runc and crun) | PASS with D2–D5 | Project writable; no write outside reaches the host; credential canaries unreadable; child environment has 6 variables, no keys or tokens; preflight failure: warning standalone, refusal in a Runtime session. Controls with the sandbox off reach every canary |

## Defects

| Id | Severity | Defect | Where |
|---|---|---|---|
| D1 | High (data loss) | A DOCX that is a valid zip with malformed `word/document.xml` reads as `ok:true` with empty text, so `write` overwrites it. All platforms | document tools; reproduction in kube RESULTS.md |
| D2 | High | Sandbox `exec` ignores `timeout_ms` when a cancel token is set; bash `sleep 25` with `timeoutMs=3000` ran 25 s | `crates/loop-agent/src/harness/sandbox/podman.rs` (`RealPodmanClient::exec`) |
| D3 | High | Cancel kills only the host `podman exec` client; the command in the container keeps running | same |
| D4 | Medium | Standalone `loop --print` leaves its container running after exit (the Runtime path cleans up) | sandbox lifecycle |
| D5 | Low | With Podman installed but failing, the warning says "not found in PATH" and prints twice in `--print` | sandbox preflight |

## Confirmed documented gaps

- `sandbox.network` is not enforced on Linux (HTTP 200 from inside the sandbox).
- `sandbox.allowWrite` is not enforced on Linux (the directory is not mounted).

## Observations for a decision

- Project `.git/hooks` and `.loop` are writable from the Linux sandbox (docs promise protection on macOS only). Later fixed: the Linux sandbox now mounts them read-only (see the sandbox section of `docs/configuration.md`).
- With the sandbox off, the shell tool sees provider key variables. `docs/runtime.md` says they are removed from tool environments.
- The container starts from unpinned `fedora:latest` without `--network`, `--cap-drop` or `--read-only`.
- Loop CI never runs real Podman; the only real-Podman test is ignored and gated by `LOOP_TEST_KRUN=1`, and its ignore text is stale.
- After `kill -9` of the Runtime, a running bash child continues as an orphan.
- A Runtime restart later than 30 s after a kill labels the old lease "removed stale", not "reclaimed" (label only).
- `usage.output == 0` is read as "context full", so a server without `usage` in its stream gets no length continuation.
- A TUI Session on a Runtime started with `--permission-mode full-access` still uses `ask`; `loop session new` does not. Not documented.

## Not covered (needs a real Linux machine)

A person looking at the TUI; owner auto-start, idle shutdown and `--continue` from a real shell (nohup, tmux, systemd); ext4/btrfs/NFS; power loss; `install.sh`, `loop update`, rollback; internet providers, web tools and OCR; runsc and krun; arm64.
