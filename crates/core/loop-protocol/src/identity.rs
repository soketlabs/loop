//! Who acts, and for which tenant ([ADR-0009]).
//!
//! - A [`Principal`] is the person or Service Identity on whose behalf
//!   something happens: `{ kind, issuer, id }`. The pair `(issuer, id)` is
//!   stable, so an OIDC `(iss, sub)` fits without a change.
//! - A [`Tenant`] is the isolation boundary the data belongs to: an opaque,
//!   validated string that is safe as a storage key and in logs.
//! - A [`Caller`] is both, plus an optional [`Via`] (how the principal
//!   reached the Runtime: `token`, `edge`, later `device:<id>`). A `Via` is
//!   an attribute of a record, never a principal.
//!
//! The Runtime stamps the caller from the authenticated connection, never
//! from a request field. A connection's caller is fixed when the connection
//! is accepted. While the server dispatches a request, the caller is in scope
//! as a task-local value: [`Caller::current`].
//!
//! Nothing here has a `Default`, so no code can make a privileged identity
//! by accident. The reserved values have named constructors:
//! [`Tenant::local`], [`Tenant::system`], [`Principal::local_uid`] and
//! [`Principal::runtime`]. Values from outside the host (an Edge Gateway
//! assertion) go through [`Caller::asserted`], which refuses every reserved
//! value.
//!
//! [ADR-0009]: https://github.com/soket-ai/loop/blob/main/docs/adr/0009-identity-fields-from-s4.md

use std::fmt;
use std::future::Future;

use serde::{Deserialize, Deserializer, Serialize};

/// Longest tenant, in bytes.
pub const MAX_TENANT_BYTES: usize = 64;
/// Longest principal issuer or id, in bytes. An OIDC `sub` is at most 255
/// ASCII characters.
pub const MAX_PRINCIPAL_PART_BYTES: usize = 255;
/// Longest [`Via`], in bytes.
pub const MAX_VIA_BYTES: usize = 128;

/// The issuer of principals that only this host knows: its OS users and the
/// Runtime's own service principal. An Edge Gateway can never assert it.
pub const LOCAL_ISSUER: &str = "local";

/// Why an identity value was refused. The messages never contain the refused
/// value, so they are safe to log and to send to a peer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The value is empty.
    #[error("{0} is empty")]
    Empty(&'static str),
    /// The value is longer than its limit.
    #[error("{field} is longer than {max} bytes")]
    TooLong {
        /// Which value.
        field: &'static str,
        /// Its limit in bytes.
        max: usize,
    },
    /// The value has a character outside its allowed set.
    #[error("{0} has a character that is not allowed")]
    Character(&'static str),
    /// The value is reserved for the Runtime or the local host.
    #[error("{0} is reserved")]
    Reserved(&'static str),
    /// The principal kind is not one this Runtime knows.
    #[error("principal kind is not known")]
    UnknownKind,
}

/// What kind of principal acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A person.
    Person,
    /// A Service Identity: unattended work, or the Runtime itself.
    Service,
    /// A kind this reader does not know. Read from a newer peer; never
    /// accepted from an Edge Gateway assertion.
    #[serde(other)]
    Unknown,
}

/// A person or Service Identity: `{ kind, issuer, id }`. `issuer` and `id`
/// are 1 to 255 visible ASCII characters (no space, no control).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Principal {
    /// Person or service.
    kind: PrincipalKind,
    /// Who vouches for `id`: `local` for this host, or an OIDC issuer URL.
    #[cfg_attr(
        feature = "schema",
        schemars(length(min = 1, max = 255), regex(pattern = r"^[!-~]+$"))
    )]
    issuer: String,
    /// Stable id within `issuer`, such as `uid:501` or an OIDC `sub`.
    #[cfg_attr(
        feature = "schema",
        schemars(length(min = 1, max = 255), regex(pattern = r"^[!-~]+$"))
    )]
    id: String,
}

