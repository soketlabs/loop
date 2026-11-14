//! Committed update publication: the pump, the tail buffer and the broadcast.
use super::leases::{renew_loop, RenewEnd, RENEW_GIVE_UP, RENEW_INTERVAL};
use super::*;

impl Runtime {
    /// Recent committed updates; the Event Log remains the replay authority.
    pub fn tail(&self, session: &SessionRef) -> BackendResult<Vec<UpdateEnvelope>> {
        Ok(self
            .route(session)?
            .tail
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect())
    }
}

pub(super) async fn barrier(live: &LiveSession) -> BackendResult<()> {
    let (tx, rx) = oneshot::channel();
    let result = match live.commands.send(Command::Barrier(tx)) {
        Ok(()) => rx.await.map_err(failed),
        Err(error) => Err(failed(error)),
    };
    if let Some(error) = live.failure.lock().unwrap().clone() {
        return Err(failed(error));
    }
    result
}
/// A turn or command of `identity` starts: the pump commits everything the
/// backend sent before this point with its own caller, then stamps the
/// updates that follow with `identity`. Returns after the switch.
pub(super) async fn act(live: &LiveSession, identity: Identity) -> BackendResult<()> {
    let (tx, rx) = oneshot::channel();
    let result = match live.commands.send(Command::Act(identity, tx)) {
        Ok(()) => rx.await.map_err(failed),
        Err(error) => Err(failed(error)),
    };
    if let Some(error) = live.failure.lock().unwrap().clone() {
        return Err(failed(error));
    }
    result
}
/// Commit one update of `live`, then publish it. `cause` is who caused it
/// when the Runtime made it for a request; without one, the Session's
/// sequencer names who caused it. The sequencer then moves its high-water
/// mark and its in-progress snapshot, under the same lock, before the
/// update is sent.
pub(super) async fn publish(
    live: &LiveSession,
    envelope: UpdateEnvelope,
    cause: Option<Identity>,
    log: &dyn EventLog,
    generation: u64,
    updates: &broadcast::Sender<UpdateEnvelope>,
) -> BackendResult<()> {
    let ts_ms = now();
    let event = live
        .announced
        .lock()
        .unwrap()
        .seq
        .record(envelope, cause, generation, ts_ms);
    // Replayed history is never stored again, nor published.
    let Some(event) = event else {
        return Ok(());
    };
    let committed = log.append_batch(vec![event]).await.map_err(failed)?;
    touch(&live.activity, &live.reference.key, ts_ms);
    if let Some(error) = live.failure.lock().unwrap().clone() {
        return Err(failed(error));
    }
    for record in committed {
        if let EventKind::Update(envelope) = record.event.kind {
            {
                let mut announced = live.announced.lock().unwrap();
                announced.seq.committed(envelope.seq, &envelope.update);
                match &envelope.update {
                    SessionUpdate::ConfigChanged(changed) => {
                        announced.commit(envelope.seq, changed)
                    }
                    // A branch change advances the one Session revision too.
                    SessionUpdate::BranchReset { revision, .. } => {
                        announced.revision = announced.revision.max(*revision);
                        announced.latest.revision = announced.latest.revision.max(*revision);
                    }
                    _ => {}
                }
            }
            let mut tail = live.tail.lock().unwrap();
            if tail.len() == 256 {
                tail.pop_front();
            }
            tail.push_back(envelope.clone());
            let _ = updates.send(envelope);
        }
    }
    Ok(())
}
/// Commit what the backend has sent so far (a barrier).
async fn drain(
    live: &LiveSession,
    receiver: &mut broadcast::Receiver<UpdateEnvelope>,
    log: &dyn EventLog,
    generation: u64,
    updates: &broadcast::Sender<UpdateEnvelope>,
) -> BackendResult<()> {
    loop {
        match receiver.try_recv() {
            Ok(envelope) => publish(live, envelope, None, log, generation, updates).await?,
            Err(broadcast::error::TryRecvError::Empty) => return Ok(()),
            Err(error) => {
                return Err(failed(format!(
                    "Session update pump lost activity: {error}"
                )))
            }
        }
    }
}
pub(super) fn fail(live: &LiveSession, error: impl std::fmt::Display) {
    *live.failure.lock().unwrap() = Some(error.to_string());
    // Nothing is in progress any more.
    live.announced
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .seq
        .reset();
    live.cancel.send_modify(|n| *n += 1);
    live.opened.native.cancel(&live.reference);
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn pump(
    live: Arc<LiveSession>,
    mut receiver: broadcast::Receiver<UpdateEnvelope>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    log: Arc<dyn EventLog>,
    generation: u64,
    updates: broadcast::Sender<UpdateEnvelope>,
    store: Arc<dyn RuntimeSessionStore>,
    identity: LeaseIdentity,
) {
    // Lease maintenance must not wait behind Event Log backpressure.
    // Dropping the JoinSet stops renewal when the pump drains or fails.
    let mut renewals = tokio::task::JoinSet::new();
    let renewing = Arc::downgrade(&live);
    let id = live.reference.native_id.clone();
    let key = live.reference.key.clone();
    let renew_log = Arc::clone(&log);
    // A lost lease is the Runtime's own record.
    let lost_identity = live.runtime_identity();
    renewals.spawn(async move {
        let end = renew_loop(
            || store.renew_lease(&id, &identity),
            RENEW_INTERVAL,
            RENEW_GIVE_UP,
        )
        .await;
        let (state, reason) = match end {
            RenewEnd::Lost(reason) => ("lost", reason),
            RenewEnd::Stale(reason) => ("stale", reason),
        };
        eprintln!("Runtime: lease of Session {id} is {state}: {reason}");
        if let Some(live) = renewing.upgrade() {
            fail(&live, format!("lease {state}: {reason}"));
        }
        let _ = renew_log
            .append_batch(vec![Event {
                generation,
                session_key: Some(key),
                turn: None,
                ts_ms: now(),
                kind: EventKind::Cancelled(
                    json!({"reason": format!("lease {state}"), "detail": reason}),
                ),
                identity: lost_identity,
            }])
            .await;
    });
    loop {
        let result: BackendResult<()> = tokio::select! {
            envelope = receiver.recv() => match envelope {
                Ok(envelope) => publish(&live, envelope, None, log.as_ref(), generation, &updates).await,
                Err(error) => Err(failed(format!("Session update pump lost activity: {error}"))),
            },
            _ = renewals.join_next() => return,
            command = commands.recv() => {
                let stop = matches!(&command, Some(Command::Stop(_)));
                match command {
                    Some(Command::Publish(update, identity, tx)) => {
                        let envelope = UpdateEnvelope::live(live.reference.instance.clone(), generation, live.reference.key.clone(), None, 0, *update);
                        let result = publish(&live, envelope, Some(identity), log.as_ref(), generation, &updates).await;
                        let _ = tx.send(result.clone());
                        result
                    }
                    Some(Command::Cancel(by)) => {
                        let result = log.append_batch(vec![Event { generation, session_key: Some(live.reference.key.clone()), turn: None, ts_ms: now(), kind: EventKind::Cancelled(json!({})), identity: live.identity(&by) }]).await.map_err(failed);
                        live.cancel.send_modify(|n| *n += 1);
                        live.opened.native.cancel(&live.reference);
                        let withdrawn = live.escalations.withdraw("turn cancelled", false, &by).await.map_err(failed);
                        result.map(|_| ()).and(withdrawn)
                    }
                    Some(Command::Barrier(tx) | Command::Stop(tx)) => {
                        let result = drain(&live, &mut receiver, log.as_ref(), generation, &updates).await;
                        if let Err(error) = &result { fail(&live, error); }
                        let _ = tx.send(());
                        if stop { return; }
                        result
                    }
                    Some(Command::Act(identity, tx)) => {
                        // Everything the backend sent before the switch keeps
                        // the caller it was sent under.
                        let result = drain(&live, &mut receiver, log.as_ref(), generation, &updates).await;
                        live.announced.lock().unwrap().seq.act(identity);
                        if let Err(error) = &result { fail(&live, error); }
                        let _ = tx.send(());
                        result
                    }
                    None => return,
                }
            }
        };
        if let Err(error) = result {
            fail(&live, error);
            return;
        }
    }
}
