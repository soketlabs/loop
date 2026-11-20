# loop-defensive

Sandboxed security workflows for Loop: `/patch` (find, reproduce, fix and
grade a vulnerability) and `/audit` (report corroborated findings). It plugs
into Loop through Loop's own extension points (an MCP server and two prompt
templates), so no other crate changes.

## Setup

```bash
cargo build --release -p loop-defensive
./target/release/loop-defensive install             # writes ~/.loop/agent/prompts/{patch,audit}.md
./target/release/loop-defensive install --settings  # also registers the MCP servers (backups kept)
```

Then in Loop: `/patch <CVE-ID | github-url | folder>` or `/audit <github-url>`.
Without the TUI: `loop-defensive patch <target>`, `loop-defensive audit <url>`,
`loop-defensive report <run-dir>`. Runs go to `$DEFENSIVE_RUNS` or
`~/.loop/defensive/runs`; you need rootless podman.

In the TUI the tools appear as `mcp__defensive__*` (`/patch`) and
`mcp__defensive-audit__*` (`/audit`); the prompts tell the agent to use only
those. Headless runs enforce it: their Loop config denies Loop's host tools.

The host drives rootless podman; no helper code runs inside the images. The
agent uses Loop's own `read`, `write`, `edit` and `bash`, bound to the task
container (`env`), plus the workflow's tools; every file operation runs inside
the container, so a symlink the agent's commands plant never reaches the host. Graders run steps
A-G in a fresh `--network none` container that never saw the agent's.

| Module | Role |
|---|---|
| `session` | One run's state behind the tools: prepare, grade, finish |
| `tools` | The run's toolset: Loop's tools bound to the container, plus the workflow's |
| `env` | An `ExecutionEnv` inside the run's current container |
| `task` | `/patch` stages for CVEs, GitHub repos and local folders |
| `audit` | `/audit` stages, corroboration and the findings report |
| `workspace` | The workflow's sandbox tools: listing, search, analyzers, diff, submissions |
| `grader` | The trusted graders (any language; Go/CVE with hidden tests) |
| `sandbox` | Images, containers, `podman exec` with time limits |
| `sources` | CVE/OSV records, git clones, local folders |
| `profiles` | Language detection: image, dependencies, build and test commands |
| `analyzers` | gosec, go vet, govulncheck, bandit, npm audit |
| `patchlib`, `scoring`, `report` | Diffs, the 12 checks, stage reports |

## Languages

`/patch <folder | github-url>` works for any language. `/patch CVE-…` uses the
upstream fix's tests as the hidden oracle, run with the project's own runner:

| Ecosystem | Hidden tests run with | Verified end to end on |
|---|---|---|
| Go | `go test -run` (per test function) | CVE-2022-36009 |
| Python | pytest | CVE-2022-29217 (PyJWT) |
| JavaScript / TypeScript | jest, vitest, mocha, ava, tap or `node --test` | CVE-2020-7788 (ini) |
| Rust | `cargo test --test <target>` (integration tests) | CVE-2022-24713 (regex) |
| Java | Maven `-Dtest=` / Gradle `--tests`, JDK picked from the build file | CVE-2022-25845 (fastjson) |
| Ruby | rspec or minitest / test-unit, gemspec-only gems included | CVE-2022-31163 (tzinfo) |
| PHP | phpunit | CVE-2022-31042 (guzzle) |

HTML/CSS sites run under the JavaScript profile. A CVE whose fix has several
commits (one per branch) is tried commit by commit until one confirms. A
fix whose test hangs on the vulnerable code (ReDoS, DoS) counts as
reproduced. If the upstream fix itself fails the existing suite, regressions
are not graded and the report says so.

Tests: `cargo test -p loop-defensive`. With podman:
`cargo test -p loop-defensive --test e2e_podman -- --ignored` and, per language,
`cargo test -p loop-defensive --test e2e_cve -- --ignored`.
