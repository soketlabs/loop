//! Who may reach a Session (task I3, ADR-0009, ADR-0007 §7.2).
//!
//! The store checks the tenant of every read; this module decides, above the
//! store, which caller may use which Session. [`decide`] is the one rule.
//! Every Runtime entry point asks it, through `Runtime::check` and the
//! filters next to it, before anything else about the Session (loaded or
//! not, busy, closing), so a refusal looks the same as a Session that does
//! not exist.
//!
//! The rules (see `docs/runtime.md`, "Who can reach a Session"):
//!
//! - [`Actor::Internal`] (work no connection asked for) may do everything.
//! - The host's user ([`Caller::is_host_user`]) may do everything to a
//!   Session of the `local` tenant, and only [`Need::List`] and
//!   [`Need::Close`] to a Session of another tenant (the operator).
//! - A user from outside the host may do everything to a Session of its own
//!   tenant whose owner of record is the same principal, and nothing to any
//!   other Session. There is no sharing yet.
//! - No owner of record: host-only. A Subagent's [`Subject`] is its root
//!   parent's.
use loop_protocol::{rpc::methods, BackendError, Caller, Principal, SessionKey, Tenant};

/// The right a request needs on its Session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Need {
    /// Appear in the Session lists and counts.
    List,
    /// Read its contents: attach, load, transcript, tree, export, a fork's
    /// source.
    Read,
    /// Act in it: prompt, cancel, change its config, answer its Escalations.
    Write,
    /// Close it.
    Close,
}

/// Who asks. Built once per request from the connection's caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// Work no connection asked for: the Runtime's own tasks (recovery,
    /// unloads, Subagents, workflows) and code that links the Runtime in
    /// process. Never a remote connection: the server refuses every request
    /// of a remote connection that has no caller.
    Internal,
    /// The caller of a connection.
    Caller(Caller),
}

impl Actor {
    /// The actor of the request served on this task. The one place where
    /// "no caller in scope" becomes [`Actor::Internal`]: the server puts
    /// every connection's caller in scope, and refuses a remote connection
    /// that has none, so no caller means no connection.
    pub fn current() -> Self {
        Self::of(Caller::current())
    }

    /// The actor for an explicit caller (`None`: in process).
    pub fn of(caller: Option<Caller>) -> Self {
        caller.map_or(Self::Internal, Self::Caller)
    }

    /// A user from outside the host: an Edge Gateway assertion.
    pub fn is_outside(&self) -> bool {
        matches!(self, Self::Caller(caller) if !caller.is_host_user())
    }
}

/// What decides access to one Session: its tenant and the owner of record
/// of its root Session (a Subagent's root parent; else itself). Neither
/// changes in a Session's life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    tenant: Tenant,
    owner: Option<Principal>,
}

impl Subject {
    /// A Session of `tenant` owned by `owner`.
    pub fn owned(tenant: Tenant, owner: Principal) -> Self {
        Self {
            tenant,
            owner: Some(owner),
        }
    }

    /// A Session of `tenant` with no owner of record: host-only.
    pub fn unowned(tenant: Tenant) -> Self {
        Self {
            tenant,
            owner: None,
        }
    }

    /// The Session's tenant.
    pub fn tenant(&self) -> &Tenant {
        &self.tenant
    }

    /// The owner of record of the root Session, if any.
    pub fn owner(&self) -> Option<&Principal> {
        self.owner.as_ref()
    }
}

/// Whether `actor` may do what needs `need` to the Session `subject`
/// describes. The only access rule of the Runtime.
pub fn decide(actor: &Actor, subject: &Subject, need: Need) -> bool {
    match actor {
        Actor::Internal => true,
        // The host's user: its own tenant fully; another tenant's Sessions
        // only to see and close them (operations), never to read or act in
        // them. A host principal is never compared: a uid is not stable.
        Actor::Caller(caller) if caller.is_host_user() => {
            subject.tenant.as_str() == Tenant::LOCAL || matches!(need, Need::List | Need::Close)
        }
        // An outside user: its own Sessions only, in its own tenant.
        Actor::Caller(caller) => {
            &subject.tenant == caller.tenant()
                && subject
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.same_principal(caller.principal()))
        }
    }
}

/// The refusal for a request [`decide`] refused: the same error as for a
/// Session that does not exist, for every caller, so a refusal tells an
/// outside user nothing.
pub fn refusal(key: &SessionKey) -> BackendError {
    BackendError::SessionNotFound(key.clone())
}

