# loop-wire

`loop-protocol` over a real connection: JSON-RPC 2.0 and the transports that
carry it.

- `transport`: whole messages between two peers. `lines` reads and writes one
  JSON message per line over any byte stream (stdio, Unix sockets) and drops
  oversized lines. `memory` connects two peers in one process.
- `connection`: JSON-RPC 2.0 over a transport. Everything the peer sends
  arrives on one ordered channel, responses included, so a request's result is
  seen only after every notification sent before it. Pending requests fail
  when the transport closes.

`loop-acp` uses this connection, over `lines`, to talk to external agents.
`loop-protocol` itself stays free of transports.

See [loop-protocol on the wire](../../../docs/design/protocol-wire.md) for the
design.
