# loop-wire

`loop-protocol` over a real connection: JSON-RPC 2.0 and the transports that
carry it.

- `transport`: whole messages between two peers. `lines` reads and writes one
  JSON message per line over any byte stream (stdio, Unix sockets) and drops
  oversized lines. `memory` connects two peers in one process.
- `socket`: Unix sockets (`unix_listen` creates the socket with mode 0600 in a
  0700 directory; `unix` wraps a connection) and WebSockets
  (`websocket_accept` refuses any client without the bearer token, even on
  loopback; `websocket_connect`). One message per line or per text frame.
- `connection`: JSON-RPC 2.0 over a transport. Everything the peer sends
  arrives on one ordered channel, responses included, so a request's result is
  seen only after every notification sent before it. Pending requests fail
  when the transport closes.

- `server::serve`: serve an `AgentBackend` to one client. `session/prompt` is
  answered only after that turn's `turn_ended` has been sent. `host/interact`
  lets the backend ask the client (`Served::host`).
- `client::RemoteBackend`: the `AgentBackend` on the other end, so a Surface
  can't tell a served backend from an in-process one.

`loop-acp` uses this connection, over `lines`, to talk to external agents.
`loop-protocol` itself stays free of transports.

See [loop-protocol on the wire](../../../docs/design/protocol-wire.md) for the
design.