/// What a method targets and the right it needs. The table in
/// `docs/runtime.md` ("Who can reach a Session") says the same in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The connection itself (`initialize`, `shutdown`): no Session.
    Connection,
    /// Creates a Session owned by the caller in its tenant; the folder rule
    /// applies to an outside user.
    Create,
    /// One named Session, with this right.
    Session(Need),
    /// One named Session: [`Need::Read`] to read, [`Need::Write`] to change.
    ReadOrWrite,
    /// A copy: [`Need::Read`] on the source; the copy is the caller's.
    Copy,
    /// Every Session, filtered to those with this right.
    Filter(Need),
    /// A workspace (a folder): the workspace rule.
    Workspace,
    /// The Runtime's own status: reduced for an outside user.
    Runtime,
    /// Host-only: served only on the host's Unix socket.
    HostOnly,
    /// Stops this connection's own feed of a Session: needs nothing.
    Detach,
    /// Sent to the client: only for Sessions the connection may read
    /// (updates) or write (Escalations).
    ToClient(Need),
}

/// Every method of `loop_protocol::rpc::methods`, with its [`Rule`]. A test
/// fails when a method is missing.
pub const METHODS: &[(&str, Rule)] = &[
    (methods::INITIALIZE, Rule::Connection),
    (methods::SHUTDOWN, Rule::Connection),
    (methods::SESSION_NEW, Rule::Create),
    (methods::SESSION_LOAD, Rule::Session(Need::Read)),
    (methods::SESSION_LIST, Rule::Filter(Need::List)),
    (methods::SESSION_CLOSE, Rule::Session(Need::Close)),
    (methods::SESSION_ATTACH, Rule::Session(Need::Read)),
    (methods::SESSION_DETACH, Rule::Detach),
    (methods::SESSION_TRANSCRIPT, Rule::Session(Need::Read)),
    (methods::SESSION_PROMPT, Rule::Session(Need::Write)),
    (methods::SESSION_CANCEL, Rule::Session(Need::Write)),
    (methods::SESSION_UPDATE, Rule::ToClient(Need::Read)),
    (methods::HOST_INTERACT, Rule::ToClient(Need::Write)),
    (
        methods::HOST_INTERACT_WITHDRAWN,
        Rule::ToClient(Need::Write),
    ),
    (methods::SESSION_CONFIG, Rule::Session(Need::Read)),
    (methods::SESSION_SKILL, Rule::Session(Need::Write)),
    (methods::SESSION_WORKFLOW, Rule::Session(Need::Write)),
    (methods::SESSION_TREE, Rule::Session(Need::Read)),
    (methods::SESSION_VIEW, Rule::Session(Need::Read)),
    (methods::SESSION_NAVIGATE, Rule::Session(Need::Write)),
    (methods::SESSION_RENAME, Rule::Session(Need::Write)),
    (methods::SESSION_FORK, Rule::Copy),
    (methods::SESSION_CLONE, Rule::Copy),
    (methods::SESSION_EXPORT, Rule::Session(Need::Read)),
    (methods::SESSION_IMPORT, Rule::Workspace),
    (methods::SESSION_MODELS, Rule::Session(Need::Read)),
    (methods::SESSION_SET_MODEL, Rule::Session(Need::Write)),
    (methods::SESSION_EFFORTS, Rule::Session(Need::Read)),
    (methods::SESSION_SET_EFFORT, Rule::Session(Need::Write)),
    (methods::SESSION_MODES, Rule::Session(Need::Read)),
    (methods::SESSION_SET_MODE, Rule::Session(Need::Write)),
    (methods::SESSION_COMPACT, Rule::Session(Need::Write)),
    (methods::SESSION_STATS, Rule::Session(Need::Read)),
    (methods::SESSION_SHELL, Rule::Session(Need::Write)),
    (methods::SESSION_TTFB, Rule::ReadOrWrite),
    (methods::SESSION_SANDBOX, Rule::ReadOrWrite),
    (methods::SESSION_PERMISSION_MODE, Rule::ReadOrWrite),
    (methods::WORKSPACE_SETTINGS, Rule::Workspace),
    (methods::WORKSPACE_MCP, Rule::Workspace),
    (methods::WORKSPACE_WEB_SEARCH, Rule::Workspace),
    (methods::WORKSPACE_SKILLS, Rule::Workspace),
    (methods::WORKSPACE_TEMPLATES, Rule::Workspace),
    (methods::WORKSPACE_RELOAD, Rule::Workspace),
    (methods::WORKSPACE_TRUST, Rule::Workspace),
    (methods::RUNTIME_STATUS, Rule::Runtime),
    (methods::RUNTIME_SESSIONS, Rule::Filter(Need::List)),
    (methods::RUNTIME_STOP, Rule::HostOnly),
    (methods::RUNTIME_STOP_EMPTY, Rule::HostOnly),
    (methods::RUNTIME_USAGE, Rule::HostOnly),
    (methods::RUNTIME_DEFAULTS, Rule::HostOnly),
    (methods::RUNTIME_PROVIDERS, Rule::HostOnly),
    (methods::RUNTIME_MCP_AUTH, Rule::HostOnly),
    (methods::RUNTIME_TRACING, Rule::HostOnly),
];

