# Phase 0 Lane B · loop-protocol on the wire: design draft

Status: approved 2026-10-29 (method set, `loop-wire` crate, exit codes). Model control comes in S3. PR 1 (`--print --output-format`) is built; user docs: [Running Loop from scripts](../print-mode.md).

Goal: `loop-protocol` runs over a real connection. This is S1, the first step
of the spine: the Runtime (S2) and the TUI as a Runtime client (S3) build on
it. This lane adds no `loop serve` and no `--connect`. Its only user-facing
part is `--print --output-format`.

## 1. What exists today

| Piece | Where | State |
|---|---|---|
| Event model | `crates/core/loop-protocol` | `UpdateEnvelope` and `SessionUpdate` are versioned (`v`, `PROTOCOL_VERSION = 1`). They serialize as snake_case JSON with lenient readers (`Unknown`, `Other`), and `tests/wire.rs` pins the JSON. |
| Backend contract | `loop-protocol/src/backend.rs` | `AgentBackend` and `HostServices` are in-process only, over a tokio broadcast. `SessionRef`, `SessionSpec`, `BackendError`, `TurnOutcome` and `ModelChoices` have no serde. |
| JSON-RPC | `loop-acp/src/rpc.rs` | Line-delimited JSON-RPC 2.0 over `AsyncRead`/`AsyncWrite`. One ordered incoming channel, so a response arrives after every notification sent before it. Pending requests fail when the stream closes. |
| `--print` | `loop-cli/src/print_mode.rs` | Prints the last agent message as text. Exits 1 on any error. |

## 2. Layout

```
crates/core/
├── loop-protocol/     # event model + AgentBackend (unchanged deps: serde, async-trait, tokio sync)
│   └── src/rpc/       # NEW, behind no feature: message types, methods, initialize (serde only)
└── loop-wire/         # NEW crate: Connection, transports, RemoteBackend, serve()
    ├── connection.rs  # lifted from loop-acp/src/rpc.rs onto a message transport
    ├── transport/     # memory (mpsc), lines (stdio, Unix socket), websocket
    ├── client.rs      # RemoteBackend: impl AgentBackend over a Connection
    └── server.rs      # serve(backend, host, transport)
```

- **Why a new crate:** `loop-protocol` stays a dependency-light contract, as its README promises and the architecture review recommends. Transports need `tokio` io/net, `tokio-util` and a WebSocket library.
- **loop-acp moves onto `loop-wire::Connection`** with its line transport. Its behaviour doesn't change, and its tests must stay green.

## 3. Framing and transports

A transport sends and receives whole messages (`Stream<Item = String>` + `Sink<String>`).

| Transport | Framing | Use |
|---|---|---|
| memory | an mpsc pair | in-process; tests |
| lines | one JSON message per line | stdio, Unix sockets (Linux and macOS) |
| websocket | one JSON message per text frame | remote Surfaces, later the Web app |

Security, carried over from `mcp serve`:
- **Unix socket:** created with mode 0600, under `~/.loop/run/`.
- **WebSocket:** binds to loopback by default and always needs a bearer token, even on loopback, because any web page can open `ws://127.0.0.1`. It refuses to bind to a non-loopback address without `--token`. This lane only adds the transport; the flags come with `loop serve` in S2.

## 4. Methods

JSON-RPC 2.0. Params and results are snake_case, with the same lenient-reader rules as the event model.

| Method | Direction | Kind | Params → result |
|---|---|---|---|
| `initialize` | client → server | request | `{protocol_version, client_info, client_capabilities}` → `{protocol_version, server_info, capabilities}` |
| `session/new` | c → s | request | `SessionSpec` → `SessionRef` |
| `session/load` | c → s | request | `{session, spec}` → `SessionRef` (history arrives as replayed updates) |
| `session/transcript` | c → s | request | `{session}` → `[SessionUpdate]` |
| `session/prompt` | c → s | request | `{session, content: [ContentBlock]}` → `TurnOutcome` |
| `session/cancel` | c → s | notification | `{session}` |
| `session/update` | s → c | notification | `UpdateEnvelope` |
| `host/interact` | s → c | request | `InteractionRequest` → `InteractionOutcome` |
| `shutdown` | c → s | request | `{}` → `{}` |

