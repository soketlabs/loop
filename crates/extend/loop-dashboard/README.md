# loop-dashboard

The terminal usage dashboard for Loop. It has two sources:

- The **live status** of the running loop: its session, context window, agents, skills and
  MCP servers. Only `/dashboard` inside the chat has this.
- The **history** in the anonymous usage log that Loop writes to
  `<OS temp folder>/loop-traces`, one JSON-lines file per process.

| View | Shows |
|------|-------|
| **Now** | Codex `/status`-style session card: model and effort, directory, permissions, sandbox, web tools, session. Context window gauge and this session's tokens. Agents: the main agent (working or idle), the subscription agent, and running and finished sub-agents. Skills loaded and active, MCP servers with their tool counts, extensions and hooks. Today's totals. |
| **Usage** | tokens, cached tokens, model calls, tool calls, agent runs and sessions; tokens per day; model and tool calls per day; model calls by hour; top tools; models by tokens |
| **Models** | calls, input, output and cached tokens, average and p95 latency, and time to first token for each model |
| **Tools** | calls and average and p95 duration for each tool, its source (built-in or MCP server), the slowest tools, and calls by source |
| **Agents** | main agent and sub-agent runs, sub-agents spawned, agent runs per day, skills used, and calls to each MCP server |
| **Sessions** | recent sessions: last active, runs, sub-agents, calls, tokens and models |

Open it with `/dashboard` inside the chat, or with `loop dashboard` on its own, which has
history only. It uses the terminal's alternate screen, so the transcript is untouched when
you close it. While it is open the chat is paused, so **Now** is a snapshot from when you
opened it. History reloads every 5 seconds.

| Key | Action |
|------|--------|
| `1`–`6`, `Tab` | switch view |
| `←` `→` | change the period: 24h, 7d, 30d or all |
| `↑` `↓`, `PgUp` `PgDn` | scroll tables |
| `r` | reload now |
| `q`, `Esc` | close |

Skill use is recorded in two ways: skills you activate with `/skill` are tagged on the
run, and skills the model loads by reading their `SKILL.md` are tagged with the skill's
folder name. Sub-agents are workflow tasks, which run in their own `wf-task-*` sessions.
MCP usage comes from tool names (`mcp__<server>__<tool>`). None of this records user
content.

`loop_dashboard::summarize_dir` returns the same history without the UI.
