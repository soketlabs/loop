//! JSON-RPC 2.0 to an agent, from [`loop_wire`]: Loop talks to an ACP
//! agent's stdio over [`loop_wire::transport::lines`].

pub use loop_wire::{codes, Connection, Incoming, PeerRequestId, RpcError};
