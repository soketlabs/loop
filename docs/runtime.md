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
| `--unstable-edge-listen IP:PORT` | off | **Unstable, not for real users yet.** Serve a trusted Edge Gateway that names each connection's user. Loopback only. Needs `--unstable-edge-token-file` and `--tool-lock chat`. See [Identity](#identity-who-acts-and-for-which-tenant) |
| `--unstable-edge-token-file FILE` | off | **Unstable.** With `--unstable-edge-listen`: a private (0600) file with the gateway's bearer token, at least 32 bytes, outside the agent directory. It must differ from the Runtime token |

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

**One root, one folder per user.** The root bounds the Runtime, not the
users in it. The safest setup is still one Runtime per user, with the
user's own folder as its root. Edge Gateway users may share one Runtime and
one root: the access rules keep each to its own Sessions, and the folder
rule (see "Who can reach a Session") refuses a Session in the folder of
another user's Session, or in a folder above or inside it. The Runtime
knows only the folders of Sessions, not whose folder is whose, so the
gateway must name each user's own folder. Never let a user's client reach the Runtime's socket.

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
- Any client of the Runtime's socket is the host's user and can use every
  Session in it. The lock limits what Sessions can do; the access rules
  (see "Who can reach a Session") limit which Sessions an outside user can
  use.

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
| The Unix socket (host authority) | `{person, local, uid:<your uid>}` (`os-user` in place of `uid:<n>` on a system without user ids) | `local` | none |
| A loopback WebSocket with the Runtime token | the same local person | `local` | `token` |
| An Edge Gateway connection (`--unstable-edge-listen`) | the person or service that the gateway names | the tenant that the gateway names | `edge` |

The `initialize` result shows the connection's `principal`, `tenant` and
`via`. Clients that do not know these fields ignore them. Identity fields in a
request (for example in `initialize`) are ignored. The identity of the host's
user changes no permission: a WebSocket client still has remote authority. An
Edge Gateway user has remote authority and the limits below.

A principal id comes from an outside identity provider. Text such as
`../x` or `a,b` is a valid id. Code must never use an id as a file or folder
name; use a hash of it. A tenant is safe as a file or folder name.

**Who acted, in the Event Log.** Every row of `events.db` names who caused
it, in the columns `tenant`, `principal_kind`, `principal_issuer`,
`principal_id`, `via` and `actor`:

- A Session's tenant is the tenant of the caller that created it. It does
  not change. Every row of the Session is in that tenant, also when a caller
  of another tenant uses the Session.
- The principal and `via` are of the caller that caused the row: the caller
  that sent the prompt (for the whole turn), the config change, the command,
  the close, or the Escalation answer. A Subagent acts for the caller of its
  parent's turn, and its rows have `actor` set to the parent Session's key.
- Rows the Runtime makes on its own (start and stop, recovery after a crash,
  a lost lease, an unload) have the Runtime's own principal
  `{service, local, runtime}`. On one host their tenant is `local`.
- The row of the opening that created a Session (`session_opened` with
  `created: true`) names its **owner of record**. The access checks use it
  (see "Who can reach a Session").
- Rows written by a Loop version before this change read as tenant `local`
  and the Runtime's principal. Their Sessions have no owner of record.
- Each read of the Event Log names a tenant. The same Session key in two
  tenants is two Sessions.
- **No downgrade once a non-local row exists.** A Loop from before this
  change writes every row as tenant `local`, also a row of a Session of
  another tenant. Such a Session then has rows in two tenants. The Runtime
  reports it at start, reads it in the tenant of its creating row, and
  refuses to load it.
- Storage note: the identity is stored in columns on each row (about 44
  bytes more per row). A lookup table with a small id per row is to be
  evaluated in step B4, if B4 rewrites rows.

**Access rules.** The owner of record and the tenant decide who may use a
Session. See "Who can reach a Session" below for the full rules and the
table of methods.

```sh
sqlite3 ~/.loop/agent/events.db \
  "SELECT global_seq, kind, tenant, principal_issuer, principal_id, via, actor
   FROM events ORDER BY global_seq DESC LIMIT 20"
```

