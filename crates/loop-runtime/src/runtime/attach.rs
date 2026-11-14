//! Watchers, Escalation recipients and attachments.
use super::publish::barrier;
use super::*;
use crate::access::{Actor, Need};

impl Runtime {
    /// Connections watching `key`. For tests that must wait for a watch
    /// registered after a reply.
    #[doc(hidden)]
    pub fn watcher_count(&self, key: &SessionKey) -> usize {
        self.watchers
            .lock()
            .unwrap()
            .get(key)
            .map_or(0, HashSet::len)
    }
    /// Record whether `connection` watches `session`, and its Escalation
    /// recipient when it answers them. Any watched Session stays loaded.
    pub(super) async fn register_watch(
        &self,
        session: &SessionRef,
        connection: u64,
        attached: bool,
        recipient: Option<Arc<dyn loop_wire::control::EscalationRecipient>>,
    ) -> BackendResult<()> {
        if session.instance != self.instance || session.key.0 != session.native_id {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        // A connection watches only a Session it may read; the server drops
        // its feed when this fails. Stopping a watch needs nothing.
        if attached {
            self.check(session, Need::Read).await?;
        }
        // Escalations go only to a recipient that may act in the Session
        // (for a Subagent below it: the same root, so the same answer).
        let recipient = match recipient {
            Some(recipient)
                if attached
                    && !self
                        .may(&Actor::of(recipient.caller()), &session.key, Need::Write)
                        .await? =>
            {
                None
            }
            other => other,
        };
        self.set_watch(session, connection, attached).await;
        let mut recipients = self.recipients.lock().await;
        match &recipient {
            Some(recipient) => {
                recipients
                    .entry(session.key.clone())
                    .or_default()
                    .insert(connection, recipient.clone());
            }
            None => {
                if let Some(attached) = recipients.get_mut(&session.key) {
                    attached.remove(&connection);
                }
            }
        }
        // The Session itself and every live Subagent below it.
        let affected: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        for live in affected {
            let lineage = self.lineage(&live.reference.key);
            if !lineage.contains(&session.key) {
                continue;
            }
            let wanted = wanted(&recipients, &lineage);
            for old in live.escalations.connections().await {
                if !wanted.contains_key(&old) {
                    live.escalations.detach(old).await;
                }
            }
            for (connection, recipient) in wanted {
                live.escalations.attach(connection, recipient).await;
            }
        }
        Ok(())
    }
    /// Add or remove `connection` as a watcher of `session`, under its
    /// lifecycle lock when it is loaded: an unload either sees the watcher
    /// and keeps the Session, or has already begun. Returns whether the
    /// registry changed.
    pub(super) async fn set_watch(
        &self,
        session: &SessionRef,
        connection: u64,
        attached: bool,
    ) -> bool {
        let target = self.route(session).ok();
        let _lifecycle = match (&target, attached) {
            (Some(live), true) => Some(live.lifecycle.lock().await),
            _ => None,
        };
        let mut watchers = self.watchers.lock().unwrap();
        if attached && self.stopped.load(Ordering::Acquire) {
            return false;
        }
        self.idle_epoch.fetch_add(1, Ordering::AcqRel);
        if attached {
            watchers
                .entry(session.key.clone())
                .or_default()
                .insert(connection)
        } else {
            let removed = watchers
                .get_mut(&session.key)
                .is_some_and(|watching| watching.remove(&connection));
            if watchers.get(&session.key).is_some_and(HashSet::is_empty) {
                watchers.remove(&session.key);
            }
            removed
        }
    }
    /// `session/attach` for `connection`: protect the Session from unloading
    /// before taking the snapshot, and load it again first if it was
    /// unloaded to make room. If it cannot be loaded, the attach fails.
    pub(super) async fn attach_watched(
        &self,
        connection: u64,
        session: &SessionRef,
        after_seq: u64,
        snapshot: bool,
    ) -> BackendResult<loop_wire::Attachment> {
        if session.instance != self.instance || session.key.0 != session.native_id {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        // Before anything else about the Session: a refusal is not found.
        self.check(session, Need::Read).await?;
        let added = self.set_watch(session, connection, true).await;
        if self.stopped.load(Ordering::Acquire) {
            return Err(failed("Runtime stopped"));
        }
        let result = async {
            let unloading = self
                .route(session)
                .map_or(true, |live| live.closing.load(Ordering::Acquire));
            // A legacy Surface whose cursor reaches a branch reset cannot
            // replay through it; give it the current branch instead.
            let baseline = !snapshot && self.reset_since(session, after_seq).await?;
            let fresh = snapshot || baseline;
            if unloading && (fresh || self.was_unloaded(&session.key)) {
                // Finished Subagents have an audit stream but cannot be reopened
                // without their parent's policy and cancellation lineage.
                if self.route(session).is_err() && self.subagent_parent(session).await?.is_some() {
                    return loop_wire::RuntimeControl::attach(self, session, after_seq).await;
                }
                if let Err(error) = self.reload(session).await {
                    // A Session that cannot be loaded here (for example, owned
                    // elsewhere) keeps read-only replay; it detaches at the reset.
                    if snapshot || self.was_unloaded(&session.key) {
                        return Err(error);
                    }
                    return loop_wire::RuntimeControl::attach(self, session, after_seq).await;
                }
            }
            if !fresh {
                return loop_wire::RuntimeControl::attach(self, session, after_seq).await;
            }
            let live = self.route(session)?;
            let _lifecycle = live.lifecycle.lock().await;
            self.check_tree_live(&live)?;
            barrier(&live).await?;
            self.announce_control(&live, None).await?;
            // Under the lock already held: read the pump's committed point.
            let (at_seq, changed) = Self::announced(&live);
            let public = self.public_snapshot(session, at_seq, changed);
            let mut attachment = self.attachment(session, after_seq, public.clone()).await?;
            if snapshot {
                attachment.snapshot = Some(self.snapshot_locked(&live, public).await?);
            } else {
                attachment.baseline = self.baseline_locked(&live, attachment.latest_seq).await?;
                attachment.replay = Box::new(std::iter::empty());
            }
            Ok(attachment)
        }
        .await;
        if result.is_err() && added {
            self.set_watch(session, connection, false).await;
        }
        result
    }
    pub(super) async fn disconnect_recipients(&self, connection: u64) {
        let mut recipients = self.recipients.lock().await;
        recipients.retain(|_, attached| {
            attached.remove(&connection);
            !attached.is_empty()
        });
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        for live in sessions {
            live.escalations.detach(connection).await;
        }
    }
    /// Committed updates after `after_seq` through the snapshot's `at_seq`.
    /// Plain Event Log replay stays read-only, including closed Sessions;
    /// callers may hold the lifecycle lock.
    ///
    /// The read pages through `(after_seq, at_seq]` on reader connections and
    /// stops at `at_seq`; it never reads updates after the snapshot. The
    /// replay is still collected here, because `Attachment::replay` is a
    /// synchronous iterator.
    pub(super) async fn attachment(
        &self,
        session: &SessionRef,
        after_seq: u64,
        snapshot: SessionSnapshot,
    ) -> BackendResult<loop_wire::Attachment> {
        let mut replay = Vec::new();
        let mut after = after_seq;
        // Read in the Session's tenant, whoever attaches (access is task I3).
        let tenant = self.session_tenant(&session.key);
        while after < snapshot.at_seq {
            let page = self
                .log
                .read_session_page(
                    &tenant,
                    &session.key,
                    after,
                    Some(snapshot.at_seq),
                    loop_store::PageLimit::DEFAULT,
                )
                .await
                .map_err(failed)?;
            for record in page.items {
                if let EventKind::Update(envelope) = record.event.kind {
                    if envelope.seq <= snapshot.at_seq {
                        replay.push(envelope);
                    }
                }
            }
            match page.next {
                Some(loop_store::Cursor(next)) => after = next,
                None => break,
            }
        }
        Ok(loop_wire::Attachment {
            baseline: Vec::new(),
            latest_seq: snapshot.at_seq,
            generation: self.generation,
            replay: Box::new(replay.into_iter()),
            snapshot: Some(snapshot),
        })
    }
}
