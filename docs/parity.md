# Feature parity list: Claude Code, Codex and Loop

This is task Q1a of the [execution plan](execution-plan.md). It compares each Claude Code and Codex feature with the status of Loop.

- **Loop code checked:** branch `staging`, commit `2bffae3`, read only.
- **Date:** 2026-11-11.
- **Method:** a feature is "yes" only when the code or a shipped command confirms it. A document alone is not enough. Names that look like a match but are not one are listed in [Name matches that mislead](#name-matches-that-mislead).
- **No code was built or run. No model tokens were used.**
- **Update rule:** update the Loop columns at the end of each slice (see "Process" in the execution plan).

## How to read the table

- **Loop status:** `yes` = works on `staging`. `partial` = a part works. `no` = absent in this repository.
- **Competitor cells:** the letters in square brackets are source keys. See [Sources](#sources). A key with `*` means the page is in the official index, but this task did not read it. A cell that says "not documented" means the pages that were read do not list the feature. It does not mean the product lacks it.
- **Plan column:** the phase or step name from `execution-plan.md`. "Not planned" means no step covers the feature.
- **Loop evidence:** paths are relative to the `loop` repository (`staging`).

## Feature table

| # | Feature | Claude Code | Codex | Loop status | Loop evidence | Where in the Loop plan | Notes |
|---|---|---|---|---|---|---|---|
| 1 | Interactive TUI | Yes, full CLI [CC-overview] | Yes, `codex` CLI and TUI [CX-cli] | yes | `crates/loop-cli` (ratatui); `README.md` | Done (S3) | Loop TUI is a Runtime client: two TUIs can share one Session (`docs/runtime.md`). |
| 2 | Headless / print mode and JSON output | `claude -p`; `text`, `json`, `stream-json`; `--json-schema`; `--bare`; cost in JSON [CC-headless] | `codex exec`; `--json` JSON Lines; `--output-schema`; `-o`; `exec resume` and `exec fork` [CX-exec] [CX-cmds] | partial | `docs/print-mode.md`; `crates/loop-cli/src/print_mode.rs`; flag `--output-format` in `main.rs` | Q3 "`--print` through the Runtime" | Loop has `text`, `json`, `stream-json` and fixed exit codes (0, 1, 2, 3, 130). Loop has no JSON-Schema output and no documented stdin pipe. `--print` runs in-process, so it has no Runtime Subagents. |
| 3 | Permission modes and rules | Six modes (`default`, `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions`); allow/ask/deny rules [CC-modes] | `read-only`, `workspace-write`, `danger-full-access`; approval policies; command-prefix rules; auto-review agent [CX-sandbox] | yes | `docs/permissions.md`; `crates/loop-app-core/src/permissions/`, `tool_gate.rs` | Done; S4 (Phase 1) makes one gate for all paths | Loop modes: `read-only`, `ask`, `accept-edits`, `full-access`, `plan`. Pattern rules such as `bash(git status*)`. Loop has no classifier mode like `auto`. |
| 4a | Sandbox: macOS | Seatbelt, with a network proxy and a domain allowlist [CC-sandbox] | Seatbelt, built in [CX-sandbox] | partial | `docs/design/macos-sandbox.md`; `crates/loop-agent/src/harness/sandbox/seatbelt.rs` | Not planned as a separate step | Loop has partial isolation only. `--full` is not available. Network is on or off for shell commands; there is no domain filter. File tools run in the Loop process. |
| 4b | Sandbox: Linux | bubblewrap and socat; seccomp option [CC-sandbox] | bubblewrap [CX-sandbox] | partial | `crates/loop-agent/src/harness/sandbox/podman.rs`, `krun.rs`; `README.md` "Sandboxing" | Q3 "light Linux sandbox (Landlock and seccomp)"; Podman stays for strong isolation | Loop needs rootless Podman (runc, crun, gVisor or libkrun). It has strong isolation but a heavy setup. `sandbox.network` and `sandbox.allowWrite` are not enforced on Linux yet (`docs/configuration.md`). Nobody ran Loop on Linux locally yet (`docs/roadmap.md`). |
| 5a | Hooks | 33 events; handlers: command, http, mcp_tool, prompt, agent [CC-hooks] | 14 events (`PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`, `SubagentStart` and more); user, project, plugin and managed hooks [CX-hooks] | partial | `docs/hooks.md`; `crates/loop-app-core/src/hooks_load.rs`, `hook_command.rs` | Q3 "hooks that can change arguments" | Loop has JSON hook files and command hooks. Events: `tool_call`, `tool_result`, `before_agent_start`, `session_before_compact`, `session_before_tree` and lifecycle events. No http, prompt or agent handlers. |
| 5b | Hooks that change tool input | `PreToolUse` `updatedInput` [CC-hooks] | `PreToolUse` `updatedInput` with `permissionDecision: "allow"` [CX-hooks] | no | `docs/hooks.md`: "Hooks can't change a tool's arguments." | Q3 "hooks that can change arguments" (Low, Sonnet, parallel) | The CC page was read only in part. The exact `updatedInput` schema for CC is not confirmed here. |
| 6 | Slash commands and custom commands | Many built-ins; custom commands and skills [CC-commands*] | Built-ins; custom prompts `/prompts:<name>`; skills with `$` [CX-slash] | yes | `crates/loop-cli/src/commands/mod.rs` (about 35 built-ins); `crates/loop-agent/src/harness/prompt_templates.rs` (`~/.loop/agent/prompts/*.md`, argument substitution) | Done | Loop also has Rhai commands (`crates/loop-app-core/src/extensions/mod.rs`). Rhai `register_tool` is a stub. `/workflow` needs the `orchestration` build feature. |
| 7 | Skills | `SKILL.md`; forked-context skills; shared in plugins [CC-skills*] [CC-checkpoint] | Skills in the CLI; `$` invocation; in plugins [CX-cli] [CX-slash] | yes | `crates/loop-agent/src/harness/skills.rs`; `/skills`; `crates/loop-cli/README.md` | Done | Loop reads `~/.claude/skills`, `~/.agents/skills` and project `.claude/skills`. It supports `disable-model-invocation`. Forked-context skills: not found in code. |
| 8a | Subagents (built-in) | `Agent` tool; nested subagents; background subagents [CC-tools] [CC-headless] | Built-in `default`, `worker`, `explorer`; parallel threads [CX-subagents] | partial | `crates/loop-runtime/src/runtime/subagent.rs` (`subagent` tool, default depth 2) | Phase 1 S4 (one gate covers Subagents); Q3 "`--print` through the Runtime"; Phase 3 Swarms v1 | The tool exists only inside the Runtime. It is not in `--print`. The child Session has its parent's Policy plus one tightening layer. |
| 8b | Custom subagent definitions | `.claude/agents/`; `--agents` flag [CC-headless] [CC-subagents*] | TOML files in `~/.codex/agents/` and `.codex/agents/` [CX-subagents] | no | None. `crates/loop-app-core/src/agents.rs` is subscription providers over ACP, not subagent definitions. | Q3 "custom Subagent definitions" (Low, Sonnet); later Phase 3 "Agent Definitions" | Phase 3 Agent Definitions is the larger manifest design. Q3 is the small first step. |
| 9a | MCP client | `--mcp-config`; `.mcp.json`; MCP pages and managed MCP are listed, not read [CC-headless] [CC-index] | `codex mcp` add, auth, list; `/mcp` [CX-cli] [CX-slash] | yes | `crates/loop-mcp/src/client.rs` (stdio and streamable HTTP, OAuth); `/mcp` command; `mcpServers` setting | Done | MCP read-only hint is used in `read-only` mode (`docs/permissions.md`). |
| 9b | MCP server (expose own tools) | Not verified in the pages read | `codex mcp-server` was removed; use app-server [CX-cmds] | partial | `crates/loop-mcp/src/server.rs`; `crates/loop-cli/src/main.rs` flag `--serve-mcp`, `--mcp-host`, `--mcp-port`, `--mcp-token` | Not planned | The docs call it `mcp serve`, but the real flag is `--serve-mcp`. It serves streamable HTTP only: no stdio transport. It cannot run with an external agent. |
| 10a | Memory files (`CLAUDE.md`, `AGENTS.md`) | `CLAUDE.md` files; reads `AGENTS.md` instead of one [CC-overview] | `AGENTS.md`, `AGENTS.override.md`, global and project, 32 KiB cap [CX-agentsmd] | yes | `crates/loop-app-core/src/system_prompt.rs` (lines 10-13: `AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md`; also `CLAUDE.local.md`, `~/.claude/CLAUDE.md`); `-c` flag to skip | Done | Loop loads all of these files for each directory up the tree. |
| 10b | Auto memory and `/init` scaffold | Auto memory saves learnings [CC-overview] | `/init` scaffold; `/memories` [CX-slash] | no | No `/init` in `commands/mod.rs`. No memory writer found. | Not planned | Low cost: a prompt template can do `/init` today. |
| 11 | Context compaction (auto and manual) | `/rewind` "summarize from here" is a targeted `/compact` [CC-checkpoint]. Automatic compaction: not verified in the pages read. | `/compact`; `PreCompact` and `PostCompact` hooks [CX-slash] [CX-hooks] | partial | Manual: `/compact` in `commands/mod.rs`; `crates/loop-agent/src/harness/compaction/mod.rs`. Auto: `should_compact` has no caller outside tests (`grep`). | Q2 "automatic compaction" (High, Opus, Q-loop, second); probe E9 | Hooks `session_before_compact` exist. Compaction is not available for subscription agents (`/compact` says so). |
| 12 | Checkpoints and rewind | Per-prompt file checkpoints; `/rewind`; restore code, conversation or both [CC-checkpoint] | Not documented in the pages read. `/fork` and `codex fork` copy a chat. [CX-slash] [CX-cmds] | no | `/tree` and `/fork` rewind the conversation only (`crates/loop-cli/README.md` "Sessions"). `checkpoint.rs` in `loop-orchestration` is for workflow state. | Q3 "file checkpoints and rewind" (High, Opus, after Q-loop) | Loop can go back in the chat but cannot undo file edits. |
| 13 | Git worktrees per session | `EnterWorktree` tool; worktree per remote-control session (`--spawn worktree`) [CC-tools] [CC-rc] | Worktree chats in the app; `--worktree` on `exec` and `fork`; handoff [CX-worktrees] [CX-cmds] | no | None. Seatbelt only handles worktrees as input (`docs/configuration.md`). | Q3 "git worktrees per Session"; Phase 1 chain C "Workspace driver: local, worktree, container" | Phase 1 chain C makes worktree a Workspace driver. |
| 14 | Background shell and long-running processes | `Bash` background commands; `Monitor`; `TaskStop` [CC-tools] [CC-headless] | Long-running work page listed, not read [CX-index*] | no | `bash` has `timeoutMs` only (`docs/permissions.md`, max 600 s). No `run_in_background`. | Q2 "background shell tool" (Low, Sonnet, Q-tools) | A dev server or watch build cannot run beside the agent. |
| 15 | `apply_patch`-style editing | None: uses `Edit` and `Write` [CC-tools] | `apply_patch` tool [CX-hooks] | no | `grep apply_patch` finds nothing. Loop has `read`, `write`, `edit`. | Q2 "`apply_patch` tool" (Low, Sonnet, Q-tools) | Claude Code has no `apply_patch`, so this is a Codex-model fit item. It may help models trained on that format. |
| 16 | Web search and web fetch | `WebSearch`, `WebFetch` [CC-tools] | `--search` live search; cached by default [CX-cmds] | yes | `crates/loop-agent/src/harness/tools/web/`; `/websearch`; backends Soket, Tavily, TinyFish, Brave | Done | `web_fetch` has an SSRF guard. Both tools run in the Loop process, not in the sandbox. |
| 17 | Image input | Attach images; photos from phone [CC-rc] | `--image`; paste in composer [CX-cli] | yes | `crates/loop-cli/src/tui/attachments.rs`; `crates/core/loop-protocol/src/content.rs` | Done | PNG, JPEG, GIF, WebP up to 5 MB. Ctrl+V, drag, or `@path`. No Wayland clipboard. |
| 18 | Plan mode | `plan` mode, `EnterPlanMode`/`ExitPlanMode` [CC-modes] [CC-tools] | `/plan` [CX-slash] | yes | `/plan`; `crates/loop-agent/src/harness/tools/plan.rs`; `docs/permissions.md` "Plan mode" | Done | In `--print`, the agent gives the plan as its answer and changes nothing. |
| 19 | Model and effort switching | `/model`, `/effort`, `/fast` [CC-headless] | `/model`, `/reasoning`, `/fast` [CX-slash] | yes | `/model`, `/effort`, `shift+tab`; `crates/loop-agent/src/effort.rs` (7 levels) | Done | Loop applies a change from the next turn. |
| 20 | Resume, continue and fork sessions | `--continue`, `--resume`, `--fork-session`, `/branch` [CC-checkpoint] [CC-headless] | `codex resume`, `codex fork`, `exec resume` [CX-cmds] | yes | `/resume`, `/sessions`, `/fork`, `/clone`, `/tree`, `/export`, `/import`; `loop --continue`, `--resume` | Done | Loop adds Session attach from another terminal (`loop session attach`). |
| 21a | IDE extension: VS Code | Extension (also Cursor) [CC-overview] | Extension for VS Code, Cursor, Windsurf [CX-ide] | no | `diffEditor` setting only opens a file in Cursor or VS Code with `--diff`. This is not an extension. | Q3 "VS Code extension" (Low, Sonnet, after the SDK) | Needs the TypeScript SDK first. |
| 21b | IDE extension: JetBrains | Plugin [CC-overview] | JetBrains and Xcode have their own integrations [CX-ide] | no | None | Q3 "Loop as an ACP agent" gives JetBrains support | |
| 21c | ACP / Zed | Not documented in the pages read | Not documented in the pages read (Loop's own docs name an adapter, `codex-acp`, which Loop uses for ChatGPT) | no | `crates/extend/loop-acp` runs other ACP agents as Loop backends. Loop is not an ACP agent. | Q3 "Loop as an ACP agent" (High, Opus, `extend/loop-acp`) | Direction is the opposite of what the crate name suggests. |
| 22 | Desktop app | Desktop app: diffs, parallel sessions, scheduled tasks [CC-overview] | ChatGPT desktop app: worktrees, remote control host [CX-index] [CX-rc] | partial | `crates/loop-desktop` (GPUI): chat, diff panel, approval modal, PTY dock | Phase 3 "Desktop and integrations" | The desktop app links `loop-app-core` and `loop-agent` directly. It is not a Runtime client. Release builds it; PR CI has a desktop check. |
| 23 | Web app: chat, cloud agent mode, PR creation | claude.ai/code cloud sessions; parallel tasks; PRs [CC-overview] | Codex Cloud: sandboxed tasks, parallel, GitHub/GitLab, commit and PR [CX-cloud] | no | No web code in this repository. A prototype exists in the separate repository `soketlabs/loop-web-agentic-app` (Rust gateway plus Next.js UI). | Phase 2 "Web v1 and the cloud Runtime" | Keep the `web/` UI, change the gateway (see the evaluation in `execution-plan.md`). It is for one machine today. |
| 24 | Remote control from a phone | Outbound HTTPS relay through the Anthropic API; URL and QR code; `claude remote-control`, `--remote-control`, `/rc`; Pro, Max, Ultra, Team, Enterprise plans; no API keys [CC-rc] | Phone pairing by QR code in the ChatGPT desktop app; relay layer; Mac or Windows host [CX-rc] [CX-mobile]. `codex --remote ws://...` attaches a TUI to an app-server [CX-appserver] | partial | `docs/runtime.md`: `loop runtime start --listen IP:PORT` (loopback only, token required); `loop --connect ws://...` | Phase 0 "S3.8b TLS", then "S3.8b connection by URL or QR code"; Phase 3 "Relays" | Loop has the transport but no TLS, no QR code, no relay and no phone app. The task text said Codex uses `app-server --remote` plus relay pairing. The docs say two separate paths: QR plus relay for phones, `--remote` for a terminal. |
| 25a | SDK: TypeScript | Agent SDK [CC-headless] | Codex SDK (Node 18+) [CX-sdk] | no | `crates/core/loop-protocol/schema/loop-protocol.json` and the `schema` feature only prepare it. | Phase 2 "TypeScript SDK (protocol client) first"; N8 | The Web Surface becomes the first SDK client. |
| 25b | SDK: Python | Agent SDK [CC-index] | Codex SDK, Python 3.10+, controls the app-server [CX-sdk] | no | None | Q3 "Python SDK" (Low, Sonnet, after the SDK) | |
| 26 | GitHub Action and PR review | GitHub Actions, GitLab CI/CD and Code Review on PRs (pages listed in the overview and the index; not read) [CC-overview] | `openai/codex-action@v1`; PR review comments [CX-action] | no | `.github/workflows/` only runs Loop's own CI. Loop `/review` shows the session's approvals. It is not code review. | Q3 "GitHub Action and PR review" (Low, Sonnet, no Rust build) | |
| 27 | Plugins and marketplace | Plugins; marketplaces; `--plugin-dir` [CC-index] [CC-headless] | Plugins bundle skills, MCP, hooks; `codex plugin`; shared catalog [CX-plugins] [CX-cmds] | no | Loop loads skills, hooks, prompts and Rhai extensions from folders. There is no bundle format or registry. | Q3 "plugins with a registry" (High, Opus, later); Phase 3 "Plugins" in the vision | |
| 28a | Enterprise: managed policy and Locks | Managed settings; server-managed settings; managed MCP [CC-index] | `requirements.toml` managed constraints and hooks [CX-admin] | partial | `crates/loop-app-core/src/permissions/layers.rs` (layers Managed to Session, Locks); used for the Subagent child Policy in `crates/loop-runtime/src/runtime/subagent.rs` (line 143) | Phase 1 "S4 Policy" (layers and Locks); Phase 3 "Managed Policy (after the H11 probe)" | The layer engine exists. No loader reads an OS-protected managed file. A project may only tighten settings today. |
| 28b | Enterprise: SSO, groups, admin console | Managed and server-managed settings pages are listed; SSO not verified [CC-index] | RBAC, groups, Compliance API; SSO page not read [CX-admin] | no | None | Phase 3 "Org: OIDC and SCIM, Admin v1"; identity fields `principal` and `tenant` move to S4 | |
| 29 | Telemetry and usage views | Monitoring and team analytics pages are listed; not read [CC-index] | `/status`, `/usage`; Analytics API [CX-slash] [CX-admin] | partial | `crates/loop-telemetry`, `crates/extend/loop-dashboard`; `/usage`, `loop usage`; Langfuse and OTLP export (`/tracing`) | Phase 3 "Admin v1: usage and cost" | Local usage log and view: yes. Org-wide analytics: no. The usage log never records prompts. |
| 30 | Multiple model providers | Anthropic; Bedrock, Vertex, Foundry; LLM gateways [CC-index] | OpenAI; `--oss` with LM Studio or Ollama [CX-cmds] | yes | `crates/loop-ai/src/api/` (`anthropic_messages.rs`, `openai_responses.rs`, `openai_completions.rs`, `google_generative_ai.rs`); `crates/loop-ai/src/providers/` (`anthropic.rs`, `gemini.rs`, `soket.rs`, `custom.rs`); ChatGPT subscription over ACP (`crates/loop-app-core/src/agents.rs`) | Done; Step 2 adds Bedrock/Vertex adapters and local inference | This is the main place where Loop is wider than both products. Bedrock and Vertex are not native in Loop. |

## Extra features seen in the competitor docs

These features are outside the requested rows. They are listed so they are not lost. They are not in the counts.

| Feature | Claude Code | Codex | Loop | Plan |
|---|---|---|---|---|
| Scheduled and recurring tasks | Cloud scheduled tasks, desktop tasks, `/loop` [CC-overview] | Scheduled tasks [CX-index] | no | Not planned |
| Browser and computer use | Chrome integration, computer use [CC-index] | Browser, Computer Use [CX-index] | no | Not planned |
| Chat integrations (Slack, Linear) | Slack, channels [CC-overview] | Slack, Linear, GitLab [CX-index] | no | Not planned |
| Multi-agent teams and workflows | Agent teams, dynamic workflows [CC-index] | Subagents [CX-subagents] | partial: `/workflow` behind the `orchestration` feature | Phase 3 Swarms v1 (gated by E2) |
| LSP code intelligence | `LSP` tool [CC-tools] | Not documented | no | Not planned |
| Open server protocol for custom clients | SDK stream (`stream-json`) | `codex app-server` (JSON-RPC, stdio, WebSocket) [CX-appserver] | partial: `loop-wire` JSON-RPC 2.0 over Unix socket and loopback WebSocket; JSON Schema checked in | Phase 2 SDK; N8 |
| Shared Session on many Surfaces | Remote Control | Remote connections | yes: Runtime Sessions survive quit; two TUIs share a Session | Done (S3) |

## Name matches that mislead

A search of the code finds these words. They are not the feature.

| Word in the code | What it is | Right status |
|---|---|---|
| `loop-acp` crate | Loop runs other ACP agents as backends. | Loop is not an ACP agent. |
| `/review` | Shows the Session's approvals and `toolPermissions`. | Not code review. |
| `checkpoint` | Workflow state in `loop-orchestration`. | Not file checkpoints. |
| `worktree` | Sandbox path handling. | Not a worktree-per-Session feature. |
| `agents.rs` | Subscription providers over ACP. | Not custom subagents. |
| `diffEditor` | Opens VS Code or Cursor with `--diff`. | Not an IDE extension. |
| `register_tool` (Rhai) | A stub. | Not a shipped feature. |
| `/workflow` | Needs the `orchestration` cargo feature. | Not in the default build. |
| `should_compact` | Has tests only. | Not wired into the loop. |
| `wss://` in `runtime_launch.rs` | Accepts the URL text. The Runtime refuses non-loopback binds until TLS exists. | No TLS yet. |

## Counts

Counts are for the 39 rows of the feature table (the extra table is not counted). They were calculated from the Status column with `awk`.

| Status | Rows |
|---|---|
| yes | 12 |
| partial | 11 |
| no | 16 |
| **total** | **39** |

## Top 10 gaps, ranked by user impact

Rank is by the effect on daily use. The plan lists gaps 1, 2 and 4 under Q2 as probable work for the correctness gap. The Q1b analysis (`docs/probes/q1b-loop-failure-analysis.md`) found other causes for run A: work left in a scratch copy, a silent end on an empty final message, and a grader false negative. It says compaction did not cause any run A failure, but it is still a gap for longer tasks. So the ranking below is by feature impact for users, not by run A cause.

| Rank | Gap (table row) | Status | Why it matters | Suggested plan step |
|---|---|---|---|---|
| 1 | Automatic compaction (row 11) | partial | Long tasks hit the context limit. `should_compact` has no caller. | Q2 "automatic compaction" (High, Opus, Q-loop, second). Test with E9. |
| 2 | Background shell (row 14) | no | The agent cannot run a dev server or watch build and keep working. | Q2 "background shell tool" (Low, Sonnet, Q-tools). |
| 3 | File checkpoints and rewind (row 12) | no | A user cannot undo a bad edit run except with git. Claude Code treats this as a basic safety tool. | Q3 "file checkpoints and rewind" (High, Opus, after the Q-loop chain). |
| 4 | `apply_patch` tool (row 15) | no | Multi-file edits take many calls. Models trained on the patch format may do better with it. | Q2 "`apply_patch` tool" (Low, Sonnet, Q-tools). |
| 5 | Hooks that change tool input, and more events (rows 5a, 5b) | partial, no | Both competitors let a hook rewrite a call. Teams use this for policy and formatting. | Q3 "hooks that can change arguments" (Low, Sonnet). |
| 6 | Custom subagent definitions (row 8b) | no | Teams cannot ship a reviewer or explorer agent with their repository. | Q3 "custom Subagent definitions" (Low, Sonnet). |
| 7 | IDE support: VS Code extension and Loop as an ACP agent (rows 21a, 21b, 21c) | no | Most users work in an editor. ACP gives Zed and JetBrains with one build. | Q3 "Loop as an ACP agent" (High, Opus), then "VS Code extension" (Low, Sonnet, after the SDK). |
| 8 | GitHub Action and PR review (row 26) | no | This is the entry point for teams and CI. | Q3 "GitHub Action and PR review" (Low, Sonnet). |
| 9 | Web app with cloud agent mode and PR creation (row 23) | no | Both competitors sell this. It reaches users without a terminal. | Phase 2 "Web v1 and the cloud Runtime" (start with the protocol move of the prototype in Phase 0). |
| 10 | Git worktrees per Session (row 13) | no | Parallel Sessions on one repository overwrite each other. | Q3 "git worktrees per Session", then Phase 1 chain C "Workspace driver: worktree". |

Close behind (not in the top 10):

- **Phone remote control (row 24, partial):** Phase 0 "S3.8b TLS", then "S3.8b connection by URL or QR code".
- **Light Linux sandbox (row 4b, partial):** Q3 "light Linux sandbox (Landlock and seccomp)". Podman is a high setup cost for Linux users.
- **Plugins with a registry (row 27), TypeScript SDK (row 25a), Python SDK (row 25b):** Phase 2 SDK, Q3 plugins and Python SDK.
- **Enterprise SSO and admin (row 28b):** Phase 3 "Org".

## Sources

All pages were fetched on 2026-11-11. Codex docs moved: `developers.openai.com/codex/<page>` now redirects (HTTP 308) to `learn.chatgpt.com/docs/<page>`. Only the pages below returned content. A key with `*` is a page listed in an index but not read.

**Claude Code**

- [CC-overview] https://code.claude.com/docs/en/overview
- [CC-index] https://code.claude.com/docs/llms.txt (page list: plugins, managed settings, analytics, monitoring, gateways, SDK, GitLab, Code Review)
- [CC-rc] https://code.claude.com/docs/en/remote-control (the local session makes outbound HTTPS requests only and opens no inbound ports; QR code; `--spawn worktree`)
- [CC-checkpoint] https://code.claude.com/docs/en/checkpointing
- [CC-hooks] https://code.claude.com/docs/en/hooks (read in part)
- [CC-sandbox] https://code.claude.com/docs/en/sandboxing (read in part)
- [CC-headless] https://code.claude.com/docs/en/headless
- [CC-modes] https://code.claude.com/docs/en/permission-modes (read in part)
- [CC-tools] https://code.claude.com/docs/en/tools-reference
- [CC-skills*] https://code.claude.com/docs/en/skills
- [CC-subagents*] https://code.claude.com/docs/en/sub-agents
- [CC-commands*] https://code.claude.com/docs/en/commands

**Codex**

- [CX-index] https://learn.chatgpt.com/docs (surface and feature list)
- [CX-cli] https://learn.chatgpt.com/docs/codex/cli
- [CX-cmds] https://learn.chatgpt.com/docs/developer-commands (read in part)
- [CX-exec] https://learn.chatgpt.com/docs/non-interactive-mode
- [CX-slash] https://learn.chatgpt.com/docs/reference/slash-commands (desktop app list)
- [CX-sandbox] https://learn.chatgpt.com/docs/sandboxing
- [CX-hooks] https://learn.chatgpt.com/docs/hooks
- [CX-subagents] https://learn.chatgpt.com/docs/agent-configuration/subagents
- [CX-worktrees] https://learn.chatgpt.com/docs/environments/git-worktrees
- [CX-agentsmd] https://learn.chatgpt.com/docs/agent-configuration/agents-md
- [CX-appserver] https://learn.chatgpt.com/docs/app-server (read in part)
- [CX-rc] https://learn.chatgpt.com/docs/remote-connections
- [CX-mobile] https://learn.chatgpt.com/docs/mobile
- [CX-cloud] https://learn.chatgpt.com/docs/cloud
- [CX-ide] https://learn.chatgpt.com/docs/ide
- [CX-sdk] https://learn.chatgpt.com/docs/codex-sdk
- [CX-action] https://learn.chatgpt.com/docs/github-action
- [CX-plugins] https://learn.chatgpt.com/docs/plugins
- [CX-admin] https://learn.chatgpt.com/docs/enterprise/admin-setup
- [CX-index*] pages listed in the index only: long-running work, SSO (`/codex/auth`), web search, image inputs
- Repository: https://github.com/openai/codex (the README lists CLI, IDE extension, `codex app` desktop app and Codex Web)

**Loop** (`staging`, `2bffae3`): `README.md`; `docs/configuration.md`, `permissions.md`, `hooks.md`, `runtime.md`, `print-mode.md`, `ci.md`; `docs/design/macos-sandbox.md`, `tui-client.md`, `external-agents.md`; `crates/loop-cli/README.md`. Plan: `docs/execution-plan.md`, `docs/roadmap.md`, `docs/vision.md` on `docs/loop-vision`.

## Limits of this list

- Competitor pages change often. Claude Code and Codex both shipped many features in the last months (for example agent view, `/goal`, channels, computer use). Check the version of the page when you use a cell.
- Some competitor cells rest on an index entry or on a page read only in part. They carry a `*` or the words "read in part".
- Branches that are in flight (for example `fix/q2-agent-correctness`, `feat/tool-call-preparing`) are not counted. They had no commit beyond `staging` when this list was written.
