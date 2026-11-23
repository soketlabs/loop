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
//! Two rules for code that uses these values:
//!
//! - Authorization compares identities with [`Principal::same_principal`]
//!   (issuer and id) and [`Caller::same_identity`] (plus the tenant), never
//!   with `==`: `==` also compares the kind and `via`, which describe how a
//!   principal was seen, not who it is.
//! - A principal id is chosen by an outside identity provider: `../x`,
//!   `a,b` and `%2F` are valid ids. Never put one in a file path or a
//!   storage key as it is; hash it. A [`Tenant`] is safe in a path.
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
/// Longest principal kind a newer peer may send, in bytes.
pub const MAX_KIND_BYTES: usize = 64;

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

/// What kind of principal acts. On the wire, a string: `person`, `service`,
/// or a kind a newer peer knows, which is kept as it was sent
/// ([`PrincipalKind::Unknown`]) so a record read and written again does not
/// change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    /// A person.
    Person,
    /// A Service Identity: unattended work, or the Runtime itself.
    Service,
    /// A kind this reader does not know, with its original text (1 to 64
    /// visible ASCII characters). Read from a newer peer; never accepted
    /// from an Edge Gateway assertion.
    Unknown(String),
}

impl PrincipalKind {
    /// The wire text: `person`, `service`, or the unknown kind as it came.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Person => "person",
            Self::Service => "service",
            Self::Unknown(kind) => kind,
        }
    }

    /// A kind from its wire text. `person` and `service` are known; any
    /// other valid text is [`PrincipalKind::Unknown`].
    pub fn parse(value: &str) -> Result<Self, IdentityError> {
        Ok(match value {
            "person" => Self::Person,
            "service" => Self::Service,
            other => {
                visible_ascii("principal kind", other, MAX_KIND_BYTES)?;
                Self::Unknown(other.to_owned())
            }
        })
    }
}