impl Principal {
    /// A principal from its parts. Checks length and characters only: it
    /// accepts reserved values, so stored records read back.
    pub fn new(
        kind: PrincipalKind,
        issuer: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self, IdentityError> {
        let issuer = issuer.into();
        let id = id.into();
        visible_ascii("principal issuer", &issuer, MAX_PRINCIPAL_PART_BYTES)?;
        visible_ascii("principal id", &id, MAX_PRINCIPAL_PART_BYTES)?;
        Ok(Self { kind, issuer, id })
    }

    /// The OS user `uid` of this host: `{ person, local, uid:<uid> }`.
    pub fn local_uid(uid: u32) -> Self {
        Self {
            kind: PrincipalKind::Person,
            issuer: LOCAL_ISSUER.into(),
            id: format!("uid:{uid}"),
        }
    }

    /// The Runtime's own service principal, for Runtime records:
    /// `{ service, local, runtime }`.
    pub fn runtime() -> Self {
        Self {
            kind: PrincipalKind::Service,
            issuer: LOCAL_ISSUER.into(),
            id: "runtime".into(),
        }
    }

    /// Person or service.
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// Who vouches for the id.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The id within the issuer.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Issued by this host (`local`), not by an outside identity provider.
    pub fn is_local(&self) -> bool {
        self.issuer == LOCAL_ISSUER
    }
}

impl fmt::Display for Principal {
    /// The `(issuer, id)` pair, which logs show (ADR-0009, Privacy).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.issuer, self.id)
    }
}

impl<'de> Deserialize<'de> for Principal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Repr {
            kind: PrincipalKind,
            issuer: String,
            id: String,
        }
        let repr = Repr::deserialize(deserializer)?;
        Principal::new(repr.kind, repr.issuer, repr.id).map_err(serde::de::Error::custom)
    }
}

/// The isolation boundary data belongs to. Opaque: 1 to 64 characters of
/// `a-z`, `0-9`, `.`, `_` and `-`, starting with a letter or digit. Lower
/// case only, so two tenants never share a key on a case-insensitive file
/// system, and no `/`, so a tenant is safe in a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Tenant(String);

impl Tenant {
    /// The tenant of single-user use and of everything on one host.
    pub const LOCAL: &'static str = "local";
    /// The reserved tenant of Runtime records in a shared cell (ADR-0007 §7.1).
    pub const SYSTEM: &'static str = "system";

    /// A tenant named outside the host. Refuses the reserved names.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let tenant = Self::parse(value.into())?;
        if tenant.is_reserved() {
            return Err(IdentityError::Reserved("tenant"));
        }
        Ok(tenant)
    }

    /// The `local` tenant.
    pub fn local() -> Self {
        Self(Self::LOCAL.into())
    }

    /// The reserved `system` tenant. Only the Runtime's own records use it.
    pub fn system() -> Self {
        Self(Self::SYSTEM.into())
    }

    /// The tenant as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `local` or `system`.
    pub fn is_reserved(&self) -> bool {
        self.0 == Self::LOCAL || self.0 == Self::SYSTEM
    }

    /// Checks syntax only; accepts the reserved names.
    fn parse(value: String) -> Result<Self, IdentityError> {
        const FIELD: &str = "tenant";
        if value.is_empty() {
            return Err(IdentityError::Empty(FIELD));
        }
        if value.len() > MAX_TENANT_BYTES {
            return Err(IdentityError::TooLong {
                field: FIELD,
                max: MAX_TENANT_BYTES,
            });
        }
        let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let bytes = value.as_bytes();
        if !allowed(bytes[0])
            || !bytes
                .iter()
                .all(|&b| allowed(b) || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(IdentityError::Character(FIELD));
        }
        Ok(Self(value))
    }
}

impl fmt::Display for Tenant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Tenant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Tenant::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Tenant {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Tenant".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "The isolation boundary data belongs to. Opaque; `local` and `system` are reserved.",
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_TENANT_BYTES,
            "pattern": "^[a-z0-9][a-z0-9._-]*$"
        })
    }
}

/// How a principal reached the Runtime: `token` (the loopback WebSocket
/// bearer token) or `edge` (asserted by a trusted Edge Gateway). An attribute
/// of the record, never a principal. 1 to 128 visible ASCII characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Via(String);

impl Via {
    /// The loopback WebSocket with the Runtime's bearer token.
    pub fn token() -> Self {
        Self("token".into())
    }

    /// An Edge Gateway connection that asserted the principal.
    pub fn edge() -> Self {
        Self("edge".into())
    }

