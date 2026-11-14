# Files and terminal: protocol methods for a Session's workspace

Status: implemented on `feat/files-terminal-protocol` (tasks P1 and P2).

This note gives a client, such as the web app's agent panel, the files and a
terminal of one Session. The pieces below it exist: the Workspace contract
(`loop-workspace`), the `host` driver (processes, PTY, transfer), the access
rules (`access.rs`) and the permission gate (`tool_gate.rs`). This note
defines the wire contract and how each call passes the access rules and the
gate. Every change is additive. The protocol version stays 1.

## 1. Decisions in short

| Question | Decision |
|---|---|
| Addressing | By Session: `session/files/*` and `session/terminal/*`. The access rules are per Session, so the Session is the unit. A Workspace is shared by the Sessions of one folder, but a client never names a Workspace |
| Paths | `WorkspacePath` text: relative, `/`-separated, inside the Session's folder. A request never holds a host path. A reply never holds one |
| Binary data | Base64 (RFC 4648, with padding) in a JSON string. The wire has text frames only, so there is no second transport |
| Size of one piece | Read and upload pieces: at most 1 MiB of data (about 1.4 MB of Base64). A message can be 64 MiB, so this is far below the frame limit |
| Which Workspace serves | The Session's `host` Workspace when its sandbox is off. With the sandbox on, every file and terminal call is `Unsupported`: the sandbox drivers do not do transfer or `spawn` yet (C4a, C4b). A host terminal or host file access for a sandboxed Session would escape the sandbox |
| Permission | Each call is a user action of that Session and passes the gate: `check_user_file_action` and `check_user_terminal`, next to `check_user_command` |
| Terminal output | Live only. It never goes to the Event Log or to the update broadcast. Each connection gets its own stream, `session/terminal/output`, after an explicit `session/terminal/attach` |
| Tool lock | Reads, lists and downloads are served inside the Session's folder, by the lock's rules. Writes, uploads, deletes, renames, new folders and terminals are refused with `ToolLocked` |
| Upload exception for the hosted chat mode | Specified in section 8, not built. Uploads stay refused under the lock |

## 2. Methods

All are client-to-server requests, except the output notification. Each
takes `session` (a `SessionRef`). "Path" is a `WorkspacePath`; the empty
path is the Session's folder.

### 2.1 Files

| Method | Params | Result |
|---|---|---|
| `session/workspace` | `session` | `SessionWorkspaceInfo`: what this Session serves now (files, writes, terminal), with a reason for each refusal, and the limits |
| `session/files/list` | `session`, `path`, `cursor?`, `limit?` (default 200, at most 1000) | `entries[]` (`name`, `path`, `kind` = `file`/`dir`/`symlink`/`other`, `size`, `mtime_ms?`, `tag?`), `next_cursor?`, `hidden` (entries of the folder that a rule hides) |
| `session/files/stat` | `session`, `path` | one entry; a regular file has a `tag` |
| `session/files/read` | `session`, `path`, `offset`, `length?` (default and most 1 MiB), `if_match?` | `offset`, `data` (Base64), `total_size`, `eof`, `tag` |
| `session/files/upload_begin` | `session`, `path`, `size`, `sha256?`, `if_match?`, `overwrite?` | `upload` (an id), `chunk_max` |
| `session/files/upload_write` | `session`, `upload`, `offset`, `data` (Base64) | `committed`, `size` |
| `session/files/upload_status` | `session`, `upload` | `committed`, `size` (for resume) |
| `session/files/upload_commit` | `session`, `upload` | `path`, `size`, `tag` |
| `session/files/upload_abort` | `session`, `upload` | `{}` |
| `session/files/delete` | `session`, `path`, `recursive?` | `{}` |
| `session/files/rename` | `session`, `from`, `to` | `{}`. It never replaces an existing `to` |
| `session/files/mkdir` | `session`, `path` | `{}`. The parent folder must exist |

Rules:

- **Tag.** A tag is the driver's `FileTag` text. Compare tags only for
  equality. A read with `if_match` fails with `changed` when the file
  changed. To read a whole file, read from offset 0, then pass the tag of
  the first piece as `if_match` for each next piece.
