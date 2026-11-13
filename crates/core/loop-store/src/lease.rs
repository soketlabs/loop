//! Generations, lease owners and fences (ADR-0007 §2.5, §8).
//!
//! Defined for later steps. Nothing uses these types yet. The present
//! `EventLog` uses plain `u64` generations. Chain D owns the lease details.

use loop_protocol::SessionKey;
use serde::{Deserialize, Serialize};

use crate::identity::Tenant;

/// A writer generation. `runtime_generations` allocates one per owner start,
/// for every owner kind.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Generation(pub u64);

/// The owner that asks for a Session lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerInfo {
    /// Owner kind: `runtime` or `standalone`.
    pub owner: String,
    /// Owning process id.
    pub pid: u32,
    /// The owner's generation.
    pub generation: Generation,
}

/// Proof that the writer holds the lease of one Session at one epoch.
///
/// A lease store makes it. A fenced append checks the epoch in the same
/// transaction and fails with `StoreError::Fenced` when the lease moved on.
/// The epoch is stored with each record as `lease_epoch`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fence {
    tenant: Tenant,
    session: SessionKey,
    epoch: u64,
}

impl Fence {
    /// For lease store implementations.
    pub fn new(tenant: Tenant, session: SessionKey, epoch: u64) -> Self {
        Self {
            tenant,
            session,
            epoch,
        }
    }
    /// The tenant of the leased Session. Each fenced event has this tenant.
    pub fn tenant(&self) -> &Tenant {
        &self.tenant
    }
    /// The leased Session.
    pub fn session(&self) -> &SessionKey {
        &self.session
    }
    /// The lease epoch. It grows at each acquire and takeover.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}
