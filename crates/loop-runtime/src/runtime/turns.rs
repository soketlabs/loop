//! Turn admission: the active-turn permit and marking a Session busy.
use super::*;

impl Runtime {
    /// A parent waiting for a Subagent releases its active-turn permit.
    pub async fn release_turn_permit(&self, session: &SessionRef) -> BackendResult<()> {
        let live = self.route(session)?;
        live.permit.lock().await.take();
        Ok(())
    }
    /// Rejoin turn admission before the parent continues executing.
    pub async fn reacquire_turn_permit(&self, session: &SessionRef) -> BackendResult<()> {
        let live = self.route(session)?;
        let mut permit = live.permit.lock().await;
        if permit.is_none() {
            let mut cancelled = live.cancel.subscribe();
            if live.closing.load(Ordering::Acquire) || !live.busy.load(Ordering::Acquire) {
                return Err(failed("Session turn ended"));
            }
            let acquired = tokio::select! {
                p = self.turns.clone().acquire_owned() => p.map_err(failed)?,
                _ = cancelled.changed() => return Err(failed("Session turn cancelled")),
            };
            if live.closing.load(Ordering::Acquire) || !live.busy.load(Ordering::Acquire) {
                return Err(failed("Session turn ended"));
            }
            *permit = Some(acquired);
        }
        Ok(())
    }
    /// Mark an idle Session busy for a turn or a user command, capturing
    /// its settled output for snapshots taken meanwhile. Caller holds the
    /// lifecycle lock and has checked the Session is open.
    pub(super) async fn reserve(&self, live: &LiveSession) -> BackendResult<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(failed("Runtime stopped"));
        }
        self.idle_epoch.fetch_add(1, Ordering::AcqRel);
        if !live.busy.load(Ordering::Acquire) {
            self.announce_control(live, None).await?;
            // The pump's committed point, without waiting on the Event
            // Log: anything still queued before this turn is a config
            // update, which transcript views leave out.
            let published = live.announced.lock().unwrap().published;
            live.turn_start_seq.store(published, Ordering::Release);
            *live.committed.lock().await =
                Some(live.opened.harness.export_session().await.map_err(failed)?);
        }
        if live
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BackendError::Busy);
        }
        Ok(())
    }
}