**Edge Gateway connections (unstable).** This option is not for real users
yet. It is for a gateway that signs users in (for example with OIDC) and then
connects to the Runtime for each user. It is off by default. To turn it on,
give both edge flags and a tool lock at start:

```sh
loop runtime start --tool-lock chat --tool-lock-root /srv/loop-users \
  --unstable-edge-listen 127.0.0.1:8790 \
  --unstable-edge-token-file /private/edge.token
```

- **A tool lock is necessary.** Without a tool lock, a Session can run shell
  commands as your OS user. Those commands can open the Runtime's Unix socket
  (host authority) and read your credentials and token files. Thus:
  - `loop runtime start` refuses the edge flags without `--tool-lock`.
  - The Runtime refuses every request of an edge connection (except
    `initialize`) when it has no tool lock. The error is `Forbidden`.
  - The Runtime always refuses `session/shell` from an edge connection with
    `Forbidden`, also under a tool lock.
  - `loop runtime start` refuses a `--tool-lock-root` that contains the agent
    directory, the Session database or the edge token file. It also refuses
    a root that contains your home directory (a project folder below it is
    fine), and a root that overlaps a path the macOS sandbox keeps from
    commands: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, `~/.netrc`,
    `~/.docker/config.json`, `~/.kube/config`, shell histories, other
    agents' folders such as `~/.codex` and `~/.claude`, and the keychains.
- The edge listener is separate from `--listen`. The Runtime token does not
  open an edge connection, and the edge token does not open an ordinary one.
- The token file must be a 0600 file that you own, in a 0700 directory, with a
  token of at least 32 bytes. It must not be in the Runtime's `run/` folder or
  be the Runtime token file under another name. The Runtime refuses to start if
  the edge token is the same as the Runtime token (also the one a previous
  start left, when you do not give `--listen`).
- The gateway names the user once, in the WebSocket handshake, with these
  headers. Each header must be present exactly once (in any letter case):

  | Header | Value |
  |---|---|
  | `Authorization` | `Bearer <edge token>`. The scheme name `Bearer` is not case-sensitive (RFC 7235). Use one space before the token |
  | `Loop-Principal-Kind` | `person` or `service` |
  | `Loop-Principal-Issuer` | the identity provider, such as `https://accounts.google.com`. Not `local` |
  | `Loop-Principal-Id` | the stable id from that provider, such as the OIDC `sub` |
  | `Loop-Tenant` | 1 to 64 characters of `a-z`, `0-9`, `.`, `_` and `-`. It starts with a letter or digit and does not end with `.`. Not `local` or `system`, and not a Windows device name (`con`, `prn`, `aux`, `nul`, `com0` to `com9`, `lpt0` to `lpt9`, also before a `.`) |

  The issuer and id are 1 to 255 visible ASCII characters. Spaces around a
  value are removed. A wrong token gets HTTP 401. A missing, repeated,
  malformed, too long or reserved value gets HTTP 400. The Runtime never falls
  back to the local identity.
- An ordinary WebSocket client that sends any `Loop-Principal-*` or
  `Loop-Tenant` header is refused with HTTP 400.
- Anyone who has the edge token can act as any user in any tenant. Keep the
  token as private as a root password, and run the gateway on the same host.
  The Runtime does not check the gateway's claims; it trusts the gateway.

**Who can reach a Session.** The Runtime checks every request against the
Session it names. One function in the Runtime (`access::decide`) makes the
decision, and every entry point calls it. The rules:

- **Rights.** A request needs one of four rights on its Session:
  - `List`: the Session shows in `session/list`, `runtime/sessions` and the
    counts of `runtime/status`.
  - `Read`: its contents: attach, load, transcript, tree, views, stats,
    export, the model, effort and mode lists, and a fork or clone of it.
  - `Write`: act in it: prompt, cancel, the config setters, rename,
    navigate, compact, skills, workflows, shell commands, and answers to its
    Escalations.
  - `Close`: close it (also with `force`).
