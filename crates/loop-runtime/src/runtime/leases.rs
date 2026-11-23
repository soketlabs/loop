//! Session lease ownership: the admission lease and the renewal loop.
use super::*;

/// Lease renewals run every interval; the lease turns stale at 30 s, so give up
/// on transient errors a little before that.
pub(super) const RENEW_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const RENEW_GIVE_UP: Duration = Duration::from_secs(25);
// Own admission's lease until the pump and Session map take responsibility.
// A Surface can drop its request future at any await point during construction.
pub(super) struct OpeningLease {
    store: Arc<dyn RuntimeSessionStore>,
    identity: LeaseIdentity,
    id: String,
    pub(super) armed: bool,
}
impl OpeningLease {
    pub(super) async fn acquire(
        store: Arc<dyn RuntimeSessionStore>,
        identity: LeaseIdentity,
        id: String,
    ) -> BackendResult<Self> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            // Never a live owner: another owner's lease is replaced only when
            // its heartbeat is stale and its pid is dead (Decision 3).
            let result = store
                .takeover_lease(&id, &identity, false)
                .await
                .map_err(failed)
                .map(|()| Self {
                    store,
                    identity,
                    id,
                    armed: true,
                });
            let _ = tx.send(result);
        });
        rx.await.map_err(failed)?
    }
}
impl Drop for OpeningLease {
    fn drop(&mut self) {
        if self.armed {
            let store = self.store.clone();
            let identity = self.identity.clone();
            let id = self.id.clone();
            tokio::spawn(async move {
                if let Err(error) = store.release_lease(&id, &identity).await {
                    eprintln!("Runtime interrupted Session admission: {error}");
                }
            });
        }
    }
}
#[derive(Debug)]
pub(super) enum RenewEnd {
    /// The lease is no longer ours.
    Lost(String),
    /// Renewals kept failing for the whole give-up window.
    Stale(String),
}
/// Renew on every tick. Only a lost lease is fatal at once; transient storage
/// errors are retried until no renewal has succeeded for `give_up`.
pub(super) async fn renew_loop<F, Fut>(
    mut renew: F,
    interval: Duration,
    give_up: Duration,
) -> RenewEnd
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), SessionError>>,
{
    let mut ticker = tokio::time::interval(interval);
    let mut last_ok = tokio::time::Instant::now();
    loop {
        ticker.tick().await;
        match renew().await {
            Ok(()) => last_ok = tokio::time::Instant::now(),
            Err(SessionError::Storage(error)) => {
                eprintln!("Runtime: lease renewal failed, will retry: {error}");
                if last_ok.elapsed() >= give_up {
                    return RenewEnd::Stale(error);
                }
            }
            Err(error) => return RenewEnd::Lost(error.to_string()),
        }
    }
}
