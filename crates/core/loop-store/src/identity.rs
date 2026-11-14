//! Identity fields of a stored record (ADR-0007 §7, ADR-0009).
//!
//! `Tenant`, `Principal`, `PrincipalKind`, `Via` and `Caller` are the
//! protocol's own types, re-exported here, so there is one definition of
//! each: a record stores exactly what the Runtime stamped from the
//! connection.
//!
//! Live from step B3: every [`crate::Event`] carries an [`Identity`], and
//! each read of the Event Log takes a [`Tenant`] or a [`Scope`].
//!
//! Two rules:
//!
//! - The tenant of a Session record is the **Session's** tenant, fixed when
//!   the Session is created. The principal is who acted. They can differ:
//!   an Escalation answer has the answerer's principal in the Session's
//!   tenant (ADR-0009).
//! - There is no `Default`. The only fallback value is
//!   [`Identity::unrecorded`], which the stores use when they read a record
//!   written before step B3. Code that appends never uses it.

use serde::{Deserialize, Serialize};

pub use loop_protocol::{Caller, Principal, PrincipalKind, Tenant, Via};

/// The most bytes of an [`Identity::actor`].
pub const MAX_ACTOR_BYTES: usize = 512;

/// The identity fields that each stored record carries (ADR-0007 §7.1).
///
/// The Runtime stamps them from the authenticated connection. A store never
/// takes them from a request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    /// The tenant the record belongs to. For a Session record, the
    /// Session's tenant.
    pub tenant: Tenant,
    /// Who acted.
    pub principal: Principal,
    /// How the principal reached the Runtime: `token`, `edge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// The Session whose agent acted for the principal: on every record of
    /// a Subagent Session, its parent Session's key. `None` when the
    /// principal's own request or turn in this Session caused the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

impl Identity {
    /// `caller` acting on a record of `tenant` (for a Session record, the
    /// Session's tenant). The principal and `via` come from the caller.
    pub fn acting(caller: &Caller, tenant: Tenant) -> Self {
        Self {
            tenant,
            principal: caller.principal().clone(),
            via: caller.via().cloned(),
            actor: None,
        }
    }

    /// The Runtime acting on its own on a record of `tenant`: recovery
    /// records, a lost lease, an unload, a turn the Runtime ended. For a
    /// Session record, `tenant` is the Session's tenant.
    pub fn runtime_in(tenant: Tenant) -> Self {
        Self {
            tenant,
            principal: Principal::runtime(),
            via: None,
            actor: None,
        }
    }

    /// The identity a store gives a record written before step B3, which
    /// stored none: tenant `local` and the Runtime's own principal
    /// `{service, local, runtime}`. A fixed value: it does not depend on
    /// the process that reads it. Before B3 only the Runtime wrote the
    /// Event Log, and it did not record who asked.
    ///
    /// Only stores (the row decoder and serde) use this. Code that appends
    /// passes the identity of who acted.
    pub fn unrecorded() -> Self {
        Self::runtime_in(Tenant::local())
    }

    /// The same identity with `actor` set.
    pub fn with_actor(mut self, actor: Option<String>) -> Self {
        self.actor = actor;
        self
    }

    /// Check the fields that the types do not check: `actor` is 1 to
    /// [`MAX_ACTOR_BYTES`] bytes with no control character.
    pub fn validate(&self) -> Result<(), ActorError> {
        if let Some(actor) = &self.actor {
            if actor.is_empty() || actor.len() > MAX_ACTOR_BYTES {
                return Err(ActorError::Length);
            }
            if actor.chars().any(char::is_control) {
                return Err(ActorError::Control);
            }
        }
        Ok(())
    }
}

/// Why an [`Identity::actor`] is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ActorError {
    /// Empty, or longer than [`MAX_ACTOR_BYTES`].
    #[error("actor must be 1 to {MAX_ACTOR_BYTES} bytes")]
    Length,
    /// Holds a control character.
    #[error("actor must not hold a control character")]
    Control,
}

/// How a Runtime is deployed, which decides the tenant of its own records
/// (records with no Session: `runtime_started`, `runtime_stopped`).
///
/// The later cell work changes only the value the Runtime is built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deployment {
    /// One host, one person: Runtime records are in tenant `local`.
    SingleNode,
    /// A cell shared by tenants: Runtime records are in the reserved tenant
    /// `system` (ADR-0007 §7.1, Q14).
    SharedCell,
}

