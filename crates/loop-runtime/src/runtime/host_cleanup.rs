//! Ending the processes of host Workspaces
//!
//! A host Workspace runs streaming processes. The Runtime stops them where
//! the resources of a workspace end their life: the last Session of a
//! workspace closing (stop; the workspace restarts with its next Session),
//! a reload (the replaced generation is destroyed when its last holder lets
//! go) and Runtime shutdown (destroy, then a process-wide kill as the final
//! net). Every wait is bounded; a stuck process is logged, never waited for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::sandbox::{SandboxStatus, Workspace};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::{LiveSession, Runtime};

/// How long one `stop` or `destroy` of a host Workspace may take. At
/// shutdown the destroys run together, so this is also the worst case that
/// the host processes add (`loop runtime stop` waits 10 s).
const HOST_STOP_LIMIT: Duration = Duration::from_secs(3);

/// One gate per folder: it orders "a Session of the folder is published, so
/// the host Workspace runs" against "the last Session of the folder closed,
/// so the host Workspace stops". A slow stop in one folder never delays
/// another folder. An entry exists only while someone holds or awaits it.
#[derive(Default)]
pub(super) struct HostGates {
    map: Arc<std::sync::Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>>,
}

/// The hold on one folder's gate; the entry goes when the last one lets go.
pub(super) struct HostGuard {
    map: Arc<std::sync::Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>>,
    cwd: PathBuf,
    guard: Option<OwnedMutexGuard<()>>,
}

impl HostGates {
    pub(super) async fn lock(&self, cwd: &Path) -> HostGuard {
        let gate = {
            let mut map = self
                .map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(map.entry(cwd.to_path_buf()).or_default())
        };
        let guard = gate.lock_owned().await;
        HostGuard {
            map: Arc::clone(&self.map),
            cwd: cwd.to_path_buf(),
            guard: Some(guard),
        }
    }

