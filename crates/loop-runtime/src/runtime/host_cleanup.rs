//! Ending the processes of host Workspaces
//!
//! A host Workspace runs streaming processes. The Runtime stops them where
//! the resources of a workspace end their life: the last Session of a
//! workspace closing (stop; the workspace restarts with its next Session),
//! a reload (the replaced generation is destroyed when its last holder lets
//! go) and Runtime shutdown (destroy, then a process-wide kill as the final
//! net). Every wait is bounded; a stuck process is logged, never waited for.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_agent::harness::sandbox::{kill_all_host_processes, SandboxStatus, Workspace};

use super::{LiveSession, Runtime};

/// How long one `stop` or `destroy` of a host Workspace may take.
const HOST_STOP_LIMIT: Duration = Duration::from_secs(5);

/// How long the final process-wide kill waits for the reaps.
const KILL_ALL_LIMIT: Duration = Duration::from_secs(5);

/// Stop (or destroy) `workspace` and so every process of it, waiting at
/// most [`HOST_STOP_LIMIT`]. A workspace that is dropped later kills what
/// is left; the Runtime shutdown also has the process-wide kill.
pub(super) async fn end_processes(workspace: &Arc<dyn Workspace>, destroy: bool, what: &str) {
    let ended = if destroy {
        tokio::time::timeout(HOST_STOP_LIMIT, workspace.destroy()).await
    } else {
        tokio::time::timeout(HOST_STOP_LIMIT, workspace.stop()).await
    };
    match ended {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("Runtime: could not stop host processes ({what}): {error}"),
        Err(_) => eprintln!(
            "Runtime: host processes ({what}) still ending after {}s; leaving them",
            HOST_STOP_LIMIT.as_secs()
        ),
    }
}

impl Runtime {
    /// The host Workspace a Session uses now: the one of its current
    /// generation, which a reload may have replaced since it was opened.
    pub(super) fn host_workspace_of(live: &LiveSession) -> Arc<dyn Workspace> {
        let current = live
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|generation| Arc::clone(&generation.resources.host_workspace));
        current.unwrap_or_else(|| Arc::clone(&live.opened.workspace.host_workspace))
    }

    /// Make sure the host Workspace of a Session that is about to be
    /// published runs: the last Session of the folder may have stopped it.
    /// The caller holds `host_gate`.
    pub(super) async fn start_host_workspace(&self, live: &LiveSession) {
        let workspace = Self::host_workspace_of(live);
        if workspace.status() == SandboxStatus::Ready {
            return;
        }
        if let Err(error) = workspace.start().await {
            eprintln!("Runtime: could not start the host workspace: {error}");
        }
    }

    /// Stop the processes of `cwd`'s host Workspace when no Session uses
    /// the folder any more. A Runtime that is stopping does it for all
    /// workspaces at once. The caller holds `host_gate`.
    pub(super) async fn stop_host_if_unused(&self, cwd: &Path) {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let used = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .any(|live| live.opened.workspace.cwd == cwd);
        if used {
            return;
        }
        let workspace = self
            .workspaces
            .lock()
            .unwrap()
            .get(cwd)
            .map(|state| Arc::clone(&state.resources.host_workspace));
        if let Some(workspace) = workspace {
            end_processes(&workspace, false, "last Session closed").await;
        }
    }

    /// Runtime shutdown: destroy the host Workspace of every loaded
    /// workspace, then kill whatever host process is left anywhere in this
    /// process. Bounded; it logs what it could not end.
    pub(super) async fn end_all_host_processes(&self, workspaces: &[Arc<dyn Workspace>]) {
        futures::future::join_all(
            workspaces
                .iter()
                .map(|workspace| end_processes(workspace, true, "Runtime shutdown")),
        )
        .await;
        let started = Instant::now();
        let net = tokio::task::spawn_blocking(|| kill_all_host_processes(KILL_ALL_LIMIT));
        match tokio::time::timeout(KILL_ALL_LIMIT + Duration::from_secs(1), net).await {
            Ok(Ok(())) if started.elapsed() < KILL_ALL_LIMIT => {}
            Ok(Ok(())) => eprintln!(
                "Runtime shutdown: host processes were still ending after {}s",
                KILL_ALL_LIMIT.as_secs()
            ),
            Ok(Err(error)) => eprintln!("Runtime shutdown: host process kill failed: {error}"),
            Err(_) => eprintln!("Runtime shutdown: host process kill did not return in time"),
        }
    }

    /// The host Workspace of a loaded workspace. For tests of other crates
    /// and tools that need to start a process in it.
    #[doc(hidden)]
    pub fn loaded_host_workspace(&self, cwd: &Path) -> Option<Arc<dyn Workspace>> {
        self.workspaces
            .lock()
            .unwrap()
            .get(cwd)
            .map(|state| Arc::clone(&state.resources.host_workspace))
    }
}
