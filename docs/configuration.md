# Configuration

Global configuration is in `~/.loop/agent/`. Run `loop config` to print the paths.

| Path | Content |
|---|---|
| `settings.json` | Settings (see below) |
| `auth.json` | API keys (file mode `0600`) |
| `models.json` | Extra OpenAI-compatible providers, written by hand |
| `sessions.db` | Saved sessions (SQLite) |
| `skills/`, `prompts/`, `hooks/`, `extensions/`, `themes/` | Skills (`SKILL.md`), prompt templates, [hooks](hooks.md), extensions, themes |

In `models.json`, custom provider `models` can be string ids (listed with metadata from `/models`) or objects that pin details the listing omits. Object entries require `id`; `name`, `contextWindow`, `maxTokens`, and `reasoning` are optional. The context and token fields also accept snake_case names.

```json
{
  "providers": [{
    "id": "local",
    "baseUrl": "http://localhost:8000/v1",
    "models": [{ "id": "my-model", "name": "My model", "contextWindow": 131072, "maxTokens": 8192, "reasoning": true }]
  }]
}
```

A project can add `.loop/settings.json` in its root. Project values override global values. Arrays replace, they do not merge.

Common settings in `settings.json`:

| Key | Default | Purpose |
|---|---|---|
| `defaultProvider`, `defaultModel` | none | Provider and model at startup |
| `defaultThinkingLevel` | `off` | Reasoning level at startup (`off`, `low`, and so on); `/effort` or **Shift+Tab** changes it for the session |
| `enabledModels` | empty | Models that **Ctrl+P** cycles through, as glob patterns; see `/scoped-models` |
| `enableSkillCommands` | `true` | Turns the `/skill:name` commands on or off |
| `defaultProjectTrust` | `ask` | Whether a new project's settings are trusted: `ask`, `always` or `never` (global only) |
| `quietStartup` | `false` | Hides the startup banner |
| `hideThinkingBlock` | `false` | Hides the model's thinking blocks in the chat |
| `uiMode` | `normal` | `normal` or `fullscreen` |
| `permissionMode` | `ask` | `read-only`, `ask`, `accept-edits`, `full-access` or `plan`; see [permissions](permissions.md). A project may only choose a stricter mode |
| `toolPermissions` | `write`, `edit`, `bash`: `ask`; `read`, `grep`, `glob`, `ls`, `web_fetch`, `web_search`: `allow` | Per-tool rules, `allow`, `ask` or `deny`, with optional patterns such as `bash(git status*)`. `deny` applies in every mode; with nobody to ask, `ask` refuses the call. A project may only add `ask`/`deny` rules |
| `bash.defaultTimeoutMs`, `bash.maxTimeoutMs` | `120000`, `600000` | `bash` time limits; a call may ask for up to the maximum (global only) |
| `doubleEscapeAction` | `tree` | What pressing **Esc** twice quickly does while Loop is idle (one **Esc** interrupts a running turn): `tree` opens `/tree`, `fork` opens `/fork`, anything else does nothing |
| `responseHeaderTimeoutMs` | `60000` | How long to wait for the model's response headers before a request fails; `0` waits indefinitely (for slow models or long-running work). `/ttfb [off\|on\|60s\|120000]` changes it for the session. Esc still aborts the turn |
| `fileEditReview` | `newSession` | Kept for old settings files: with no `permissionMode`, `never` starts in `accept-edits`. Other values change nothing |
| `sandbox.mode` | `off` | `off` or `local` (Linux only) |
| `compaction` | enabled | `reserveTokens`, `keepRecentTokens` for `/compact` |
| `skills`, `disabledSkills` | empty | Extra skill paths, and skills to hide |
| `mcpServers`, `disabledMcpServers` | empty | MCP servers (stdio `command` or remote `url`) |
| `providers` | empty | Custom providers added with `/login custom` |
| `runtime.maxSessions`, `runtime.maxActiveTurns` | `16`, `8` | Limits of `loop runtime start`; see [the Runtime](runtime.md#start-the-runtime). Each must be a whole number of at least 1. A command-line flag overrides it (global only) |

Environment variables:

| Variable | Purpose |
|---|---|
| `SOKET_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Provider API keys |
| `GEMINI_API_KEY`, `GOOGLE_API_KEY` | Gemini API key (the first one set wins) |
| `LOOP_CODING_AGENT_DIR` | Use a different config directory |
| `LOOP_CODING_AGENT_SESSION_DIR` | Use a different directory for `sessions.db` |
| `LOOP_DEBUG=1` | Write debug logs (same as `--debug`) |
