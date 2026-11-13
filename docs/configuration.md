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

Custom providers added with `/login custom` can also save per-model thinking metadata in the global `settings.json`:

```json
{
  "providers": [{
    "id": "my-gateway",
    "name": "My Gateway",
    "baseUrl": "https://gateway.example/v1",
    "models": [
      { "id": "glm-5.3", "reasoning": true, "thinkingFormat": "zai" },
      { "id": "another-model", "reasoning": true, "thinkingFormat": "openai" }
    ]
  }]
}
```

`models` is optional, so existing provider settings still work. Each entry pins an exact model id and its capabilities. `reasoning` defaults to `false`; `thinkingFormat` is optional and otherwise uses automatic compatibility detection. `supportsMetadata` (`true` or `false`) is optional: it decides whether requests carry the `metadata` object used for session affinity. Without it, Loop sends `metadata` only to Soket hosts, because strict servers reject the unknown field. `openai` sends `reasoning_effort` for each enabled effort level and omits it for off. `zai` sends `thinking: {"type": "enabled"}` for every enabled level and `thinking: {"type": "disabled"}` for off. Zai does not distinguish the enabled effort levels.

After the name, URL and key, `/login custom` asks which model ids support thinking and which format they use (default `openai`). Enter comma-separated ids, or leave the answer blank to keep saved metadata on reconnect and use server defaults on first login. Enter `none` to clear saved metadata. For mixed capabilities or formats, edit individual entries in settings. Reconnect, restart and catalog refresh retain the metadata.

Precedence for a matching model id, from highest to lowest: a `models.json` entry, a saved provider `models` entry, server-published metadata (including `supported_parameters`), then built-in defaults. Entries are pinned as complete model specifications; unspecified context and output limits use the custom-provider defaults (128,000 and 16,384 tokens). Other models remain discoverable from `/models`. `models.json` overrides also accept `thinkingFormat` (or `thinking_format`) and `supportsMetadata` (or `supports_metadata`) alongside the existing model fields; its provider URL and key environment settings take precedence when that provider id appears in both files. These overrides apply consistently during login and at startup.

A project can add `.loop/settings.json` in its root. Project values override global values; a key the project file omits keeps its global value. Arrays replace, they do not merge.

Common settings in `settings.json`:

| Key | Default | Purpose |
|---|---|---|
| `defaultProvider`, `defaultModel` | none | Provider and model at startup |
| `defaultThinkingLevel` | `off` | Reasoning level at startup (`off`, `low`, and so on); `/effort` or **Shift+Tab** changes it for the session |
| `enabledModels` | empty | Models that **Ctrl+P** cycles through, as exact model ids (`id`) or `provider/id`; no wildcards; see `/scoped-models` |
| `defaultProjectTrust` | `ask` | Whether a new project's settings are trusted: `ask`, `always` or `never` (global only) |
| `hideThinkingBlock` | `false` | Hides the model's thinking blocks in the chat |
| `permissionMode` | `ask` | `read-only`, `ask`, `accept-edits`, `full-access` or `plan`; see [permissions](permissions.md). A project may only choose a stricter mode |
| `toolPermissions` | `write`, `edit`, `bash`: `ask`; `read`, `grep`, `glob`, `ls`, `web_fetch`, `web_search`: `allow` | Per-tool rules, `allow`, `ask` or `deny`, with optional patterns such as `bash(git status*)`. `deny` applies in every mode; with nobody to ask, `ask` refuses the call. A project may only add `ask`/`deny` rules |
| `bash.defaultTimeoutMs`, `bash.maxTimeoutMs` | `120000`, `600000` | `bash` time limits; a call may ask for up to the maximum (global only) |
| `tools.passEnv` | empty | Variable names that stay in the environment of the `bash` tool and `!shell` commands, although Loop removes provider keys and Runtime tokens from them. See [Tool environment](#tool-environment) (global only; a project file cannot set it) |
| `doubleEscapeAction` | `tree` | What pressing **Esc** twice quickly does while Loop is idle (one **Esc** interrupts a running turn): `tree` opens `/tree`, `fork` opens `/fork`, anything else does nothing |
| `responseHeaderTimeoutMs` | `120000` | How long to wait for the model's response headers before a request fails; `0` waits indefinitely (for slow models or long-running work). `timeouts.responseHeaderMs` takes precedence when set; see [stream timeouts](stream-timeouts.md). `/ttfb [off\|on\|60s\|120000]` changes it for the session; `/ttfb on` chooses 120 seconds. Esc still aborts the turn |
| `fileEditReview` | `newSession` | Kept for old settings files: with no `permissionMode`, `never` starts in `accept-edits`. Other values change nothing |
| `sandbox.mode` | `off` | `off` or `local`. Podman on Linux, Seatbelt partial isolation on macOS. A project may enable the sandbox, never turn it off |
| `sandbox.isolation` | platform default | `full` or `partial`. Unset means `full` on Linux and `partial` on macOS. `full` on macOS is an error. Any other value is an error when `mode` is `local`. A project value overrides the global one only when the project sets it |
| `sandbox.runtime` | platform default | `runc`, `crun`, `runsc` or `krun`. Unset means `runc` on Linux. These are Linux container runtimes: setting one on macOS is an error. Any other value is an error when `mode` is `local`. A project value overrides the global one only when the project sets it. Older versions saved `"isolation": "full", "runtime": "runc"` into every settings file; exactly that pair is read as unset |
| `sandbox.network` | `false` | Allow network access from sandboxed commands. Enforced for macOS shell commands; the Linux sandbox does not enforce it yet. Unix sockets remain denied on macOS. A project may turn it off but not on |
| `sandbox.allowWrite` | empty | Extra paths macOS sandboxed commands and file tools may write, such as `~/.cargo`. Secret paths remain denied under broad grants. The Linux sandbox does not enforce this setting yet (global only) |
| `sandbox.denyHomeReads` | `false` | macOS: deny reading files under the home directory, except the project, `allowWrite`, `allowRead` and git/Rust toolchain settings. Credential files stay denied either way. A project may turn it on but not off |
| `sandbox.allowRead` | empty | macOS, with `denyHomeReads`: paths under the home directory sandboxed commands and file tools may still read, such as `~/.npm` (global only) |
| `compaction` | enabled | `enabled`, `reserveTokens` (`16384`), `keepRecentTokens` (`20000`). See [Context compaction](#context-compaction) |
| `skills`, `disabledSkills` | empty | Extra skill paths, and skills to hide |
| `mcpServers`, `disabledMcpServers` | empty | MCP servers (stdio `command` or remote `url`) |
| `providers` | empty | Custom providers added with `/login custom` |
| `runtime.maxSessions`, `runtime.maxActiveTurns` | `16`, `8` | Limits of `loop runtime start`; see [the Runtime](runtime.md#start-the-runtime). Each must be a whole number of at least 1. A command-line flag overrides it (global only) |
| `runtime.idleShutdownMinutes` | `10` | Minutes an auto-started Runtime waits with nothing attached or running before it exits. `0` disables it. An owner from `loop runtime start` never idles out (global only) |
| `runtime.forwardEnv` | empty | Extra environment variable names a Surface forwards to a Runtime over WebSocket, on top of the built-in allowlist. Unix sockets forward the full environment. Values are never logged (global only) |

## Context compaction

Compaction replaces the older part of a Session's context with a summary that
the Session's model writes. The model call counts in the Session's usage
(`/session` shows it under `Tools/summaries`). `/compact` runs it on request.
With `compaction.enabled` (the default), Loop also compacts automatically
during a turn:

- **Before a model request**, when the context plus `reserveTokens` reaches
  the model's context window. Loop checks this before each request, also
  between the tool rounds of one long turn. It uses the token usage of the
  last response and estimates only the messages after it; without usage
  numbers, it estimates the whole context.
- **After a context-length error**, Loop compacts once and sends the request
  once more. If that compaction fails, or the request is still too long, the
  turn ends with an error that says so. Loop does not try again.

At the threshold, compaction summarizes earlier turns and keeps about
`keepRecentTokens` of the newest messages (at most half the room the window
leaves after `reserveTokens`, the system prompt and the tool definitions).
It never cuts into the current turn: the user's request stays as it was
written, images included. When the current turn alone is too large, the
outputs of its older tool rounds are shortened instead, each marked
`[output shortened by automatic compaction …]`; the newest round stays
whole. Before it asks the model for a summary, Loop checks that the result
will fit; when it cannot, it says so in a notice and does not compact again
in that turn. A turn compacts at most 8 times. When the window is not larger
than `reserveTokens` and the system prompt and tools, Loop does not compact
automatically and says so once per turn.

Only the recovery after a context-length error may cut into the current
turn, at the start of a tool round. The summary then quotes the request as
the user wrote it when the turn started.

A cut never falls between a tool call and its result. The Session tree
records an automatic compaction as the same entry `/compact` writes, with
the shortened outputs, so resume, fork and the transcript show the same
context. A cancelled turn records nothing. A `session_before_compact` hook
(see [hooks](hooks.md)) can block a compaction or give the summary. The
summary request uses the Session's model with no reasoning and an answer of
at most 2048 tokens.

Every Session compacts this way, also a subagent's (a subagent is a
Session). Workflow tasks of `/workflow` do not compact.

When a model's context window is unknown (0), Loop has no threshold: it
compacts only after a context-length error, once per request.

Set `"compaction": {"enabled": false}` to turn automatic compaction off; a
request that is too long then fails with the provider's error. A change in
the settings file reaches open Sessions with `workspace/reload`.

## Tool environment

Loop removes these variables from the environment of the `bash` tool and of
`!shell` commands, in standalone mode and in the Runtime:

- every built-in provider key variable: `SOKET_API_KEY`, `TENSORSTUDIO_API_KEY`,
  `LOOP_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
  `GEMINI_API_KEY` and `GOOGLE_API_KEY`;
- every name in `apiKeyEnv` of the custom providers in `models.json`;
- the default key variables of the built-in web search backends
  (`TAVILY_API_KEY`, `BRAVE_API_KEY`, `TINYFISH_API_KEY`; the Soket backend uses
  the Soket names above) and every name in `webSearch.apiKeyEnv`;
- the other secrets that Loop reads: `LOOP_PDF_OCR_API_KEY`,
  `LANGFUSE_SECRET_KEY` and `LANGFUSE_PUBLIC_KEY`;
- the Runtime token variable `LOOP_RUNTIME_TOKEN`.

The list is exact names, not a pattern. `GITHUB_TOKEN`, `NPM_TOKEN`, `AWS_*` and
all other variables stay. Name a variable in `tools.passEnv` in the global
`settings.json` to keep it:

```json
{ "tools": { "passEnv": ["OPENAI_API_KEY"] } }
```

A project `.loop/settings.json` cannot set `tools.passEnv`. Loop ignores it and
shows a notice at startup. Over a WebSocket, a name in `tools.passEnv` is also
forwarded, like a name in `runtime.forwardEnv` (see [the Runtime](runtime.md)).
The Runtime reads `tools.passEnv` from the global settings of the host that runs
the tools. With the sandbox on, the sandbox controls the environment of commands
(on macOS it keeps only an allowlist of names), and `tools.passEnv` does not
change that.

When a `bash` command fails and its output names a removed variable that was
set, Loop adds one short note to the result, for example `Note: OPENAI_API_KEY
was removed from the command environment by Loop; allow it with tools.passEnv in
global settings.` A variable gets at most one note for each turn. `!shell` shows
no note.

Scope: this applies to `bash` and `!shell` only. [Hooks](hooks.md) and MCP
servers are not changed. An MCP server gets the `env` values set in its
configuration as written. Loop reads the `apiKeyEnv` names when a Session
starts, so a change to `models.json` or `webSearch` applies to new Sessions. With the sandbox
off, this removal reduces accidental leaks. It is not a security boundary: see
[the Runtime](runtime.md#tool-environment-and-the-sandbox).

On macOS, `/sandbox local` starts Seatbelt after a preflight. Shell commands may
write the real project, private sandbox temp directory and `sandbox.allowWrite`; file tools
may write the project and `allowWrite`. Reads are allowed except for credential
paths, Keychains, and the resolved agent directory's `auth.json` and `run/`.
Project `.loop`, the entire agent directory and Git hooks, config and pointers
in every repository (submodules and nested ones included) are write-protected,
and commands cannot create or replace a `.git` entry: run `git clone` and `git
init` outside the sandbox. Git worktrees and submodule checkouts work;
a project whose `.git` is a symlink or any other outside pointer cannot start
the sandbox. OAuth/ACP credentials, databases, history, logs and common home
credential files are also unreadable. Provider keys and Runtime tokens are
removed from child environments. Child TMPDIR is a private Loop-created 0700
directory; environment overrides cannot replace it. Network
settings apply to shell subprocesses; host web/MCP services remain separate
capabilities. See [the macOS sandbox decision](design/macos-sandbox.md).

On Linux, `/sandbox local` runs shell commands in a Podman container that
mounts the project at the same path. These paths are read-only in the
container:

- project `.loop`;
- in the project's git directory, in each submodule git directory below
  `.git/modules` and in each worktree directory below `.git/worktrees`:
  `hooks`, `config`, `config.worktree`, `commondir`, `gitdir` and
  `info/attributes`, and the `modules` and `worktrees` directories
  themselves (so `git submodule add` and `git worktree add` fail in the
  sandbox);
- the directory that `core.hooksPath` names, when it is in the project
  (Loop reads it from the repository config, `~/.gitconfig` and
  `$XDG_CONFIG_HOME/git/config`; it does not follow `include` and does not
  read the system config).

Each git directory, and each directory between the project root and a
protected path, is a mount point, so a command cannot rename it away and
build a new one. When a protected path does not exist yet, Loop first
creates a placeholder on the host: an empty directory (`.git`, `.loop`,
`hooks`, `info`, `modules`, `worktrees`), an empty file (`config`,
`config.worktree`, `info/attributes`) or a `commondir` that contains `.`
(the git directory itself; an empty one would make Git fail). With that
`commondir`, `git rev-parse --git-common-dir` prints an absolute path while
a sandbox runs. An empty read-only `.git` means `git init` fails: run
`git init` and `git clone` outside the sandbox. The last sandbox of a
project to stop removes the placeholders that are still unchanged; after a
crash, the next start removes them. A `.git` file that points into the
project must point to a git directory on a `.git` path, which is then
protected the same way; one that points outside the project is not mounted,
so Git does not work in the container there. A symlinked protected path, and
Loop's agent directory inside the project but outside `.loop`, refuse the
start. File tools refuse the same paths, and also Git metadata and `.git`
entries in any other repository in the project, as on macOS.

Limits on Linux:

- A shell command can still make a new repository in the project
  (`git init sub`). Host Git runs that repository's hooks and config when
  you work in it, and `git status` in the project also runs them when the
  project's index records it as a gitlink (`git add sub`). After each shell
  command Loop scans the project for new `.git` entries (up to 50,000
  directory entries) and adds a warning to the command's output.
- Hook managers read files in the working tree: husky (`.husky/`),
  `pre-commit` (`.pre-commit-config.yaml`) and lefthook (`lefthook.yml`).
  A command can change those files; the hooks then run them on the host.
- `.git/rebase-merge`, `.git/rebase-apply` and `.git/sequencer` stay
  writable, so a command can change the steps that `git rebase --continue`
  or `git cherry-pick --continue` runs on the host.
- With partial isolation, file tools run on the host and refuse symlinks
  that lead out of the project, but a command in the container can still
  make a symlink that a later host program follows.

Loop supports Linux and macOS only. If sandbox preflight fails, standalone Loop leaves
the sandbox off with a warning, while a Runtime started with `loop runtime start`
refuses to admit the Session with `required workspace sandbox failed`. Runtime
Sessions require the configured sandbox to start successfully.

Environment variables:

| Variable | Purpose |
|---|---|
| `SOKET_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Provider API keys |
| `GEMINI_API_KEY`, `GOOGLE_API_KEY` | Gemini API key (the first one set wins) |
| `LOOP_CODING_AGENT_DIR` | Use a different config directory |
| `LOOP_CODING_AGENT_SESSION_DIR` | Use a different directory for `sessions.db` |
| `LOOP_DEBUG=1` | Write debug logs (same as `--debug`) |
| `LOOP_TOOL_LOCK=chat` | Start `loop runtime start` with the `chat` tool lock, as `--tool-lock chat` does: every Session can only read files in its folder and use the web tools. No setting can widen it. An empty value is an error. See [the Runtime](runtime.md#tool-lock) |
| `LOOP_TOOL_LOCK_ROOT` | The folder every Session of a locked Runtime must be in, as `--tool-lock-root` does. Required with a lock |
| `LOOP_PDF_OCR_API_KEY` | Turn on OCR for PDFs without a text layer, through a Mistral-compatible OCR API (needs the `web` feature). Unset, such a PDF reads as empty. **OCR uploads the whole PDF** to the endpoint, see below |
| `LOOP_PDF_OCR_URL`, `LOOP_PDF_OCR_MODEL` | Use another OCR endpoint or model (defaults: `https://api.mistral.ai/v1/ocr`, `mistral-ocr-latest`). The URL must be `https://` |

Document files: `read` returns a PDF, Word (`.docx`), PowerPoint (`.pptx`) or RTF file as markdown, and an Excel (`.xlsx`) or OpenDocument (`.ods`) spreadsheet as sheet tables with formulas. `write` builds `.docx`, `.pptx`, `.rtf`, `.pdf`, `.xlsx` and `.ods` files from the same text; the tool description lists the format. `edit` refuses a document: read it, then write the whole file again.

Documents are parsed in a separate process, `loop __doc-to-text <format>`, never in the Loop process or the Runtime, so a damaged or hostile file cannot stop your Sessions. In a sandboxed Session the file's bytes are read through the sandbox and parsed by this child on the host. The child gets an empty environment (no API keys), no core dumps, a CPU limit and a file-size limit of 0 (it cannot write data into files; that limit does not stop it from creating, renaming or deleting files). Its code makes no network calls and has no keys, but the operating system does not block network access for it. It is stopped after 30 seconds, above 2 GB of memory, or when the tool call is cancelled; the tool call then fails with "could not read this document safely". At most 3 readers run at once. On Linux the memory limit is `RLIMIT_AS`. macOS does not enforce that limit, so Loop polls the child's memory and stops it above the limit (or when it cannot read the child's memory); a very fast allocation can briefly pass the limit before the poll sees it. Further limits: at most 50 MB per document, 256 MB unpacked from one Office or OpenDocument file, 5 000 000 cells per sheet, 5 000 slides, and 50 KB of returned text (the rest is cut off, and `read` says so).

The reader runs on the host, so it must be the program Loop started from. On Linux it is the running image (`/proc/self/exe`). On macOS Loop records the executable's identity at start, refuses to start the reader if the file changed (restart Loop after an update), and refuses an executable inside the Session's workspace or the temp directory, where a Session could replace it: run Loop from a copy installed outside the workspace. A replacement between that check and the start is still possible there.

Writing over an existing document rebuilds it from the text: formatting, images, comments and anything else the text does not show are not kept. The review diff and the tool result say this, and rejecting the change in review restores the original file. A document with more text than `read` returns cannot be overwritten, and neither can a non-empty document the reader cannot read (too large, damaged, or the reader failed): write the new version to another path.

OCR: with `LOOP_PDF_OCR_API_KEY` set, a PDF whose text layer is missing is sent, whole and base64-encoded, from the Loop process (the Runtime host) to the OCR endpoint. This upload does not go through the Session's sandbox, its network settings, web permissions or permission rules. Loop only sends it over `https`, only for PDFs of at most 20 MB that are not encrypted, only after the document reader found no usable text, and never when the reader crashed or timed out; the response is limited to 32 MB. Do not set the key where documents must not leave the machine.
