//! The Runtime side of the access rules ([`crate::access`]): find the
//! [`Subject`] of a Session and ask [`decide`]. Every entry point calls
//! [`Runtime::check`] (or a filter here) before it reads anything else about
//! the Session.
use super::*;
use crate::access::{decide, refusal, Actor, Need, Subject};

/// The deepest Subagent nesting followed to a root; deeper is host-only.
const MAX_LINEAGE: usize = 64;

impl Runtime {
    /// Refuse the request served on this task unless its caller has `need`
    /// on `session`. A refusal is [`BackendError::SessionNotFound`].
    pub(crate) async fn check(&self, session: &SessionRef, need: Need) -> BackendResult<()> {
        self.check_as(&Actor::current(), &session.key, need).await
    }

    /// [`Runtime::check`] for an explicit actor.
    pub(crate) async fn check_as(
        &self,
        actor: &Actor,
        key: &SessionKey,
        need: Need,
    ) -> BackendResult<()> {
        if self.may(actor, key, need).await? {
            Ok(())
        } else {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            Err(refusal(key))
        }
    }

    /// How many requests the access check refused. For tests and probes.
    #[doc(hidden)]
    pub fn access_refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    /// Whether `actor` has `need` on Session `key`. Internal work and the
    /// host's user never read the Event Log here: their rule needs only the
    /// tenant. An outside user reads an owner only for a key of its own
    /// tenant, once (then from the cache).
    pub(crate) async fn may(
        &self,
        actor: &Actor,
        key: &SessionKey,
        need: Need,
    ) -> BackendResult<bool> {
        let subject = match actor {
            Actor::Internal => return Ok(true),
            Actor::Caller(caller) => {
                let tenant = self.session_tenant(key);
                if caller.is_host_user() || &tenant != caller.tenant() {
                    Subject::unowned(tenant)
                } else {
                    self.subject(key).await?
                }
            }
        };
        Ok(decide(actor, &subject, need))
    }

    /// Whether `actor` has `need` on the loaded Session `live`. No I/O.
    pub(super) fn may_live(actor: &Actor, live: &LiveSession, need: Need) -> bool {
        decide(actor, &live.access, need)
    }

    /// The access subject of Session `key`: a loaded Session's own; else its
    /// root's owner of record, read from the Event Log in its tenant. A key
    /// the Event Log does not know (no records: made outside the Runtime,
    /// or no such Session) has no owner, in the `local` tenant.
    pub(crate) async fn subject(&self, key: &SessionKey) -> BackendResult<Subject> {
        let mut current = key.clone();
        for _ in 0..MAX_LINEAGE {
            if let Some(live) = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&current)
            {
                return Ok(live.access.clone());
            }
            if let Some(known) = self
                .subjects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&current)
            {
                return Ok(known.clone());
            }
            let Some(tenant) = self
                .tenants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&current)
                .cloned()
            else {
                return Ok(Subject::unowned(Tenant::local()));
            };
            let owner = self
                .log
                .session_owner(&tenant, &current)
                .await
                .map_err(failed)?;
            let subject = match owner {
                // A Subagent: its root parent decides. Forks record a parent
                // too, but only a Subagent's records name an `actor`, and only
                // the Event Log's Subagent list confirms it.
                Some(owner) if owner.actor.is_some() => {
                    let parent = owner.actor.clone().unwrap_or_default();
                    if self.subagent_keys().await?.contains(&current) {
                        current = SessionKey::from(parent.as_str());
                        continue;
                    }
                    Subject::unowned(tenant)
                }
                Some(owner) => Subject::owned(tenant, owner.principal),
                None => Subject::unowned(tenant),
            };
            self.subjects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key.clone(), subject.clone());
            return Ok(subject);
        }
        Ok(Subject::unowned(self.session_tenant(key)))
    }

    /// The folder rule for an outside user's Session in `cwd` (other than
    /// `own`, the Session being opened): refused when `cwd` is the folder
    /// of a Session the caller may not read, or is inside or above one. The
    /// tool lock lets a Session read its whole folder. The host's user and
    /// internal work are not limited.
    pub(crate) async fn check_folder(
        &self,
        actor: &Actor,
        cwd: &std::path::Path,
        own: Option<&SessionKey>,
    ) -> BackendResult<()> {
        if !actor.is_outside() {
            return Ok(());
        }
        let refused = || {
            BackendError::Forbidden(format!(
                "a Session in {} (another user's Session uses this folder, a folder \
                 inside it or above it)",
                cwd.display()
            ))
        };
        let cwd = tokio::fs::canonicalize(cwd)
            .await
            .unwrap_or_else(|_| cwd.to_path_buf());
        let overlaps = |other: &std::path::Path| cwd.starts_with(other) || other.starts_with(&cwd);
        let mut folders: Vec<(SessionKey, PathBuf)> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|live| {
                (
                    live.reference.key.clone(),
                    live.opened.workspace.cwd.clone(),
                )
            })
            .collect();
        let known = self
            .factory
            .globals
            .repository
            .list(None)
            .await
            .map_err(failed)?;
        folders.extend(known.into_iter().filter_map(|meta| {
            let cwd = PathBuf::from(meta.cwd?);
            Some((reference(&meta.id).key, cwd))
        }));
        for (key, folder) in folders {
            if Some(&key) == own || !overlaps(&folder) {
                continue;
            }
            if !self.may(actor, &key, Need::Read).await? {
                return Err(refused());
            }
        }
        Ok(())
    }

    /// The folders an actor may name by workspace id: for an outside user,
    /// only the folders of loaded Sessions it may read; `None` for everyone
    /// else (no limit).
    pub(crate) fn readable_folders(&self, actor: &Actor) -> Option<Vec<PathBuf>> {
        if !actor.is_outside() {
            return None;
        }
        Some(
            self.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter(|live| Self::may_live(actor, live, Need::Read))
                .map(|live| live.opened.workspace.cwd.clone())
                .collect(),
        )
    }

    /// Whether `actor` may change workspace `cwd` as a whole (a reload): for
    /// an outside user, every loaded Session in it must be its own.
    pub(crate) fn may_change_folder(&self, actor: &Actor, cwd: &std::path::Path) -> bool {
        if !actor.is_outside() {
            return true;
        }
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|live| live.opened.workspace.cwd == cwd)
            .all(|live| Self::may_live(actor, live, Need::Write))
    }
}