- **The host's user** (the Unix socket and the WebSocket with the Runtime
  token, `is_host_user`) has every right on every Session in the `local`
  tenant, as before. Nothing changes for a single local user.
- **The host's user on a Session of another tenant** (the operator) has
  `List` and `Close` only. It sees the Session in the lists and can close
  it, but cannot attach to it, read it, prompt it or answer its
  Escalations. ADR-0009 refuses attach by another principal unless the
  Session is shared, and the operator must not act as another user in
  silence. Such a request gets the same `SessionNotFound` as every other
  refusal (one refusal for every caller), although the host's user can see
  in the list that the Session exists.
- **A user from outside the host** (an Edge Gateway user) has every right on
  a Session only when the Session's tenant is the user's tenant AND the
  user is its owner of record (the same principal: same issuer and id).
  Two principals of one tenant do not share Sessions. Sharing is later
  work.
- **No owner of record** (a Session from before identity was stored, or
  made outside the Runtime): host-only.
- **A Subagent** has the access of its root parent Session (the Session that
  a Surface opened). Its own owner row (the caller of the parent's turn)
  grants nothing.
- **A copy** (fork, clone, import) belongs to the caller that made it, in
  that caller's tenant. A fork or clone first needs `Read` on the source.
- **No existence oracle.** A request that a caller may not make on a
  Session fails exactly as for a Session that does not exist:
  `SessionNotFound`, from the same check, before any other check (busy,
  closing, loaded or not). The lists leave such Sessions out.
- **In-process calls.** A call with no connection (the Runtime's own work:
  recovery, Subagents, workflows, unloads, and code that links the Runtime)
  acts with internal authority. A connection always has a caller: the
  server refuses every request except `initialize` from a remote connection
  that has no caller.

| Method | Target | Right | Where it is checked |
|---|---|---|---|
| `initialize` | the connection | none | server (no Session) |
| `shutdown` | the connection | none (a Runtime connection does not own the Runtime) | server |
| `session/new` | a new Session | creates it, owned by the caller in its tenant; the folder rule below | `Runtime::open_as` |
| `session/load` | a Session | `Read` | `AgentBackend::load_session` |
| `session/list` | every Session | `List` (filter) | `Runtime::list` |
| `session/attach` | a Session | `Read` | `attach_watched`, `RuntimeControl::attach` |
| `session/detach` | a Session | none: it only stops this connection's feed | server |
| `session/transcript` | a Session | `Read` | `AgentBackend::transcript` |
| `session/prompt` | a Session | `Write` | `AgentBackend::prompt` |
| `session/cancel` (notification) | a Session | `Write` | `AgentBackend::cancel` |
| `session/config` | a Session | `Read` | `RuntimeControl::config` |
| `session/models`, `session/efforts`, `session/modes` | a Session | `Read` | `ModelControl` |
| `session/set_model`, `session/set_effort`, `session/set_mode` | a Session | `Write` | `ModelControl` |
| `session/ttfb` | a Session | `Read` to read, `Write` to change | `ModelControl::ttfb` |
| `session/compact` | a Session | `Write` | `Runtime::compact` |
| `session/stats` | a Session | `Read` | `SessionStats::stats` |
| `session/shell` | a Session | `Write`; never for an outside user | server, then `Runtime::shell` |
| `session/skill` | a Session | `Write` | `RuntimeControl::skill` |
| `session/workflow` | a Session | `Write` (refused under a tool lock) | `RuntimeControl::workflow` |
| `session/tree` | a Session | `Read`; `family` lists only readable Sessions | `Runtime::tree` |
| `session/view` | a Session | `Read` | `Runtime::view` |
| `session/navigate` | a Session | `Write` | `Runtime::navigate` |
| `session/rename` | a Session | `Write` | `Runtime::rename` |
| `session/fork`, `session/clone` | the source; a new Session | `Read` on the source; the copy is the caller's | `Runtime::fork`, `Runtime::clone_session` |
| `session/export` | a Session | `Read` | `Runtime::export` |
| `session/import` | a workspace; a new Session | the workspace rule; the copy is the caller's | `Runtime::import` |
| `session/close` | a Session | `Close` | `Runtime::close` |
| `session/sandbox` | a Session | `Read` to read, `Write` to set | `RuntimeAdmin::sandbox` |
| `session/permission_mode` | a Session | `Read` to read, `Write` to set | `RuntimeAdmin::permission_mode` |
| `session/update` (to the client) | a Session | `Read`: only Sessions this connection created, loaded or attached | server forwarder |
| `host/interact`, `host/interact_withdrawn` (to the client) | a Session | `Write` | `register_watch`, `EscalationHost` |
| `workspace/settings`, `workspace/mcp`, `workspace/web_search`, `workspace/skills`, `workspace/templates`, `workspace/trust` | a workspace | the workspace rule | `resolve_workspace` |
| `workspace/reload` | a workspace | the workspace rule, and `Write` on every loaded Session in it | `RuntimeAdmin::reload` |
| `runtime/sessions` | every loaded Session | `List` (filter; the totals count only the rows shown) | `RuntimeAdmin::sessions` |
| `runtime/status` | the Runtime | for an outside user: no pid, socket, URL or limit sources; counts of its own Sessions only | server |
| `runtime/stop`, `runtime/stop_empty`, `runtime/usage`, `runtime/defaults`, `runtime/providers`, `runtime/mcp_auth`, `runtime/tracing` | the host | host-only (Unix socket) | server (`HOST_ONLY`) |

A test fails when `rpc.rs` gets a method that this table (in
`crates/loop-runtime/src/access.rs`) does not classify.

**The workspace rule.** A workspace is a folder, named by an id. The host's
user reaches every workspace the Runtime knows, as before. An outside user
reaches a workspace only when it is the folder of a loaded Session that the
user may read. Any other id gets the same `unknown workspace` error as an id
that names nothing. Under a tool lock the workspace changes are refused for
everyone; a reload by an outside user also needs `Write` on every loaded
Session in that folder.

**The folder rule (outside users).** The tool lock lets a Session read its
whole folder, subfolders included. So an outside user:

- must name the folder (`cwd`) in `session/new`, and the folder must be
  strictly inside the tool-lock root: no `cwd`, and the root itself, are
  refused with `Forbidden`. (A Session in the root would read every user's
  folder and, by the rule below, block every other user.)
- cannot open, load, fork, clone or import a Session in a folder that is
  the same as, inside, or above the folder of a Session it may not read
  (another user's, or the host's). The request gets `Forbidden`.
- gets the same rule for its Subagents: a child opens only in folders its
  root Session's owner may use. (Under the tool lock the `subagent` tool is
  off; the rule holds without it.)

