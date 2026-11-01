# The Runtime: `loop serve` and `loop attach`

The Runtime is one long-running `loop` process that owns your Sessions. Your
terminal is then only a Surface. You can close it, open another, or watch the
same Session from two terminals. Work keeps running in the Runtime, which keeps
an Event Log so any Surface can catch up later.

The Runtime works on **Linux and macOS** only, because it listens on a Unix
socket. On other platforms `loop serve` and `loop attach` exit with an error.

## Start the Runtime

```bash
loop serve
```

`loop serve` never reads stdin, never asks questions and never starts the TUI.
It runs in the foreground until you press Ctrl+C or send SIGTERM. To keep it
running after you close the terminal, use your usual tool, such as `nohup`,
`tmux` or a systemd user unit.

| Flag | Default | Meaning |
|---|---|---|
| `--socket PATH` | `~/.loop/agent/run/runtime.sock` | Where Surfaces connect |
| `--max-sessions N` | 16 | Most Sessions loaded at once |
| `--max-active-turns N` | 8 | Most turns running at once. More turns wait in a queue; they are not refused |
| `--permission-mode MODE` | from settings | `read-only`, `ask`, `accept-edits` or `full-access` |
| `--cwd DIR` | current folder | Folder for Sessions that name none |

Only one Runtime can use an agent directory, or a Session database, at a time.
A second `loop serve` exits with `a Runtime is already running (pid N)`, or
with `another Runtime already owns the Session database at …` when only the
Session database is shared.

### Permissions and trust

- **`full-access`** applies only when you pass `--permission-mode full-access`
  to `loop serve`. If your settings ask for it, the Runtime uses `ask` and
  shows a notice.
- **Deny rules, hooks and Policy limits** still apply in every mode.
- **Untrusted folders.** The Runtime never shows a trust prompt. In a folder you
  have not trusted, it uses only your global settings and shows a notice. Trust
  the folder first by opening it once with `loop`.

## Attach a Surface

```bash
loop attach                  # list Sessions: id, folder, title, busy, last activity, parent
loop attach 3f2a             # attach to a Session by id or unique id prefix
loop attach --new            # create a Session in the current folder and attach
loop attach --new --cwd DIR  # create a Session in DIR and attach
```

When you attach, the Surface replays the Session's history and then follows it
live. A Session that is not loaded, for example after the Runtime restarted, is
loaded when you attach. Child Sessions started by a Subagent show their parent
in the list. A finished child is never loaded again: attaching replays its
output, but you cannot prompt it.

`--socket PATH` connects to a Runtime that was started with a custom socket.

### Keys

| Input | Effect |
|---|---|
| A line of text | Starts a turn with it as the prompt. If the Session is busy, you are told |
| A number, while an approval is shown | Answers the approval with that choice |
| `> ` then text | Always a prompt, even when it starts with a number |
| Ctrl+C | Cancels the current turn |
| Ctrl+C again, or Ctrl+D | Detaches |

Detaching, or losing the connection, never stops the agent. Attach again to
continue watching.

## Approvals

An approval is an Escalation: a tool that needs your decision before it runs.

- Every attached Surface sees the request as numbered choices.
- The first valid answer wins. The other Surfaces show
  `Another client answered; Escalation withdrawn.`
- If no Surface is attached, the request waits, with no timeout. The next
  Surface to attach sees it.
- Cancelling the turn withdraws its pending approvals.
- An "allow for this Session" grant lasts only while the Runtime runs. It is
  never restored after a restart.

## Sessions held by the Runtime

While the Runtime has a Session loaded, it holds that Session's lease, and no
other process can write to it. The Runtime renews the lease every 5 seconds.
Opening the same Session in the terminal app is refused:

```text
Session <id> is in use by Runtime <pid>. Use `loop attach <id>` instead.
```

Closing a Session, or stopping the Runtime cleanly, releases its lease. If the
Runtime was killed, there are two ways to get the Session back:

- **Restart `loop serve`.** It reclaims its old leases straight away.
- **Use `loop --resume <id>`.** This works once the lease is at least 30
  seconds old and the old Runtime process is gone.

The same rule works the other way round. If a terminal `loop` crashed while
holding a Session, the Runtime loads that Session once the lease is at least
30 seconds old and that process is gone. A lease whose owner is still running
is never taken over.

## After a crash

When `loop serve` starts, it checks what the previous Runtime left unfinished,
before it accepts any Surface:

- **Interrupted turns** get a final turn-end marked `runtime_restarted`.
  `loop attach` shows it as
  `Turn ended uncertain — Runtime restarted`.
  Loop does not know whether that turn's tools ran, so it never retries the
  prompt. Check the work, then send a new prompt.
- **Pending approvals** from the old Runtime are withdrawn. An old approval
  is never shown again or applied to a new turn.
- **Leases** of the old Runtime are reclaimed.

`loop serve` prints what it recovered, for example
`recovered 1 interrupted turn(s), withdrew 1 Escalation(s), reclaimed 1 lease(s)`.
It prints nothing when there was nothing to recover. Starting it again
recovers nothing more.

- **What startup reads.** Recovery reads only turn starts and ends and
  approval records, and only since the last Runtime run whose recovery
  finished. It never loads streamed text. A Runtime that ran for a long time
  without a restart still means more records to check on the next start.
- **Leases that cannot be reclaimed.** If one Session's lease cannot be
  reclaimed, `loop serve` prints `Runtime recovery: lease of Session <id>: …`
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
[`design/protocol-wire.md`](design/protocol-wire.md#11-runtime-methods-s2).
