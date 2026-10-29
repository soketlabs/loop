//! `loop-protocol` over a real connection.
//!
//! - [`transport`]: whole messages over a byte stream ([`transport::lines`])
//!   or in one process ([`transport::memory`]).
//! - [`connection`]: JSON-RPC 2.0 over a transport, with responses kept in
//!   order with the notifications sent before them.
//!
//! See `docs/design/protocol-wire.md`.

#![deny(missing_docs)]

pub mod connection;
pub mod transport;

pub use connection::{codes, Connection, Incoming, PeerRequestId, RpcError};
pub use transport::Transport;