- **Upload.** `upload_write` takes pieces in order. A piece at an offset
  after the committed offset is refused with `upload_offset` and changes
  nothing; resume from `upload_status`. `upload_commit` checks the size and
  the SHA-256, then moves the file into place in one rename. A new file gets
  mode 0644 (never an execute bit). Without `if_match` and without
  `overwrite: true`, a destination that exists at `upload_begin` is refused
  with `exists`.
- **Listing.** Entries come in name order. The cursor is the last name of
  the page. An entry that a `read` or `ls` deny rule hides, or (under the
  tool lock) an entry that leads out of the folder or has more than one hard
  link, is left out and counted in `hidden`. `mtime_ms` comes from the env's
  `file_identity` (absent when the env cannot tell). `tag` is absent in a
  listing; use `stat`.
- **Paths.** `..` above the root, an absolute path, a NUL byte, a backslash
  and a drive prefix are refused with `invalid_path` before any work. A path
  whose folder leads out of the Session's folder through a link is refused
  with `invalid_path` (list, stat, delete, rename, mkdir: checked through the
  env; read and upload: checked by descriptor in the driver).
- **Delete** removes a link, not its target. A folder that is not empty needs
  `recursive: true`.
- **Folders.** A recursive delete or the rename of a folder first walks the
  folder (links not followed, at most 10 000 entries). An entry that the user
  could not change on its own (a deny rule, `.git`, `.loop`) refuses the
  whole call.
- **Order of checks.** Path syntax, containment, the gate, and only then
  whether the target exists. So a refused path never tells whether a file is
  there. (A missing folder on the way inside the Session's folder is still
  `not_found` before the gate.)
- **Rename** works only for a Workspace whose paths are host paths: the
  contract has no rename yet (follow-up: `FileSystem::rename`). Another
  Workspace gives `Unsupported`.

### 2.2 Terminal

| Method | Params | Result |
|---|---|---|
| `session/terminal/open` | `session`, `cols`, `rows`, `command?` | `terminal` (an id), `cwd` (a WorkspacePath) |
| `session/terminal/attach` | `session`, `terminal`, `after_seq` | `last_seq`, `exit?`. Then `session/terminal/output` notifications |
| `session/terminal/detach` | `session`, `terminal` | `{}`. The terminal runs on |
| `session/terminal/input` | `session`, `terminal`, `data` (Base64) | `{}` |
| `session/terminal/resize` | `session`, `terminal`, `cols`, `rows` | `{}` |
| `session/terminal/signal` | `session`, `terminal`, `signal` = `interrupt`/`terminate`/`kill`/`hangup` | `{}` |
| `session/terminal/close` | `session`, `terminal` | `exit?` |
| `session/terminal/list` | `session` | `terminals[]`: `terminal`, `command`, `cols`, `rows`, `last_seq`, `attached`, `opened_ms`, `exit?` |
| `session/terminal/output` (server to client) | | `session`, `terminal`, `seq`, and one of `data` (Base64), `dropped` (bytes lost), `exit` |

Rules:

- **Default command.** `exec "${SHELL:-/bin/sh}" -i` in the Session's cwd,
  with `TERM=xterm-256color`.