**A load never moves a Session.** A Session keeps the folder it was created
in. `session/load` with a `cwd` other than that folder is refused with
`Unsupported`, for every caller (the host's user too; `loop` never sends a
`cwd` on a load). So the folder in `session/list` is always the folder the
Session runs in.

**Operator requirement and gateway rules.**

- Give each outside user a folder of their own under the tool-lock root, for
  example `/srv/loop-users/<hash of tenant and id>/`. Never use the
  principal id itself as a folder name. Loop does not create these folders.
- The gateway always sends that explicit per-user `cwd` on `session/new`,
  and never passes a `cwd` that a client chose, on `session/new` or on
  `session/load` (a load needs none).
- A user's folder is that user's once used: the folder rule counts every
  Session the Runtime's store knows, also closed ones, the host's own and
  those made before identity was stored. It compares only with Sessions of
  other owners, so a user's closed Session blocks only its own folder (and
  the folders above and inside it), never an unrelated one.
- A host Session made later in a user's folder makes that user's next load
  of its own Session fail with `Forbidden`. Keep the host's own Sessions out
  of the users' folders.
- To find which Session blocks a folder, the host's user runs
  `loop session list` (`session/list` on the Unix socket): it lists every
  Session, closed ones and other tenants' included, with its folder.
  `runtime/sessions` lists only the loaded ones.