impl Deployment {
    /// The tenant of the Runtime's own records.
    pub fn runtime_tenant(self) -> Tenant {
        match self {
            Self::SingleNode => Tenant::local(),
            Self::SharedCell => Tenant::system(),
        }
    }

    /// The identity of the Runtime's own records: the Runtime's service
    /// principal in [`Deployment::runtime_tenant`].
    pub fn runtime_identity(self) -> Identity {
        Identity::runtime_in(self.runtime_tenant())
    }
}

/// Permission to read across tenants: recovery, audit and projection rebuild.
///
/// Not `Clone`: the Runtime makes one at startup and lends it. Rust cannot
/// limit a constructor to one other crate (this crate must not depend on
/// the Runtime), so the Runtime crate has a test that scans the whole
/// workspace: outside test code, only `Runtime::new` calls it.
#[derive(Debug)]
pub struct SystemScope(());

impl SystemScope {
    /// For the Runtime startup code only.
    #[doc(hidden)]
    pub fn for_runtime_startup() -> Self {
        Self(())
    }
}

/// Which rows a read that is not about one Session may return: those of one
/// tenant, or (with a [`SystemScope`]) those of all tenants.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// One tenant's rows.
    Tenant(&'a Tenant),
    /// Every tenant's rows.
    System(&'a SystemScope),
}

impl Scope<'_> {
    /// Whether a row of `tenant` is in this scope.
    pub fn admits(&self, tenant: &Tenant) -> bool {
        match self {
            Self::Tenant(scope) => *scope == tenant,
            Self::System(_) => true,
        }
    }

    /// The one tenant, or `None` for a system scope.
    pub fn tenant(&self) -> Option<&Tenant> {
        match self {
            Self::Tenant(tenant) => Some(tenant),
            Self::System(_) => None,
        }
    }
}

impl<'a> From<&'a Tenant> for Scope<'a> {
    fn from(tenant: &'a Tenant) -> Self {
        Self::Tenant(tenant)
    }
}

impl<'a> From<&'a SystemScope> for Scope<'a> {
    fn from(scope: &'a SystemScope) -> Self {
        Self::System(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_and_via_come_from_the_caller_and_tenant_from_the_session() {
        let edge = Caller::asserted(
            Principal::new(PrincipalKind::Person, "https://idp.example", "u1").unwrap(),
            Tenant::new("acme").unwrap(),
        )
        .unwrap();
        // An edge user acting on a local Session: the Session's tenant.
        let identity = Identity::acting(&edge, Tenant::local());
        assert_eq!(identity.tenant, Tenant::local());
        assert_eq!(identity.principal, *edge.principal());
        assert_eq!(identity.via, Some(Via::edge()));
        assert_eq!(identity.actor, None);
        assert_eq!(
            Identity::unrecorded(),
            Identity::runtime_in(Tenant::local())
        );
        assert_eq!(Identity::unrecorded().principal, Principal::runtime());
        assert_eq!(Deployment::SingleNode.runtime_tenant(), Tenant::local());
        assert_eq!(Deployment::SharedCell.runtime_tenant(), Tenant::system());
    }

    #[test]
    fn actor_is_bounded() {
        let ok = Identity::unrecorded().with_actor(Some("s1".into()));
        assert!(ok.validate().is_ok());
        assert!(Identity::unrecorded()
            .with_actor(Some(String::new()))
            .validate()
            .is_err());
        assert!(Identity::unrecorded()
            .with_actor(Some("a\nb".into()))
            .validate()
            .is_err());
        assert!(Identity::unrecorded()
            .with_actor(Some("x".repeat(MAX_ACTOR_BYTES + 1)))
            .validate()
            .is_err());
    }

    #[test]
    fn scope_admits_its_tenant_or_all() {
        let acme = Tenant::new("acme").unwrap();
        let local = Tenant::local();
        let system = SystemScope::for_runtime_startup();
        assert!(Scope::from(&acme).admits(&acme));
        assert!(!Scope::from(&acme).admits(&local));
        assert!(Scope::from(&system).admits(&acme));
        assert_eq!(Scope::from(&system).tenant(), None);
    }
}