    /// The value as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Via {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Via {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        visible_ascii("via", &value, MAX_VIA_BYTES).map_err(serde::de::Error::custom)?;
        Ok(Self(value))
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Via {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Via".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "How the principal reached the Runtime, such as `token` or `edge`. Not a principal.",
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_VIA_BYTES,
            "pattern": "^[!-~]+$"
        })
    }
}

/// Who a connection acts for: a principal in a tenant, and how it came.
/// Fixed when the connection is accepted; never changed after.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Caller {
    principal: Principal,
    tenant: Tenant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<Via>,
}

tokio::task_local! {
    static CALLER: Caller;
}

impl Caller {
    /// The host's own user on a host-local connection (the Runtime's 0600
    /// Unix socket or an embedded adapter): `principal` in the `local`
    /// tenant, with no `via`.
    pub fn host(principal: Principal) -> Self {
        Self {
            principal,
            tenant: Tenant::local(),
            via: None,
        }
    }

    /// The host's own user on the loopback WebSocket with the Runtime's
    /// bearer token: `principal` in the `local` tenant, `via: token`.
    pub fn local_token(principal: Principal) -> Self {
        Self {
            principal,
            tenant: Tenant::local(),
            via: Some(Via::token()),
        }
    }

    /// A principal and tenant asserted by a trusted Edge Gateway, `via:
    /// edge`. Refuses an unknown kind, the `local` issuer and the reserved
    /// tenants, so a gateway cannot claim to be the host's user or the
    /// Runtime.
    pub fn asserted(principal: Principal, tenant: Tenant) -> Result<Self, IdentityError> {
        if principal.kind == PrincipalKind::Unknown {
            return Err(IdentityError::UnknownKind);
        }
        if principal.is_local() {
            return Err(IdentityError::Reserved("principal issuer"));
        }
        if tenant.is_reserved() {
            return Err(IdentityError::Reserved("tenant"));
        }
        Ok(Self {
            principal,
            tenant,
            via: Some(Via::edge()),
        })
    }

    /// Who acts.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The tenant the data belongs to.
    pub fn tenant(&self) -> &Tenant {
        &self.tenant
    }

    /// How the principal reached the Runtime, when not host-local.
    pub fn via(&self) -> Option<&Via> {
        self.via.as_ref()
    }

    /// The caller of the request being served on this task, if any.
    ///
    /// The server sets it around each dispatched request. It is a task-local
    /// value, so it is lost across `tokio::spawn`: read it at the entry of a
    /// call and pass it on. `None` means no connection is in scope (an
    /// in-process call, or a task the Runtime started itself). There is no
    /// fallback identity on purpose.
    pub fn current() -> Option<Caller> {
        CALLER.try_with(Clone::clone).ok()
    }

    /// Run `future` with `self` as [`Caller::current`].
    pub fn scope<F: Future>(self, future: F) -> impl Future<Output = F::Output> {
        CALLER.scope(self, future)
    }
}

impl fmt::Display for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in {}", self.principal, self.tenant)?;
        if let Some(via) = &self.via {
            write!(f, " via {via}")?;
        }
        Ok(())
    }
}