**Copy and import request ids are per user.** A receipt of `session/fork`,
`session/clone` or `session/import` is keyed by the tenant and principal of
the caller (hashed) and the request id. One user can neither learn that
another used an id nor take it. Receipts written before this change had no
user in the key; the host's user still finds them, so a retry across an
upgrade answers from its receipt.

**Limits are shared.** `max_sessions` and `max_active_turns` count the
Sessions of all users. A user who opens Sessions up to `max_sessions`
makes the Runtime unload other users' idle Sessions (they load again on
their next use), and a full Runtime answers `Busy` to everyone. Per-user
limits are later work.

**Updates and Escalations.** The Runtime sends every update to one channel.
Each connection forwards only the updates of Sessions that it created,
loaded or attached, and each of those requests passed the check above. The
Runtime checks `Read` again when it registers the connection as a watcher,
and the connection drops the feed when that check fails. An owner and a
tenant never change, so an access granted once stays true. An Escalation is
sent to, and accepted from, only a connection whose caller may `Write` the
Session (for a Subagent: its root). This replaces the earlier rule, which
used the caller of the turn.

**What an outside user can still observe.**

- That the Runtime is full (`Busy` at `session/new`), and the Runtime's
  limits in `runtime/status`.
- That a folder is in use by another user's Session (the folder rule).
- The time of a refused request: a refusal for another user's Session of the
  same tenant reads its owner once (then from a cache); a refusal for an
  unknown Session reads nothing. The time of `session/list` grows with the
  number of Sessions of all users.
- A host's user listing another tenant's Sessions (`session/list`,
  `runtime/sessions`) sees each one's key, folder, size, owner kind, times
  and state, not its title, name, model or tokens.
- Escalation ids (`approval-<n>`) count across the whole Runtime.
- That other users' idle Sessions were unloaded or loaded (the shared
  limits above), only through timing.
- Not observable: a detach of a Session the connection does not watch
  changes nothing (it does not count as activity for the idle check).
- Being dropped for lag: the update channel is shared, so a burst of
  another user's updates can make a slow connection fall behind. The
  notice does not say how many updates were missed.
- Other users' load, through response times.

**What else an edge user can do.** Through the agent of its Session: read
the files in that Session's folder and use the web tools. The tool lock turns
off the shell, file changes, Subagents, workflows, MCP servers and hooks. The
workspace reads that the lock serves return no secrets:

- `workspace/settings`: the thinking level, the sandbox settings (mode and
  paths), the permission mode, the `toolPermissions` rules, the diff editor
  command, the response timeout, the enabled model names and resource
  counts. Never provider settings, API keys or MCP server definitions.
- `workspace/mcp` (list): each server's name, whether it is on and
  connected, and its tool count. Never its command, arguments, `env` or
  headers.
- `workspace/web_search` (read): on or off, the provider id and a status
  line. Never the key.
- `workspace/skills`: each skill's name, description and state.
- `workspace/templates`: each prompt template's name and description, and
  with `name`, the template's text. A template is text you wrote for chat
  clients: do not put a secret in one.

An edge user cannot run `session/shell`, call a host-only method, or change
the tool lock. The edge listener stays unstable (`--unstable-edge-listen`):
the transport, device pairing and per-user limits are not done. One user
can still slow down another (shared Runtime limits and update channel).

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

