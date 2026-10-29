//! `loop-protocol` over a real connection.
//!
//! - [`transport`]: whole messages over a byte stream ([`transport::lines`])
//!   or in one process ([`transport::memory`]).
//! - [`socket`]: Unix sockets and WebSockets as transports.
//! - [`connection`]: JSON-RPC 2.0 over a transport, with responses kept in
//!   order with the notifications sent before them.
//! - [`server`]: [`serve`] an `AgentBackend` to one client.
//! - [`client`]: [`RemoteBackend`], the `AgentBackend` on the other end.
//!
//! See `docs/design/protocol-wire.md`.

#![deny(missing_docs)]

pub mod client;
pub mod connection;
pub mod server;
pub mod socket;
pub mod transport;

pub use client::RemoteBackend;
pub use connection::{codes, Connection, Incoming, PeerRequestId, RpcError};
pub use server::{serve, Served};
pub use transport::Transport;
