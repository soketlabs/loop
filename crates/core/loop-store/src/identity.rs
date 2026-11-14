//! Identity fields of a stored record (ADR-0007 §7, ADR-0009).
//!
//! `Tenant`, `Principal`, `PrincipalKind` and `Via` are the protocol's own
//! types, re-exported here, so there is one definition of each: a record
//! stores exactly what the Runtime stamped from the connection. Step B3 adds
//! the columns and the `&Tenant` argument on each read.

use serde::{Deserialize, Serialize};

pub use loop_protocol::{Principal, PrincipalKind, Tenant, Via};

/// The identity fields that each stored record will carry (ADR-0007 §7.1).
///
/// The Runtime stamps them from the authenticated connection. A store never
/// takes them from a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Required.
    pub tenant: Tenant,
    /// Required.
    pub principal: Principal,
    /// How the principal reached the Runtime: `token`, `edge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// The Session or agent that acted for the principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// Permission to read across tenants: recovery, audit and projection rebuild.
///
/// Only the Runtime startup code should make one. This crate cannot enforce
/// that; a later step can seal the constructor.
#[derive(Debug)]
pub struct SystemScope(());

impl SystemScope {
    /// For the Runtime startup code only.
    pub fn for_runtime_startup() -> Self {
        Self(())
    }
}