- An edge user's connection gets only the Escalations of the turns that user
  started ([above](#identity-who-acts-and-for-which-tenant)). The rest of this
  section is about the host user's connections.

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

## Commands you type (`session/shell`)

`!command` in the TUI (the `session/shell` method) runs a command in the
Session's tool environment while the Session is idle. Before it runs:

1. The tool lock refuses it (`ToolLocked`), and the Runtime refuses it from an
   Edge Gateway connection (`Forbidden`).
2. The Session's permission check judges it as a `bash` call: a Policy layer
   that turns the shell off, a `bash` deny rule, and `read-only` or `plan`
   mode (except read-only commands) refuse it. Where `bash` would ask, you are
   the approver, because you typed it, so it runs with no prompt. A caller
   from outside the host cannot approve, so for that caller the command is
   refused. The rules are in [permissions](permissions.md#commands-you-type).
3. The decision record is written (below). Then the `bash` block list
   applies, and the command runs.

A refusal is an error with the reason, for example
`the command is refused: Tool `bash` is denied by settings.toolPermissions`.

## Decision records

Every permission decision of a Session is a `policy_decision` row in the
Event Log: each tool call of the agent and its Subagents, each `/workflow`
shell task, each permission request of a subscription agent, and each
command you type. The fields are in [permissions](permissions.md#decision-records).

- **Order.** For a tool call with an effect, the Runtime first commits the
  call's `tool_call` update, then the decision, and only then runs the tool.
  An allowed read of files has no effect to protect: its decision commits
  before the read runs, but it does not wait for the `tool_call` update. If
  the record cannot be written, the call is refused. An approver's answer
  takes effect (a plan approval's mode change, an "allow for this Session"
  grant) only after its record commits.
- **Durability.** The Event Log commits with SQLite `synchronous=NORMAL`. A
  crash of the Runtime process keeps every committed record. A power loss
  can lose the last records while the effects of their calls stay on disk.
- **Hooks run first.** `tool_call` hooks are part of the decision, so a
  command hook runs before the record is written.
- **Audit only.** The row has no Session sequence. Attach, replay and
  `session_seq` never include it, and recovery does not read it. An older
  Loop reads such a row as an unknown record and keeps it.
- **Who.** The record names the caller that sent the prompt or the command.
  An answered approval also names who answered (`answered_by`, a
  principal); the `escalation_answered` row names the same caller, with its
  tenant and `via`.
- **Read them** with SQLite:

  ```sh
  sqlite3 ~/.loop/agent/events.db \
    "SELECT json_extract(payload,'$.event.payload') FROM events
     WHERE kind='policy_decision' ORDER BY global_seq DESC LIMIT 20"
  ```

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

## Context compaction in a Session

A turn compacts its context automatically when it is near the model's window
or when the provider refuses a request as too long (see
[Context compaction](configuration.md#context-compaction)). Clients see an
automatic compaction through the existing update kinds:

- `notice` updates (level `info`): one when compaction starts (for example
  `context near the window (~7100 of 8000 tokens): compacting…`) and one when
  it is done (`context compacted: ~7100 to ~1900 tokens`). The terminal app
  shows them in its status line.
- `config_changed` after the turn ends: the compaction entry counts in the
  Session revision, as `session/compact` does.
- A failed compaction after a context-length error ends the turn with
  `turn_ended` (`stop: error`) and an error that names the cause.

`session/compact` asks the model for the summary without holding the
Session: attach, close and settings answer meanwhile, a prompt is refused as
busy, and closing the Session cancels the compaction. If the Session's branch
changed meanwhile, nothing is written. `session/compact` then replaces the
transcript with a `branch_reset` (reason `compact`). An automatic compaction does not: the live transcript keeps the
whole turn, and the compaction appears when the transcript is read again
(`session/transcript`, attach, reload), as a `compaction summary` notice
before the kept messages. Tool outputs that an automatic compaction
shortened appear shortened in that transcript, although live clients saw
them in full.

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
| `events.db` | The Event Log: every update, prompt, approval, permission decision (`policy_decision`) and Session start and stop, used for replay and audit |
| `audit/decisions.jsonl` | Permission decisions of Sessions that run without the Runtime (`--print` alone, the TUI's fallback, `--serve-mcp`). Mode 0600 |
| `sessions.db` | Sessions, with a `runtime.lock` beside it. `LOOP_CODING_AGENT_SESSION_DIR` moves this file, but not `events.db` |

The wire protocol that Surfaces speak is described in
[`design/protocol-wire.md`](design/protocol-wire.md#11-runtime-methods-s2); the
methods added for the TUI are in
[§12](design/protocol-wire.md#12-session-and-workspace-methods-s3).