    #[cfg(test)]
    pub(super) fn entries(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        // Release first; an entry that nobody else holds or awaits goes.
        drop(self.guard.take());
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map
            .get(&self.cwd)
            .is_some_and(|gate| Arc::strong_count(gate) == 1)
        {
            map.remove(&self.cwd);
        }
    }
}

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
    /// workspaces at once. The caller holds the gate of `cwd` (and no other
    /// lock of the Runtime).
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
            #[cfg(test)]
            {
                let delay = self.host_stop_delay.lock().unwrap().get(cwd).copied();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
            }
            end_processes(&workspace, false, "last Session closed").await;
        }
    }

    /// Whether unloading `live` would take running host processes with it:
    /// it is the last loaded Session of its folder and the folder's host
    /// Workspace has a process that runs. Unload is the Runtime's memory
    /// decision, never a request to stop work, so such a Session stays.
    /// (The Workspace has no way to list open uploads; they are not seen.)
    pub(super) async fn host_processes_pin(&self, live: &LiveSession) -> bool {
        let cwd = &live.opened.workspace.cwd;
        let shared = self.sessions.lock().unwrap().values().any(|other| {
            other.reference.key != live.reference.key && other.opened.workspace.cwd == *cwd
        });
        if shared {
            return false;
        }
        let workspace = self
            .workspaces
            .lock()
            .unwrap()
            .get(cwd)
            .map(|state| Arc::clone(&state.resources.host_workspace))
            .unwrap_or_else(|| Self::host_workspace_of(live));
        match workspace.list_processes().await {
            Ok(processes) => processes.iter().any(|process| process.exit.is_none()),
            Err(_) => false,
        }
    }

    /// Runtime shutdown: destroy every host Workspace this Runtime made
    /// (loaded workspaces, replaced ones still held, and ones of Sessions
    /// that are closed), together and bounded. It touches no other
    /// Runtime's processes.
    pub(super) async fn end_all_host_processes(&self) {
        let workspaces = self.factory.globals.host_workspaces();
        futures::future::join_all(
            workspaces
                .iter()
                .map(|workspace| end_processes(workspace, true, "Runtime shutdown")),
        )
        .await;
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

/// Orders of the per-folder gate. They start no process: the stop of a
/// folder is made slow by a hook instead.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::MemoryEventLog;
    use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
    use loop_protocol::{AgentBackend, SessionRef, SessionSpec};
    use std::time::Instant;

    struct Dirs {
        _tmp: tempfile::TempDir,
        x: PathBuf,
        y: PathBuf,
    }

    async fn runtime() -> (Arc<Runtime>, Dirs) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dirs = Dirs {
            x: root.join("x"),
            y: root.join("y"),
            _tmp: tmp,
        };
        std::fs::create_dir_all(&dirs.x).unwrap();
        std::fs::create_dir_all(&dirs.y).unwrap();
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let globals = GlobalResources::with_models(root.join("agent"), models).unwrap();
        let opts = BootstrapOpts {
            cwd: dirs.x.clone(),
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            theme: None,
            system_prompt: None,
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: None,
            no_web: true,
            permission_mode: Some("ask".into()),
            agent: None,
        };
        let store = globals.open_runtime_session_store().unwrap();
        let rt = Runtime::new(
            globals,
            opts,
            Arc::new(MemoryEventLog::new()),
            store,
            crate::RuntimeLimits::default(),
        )
        .await
        .unwrap()
        .into_shared();
        (rt, dirs)
    }

    async fn open(rt: &Runtime, cwd: &Path) -> SessionRef {
        rt.new_session(SessionSpec {
            cwd: Some(cwd.to_path_buf()),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    async fn until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A Session that opens in a folder while the last Session of it is
    /// still stopping its workspace waits for the stop and then restarts
    /// the workspace: it is never published into a stopped one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_open_racing_the_last_close_of_its_folder_restarts_the_workspace() {
        let (rt, dirs) = runtime().await;
        let first = open(&rt, &dirs.x).await;
        let workspace = rt.loaded_host_workspace(&dirs.x).unwrap();
        rt.host_stop_delay
            .lock()
            .unwrap()
            .insert(dirs.x.clone(), Duration::from_millis(600));
        let closing = {
            let rt = Arc::clone(&rt);
            let key = first.key.clone();
            tokio::spawn(async move { rt.close(&key, false).await })
        };
        // The Session is gone and its stop is under way (slowed by the hook).
        until("the first Session is removed", || {
            rt.sessions.lock().unwrap().is_empty()
        })
        .await;
        let second = open(&rt, &dirs.x).await;
        closing.await.unwrap().unwrap();
        assert!(rt.sessions.lock().unwrap().contains_key(&second.key));
        assert_eq!(
            workspace.status(),
            loop_agent::harness::sandbox::SandboxStatus::Ready,
            "the new Session runs in a started workspace"
        );
        assert_eq!(rt.host_gates.entries(), 0, "no gate entry is left");
        rt.shutdown().await;
    }

    /// Two Sessions of a folder closing together: only the last one stops
    /// the workspace (once), and the first one leaves it running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn of_two_closes_in_one_folder_exactly_the_last_stops() {
        let (rt, dirs) = runtime().await;
        let one = open(&rt, &dirs.x).await;
        let two = open(&rt, &dirs.x).await;
        let workspace = rt.loaded_host_workspace(&dirs.x).unwrap();
        let (a, b) = tokio::join!(rt.close(&one.key, false), rt.close(&two.key, false));
        a.unwrap();
        b.unwrap();
        assert_eq!(
            workspace.status(),
            loop_agent::harness::sandbox::SandboxStatus::Stopped
        );
        // One Session left while the other closes: nothing stops.
        let three = open(&rt, &dirs.x).await;
        let four = open(&rt, &dirs.x).await;
        rt.close(&three.key, false).await.unwrap();
        assert_eq!(
            workspace.status(),
            loop_agent::harness::sandbox::SandboxStatus::Ready
        );
        rt.close(&four.key, false).await.unwrap();
        rt.shutdown().await;
    }

    /// A stop that hangs in folder X delays no open in folder Y.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stuck_stop_in_one_folder_does_not_delay_an_open_in_another() {
        let (rt, dirs) = runtime().await;
        let first = open(&rt, &dirs.x).await;
        rt.host_stop_delay
            .lock()
            .unwrap()
            .insert(dirs.x.clone(), Duration::from_millis(1500));
        let closing = {
            let rt = Arc::clone(&rt);
            let key = first.key.clone();
            tokio::spawn(async move { rt.close(&key, false).await })
        };
        until("the stop of X is under way", || {
            rt.sessions.lock().unwrap().is_empty()
        })
        .await;
        let started = Instant::now();
        let other = open(&rt, &dirs.y).await;
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(700),
            "the open in Y waited {waited:?} for the stop in X"
        );
        assert!(!closing.is_finished(), "the stop of X was still running");
        // A close in Y is not held up either.
        rt.close(&other.key, false).await.unwrap();
        closing.await.unwrap().unwrap();
        assert_eq!(rt.host_gates.entries(), 0);
        rt.shutdown().await;
    }
}