- **Environment.** The Session's tool environment: the caller environment
  given at `session/new` (or the Runtime's), minus the names the tool env
  policy removes (provider keys, Runtime tokens, `LOOP_*` secrets, custom
  `apiKeyEnv` names; `tools.passEnv` keeps names). The Runtime builds the
  `SpawnRequest` with this list (the minimum of follow-up C4d).
- **Sequence numbers.** Each output event has a `seq`: 1, 2, 3 … with no gap
  for a reader that keeps up. `dropped` stands for the events that this
  reader lost; its `seq` is the last lost one, so the next event has the next
  `seq`. `exit` is the last event.
- **Input order.** The server runs `input` and `resize` of one connection in
  the order they arrive, on one queue (256 requests; more are refused with
  `Busy`). A `signal` does not wait in that queue, so a stuck write never
  holds up a kill. Ctrl-C typed as input (byte 3) keeps its place.
- **Limits at the same time.** An open takes its slot under the same lock as
  the count, so calls at the same time cannot pass a limit together.
- **Shared Runtime only.** Terminals and uploads need the Runtime's sweep
  task, which only a shared Runtime runs; otherwise they are `Unsupported`.
- **Detach and close.** `detach` (or a lost connection) stops this
  connection's stream only. The terminal runs on and its output stays in the
  ring buffer (1 MiB). `close` stops the process tree and removes the
  terminal.

## 3. The terminal stream

| Item | Design |
|---|---|
| Channel | One task per (connection, terminal), made by `session/terminal/attach`. It reads the driver's process handle and sends `session/terminal/output` on that connection only |
| Not durable | The bytes never pass `publish`, the update broadcast or the Event Log. A test reads the Event Log after a terminal session and finds none of the bytes |
| Back-pressure | The task asks for the next event only when the connection has room: the connection queue has at least 1024 free places and the transport holds at most 1 MiB and 32 messages of unsent text (the Unix-socket writer, the memory transport and the WebSocket count what they hold). Otherwise it waits (polls every 10 ms). The process never waits: the driver's ring buffer (1 MiB) evicts the oldest output, and the reader then gets one `dropped` event with the lost bytes. So a slow client never fills its queues, is never disconnected because of terminal output, and its `session/update` feed is not delayed. Other connections are not touched |
| Re-attach | A new connection (or the same one) calls `attach` with `after_seq` = the last `seq` it has. It gets the buffered events after it. If some were evicted, the first event is `dropped`. `after_seq` past the newest event is refused with `seq_ahead` |
| Attach twice | A second attach of the same terminal on one connection replaces the first stream |

## 4. Permission rules

### 4.1 Files: `ToolGate::check_user_file_action(action, path)`

The action is judged as a tool call of the Session, with the same Policy
layers, `toolPermissions` rules and mode: `list` as `ls`, `read` as `read`,
`write` (upload, mkdir, rename source and target) and `delete` as `write`,
on the absolute path in the Workspace. Path facts come from the Session's
Workspace env (`GatePaths`). A path the gate cannot resolve is refused
(fail closed). Hooks do not run: a click is the user's own action.

| Mode or rule | list, read | write, delete |
|---|---|---|
| A deny rule that matches (plain or pattern) | refused | refused |
| A Policy layer or Lock that turns the Capability off | refused | refused |
| The tool lock | allowed inside the Session's folder, by the lock's rules (no link out, one hard link) | refused with `ToolLocked`, before the gate |
| `.git` or `.loop` in the path (as written or resolved, below the workspace, any case) | allowed | refused in every mode (`by: protected`) |
| `read-only`, `plan` | allowed | refused |
| `ask`, `accept-edits`, a pattern `ask`, a layer's ask | allowed; the user who clicks is the approver (`by: user`), no prompt | the same |
| `full-access` | allowed | allowed |
| A caller from outside the host where the gate asks | refused (`no_approver`): an outside user cannot approve | refused |

### 4.2 Terminal: `ToolGate::check_user_terminal(command)`

A terminal is a shell with a stream of commands that Loop cannot read. So it
needs the strongest shell right.

| State | Result |
|---|---|
| The tool lock | refused (`ToolLocked`), before the gate |
| A caller from outside the host (Edge Gateway) | refused (`Forbidden`) by the server, as `session/shell` |
| The Session's sandbox is on | `Unsupported`: never a host terminal for a sandboxed Session |
| A Policy layer or Lock turns the shell off | refused |
| A plain `bash` deny rule | refused |
| Any `bash` deny pattern | refused: a terminal cannot be checked against patterns (`by: unreadable`) |
| `read-only`, `plan` | refused |
| `ask`, `accept-edits`, a layer's ask | allowed once for this terminal; the user who opens it is the approver (`by: user`) |
| `full-access` | allowed |

The gate judges the terminal as a `bash` call of the unreadable command
`exec "${SHELL:-/bin/sh}" -i`, so an `allow` pattern never matches it. While
the terminal runs, the Runtime checks again every 250 ms (without a record).
When the check now refuses (a mode change to `read-only`, a new deny rule
after a reload, the sandbox turned on), the Runtime closes the terminal with
that reason.

### 4.3 Decision records

| Event | Record |
|---|---|
| Each file action | One record, `source: user_files`, tool `ls`, `read` or `write`, the path as `subject`, before the action |
| Terminal open | One record, `source: user_terminal`, tool `bash`, `call_id` = the terminal id |
| Terminal end | One more record with the same `call_id`, outcome `allow`, `by: user`, and the reason and exit state in `reason` (for example `terminal closed by the user: exited with code 0`) |
| Keystrokes | No record |

`user_files` and `user_terminal` are stored as text (`DecisionSource::Other`
today; a later change in `loop-store` can name them).

### 4.4 Where the checks sit

| Check | Where |
|---|---|
| Access (`decide`) | The first line of each Runtime entry point (`Runtime::check`) |
| Outside caller and terminal | Server (`outside_user_refusal`), then the Runtime again |
| Tool lock | Runtime, after access, before the gate |
| Sandbox | Runtime, after the lock |
| Path syntax | Runtime (`WorkspacePath::parse`), before any work |
| Gate | Runtime, through the Session's `ToolGate` |
| Containment by descriptor | `host` driver (read, upload) |

## 5. Access

| Method | Right |
|---|---|
| `session/workspace`, `session/files/list`, `stat`, `read` | `Read` |
| `session/files/upload_*`, `delete`, `rename`, `mkdir` | `Write` |
| `session/terminal/*` (also `list` and `attach`: the output can hold secrets) | `Write` |
| `session/terminal/output` (to the client) | `Write`, at attach |

- A refusal is `SessionNotFound`, the same as for any Session.
- **Owned ids.** A terminal and an upload belong to the Session and to the
  caller that made them (same tenant and same principal). Another caller of
  the same Session gets `unknown_terminal` or `unknown_upload`, the same as
  for an id that does not exist. The same principal can use them from
  another connection (to re-attach after a reconnect).
- **Ids.** Terminal and upload ids are random (`term-` or `up-` and 32 hex
  digits from two UUIDv7 random parts through SHA-256). A driver id never
  goes to the client, and a client id never goes to the driver: the Runtime
  maps one to the other.
- **Not host-only, not admin.** None is in `HOST_ONLY` or in `LOCKED_ADMIN`.
  The Runtime itself refuses the writes and the terminal under the lock.
- **Host users.** Every host-user connection has the same principal, so
  they share terminals and uploads. Outside users exist only on a locked
  Runtime, where uploads and terminals are refused. So two principals can
  hold ids at the same time only for in-process callers; a test covers
  them (`crates/loop-runtime/tests/session_files.rs` and `session_terminal.rs`:
  an in-process call is not the owner of a host user's terminal or upload).

## 6. Capabilities

| Where | Field |
|---|---|
| `initialize` | `session_files` (the Runtime serves the file methods), `session_files_write` (false under the tool lock), `session_terminal` (false under the tool lock and for an outside caller) |
| Per Session | `session/workspace`: `files`, `files_write`, `terminal`, each `{available, reason?}`, plus `sandboxed`, `tool_lock` and the limits. A client hides what will be refused. The gate still decides each call |

## 7. Life cycle and limits

| Item | Default | Rule |
|---|---|---|
| Terminals per Session | 4 | `limit` error after |
| Terminals per caller (whole Runtime) | 8 | `limit` error after |
| Detached terminal | 10 minutes | A terminal that no connection streams is closed after this time, with a record |
| Open uploads per caller | 8 | `limit` error after; the driver also has 16 per Workspace |
| Upload idle time | 10 minutes | The driver removes it; the Runtime forgets it at the next sweep |
| Ring buffer | 1 MiB | Per terminal (driver) |
| Read and upload piece | 1 MiB | |
| Session closed, unloaded or reloaded into a new instance | | Its terminals close with a record ("the Session closed or was unloaded"); its uploads are aborted |
| Sandbox turned on, mode or rule change | | The terminal closes with a record, within 250 ms |
| Runtime shutdown | | Every terminal closes with a record and every upload is aborted, before the Sessions close |
| Idle shutdown | | An open terminal does not keep an auto-started owner alive |

The limits are in `SessionWorkspaceLimits` on the Runtime
(`Runtime::set_workspace_limits`). Reading them from `settings.json` is a
follow-up. A Session with a live terminal can still be unloaded to make room
when no connection watches it; its terminals then close with the reason in
the record.

## 8. Tool lock: upload exception (specified, not built)

The hosted chat mode needs file attachments. The lock refuses every write.
The exception would be one Runtime start option:

`--tool-lock-uploads SUBFOLDER` (and `LOOP_TOOL_LOCK_UPLOADS`):

| Rule | |
|---|---|
| Where | Only into `<Session folder>/<SUBFOLDER>`. `SUBFOLDER` is one plain name (no `/`, not `.` or `..`, not hidden). The Runtime makes it (0700) when the Session opens, checks it is a real folder owned by the Runtime user, and keeps its resolved path |
| What | Regular files only, made new (`O_EXCL`): never a replace, never a link, never a folder |
| Mode | 0600, no execute bit |
| Limits | Size per file (default 25 MiB), files per Session (default 100), total bytes per Session (default 500 MiB) |
| Gate | A record `source: user_files`, tool `write`, `by: user` |
| Not allowed | delete, rename, mkdir, terminal |

It is not built here because the `host` driver refuses every upload when it
confines reads, and both the driver and the start flags are outside this
task. With the lock, `upload_begin` is refused with `ToolLocked`, and a test
checks that.

## 9. Errors

`BackendError::Workspace { code, message }` (JSON-RPC code `-32008`, data
`{"code": …}`). An older client reads it as a failure with its message.

| Code | Meaning | Retry |
|---|---|---|
| `invalid_path` | Bad path syntax, or a link out of the folder | no |
| `not_found` | No such file or folder | no |
| `exists` | The target exists | no |
| `changed` | `if_match` did not match | read again |
| `not_a_file`, `not_a_folder`, `not_empty` | Wrong kind, or a folder with entries | no |
| `too_large` | More data than declared or allowed | no |
| `upload_offset` | A piece after the committed offset | resume from `upload_status` |
| `checksum` | SHA-256 differs at commit | upload again |
| `unknown_upload`, `unknown_terminal` | No such id for this caller and Session | no |
| `seq_ahead` | `after_seq` is past the newest event | attach with 0 |
| `limit` | A count limit | after a close |
| `refused` | The permission gate refused; the message says why | no |
| `io` | Any other driver error | maybe |

## 10. Compatibility

- Protocol version 1. Every field and method is new. `Capabilities` reads
  with defaults, so an old client ignores the new flags.
- The Loop client (`loop-wire` `Connection`) puts every notification on its
  `Incoming` channel; the Surface code matches only the methods it knows and
  drops the rest. The vendored web types are generated from the schema and
  switch on the method name; a method they do not know falls to their default
  case. A client gets `session/terminal/output` only after it calls
  `attach`.
- The `session/delta` notification and protocol version 2 (ADR-0007) are
  separate. Nothing here uses `session/update`, so that step is not blocked.

## 11. Gaps and follow-ups

| Gap | Follow-up |
|---|---|
| No rename in the Workspace contract; the time stamp costs one more env call per entry | Add `FileSystem::rename` and `mtime` to `FileInfo` (C4 owner) |
| List, stat, delete, mkdir check containment by path, not by descriptor: a folder swapped for a link during the call is a race | Descriptor-checked versions in the driver |
| Sandbox on: no files, no terminal | C4a and C4b |
| A transport made outside `loop-wire` (an unbounded channel given to `Transport`) cannot tell its backlog, so back-pressure there uses the connection queue only. `lines` (Unix socket), `memory` and the WebSocket count theirs | — |
| `DecisionSource` has no named variants for these sources | `loop-store` owner |
| Limits are not in `settings.json` yet | Settings keys |