/// The [`Rule`] of `method`, if the table has it.
pub fn rule(method: &str) -> Option<Rule> {
    METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, rule)| *rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_protocol::PrincipalKind;

    fn edge(id: &str, tenant: &str) -> Actor {
        let principal = Principal::new(PrincipalKind::Person, "https://idp", id).unwrap();
        Actor::Caller(Caller::asserted(principal, Tenant::new(tenant).unwrap()).unwrap())
    }
    fn host() -> Actor {
        Actor::Caller(Caller::host(Principal::local_uid(501)))
    }
    fn token() -> Actor {
        Actor::Caller(Caller::local_token(Principal::local_uid(501)))
    }
    fn owned(id: &str, tenant: &str) -> Subject {
        Subject::owned(
            Tenant::new(tenant).unwrap(),
            Principal::new(PrincipalKind::Person, "https://idp", id).unwrap(),
        )
    }
    const ALL: [Need; 4] = [Need::List, Need::Read, Need::Write, Need::Close];

    /// Every method of `rpc.rs` has a rule: a new method without one fails.
    #[test]
    fn every_protocol_method_is_classified() {
        let source = include_str!("../../core/loop-protocol/src/rpc.rs");
        let start = source.find("pub mod methods").unwrap();
        let module = &source[start..];
        let end = module.find("\n}\n").unwrap();
        let mut names = Vec::new();
        for line in module[..end].lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("pub const ") {
                if rest.contains(": &str") {
                    let value = rest.split('"').nth(1).unwrap();
                    names.push(value.to_owned());
                }
            }
        }
        assert!(names.len() > 40, "parsed only {names:?}");
        let missing: Vec<_> = names.iter().filter(|n| rule(n).is_none()).collect();
        assert!(
            missing.is_empty(),
            "methods with no access rule: {missing:?}"
        );
        assert_eq!(METHODS.len(), names.len(), "a rule names an unknown method");
        // Host-only in the protocol is host-only here, and only those.
        for (name, rule) in METHODS {
            assert_eq!(
                methods::HOST_ONLY.contains(name),
                *rule == Rule::HostOnly,
                "{name}"
            );
        }
    }

    #[test]
    fn internal_work_may_do_everything() {
        for need in ALL {
            assert!(decide(&Actor::Internal, &owned("a", "acme"), need));
            assert!(decide(
                &Actor::Internal,
                &Subject::unowned(Tenant::local()),
                need
            ));
        }
    }

    #[test]
    fn the_host_user_keeps_the_local_tenant_and_operates_others() {
        let local = Subject::unowned(Tenant::local());
        let mine = Subject::owned(Tenant::local(), Principal::local_uid(501));
        let other_uid = Subject::owned(Tenant::local(), Principal::local_uid(7));
        for actor in [host(), token()] {
            for need in ALL {
                assert!(decide(&actor, &local, need));
                assert!(decide(&actor, &mine, need));
                // A local uid is not compared: it is not stable.
                assert!(decide(&actor, &other_uid, need));
            }
            let theirs = owned("alice", "acme");
            assert!(decide(&actor, &theirs, Need::List));
            assert!(decide(&actor, &theirs, Need::Close));
            assert!(!decide(&actor, &theirs, Need::Read));
            assert!(!decide(&actor, &theirs, Need::Write));
        }
    }

    #[test]
    fn an_outside_user_reaches_only_its_own_sessions() {
        let alice = edge("alice", "acme");
        for need in ALL {
            assert!(decide(&alice, &owned("alice", "acme"), need));
            // Another principal of the same tenant: no sharing yet.
            assert!(!decide(&alice, &owned("bob", "acme"), need));
            // The same id in another tenant is another Session's owner.
            assert!(!decide(&alice, &owned("alice", "zeta"), need));
            // No owner of record: host-only.
            assert!(!decide(
                &alice,
                &Subject::unowned(Tenant::new("acme").unwrap()),
                need
            ));
            assert!(!decide(&alice, &Subject::unowned(Tenant::local()), need));
            // The host's own Sessions.
            assert!(!decide(
                &alice,
                &Subject::owned(Tenant::local(), Principal::local_uid(501)),
                need
            ));
        }
        // Another issuer with the same id is another principal.
        let other_issuer = Subject::owned(
            Tenant::new("acme").unwrap(),
            Principal::new(PrincipalKind::Person, "https://other", "alice").unwrap(),
        );
        assert!(!decide(&alice, &other_issuer, Need::Read));
    }

    #[test]
    fn a_refusal_is_the_not_found_error() {
        let key = SessionKey::from("s");
        assert_eq!(refusal(&key), BackendError::SessionNotFound(key));
    }
}