- **Ordering:** the `session/prompt` response is sent only after that turn's `turn_ended` update. A client that has the outcome has therefore seen the whole turn.
  - Backends don't promise which comes first: ACP resolves the outcome before it publishes `turn_ended`. So one forwarder task sends everything to the client.
  - A prompt is registered with the forwarder before the backend starts it. Its turn is the session's next `turn_started`, and the response goes out once that turn's `turn_ended` has been sent and the outcome is known.
  - If the backend refused the prompt before any turn started, the response goes out 2 s after the outcome.
- **Busy:** a second `session/prompt` on a busy session fails with `BackendError::Busy`, as it does in-process.
  - `RemoteBackend` tracks running turns from the update stream and refuses locally.
  - A turn another client started first comes back as a `-32001` error, which `RemoteBackend` reports as a failed outcome, because by then `prompt()` has already returned its handle.
- **Optional services:** `ModelControl`, `Compaction`, `SessionStats` and `LocalShell`, and the host's file system and terminals. Each is advertised in `capabilities` and gets methods in a later version. In v1, `RemoteBackend` returns `None` for each.
- **Errors:** a `BackendError` maps to a JSON-RPC error. The code is `-32000 - variant` (busy, unsupported, session not found, failed), and `data` carries the `ErrorInfo`. Malformed requests use the standard codes.
- **Lost connection:** a prompt whose connection closes before its response resolves as `TurnOutcome::Uncertain`.
- **Lag:** if the server falls behind the backend's broadcast (`RecvError::Lagged`), it sends `session/update` with a `notice` saying how many updates were missed, then closes the connection. A client never silently misses part of a turn.

## 5. Versioning and negotiation

One constant, `PROTOCOL_VERSION`, covers the events and the methods.
- The client sends the highest version it speaks. The server answers with that version if it speaks it, or else its own highest. A client that can't use the answer closes the connection.
- Additive changes, such as a new method, field or update type, keep the version. A reader ignores what it doesn't know, and calling an unknown method returns `-32601`.
- `server_info` / `client_info` are `{name, version}` for logs, never for feature checks. Feature checks use `capabilities`.

## 6. JSON Schema

- A checked-in `crates/core/loop-protocol/schema/loop-protocol.json` covers every event type and every method's params and result.
- It's generated with `schemars`, and a test fails if the file is out of date.
- Tagged enums come out as `oneOf` with a `const` tag, so the Phase 1 TypeScript SDK generator produces discriminated unions.

## 7. First consumer: `--print --output-format`

`loop --print "…" --output-format text|json|stream-json`. The default is `text`, which keeps today's output.

- **`json`:** one object when the turn ends:
  `{"type":"result","session":"…","stop":"end_turn","text":"…","usage":{…},"error":null,"exit_code":0}`
- **`stream-json`:** every `UpdateEnvelope` of the turn as one line, then the same `result` object as the last line.
- **stdout carries only the selected format.** Notices and the trace id stay on stderr.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | the turn completed |
| 1 | the turn failed, or Loop failed to start (unchanged) |
| 2 | usage error (clap) |
| 3 | lost contact with the agent mid-turn; edits or commands may have run (`TurnOutcome::Uncertain`) |
| 130 | the turn was cancelled |

## 8. Tests

- **Parity, the test that decides correctness:** drive `NativeBackend` with the faux provider, once directly and once through `RemoteBackend` over each transport (memory, lines over a Unix socket, websocket). Each run must give the same update sequence and the same `TurnOutcome`.
  - The outcome resolves only after `turn_ended`.
  - `host/interact` makes a round trip: a permission prompt answered by the client.
  - A second prompt on a busy session returns `Busy`.
  - A lagging client gets the notice and then the close.
- **Negotiation:** same version; a newer client against an older server; a server version the client refuses.
- **Wire:** golden JSON for every method and error, in the style of `tests/wire.rs`.
- **Schema:** the file is up to date, and the golden examples validate against it.
- **`--print`:** each format's stdout against the faux provider, and each exit code.

## 9. PR plan

1. `--print --output-format` and exit codes. This needs no transport.
2. `loop-wire` with `Connection` lifted onto message transports, and loop-acp moved onto it (no behaviour change).
3. Serde for the backend types, the methods and `initialize`, and `RemoteBackend` + `serve`, with the in-process parity test.
4. The Unix socket and WebSocket transports, running the same parity test.
5. The JSON Schema and its freshness test.

## 10. Open questions

- The method names: `session/*` follows ACP. Is that the right choice for Loop's own API, which the SDK will expose?
- Should v1 include `ModelControl`? The TUI uses it to switch models, but S3 can add it.
- The WebSocket library: `tokio-tungstenite`. axum's `ws` feature covers only the server side.