impl fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for PrincipalKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PrincipalKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        PrincipalKind::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for PrincipalKind {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PrincipalKind".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "What kind of principal acts: `person` or `service`. A reader keeps a kind it does not know as it came.",
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_KIND_BYTES,
            "pattern": "^[!-~]+$",
            "examples": ["person", "service"]
        })
    }
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
        if let PrincipalKind::Unknown(kind) = &kind {
            visible_ascii("principal kind", kind, MAX_KIND_BYTES)?;
        }
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

    /// The host's user on a platform without numeric user ids:
    /// `{ person, local, os-user }`. Distinct from every `uid:<n>`, so it is
    /// never mistaken for root (`uid:0`).
    pub fn local_os_user() -> Self {
        Self {
            kind: PrincipalKind::Person,
            issuer: LOCAL_ISSUER.into(),
            id: "os-user".into(),
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
    pub fn kind(&self) -> &PrincipalKind {
        &self.kind
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

    /// The same principal: the same `issuer` and `id`. This is the identity
    /// comparison for authorization. `==` also compares the kind, which a
    /// newer peer may report differently for the same principal, so it must
    /// not decide ownership.
    pub fn same_principal(&self, other: &Principal) -> bool {
        self.issuer == other.issuer && self.id == other.id
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
/// `a-z`, `0-9`, `.`, `_` and `-`, starting with a letter or digit and not
/// ending with `.`. Lower case only, so two tenants never share a key on a
/// case-insensitive file system; no `/`, no trailing dot and no Windows
/// device name (`con`, `prn`, `aux`, `nul`, `com0`-`com9`, `lpt0`-`lpt9`,
/// also with an extension such as `con.x`), so a tenant is safe as a file or
/// folder name on every platform.
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
        if value.ends_with('.') {
            return Err(IdentityError::Character(FIELD));
        }
        if is_windows_device(&value) {
            return Err(IdentityError::Reserved(FIELD));
        }
        Ok(Self(value))
    }
}

/// `con`, `prn`, `aux`, `nul`, `com0`-`com9` or `lpt0`-`lpt9`, alone or
/// before a `.`: names Windows reserves for devices in every folder.
fn is_windows_device(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit())
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
            "description": "The isolation boundary data belongs to. Opaque; `local` and `system` are reserved. Windows device names (`con`, `prn`, `aux`, `nul`, `com0`-`com9`, `lpt0`-`lpt9`, also before a `.`) are refused.",
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_TENANT_BYTES,
            "pattern": "^[a-z0-9]([a-z0-9._-]*[a-z0-9_-])?$"
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
///
/// `==` compares every field, `via` included. For authorization use
/// [`Caller::same_identity`].
///
/// Reading one (serde) applies the rules of the constructor its `via` names,
/// so text from a peer can never make a caller the constructors refuse: no
/// `via` or `token` is the host's user ([`Caller::host`],
/// [`Caller::local_token`]: `local` issuer and tenant); `edge` is an
/// assertion ([`Caller::asserted`]: never `local`, never a reserved tenant,
/// a known kind); any other `via` is refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Caller {
    principal: Principal,
    tenant: Tenant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<Via>,
}

impl<'de> Deserialize<'de> for Caller {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Repr {
            principal: Principal,
            tenant: Tenant,
            #[serde(default)]
            via: Option<Via>,
        }
        let Repr {
            principal,
            tenant,
            via,
        } = Repr::deserialize(deserializer)?;
        let refused = |what: &'static str| serde::de::Error::custom(IdentityError::Reserved(what));
        let host = principal.is_local() && tenant.as_str() == Tenant::LOCAL;
        match via.as_ref().map(Via::as_str) {
            None if host => Ok(Caller::host(principal)),
            Some("token") if host => Ok(Caller::local_token(principal)),
            None | Some("token") => Err(refused("host caller")),
            Some("edge") => Caller::asserted(principal, tenant).map_err(serde::de::Error::custom),
            Some(_) => Err(refused("via")),
        }
    }
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
        if matches!(principal.kind, PrincipalKind::Unknown(_)) {
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

    /// This host's own user or the Runtime, in the `local` tenant: the
    /// identities only the host itself can give. Every other caller (an
    /// Edge Gateway assertion, or a value read from a peer) is outside the
    /// host, so code that must fail closed for outside users tests
    /// `!is_host_user()`, never the `via` string.
    pub fn is_host_user(&self) -> bool {
        self.principal.is_local() && self.tenant.as_str() == Tenant::LOCAL
    }

    /// The same principal in the same tenant: the identity comparison for
    /// authorization. Unlike `==` it ignores `via` (how the principal came)
    /// and the principal's kind.
    pub fn same_identity(&self, other: &Caller) -> bool {
        self.principal.same_principal(&other.principal) && self.tenant == other.tenant
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
    fn an_unknown_kind_reads_back_unchanged_but_is_never_asserted() {
        let stored = json!({"kind": "robot", "issuer": "x", "id": "y"});
        let principal: Principal = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(principal.kind(), &PrincipalKind::Unknown("robot".into()));
        // Written again, the record does not change.
        assert_eq!(serde_json::to_value(&principal).unwrap(), stored);
        for known in ["person", "service"] {
            let kind: PrincipalKind = serde_json::from_value(json!(known)).unwrap();
            assert!(!matches!(kind, PrincipalKind::Unknown(_)));
            assert_eq!(serde_json::to_value(&kind).unwrap(), json!(known));
        }
        // The kept text is validated like any other part.
        for bad in [
            json!(""),
            json!("a b"),
            json!("x".repeat(MAX_KIND_BYTES + 1)),
        ] {
            assert!(serde_json::from_value::<PrincipalKind>(bad).is_err());
        }
        assert!(Principal::new(PrincipalKind::Unknown("a b".into()), "x", "y").is_err());
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
            "a.b",
            "acme-",
            "console",
            "com",
            "com10",
            "lpt",
            "auxiliary",
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
            ("acme.", IdentityError::Character("tenant")),
            ("a.", IdentityError::Character("tenant")),
            ("con", IdentityError::Reserved("tenant")),
            ("con.x", IdentityError::Reserved("tenant")),
            ("prn", IdentityError::Reserved("tenant")),
            ("aux", IdentityError::Reserved("tenant")),
            ("nul", IdentityError::Reserved("tenant")),
            ("com1", IdentityError::Reserved("tenant")),
            ("com9", IdentityError::Reserved("tenant")),
            ("lpt1", IdentityError::Reserved("tenant")),
            ("lpt9.log", IdentityError::Reserved("tenant")),
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
    fn identity_for_authorization_ignores_kind_and_via() {
        let person = google("1");
        let other_kind = Principal::new(
            PrincipalKind::Unknown("robot".into()),
            "https://accounts.google.com",
            "1",
        )
        .unwrap();
        assert!(person.same_principal(&other_kind));
        assert_ne!(person, other_kind);
        assert!(!person.same_principal(&google("2")));
        let other_issuer =
            Principal::new(PrincipalKind::Person, "https://idp.example", "1").unwrap();
        assert!(!person.same_principal(&other_issuer));

        let acme = Tenant::new("acme").unwrap();
        let edge = Caller::asserted(person.clone(), acme.clone()).unwrap();
        let as_service = Caller::asserted(
            Principal::new(PrincipalKind::Service, "https://accounts.google.com", "1").unwrap(),
            acme,
        )
        .unwrap();
        assert!(edge.same_identity(&as_service));
        assert_ne!(edge, as_service);
        assert!(
            !edge.same_identity(&Caller::asserted(person, Tenant::new("other").unwrap()).unwrap())
        );
        // The host user over the socket and over the token is one identity.
        let local = Principal::local_uid(501);
        let host = Caller::host(local.clone());
        let token = Caller::local_token(local);
        assert!(host.same_identity(&token));
        assert_ne!(host, token);
    }

    #[test]
    fn a_caller_read_from_text_follows_the_rules_of_its_via() {
        let read = |value: serde_json::Value| serde_json::from_value::<Caller>(value);
        let local = json!({"kind": "person", "issuer": "local", "id": "uid:501"});
        let outside = json!({"kind": "person", "issuer": "https://idp", "id": "alice"});
        // The host's forms read back as built.
        let host = read(json!({"principal": local, "tenant": "local"})).unwrap();
        assert_eq!(host, Caller::host(Principal::local_uid(501)));
        let token = read(json!({"principal": local, "tenant": "local", "via": "token"})).unwrap();
        assert_eq!(token, Caller::local_token(Principal::local_uid(501)));
        let edge = read(json!({"principal": outside, "tenant": "acme", "via": "edge"})).unwrap();
        assert!(!edge.is_host_user());
        // Text can never make a caller the constructors refuse.
        for forged in [
            // An outside principal claiming the host's (no via, or token).
            json!({"principal": outside, "tenant": "acme"}),
            json!({"principal": outside, "tenant": "local"}),
            json!({"principal": outside, "tenant": "acme", "via": "token"}),
            json!({"principal": local, "tenant": "acme"}),
            // An assertion of a reserved identity.
            json!({"principal": local, "tenant": "local", "via": "edge"}),
            json!({"principal": outside, "tenant": "local", "via": "edge"}),
            json!({"principal": outside, "tenant": "system", "via": "edge"}),
            json!({"principal": {"kind": "robot", "issuer": "https://idp", "id": "a"},
                   "tenant": "acme", "via": "edge"}),
            // A `via` no constructor makes.
            json!({"principal": local, "tenant": "local", "via": "device:1"}),
        ] {
            assert!(read(forged.clone()).is_err(), "{forged}");
        }
    }

    #[test]
    fn the_host_user_without_a_uid_is_not_root() {
        let user = Principal::local_os_user();
        assert!(user.is_local());
        assert!(!user.same_principal(&Principal::local_uid(0)));
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
