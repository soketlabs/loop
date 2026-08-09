# loop-cli

Interactive coding agent CLI for **Loop** by Soket AI.

Built on [`loop-ai`](../loop-ai) and [`loop-agent`](../loop-agent), with a ratatui TUI inspired by pi’s coding-agent UX.

## Install / run

```bash
cargo run -p loop-cli
# or
cargo install --path crates/loop-cli
loop
```

On first start, Loop prompts for your **Soket API key** (unless `SOKET_API_KEY`, `TENSORSTUDIO_API_KEY`, or `LOOP_API_KEY` is set). Keys are stored in `~/.loop/agent/auth.json` (mode `0600`).

Default provider/model: **`soket` / `qwen3-30b`** at `https://api.tensorstudio.ai/v1`. The model catalog is refreshed from `GET /v1/models` and cached in `models-store.json`.

## Config layout

| Path | Purpose |
|------|---------|
| `~/.loop/agent/settings.json` | Defaults (model, theme, sandbox, skills…) |
| `~/.loop/agent/auth.json` | API keys |
| `~/.loop/agent/models.json` | Extra OpenAI-compat providers |
| `~/.loop/agent/models-store.json` | Dynamic catalog cache |
| `~/.loop/agent/themes/*.json` | Custom themes (pi-compatible tokens) |
| `~/.loop/agent/skills/` | Agent Skills (`SKILL.md`) |
| `~/.loop/agent/prompts/` | Prompt templates → `/name` |
| `~/.loop/agent/extensions/*.rhai` | Rhai extensions |
| `~/.loop/agent/hooks/*.json` | Declarative hooks |
| `~/.loop/agent/sessions.db` | SQLite sessions |
| `.loop/` (project) | Project overlays (requires trust) |

Override root with `LOOP_CODING_AGENT_DIR`.

Claude skills are **opt-in** via settings:

```json
{ "skills": ["~/.claude/skills"] }
```

`AGENTS.md` / `CLAUDE.md` are loaded automatically from the agent dir and cwd ancestors.

## Slash commands (highlights)

`/theme`, `/sandbox`, `/model`, `/settings`, `/login`, `/logout`, `/new`, `/review`, `/compact`, `/resume`, `/tree`, `/fork`, `/clone`, `/trust`, `/reload`, `/hotkeys`, `/help`, `/quit`, plus `/skill:name` and prompt templates.

## Keybindings

| Shortcut | Action |
|----------|--------|
| `enter` | Send |
| `shift+enter` / `ctrl+j` | New line |
| `escape` | Interrupt |
| `ctrl+c` | Clear (twice quits) |
| `ctrl+l` | Model picker |
| `ctrl+p` | Cycle models |
| `shift+tab` | Cycle thinking |
| `ctrl+t` | Toggle thinking visibility |
| `ctrl+x` | Copy last assistant message |
| `ctrl+g` | External editor (`$EDITOR`) |

See `/hotkeys`. Override in `keybindings.json`.

## Themes

Ship `dark` and `light`. Custom JSON themes use the same color tokens as pi. Change with `/theme name` or `settings.theme`.

## Sandbox

`/sandbox off` — host tools  
`/sandbox local-shell` — soft workdir isolation via `LocalShellSandbox`

## File edit review / tool approval

On **new sessions**, `write` / `edit` / `bash` pause for approval (per `toolPermissions`). File edits open Cursor or VS Code with `--diff`. Options:

1. **Accept** — this change only  
2. **Accept all … for this session** — remembered on the session (survives resume)  
3. **Reject** — revert / block; **Tab** adds a reason for the model

```json
{
  "fileEditReview": "newSession",
  "diffEditor": "cursor",
  "toolPermissions": {
    "write": "ask",
    "edit": "ask",
    "bash": "ask",
    "read": "allow"
  }
}
```

- `fileEditReview`: `newSession` (default) · `always` · `never` — when `ask` tools prompt  
- `toolPermissions`: per-tool `ask` | `allow` | `deny` in `~/.loop/agent/settings.json`  
- Interactive workflows are wired for `write`, `edit`, and `bash` (others can be configured for later)  
- `/review [newSession|always|never]` toggles the ask policy

## Extensions & hooks

- **Rhai**: `register_command`, `notify`, stubs for `register_tool` / `on` / …
- **JSON hooks**: `on` lifecycle names (`before_agent_start`, `session_before_compact`, …)

Examples under [`examples/`](examples/).

## CLI flags

```
loop [--provider soket] [--model qwen3-30b] [--theme dark]
     [--resume <id>] [--print "prompt"] [--no-context-files]
loop config
```

## Tests

```bash
cargo test -p loop-cli
cargo test -p loop-ai
cargo test -p loop-agent
```