/// 1 to `max` bytes of visible ASCII (`!` to `~`).
fn visible_ascii(field: &'static str, value: &str, max: usize) -> Result<(), IdentityError> {
    if value.is_empty() {
        return Err(IdentityError::Empty(field));
    }
    if value.len() > max {
        return Err(IdentityError::TooLong { field, max });
    }
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(IdentityError::Character(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn google(id: &str) -> Principal {
        Principal::new(PrincipalKind::Person, "https://accounts.google.com", id).unwrap()
    }

    #[test]
    fn wire_forms() {
        let host = Caller::host(Principal::local_uid(501));
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            json!({"principal": {"kind": "person", "issuer": "local", "id": "uid:501"},
                   "tenant": "local"})
        );
        let token = Caller::local_token(Principal::local_uid(501));
        assert_eq!(serde_json::to_value(&token).unwrap()["via"], "token");
        let edge = Caller::asserted(google("1234"), Tenant::new("acme-1").unwrap()).unwrap();
        let value = serde_json::to_value(&edge).unwrap();
        assert_eq!(
            value,
            json!({"principal": {"kind": "person", "issuer": "https://accounts.google.com",
                                 "id": "1234"},
                   "tenant": "acme-1", "via": "edge"})
        );
        assert_eq!(serde_json::from_value::<Caller>(value).unwrap(), edge);
        assert_eq!(
            serde_json::to_value(Principal::runtime()).unwrap(),
            json!({"kind": "service", "issuer": "local", "id": "runtime"})
        );
    }

    #[test]
    fn an_unknown_kind_reads_but_is_never_asserted() {
        let principal: Principal =
            serde_json::from_value(json!({"kind": "robot", "issuer": "x", "id": "y"})).unwrap();
        assert_eq!(principal.kind(), PrincipalKind::Unknown);
        assert_eq!(
            Caller::asserted(principal, Tenant::new("t").unwrap()),
            Err(IdentityError::UnknownKind)
        );
    }

    #[test]
    fn tenants_are_validated() {
        for good in [
            "a",
            "0",
            "acme",
            "acme-1.eu_west",
            &"x".repeat(MAX_TENANT_BYTES),
        ] {
            assert!(Tenant::new(good).is_ok(), "{good}");
        }
        let long = "x".repeat(MAX_TENANT_BYTES + 1);
        for (bad, error) in [
            ("", IdentityError::Empty("tenant")),
            (
                long.as_str(),
                IdentityError::TooLong {
                    field: "tenant",
                    max: MAX_TENANT_BYTES,
                },
            ),
            ("Acme", IdentityError::Character("tenant")),
            ("-acme", IdentityError::Character("tenant")),
            (".", IdentityError::Character("tenant")),
            ("a/b", IdentityError::Character("tenant")),
            ("a b", IdentityError::Character("tenant")),
            ("a\nb", IdentityError::Character("tenant")),
            ("é", IdentityError::Character("tenant")),
            ("local", IdentityError::Reserved("tenant")),
            ("system", IdentityError::Reserved("tenant")),
        ] {
            assert_eq!(Tenant::new(bad), Err(error), "{bad:?}");
        }
        // Stored and wire values read back, reserved ones included.
        for reserved in ["local", "system"] {
            let tenant: Tenant = serde_json::from_value(json!(reserved)).unwrap();
            assert!(tenant.is_reserved());
        }
        assert!(serde_json::from_value::<Tenant>(json!("A/B")).is_err());
    }

    #[test]
    fn principal_parts_are_validated() {
        let long = "x".repeat(MAX_PRINCIPAL_PART_BYTES + 1);
        for (issuer, id) in [
            ("", "1"),
            ("i", ""),
            ("i", "a b"),
            ("i", "a\u{7f}"),
            ("i", "ü"),
            (long.as_str(), "1"),
            ("i", long.as_str()),
        ] {
            assert!(Principal::new(PrincipalKind::Person, issuer, id).is_err());
            assert!(serde_json::from_value::<Principal>(
                json!({"kind": "person", "issuer": issuer, "id": id})
            )
            .is_err());
        }
        assert!(serde_json::from_value::<Via>(json!("a b")).is_err());
    }

    #[test]
    fn an_assertion_cannot_claim_a_reserved_identity() {
        let tenant = Tenant::new("acme").unwrap();
        assert_eq!(
            Caller::asserted(Principal::local_uid(0), tenant.clone()),
            Err(IdentityError::Reserved("principal issuer"))
        );
        assert_eq!(
            Caller::asserted(Principal::runtime(), tenant),
            Err(IdentityError::Reserved("principal issuer"))
        );
        for reserved in [Tenant::local(), Tenant::system()] {
            assert_eq!(
                Caller::asserted(google("1"), reserved),
                Err(IdentityError::Reserved("tenant"))
            );
        }
    }

    #[test]
    fn errors_do_not_echo_the_value() {
        let secret = "s3cr3t value";
        let error = Tenant::new(secret).unwrap_err();
        assert!(!error.to_string().contains("s3cr3t"));
    }

    #[tokio::test]
    async fn the_current_caller_is_scoped_to_the_task() {
        assert_eq!(Caller::current(), None);
        let caller = Caller::host(Principal::local_uid(7));
        let seen = caller.clone().scope(async { Caller::current() }).await;
        assert_eq!(seen, Some(caller.clone()));
        // Lost across a spawn: the Runtime must capture it at entry.
        let spawned = caller
            .scope(async { tokio::spawn(async { Caller::current() }).await.unwrap() })
            .await;
        assert_eq!(spawned, None);
        assert_eq!(Caller::current(), None);
    }
}
