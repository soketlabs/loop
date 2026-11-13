# The Runtime: `loop runtime` and `loop session`

The Runtime is one long-running `loop` process that owns your Sessions. Your
terminal is then only a Surface. You can close it, open another, or watch the
same Session from two terminals. Work keeps running in the Runtime, which keeps
an Event Log so any Surface can catch up later.

Loop supports **Linux and macOS** only (on Windows, use WSL). The Runtime
listens on a Unix socket.

## Plain `loop` uses the Runtime

Interactive `loop` connects to the Runtime for the current agent directory and
opens a new Session in the caller's working directory. If none is reachable,
it starts a detached owner and waits for its socket. A second terminal reuses
that owner. Discovery currently checks one agent directory; when discovery
returns several reachable owners, Loop asks you to select one. With piped
input it chooses the first owner. `--continue` starts a new Session with a
notice when this folder has no history.

```bash
loop                                # new Session, auto-start if needed
loop --continue                     # attach to this folder's latest Session
loop -c                             # skip AGENTS.md / CLAUDE.md context files
loop --resume                       # choose an existing Session
loop --resume SESSION_ID             # attach by id or unique prefix
loop --connect /path/to/runtime.sock # choose a particular owner
loop --connect ws://127.0.0.1:8765 --token-file /private/runtime.token
```

