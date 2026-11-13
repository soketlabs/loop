//! User commands run in a Session's environment (`session/shell`).
use super::*;

impl Runtime {
    /// Run a user command in the Session's environment while it is idle.
    /// The command holds the Session busy, not its lifecycle lock, and a
    /// cancel or forced close stops it.
    pub(super) async fn shell(
        &self,
        session: &SessionRef,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> BackendResult<ShellResult> {
        let live = self.loaded(session).await?;
        // Checked before the Session is reserved.
        let timeout_ms = live.opened.native.shell_timeout(timeout_ms)?;
        let mut cancelled = live.cancel.subscribe();
        {
            let _lifecycle = live.lifecycle.lock().await;
            self.check_tree_live(&live)?;
            self.reserve(&live).await?;
        }
        // Owned by the Runtime: a dropped request still clears `busy`.
        let session = session.clone();
        let command = command.to_owned();
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                result = live.opened.native.run(&session, &command, Some(timeout_ms)) => result,
                _ = cancelled.changed() => Err(BackendError::failed(
                    ErrorCategory::Cancelled,
                    "the command was cancelled",
                )),
            };
            let _lifecycle = live.lifecycle.lock().await;
            live.busy.store(false, Ordering::Release);
            live.apply_pending().await;
            result
        });
        task.await.map_err(failed)?
    }
}
