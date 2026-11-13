//! Identity fields of a stored record (ADR-0007 §7, ADR-0009).
//!
//! Defined for later steps. Nothing uses these types yet. Step B3 adds the
//! columns and the `&Tenant` argument on each read.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The isolation boundary that data belongs to. An opaque string.
///
/// Old rows read as [`Tenant::LOCAL`]. Runtime records in a shared cell use
/// [`Tenant::SYSTEM`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tenant(String);

impl Tenant {
    /// The tenant of a single node, and of every row written before B3.
    pub const LOCAL: &'static str = "local";
    /// The reserved tenant of Runtime records in a shared cell (ADR-0007 Q14).
    pub const SYSTEM: &'static str = "system";

    /// A tenant with this id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    /// The single-node tenant, `local`.
    pub fn local() -> Self {
        Self(Self::LOCAL.to_owned())
    }
    /// The reserved tenant `system`.
    pub fn system() -> Self {
        Self(Self::SYSTEM.to_owned())
    }
    /// The tenant id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tenant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a principal is a person or a Service Identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A person.
    Person,
    /// A Service Identity (unattended work).
    Service,
}

/// The person or Service Identity on whose behalf something happens.
///
/// The pair `(issuer, id)` is stable, so an OIDC `(iss, sub)` fits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Principal {
    kind: PrincipalKind,
    issuer: String,
    id: String,
}

impl Principal {
    /// A principal from its parts.
    pub fn new(kind: PrincipalKind, issuer: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind,
            issuer: issuer.into(),
            id: id.into(),
        }
    }
    /// Person or service.
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }
    /// Who issued the id, for example `local` or an OIDC issuer URL.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    /// The id within the issuer.
    pub fn id(&self) -> &str {
        &self.id
    }
}

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
    /// How the principal reached the Runtime: `token`, `device:<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
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