WebSockets require `LOOP_RUNTIME_TOKEN` or a private `--token-file`. For another
host, tunnel its loopback listener over SSH (see "Remote use and WebSocket
clients"). WebSocket clients have no host-only credential or telemetry access.

Each new Session receives the caller's cwd and environment for tools and
`!shell`. Provider credentials resolve in the Runtime. A notice warns when an
exported provider API key differs; it never shows the key. Runtime bearer tokens
and built-in provider key variables are removed from tool environments. This
holds for Runtime Sessions and for standalone `loop`, and it also removes the
`apiKeyEnv` names of custom providers. `tools.passEnv` in the global settings
keeps named variables. See [Tool environment](configuration.md#tool-environment).
A Session opened without a caller environment (`loop session new`) uses the
Runtime's own environment, with the same names removed.

### Tool environment and the sandbox

With the sandbox off, removing variables reduces accidental leaks of keys into
tool output and logs. It is not a security boundary. A command can still read
`.env` files, shell rc files and, on Linux, `/proc/<pid>/environ` of processes of
the same user. The sandbox is the real protection: with it on, commands get a
minimal environment and cannot read credential files.

Resuming keeps the Session's original cwd and tool environment, so attaching a
second Surface cannot change tools that are already running. The TUI says so on
resume. Environments are held in memory only; after a Runtime restart, an old
Session uses the Runtime's environment. Credential-difference notices compare
the attaching shell with the selected provider's actual Runtime credentials.

Unix sockets can forward the full environment. WebSockets forward only `PATH`,
`HOME`, `USER`, `SHELL`, `LANG`, `LC_*`, `TERM`, `TMPDIR`, `VIRTUAL_ENV` and
`CONDA_PREFIX`. Add explicit variable names in the Surface's global settings:

```json
{ "runtime": { "forwardEnv": ["TASK_VALUE"] } }
```

Names in `forwardEnv` are an explicit choice to share those values with the
remote owner. Environment maps are redacted from diagnostics and never saved in
the Event Log or settings.

Project trust is prompted locally only for the default owner. An explicit
`--connect` target uses the Runtime host's recorded trust; its startup notice
reports an untrusted workspace, and `/trust` uses that host's service.

Quitting detaches the TUI. It keeps the Session, running work and parked
approvals alive. Another Surface can attach and answer a parked request.

An auto-started owner stops after ten idle minutes. Set the global
`runtime.idleShutdownMinutes` to change this; `0` disables it. Attached Surfaces,
active turns, parked approvals and running Subagents prevent idle shutdown.
An owner started explicitly with `loop runtime start` never idles out.

Loop checks the owner's application version before creating or attaching a
Session. When versions differ, it replaces only an empty local owner known to
have been auto-started from this agent directory. Explicit owners and owners
with loaded Sessions stay running; Loop reports both versions, suggests
`loop runtime restart`, and uses the in-process path for this run. A startup,
connection or Session-open failure also prints one warning and falls back.
Subscription/ACP agents and ordinary `--print` runs use the in-process path. If an owner dies while the TUI is open,
a recovery banner suggests `loop --continue`; quitting still succeeds.

## Start the Runtime

```bash
loop runtime start
```

`loop runtime start` never reads stdin, never asks questions and never starts
the TUI. It runs in the foreground until you press Ctrl+C, send SIGTERM or run
`loop runtime stop`. To keep it
running after you close the terminal, use your usual tool, such as `nohup`,
`tmux` or a systemd user unit.

| Flag | Default | Meaning |
|---|---|---|
| `--socket PATH` | `~/.loop/agent/run/runtime.sock` | Where Surfaces connect |
| `--listen IP:PORT` | off | Also serve authenticated WebSockets. Loopback addresses only. Other binds are refused. For remote access, use an SSH tunnel |
| `--max-sessions N` | `runtime.maxSessions`, else 16 | Most Sessions loaded at once. At the limit, the least recently active idle Session is unloaded to make room |
| `--max-active-turns N` | `runtime.maxActiveTurns`, else 8 | Most turns running at once. More turns wait in a queue; they are not refused |
| `--permission-mode MODE` | from settings | `read-only`, `ask`, `accept-edits`, `full-access` or `plan` |
| `--cwd DIR` | current folder | Folder for Sessions that name none |
| `--tool-lock LOCK` | `LOOP_TOOL_LOCK`, else off | `chat`: lock the tools of every Session for the life of the Runtime. See [Tool lock](#tool-lock) |
| `--tool-lock-root DIR` | `LOOP_TOOL_LOCK_ROOT` | Required with a lock: every Session folder must be in it |
| `--edge-listen IP:PORT` | off | Serve a trusted Edge Gateway that names each connection's user. Loopback only. Needs `--edge-token-file`. See [Identity](#identity-who-acts-and-for-which-tenant) |
| `--edge-token-file FILE` | off | With `--edge-listen`: a private (0600) file with the gateway's bearer token. It must differ from the Runtime token |

The two limits can also be set in the global `~/.loop/agent/settings.json`
(a project's settings cannot change them, since one Runtime serves many
folders). A flag wins over the setting, which wins over the default:

```json
{ "runtime": { "maxSessions": 32, "maxActiveTurns": 12 } }
```

Each must be a whole number of at least 1. Anything else, including `0`,
a negative number or `null`, stops `loop runtime start` with an error that
names the key, for example
``settings.json: `runtime.maxSessions` must be a whole number of at least 1, not 0``.
`loop runtime status` shows each limit with where it came from: `flag`,
`settings` or `default` (`max_sessions_from` and `max_active_turns_from`
with `--json`).

Only one Runtime can use an agent directory, or a Session database, at a time.
A second `loop runtime start` exits with `a Runtime is already running (pid N)`, or
with `another Runtime already owns the Session database at …` when only the
Session database is shared.

### Tool lock

```bash
loop runtime start --tool-lock chat --tool-lock-root /srv/users/alice
```

The tool lock is for a host that runs a Runtime for a user who may only
chat: the Runtime must not run commands, change files or read past one
folder. `LOOP_TOOL_LOCK=chat` and `LOOP_TOOL_LOCK_ROOT=DIR` in the
Runtime's environment do the same; a flag wins over its variable. These
stop `loop runtime start` with an error, so a mistake never starts a
Runtime without the lock it asked for:

- an unknown or empty lock name (``unknown tool lock `…` ``, ``… is empty``);
- a lock without a root, or a root without a lock;
- a root that is not an existing folder, or is `/`.

The lock is fixed until the Runtime stops. With the `chat` lock, every
Session of the Runtime:

- **opens only inside the root.** Its folder, with symlinks resolved, must
  be the root or a folder in it. `session/new`, `session/load` (also of a
  Session recorded before the lock), a fork, an import and a Subagent with
  a folder outside are refused with `ToolLocked`.
- **keeps the folder it opened in.** The folder and the root are stored as
  they resolved when the Session opened, and every check compares against
  them. If the folder or the root is later replaced by a link, every call
  of the Session is refused ("moved or replaced"); open a new Session.
- **is offered only `read`, `grep`, `glob`, `ls`** (documents included,
  through `read`), `web_fetch` and `web_search`. Every other tool is not
  sent to the model: `bash`, `write`, `edit`, `subagent`, `todo`,
  `exit_plan`, the workflow tools and every MCP tool. A tool that a later
  version adds is off too, unless the lock names it. A call to a removed
  tool that arrives anyway is refused.
- **reads only inside its own folder** (its cwd, not the git root above
  it). A path that leads out through `..` or a symlink is refused, and
  `read` must name a path. `grep`, `glob` and `ls` skip every entry that
  leads out of the folder, so a walk never enters a linked folder outside.
  A file with more than one hard link is refused and skipped, because a
  hard link to a file outside the folder looks like a file inside it.
- **treats its folder as untrusted**, whatever the trust store says. So
  nothing from the folder loads: no project `.loop/settings.json` (and so
  no project `skills`, `prompts`, `extensions` or `themes` paths), no
  context files (`AGENTS.md` and the like, in the folder or above it), no
  project skills or prompt templates, and no system prompt files. The
  global settings and resources in the agent directory, which the host
  controls, still load.
- **starts no process for the folder:** no MCP server (stdio or remote), no
  hooks, no Rhai extensions and no sandbox (`sandbox.mode` is forced off).
  A skill from the agent directory whose folder holds anything but text
  files (`.md`, `.markdown`, `.txt`) is off. The one process the Runtime
  still starts is its own document reader, for `read`.
- **can't fetch or search private addresses,** even if
  `allowPrivateAddresses` is set.
- **refuses `session/shell` and `session/workflow`** with the typed error
  `ToolLocked` (JSON-RPC code `-32007`, data
  `{"what": "session/shell", "lock": "chat"}`).

Admin calls under the lock. Over the wire, only the methods in this table
are served. Any other admin method, also one a later version adds, is
refused with `ToolLocked` before it runs.

| Call | Decision |
|---|---|
| `workspace/settings` | Served: it only reads |
| `workspace/templates` | Served: it only lists and renders (the folder's own templates do not load) |
| `workspace/reload` | Served: a reload loads the folder under the same rules |
| `workspace/mcp` | List and reload served; enable and disable refused |
| `workspace/skills` | List served; enable and disable refused |
| `workspace/web_search` | Read served; a change refused |
| `workspace/trust` | Read served (always "not trusted"); a change refused |
| `session/sandbox` | Read served (always off); a change refused |
| `session/permission_mode` | Served: no mode widens the lock, and the result names the lock |
| `runtime/sessions` | Served |
| `runtime/defaults` | Refused (it writes the global settings) |
| `runtime/providers`, `runtime/mcp_auth` | Refused (they write credentials) |
| `runtime/tracing` | Refused (a host-only setting) |
| `runtime/usage` | Refused (it reads the host's usage log) |

Every workspace setting change goes through one write step, which the
Runtime refuses under the lock, so a later setter is refused too.

Nothing widens the lock: not the global or a project's `settings.json`
(`permissionMode`, `toolPermissions` allow rules, `mcpServers`, hooks,
`tools.passEnv`), not Session options, not `session/permission_mode` (also
`full-access` and `plan`), not the `workspace/*` setters or
`workspace/reload`, and not a skill.

The lock is the top Policy layer (see [permissions](permissions.md)): it
turns off the shell, file writes and other tools, and offers only the tools
it names. Clients see it in `initialize`: `capabilities.tool_lock` is
`"chat"`, and `local_shell`, `workflow` and the host-only flags are false,
so a client can hide those controls. `runtime/status` has `"tool_lock":
"chat"`, and `loop runtime status` shows `tool lock: chat`.

**One Runtime and one root per user.** The root bounds the Runtime, not the
users in it. A Session may open in the root itself, so a root shared by
several users gives each of them the other users' files. Give each user a
Runtime of their own, with their own folder as its root, and do not let one
user's client reach another user's Runtime socket.

**For the program that writes uploads into the folder.** The lock's
guarantees assume that this writer:

- writes each upload as a regular file, atomically: write a temporary file
  in the folder, then rename it into place;
- never creates a hard link or a symlink in the folder;
- never replaces a folder (the Session's folder or one in it) with a link.

Under the lock a file with more than one hard link is refused and hidden,
and a link that leads out is refused, so a writer that breaks these rules
makes files unreadable rather than leaking them, as far as Loop can see.

Limits:

- The lock does not isolate the process. The Runtime runs as one
  operating-system user; use one container per user for that.
- File contents are checked on the file that was opened (its real path, a
  regular file, one link), so a path changed into a link between the
  check and the open is refused. This needs macOS or Linux; elsewhere a
  locked read always fails. Folder listings (`ls`, the names that `grep`
  and `glob` walk) are checked by path only: a folder swapped during one
  call can show the names, not the contents, of what it links to.
- Any client of the Runtime's socket can use every Session in it. The lock
  limits what Sessions can do, not who can use them.

### Check and stop it

```bash
loop runtime status          # pid, socket, generation, loaded and busy Sessions
loop runtime status --json   # the same as one JSON object, for scripts
loop runtime stop            # ask the Runtime to shut down, then wait for it
loop runtime restart         # stop, then start a detached explicit owner
```

Both talk to the Runtime through its socket, and both take `--socket PATH`.
They never read the pid file or signal a process: a pid file can be stale and
its pid may now belong to another program.

- `status` exits 0 when a Runtime answers and 1 when none does. With `--json`
  it prints nothing in that case.
- `stop` runs the same shutdown as Ctrl+C: every Session is closed and its
  lease released, then the socket is removed. It waits up to 10 seconds for
  the socket to go and fails if it is still there. With no Runtime answering
  it prints `No Runtime running at …` and exits 0.

### Remote use and WebSocket clients

The Runtime does not serve TLS (`wss://`) and does not manage certificates.
Loop decided in Phase 0 not to add them. Today, remote access means an SSH tunnel
to the Runtime's loopback listener. SSH encrypts and authenticates the link, so
the Runtime opens no port to the network.

1. On the host, start the Runtime on loopback:

   ```bash
   loop runtime start --listen 127.0.0.1:PORT
   ```

2. On the client, open the tunnel and leave it running:

   ```bash
   ssh -N -L PORT:127.0.0.1:PORT user@host
   ```

3. Get the token to the client. Each start makes a new token, so copy it again
   after a restart. Copy the file into a private (0700) directory, for example:

   ```bash
   mkdir -m 700 -p ~/.loop-remote
   scp user@host:.loop/agent/run/runtime.token ~/.loop-remote/runtime.token
   ```

   Or set `LOOP_RUNTIME_TOKEN` to its contents instead of using a file.

4. Connect through the tunnel:

   ```bash
   loop --connect ws://127.0.0.1:PORT/ --token-file ~/.loop-remote/runtime.token
   loop session attach SESSION_ID --url ws://127.0.0.1:PORT/ --token-file ~/.loop-remote/runtime.token
   ```

Traffic that arrives through the tunnel is still remote. The Runtime cannot tell
it from any other WebSocket client, so host-only methods are refused (see
below). Use the host's Unix socket for those.

URL or QR remote control is planned for Phase 2 as a relay. The Runtime will open
an authenticated outbound connection to the Soket edge gateway, and a phone or
browser will pair with a one-time code in a QR. It needs no inbound port and no
certificates on your machines. Until then, use the SSH tunnel. Later, Loop may
add `--tls-cert` and `--tls-key` for self-hosters if users ask.

Local examples on the host itself:

```bash
loop runtime start --listen 127.0.0.1:8080
loop session list --url ws://127.0.0.1:8080/ --token-file ~/.loop/agent/run/runtime.token
loop session attach SESSION_ID --url ws://127.0.0.1:8080/ --token-file ~/.loop/agent/run/runtime.token
```

Each start with `--listen` generates a fresh bearer token in the agent directory's
`run/runtime.token`. The directory is private (0700) and the file is private
(0600); symlinks, foreign ownership and shared files are refused. A copied
`--token-file` also needs a private, user-owned directory. The file is removed on
clean shutdown. `loop runtime status` shows the WebSocket URL, never the token.
Clients can use `LOOP_RUNTIME_TOKEN` instead of `--token-file`; an explicit file
takes precedence. Tokens are required even on loopback.

Only loopback binds are supported, and non-loopback binds are refused. For another
host, use an SSH tunnel to its loopback listener and keep any copied token in a
private directory. Authenticated WebSocket clients have remote authority: host-only
methods, including Runtime stop, provider and MCP credentials, tracing, saved
defaults and usage history, and workspace trust writes are refused. The host's protected Unix socket still
serves those operations. A slow WebSocket client is disconnected when its queues
fill and can attach again to replay missed events.

### Identity: who acts and for which tenant

Each connection has an identity: a **principal** (who acts) and a **tenant**
(the boundary the data belongs to). See ADR-0009. The Runtime sets the
identity when it accepts the connection. It never reads it from a request,
and it does not change it while the connection is open.

For local use, nothing changes. You do not configure anything:

| Connection | `principal` | `tenant` | `via` |
|---|---|---|---|
| The Unix socket (host authority) | `{person, local, uid:<your uid>}` | `local` | none |
| A loopback WebSocket with the Runtime token | the same local person | `local` | `token` |
| An Edge Gateway connection (`--edge-listen`) | the person or service that the gateway names | the tenant that the gateway names | `edge` |

The `initialize` result shows the connection's `principal`, `tenant` and
`via`. Clients that do not know these fields ignore them. Identity fields in a
request (for example in `initialize`) are ignored. In this release the
identity changes no permission: a WebSocket client still has remote authority,
and an Edge Gateway connection has remote authority too.

**Edge Gateway connections.** This option is for a gateway that signs users in
(for example with OIDC) and then connects to the Runtime for each user. It is
off by default. To turn it on, give both flags at start:

```sh
loop runtime start --edge-listen 127.0.0.1:8790 --edge-token-file /private/edge.token
```

- The edge listener is separate from `--listen`. The Runtime token does not
  open an edge connection, and the edge token does not open an ordinary one.
- The token file must be a 0600 file that you own, in a 0700 directory. The
  Runtime refuses to start if the edge token is the same as the Runtime token.
- The gateway names the user once, in the WebSocket handshake, with these
  headers. Each header must be present exactly once:

  | Header | Value |
  |---|---|
  | `Authorization` | `Bearer <edge token>` |
  | `Loop-Principal-Kind` | `person` or `service` |
  | `Loop-Principal-Issuer` | the identity provider, such as `https://accounts.google.com`. Not `local` |
  | `Loop-Principal-Id` | the stable id from that provider, such as the OIDC `sub` |
  | `Loop-Tenant` | 1 to 64 characters of `a-z`, `0-9`, `.`, `_` and `-`, starting with a letter or digit. Not `local` or `system` |

  The issuer and id are 1 to 255 visible ASCII characters. A wrong token gets
  HTTP 401. A missing, repeated, malformed, too long or reserved value gets
  HTTP 400. The Runtime never falls back to the local identity.
- An ordinary WebSocket client that sends any `Loop-Principal-*` or
  `Loop-Tenant` header is refused with HTTP 400.
- Anyone who has the edge token can act as any user in any tenant. Keep the
  token as private as a root password, and run the gateway on the same host.
  The Runtime does not check the gateway's claims; it trusts the gateway.

### Permissions and trust

- **`full-access`** applies only when you pass `--permission-mode full-access`
  to `loop runtime start`. If your settings ask for it, the Runtime uses `ask` and
  shows a notice.
- **Deny rules, hooks and Policy limits** still apply in every mode.
- **Untrusted folders.** The Runtime never shows a trust prompt. In a folder you
  have not trusted, it uses only your global settings and shows a notice. Trust
  the folder first by opening it once with `loop`.

## Attach a Surface

```bash
loop session list               # loaded Sessions and this folder's: id, folder, title, busy, last activity, parent
loop session list --all         # every Session: all folders, closed Subagents too
loop session list --json        # the same Sessions as JSON, for scripts
loop session attach 3f2a        # attach to a Session by id or unique id prefix
loop session new                # create a Session in the current folder and attach
loop session new --cwd DIR      # create a Session in DIR and attach
loop session close 3f2a         # unload a Session from the Runtime; its history is kept
```

When you attach, the Surface replays the Session's history and then follows it
live. Prompts show as `› <text>`, whichever Surface sent them; the Surface you
typed a prompt in does not print it a second time. A Session that is not loaded, for example after the Runtime restarted, is
loaded when you attach. Child Sessions started by a Subagent are named after
their task and show their parent in the list. Once the parent has its answer,
a finished child is closed: it frees its slot and is listed only with
`--all`. So are children that a cancel or a Runtime restart ended; the list
says how many it left out. It is never loaded again: attaching replays its output, but you
cannot prompt it.

By default the list shows the Sessions loaded in the Runtime and the Sessions
of the current folder (or `--cwd`), newest first, at most 20, then a line
`N more; loop session list --all`. Closed Subagent Sessions are left out.
`--all` lists every Session of every folder, closed Subagents included.

The table is for people: it fits the terminal width. Folders show `~` for your
home folder and are cut in the middle, titles are cut short, and PARENT shows
the first 8 characters of the parent id. Scripts should use
`loop session list --json` (with `--all` for everything). It prints the same
Sessions in the same order, as a JSON array with one `SessionInfo` each, with
full ids and fields `live`, `busy`, `parent`, `created_ms` and
`last_active_ms`.

`--socket PATH` connects to a Runtime that was started with a custom socket.

### Keys

| Input | Effect |
|---|---|
| A line of text | Starts a turn with it as the prompt. While a turn runs it is queued (`queued; sends when the current turn ends`) and sent, in order, when the turn ends. Detaching drops prompts still queued |
| A number, while an approval is shown | Answers the approval with that choice. Each approval is numbered from 1; a line typed before it appeared is ignored (`Choice no longer active`). An approval always comes before queued prompts |
| `> ` then text | Always a prompt, even when it starts with a number |
| Ctrl+C | Cancels the current turn and drops prompts still queued, saying how many |
| Ctrl+C again, or Ctrl+D | Detaches |

Detaching, or losing the connection, never stops the agent. Attach again to
continue watching.

## Close and unload Sessions

`loop session close <id>` unloads a Session from the Runtime and releases its
lease. Nothing is deleted: `loop session attach <id>` loads it again with its
full history.

- A busy Session is refused, and so is a parent whose Subagents are running.
  `--force` cancels the turn, and its Subagents, then closes.
- A Session that is not loaded is reported, and the command succeeds.

The Runtime also unloads Sessions by itself. When `--max-sessions` Sessions
are loaded and another one is opened, it unloads the least recently active
Session that nothing needs, and logs it as `unloaded`. A Session is kept
while any of these is true:

- a turn is running, or it is closing;
- an approval is waiting;
- a Surface is attached to it, including one that only watches and never answers approvals;
- it is a parent waiting on Subagents, or it is itself a running Subagent.

Prompting or attaching to a Session that was just unloaded loads it again;
if there is no room, the prompt or attach fails with the message below.
If every loaded Session must be kept, opening another fails with, for example:

```text
The Runtime is full: all 16 loaded Sessions are busy or watched (limit 16). Close one with `loop session close <id>`, or restart the Runtime with a higher --max-sessions.
```

## Approvals

An approval is an Escalation: a tool that needs your decision before it runs.

- Every attached Surface sees the request as numbered choices.
- A Subagent's request also goes to every Surface attached to its parent, or
  to any Session above it, so watching the parent is enough. `loop session attach`
  labels it with the child, for example
  `Escalation from Subagent 1a2b3c4d (check the tests): bash cargo test`.
- The first valid answer wins. The other Surfaces show
  `Another client answered; Escalation withdrawn.`
- If no Surface is attached, the request waits, with no timeout. The next
  Surface to attach sees it.
- Detaching or quitting while an approval is shown never answers it: it stays
  pending for the other Surfaces, or waits for the next one.
- **File edits are applied before you answer.** In `ask` mode a file edit
  (`write`, `edit`) is written first and reverted if you reject it. While its
  approval is parked, the change is already on disk; rejecting it, or
  cancelling the turn, puts the file back. If the Runtime is killed while
  that approval waits, the change stays on disk.
- Cancelling the turn withdraws its pending approvals.
- An "allow for this Session" grant lasts only while the Runtime runs. It is
  never restored after a restart.

## Sessions held by the Runtime

While the Runtime has a Session loaded, it holds that Session's lease, and no
other process can write to it. The Runtime renews the lease every 5 seconds.
Opening the same Session in the terminal app is refused:

```text
Session <id> is in use by Runtime <pid>. Use `loop session attach <id>` instead.
```

Closing a Session, or stopping the Runtime cleanly, releases its lease. If the
Runtime was killed, there are two ways to get the Session back:

- **Restart `loop runtime start`.** It reclaims its old leases straight away.
- **Use `loop --resume <id>`.** This works once the lease is at least 30
  seconds old and the old Runtime process is gone.

A terminal `loop` releases its lease whenever it ends: when you quit, after
`--print` (whatever its exit code), on an error, and on SIGTERM, SIGHUP or
Ctrl+C. Only a hard kill (SIGKILL, a crash, power loss) leaves a lease
behind. The next terminal `loop`, or `loop runtime start`, removes such a
row once it is at least 30 seconds old and its process is gone. A row whose
process may still be running is never removed.

The same rule works the other way round. If a terminal `loop` crashed while
holding a Session, the Runtime loads that Session once the lease is at least
30 seconds old and that process is gone. A lease whose owner is still running
is never taken over.

## After a crash

When `loop runtime start` starts, it checks what the previous Runtime left unfinished,
before it accepts any Surface:

- **Interrupted turns** get a final turn-end marked `runtime_restarted`.
  `loop session attach` shows it as
  `Turn ended uncertain — Runtime restarted`.
  Loop does not know whether that turn's tools ran, so it never retries the
  prompt. Check the work, then send a new prompt.
- **Pending approvals** from the old Runtime are withdrawn. An old approval
  is never shown again or applied to a new turn.
- **Leases** of the old Runtime are reclaimed, and stale leases of terminal
  `loop` processes that exited are removed.

`loop runtime start` prints what it recovered, for example
`recovered 1 interrupted turn(s), withdrew 1 Escalation(s), reclaimed 1 lease(s), removed 0 stale lease(s) of exited processes`.
It prints nothing when there was nothing to recover. Starting it again
recovers nothing more.

- **What startup reads.** Recovery reads only turn starts and ends and
  approval records, and only since the last Runtime run whose recovery
  finished. It never loads streamed text. A Runtime that ran for a long time
  without a restart still means more records to check on the next start.
- **Leases that cannot be reclaimed.** If one Session's lease cannot be
  reclaimed, `loop runtime start` prints `Runtime recovery: lease of Session <id>: …`
  to stderr and carries on. Attaching to that Session reports the conflict.
- **What the model sees.** The Session history may not contain the
  interrupted turn at all. Where it does contain a tool call without a result,
  the model is told the call was interrupted and may or may not have run.

## Where things live

Paths are under the agent directory, `~/.loop/agent/`, or `LOOP_CODING_AGENT_DIR`
when that is set.

| Path | What |
|---|---|
| `run/runtime.sock` | Default socket (mode 0600, folder mode 0700). If the path is too long for a Unix socket, `/tmp/loop-<uid>/runtime.sock` is used |
| `run/runtime.lock` | File lock held while the Runtime runs. Holding it proves no other Runtime is alive |
| `run/runtime.pid` | The Runtime's pid, for diagnostics only |
| `logs/runtime.log` | Runtime log |
| `events.db` | The Event Log: every update, prompt, approval and Session start and stop, used for replay and audit |
| `sessions.db` | Sessions, with a `runtime.lock` beside it. `LOOP_CODING_AGENT_SESSION_DIR` moves this file, but not `events.db` |

The wire protocol that Surfaces speak is described in
[`design/protocol-wire.md`](design/protocol-wire.md#11-runtime-methods-s2); the
methods added for the TUI are in
[§12](design/protocol-wire.md#12-session-and-workspace-methods-s3).
