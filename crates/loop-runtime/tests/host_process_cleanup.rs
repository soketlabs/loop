//! A Runtime ends the processes of its host workspaces: at shutdown, when
//! the last Session of a folder closes or is unloaded, and when
//! `workspace/reload` replaces the workspace.
//!
//! This test binary starts real processes, so it has its own binary. Every
//! test holds `PROCESSES`: the shutdown of a Runtime kills the host
//! processes of the whole test process, tests of other Runtimes included.
#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use loop_agent::harness::sandbox::{
    HostWorkspaceDriver, SandboxStatus, SpawnRequest, Workspace, WorkspaceDriver, WorkspaceSource,
    WorkspaceSpec,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{admin::ReloadParams, AgentBackend, SessionRef, SessionSpec};
use loop_runtime::{log::MemoryEventLog, Runtime, RuntimeLimits};
use loop_wire::RuntimeAdmin;

static PROCESSES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long a started process lives before it writes its marker.
const LIFE: Duration = Duration::from_secs(2);

struct Env {
    _tmp: tempfile::TempDir,
    agent: PathBuf,
    ws1: PathBuf,
    ws2: PathBuf,
    out: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let env = Self {
            agent: root.join("agent"),
            ws1: root.join("ws1"),
            ws2: root.join("ws2"),
            out: root.join("out"),
            _tmp: tmp,
        };
        for dir in [&env.agent, &env.ws1, &env.ws2, &env.out] {
            std::fs::create_dir_all(dir).unwrap();
        }
        env
    }

    fn opts(&self) -> BootstrapOpts {
        BootstrapOpts {
            cwd: self.ws1.clone(),
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            theme: None,
            system_prompt: Some("cleanup test".into()),
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: None,
            no_web: true,
            permission_mode: Some("ask".into()),
            agent: None,
        }
    }

    async fn runtime(&self, limits: RuntimeLimits) -> Arc<Runtime> {
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let globals = GlobalResources::with_models(self.agent.clone(), models).unwrap();
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(
            globals,
            self.opts(),
            Arc::new(MemoryEventLog::new()),
            store,
            limits,
        )
        .await
        .unwrap()
        .into_shared()
    }
}

async fn open(rt: &Runtime, cwd: &Path) -> SessionRef {
    rt.new_session(SessionSpec {
        cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    })
    .await
    .unwrap()
}

fn host(rt: &Runtime, cwd: &Path) -> Arc<dyn Workspace> {
    rt.loaded_host_workspace(cwd)
        .expect("the Runtime loaded this workspace")
}

fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A process this test started: it writes its pid, lives [`LIFE`], then
/// writes its marker. Killed when the test ends, whatever happened.
struct Started {
    life: Duration,
    pid: i32,
    marker: PathBuf,
    at: Instant,
}

impl Drop for Started {
    fn drop(&mut self) {
        if alive(self.pid) {
            let _ = std::process::Command::new("kill")
                .args(["-9", &self.pid.to_string()])
                .status();
        }
    }
}

async fn start(workspace: &Arc<dyn Workspace>, out: &Path, name: &str) -> Started {
    start_for(workspace, out, name, LIFE).await
}

async fn start_for(
    workspace: &Arc<dyn Workspace>,
    out: &Path,
    name: &str,
    life: Duration,
) -> Started {
    let pid_file = out.join(format!("{name}.pid"));
    let marker = out.join(format!("{name}.marker"));
    let at = Instant::now();
    let process = workspace
        .spawn(SpawnRequest {
            command: format!(
                "echo $$ > '{}'; sleep {}; touch '{}'",
                pid_file.display(),
                life.as_secs(),
                marker.display()
            ),
            inherit_env: true,
            detach: true,
            ..Default::default()
        })
        .await
        .unwrap();
    // Detached: the process lives on without its handle.
    drop(process);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            break pid;
        }
        assert!(
            at.elapsed() < Duration::from_secs(5),
            "process {name} did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(alive(pid), "{name} runs");
    Started {
        life,
        pid,
        marker,
        at,
    }
}

impl Started {
    /// It is gone now, and stays without a marker for its whole life.
    async fn assert_killed(&self, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(self.pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!alive(self.pid), "{what}: process {} still runs", self.pid);
        let lived = self.life + Duration::from_millis(700);
        if let Some(rest) = lived.checked_sub(self.at.elapsed()) {
            tokio::time::sleep(rest).await;
        }
        assert!(
            !self.marker.exists(),
            "{what}: the process went on to its marker"
        );
    }

    /// It runs, and does not stop before the check.
    fn assert_running(&self, what: &str) {
        assert!(alive(self.pid), "{what}: process {} was killed", self.pid);
    }
}

fn reload_params(cwd: &Path) -> ReloadParams {
    ReloadParams {
        workspace_id: loop_runtime::workspace_id(cwd),
        expected_revision: None,
    }
}

/// Shutdown: the process of an open Session's workspace is gone, and no
/// marker appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_the_processes_of_every_workspace() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env.runtime(RuntimeLimits::default()).await;
    open(&rt, &env.ws1).await;
    open(&rt, &env.ws2).await;
    let one = start(&host(&rt, &env.ws1), &env.out, "one").await;
    let two = start(&host(&rt, &env.ws2), &env.out, "two").await;
    rt.shutdown().await;
    one.assert_killed("shutdown").await;
    two.assert_killed("shutdown").await;
}

/// `shutdown_if_empty` (version replacement) ends the processes of
/// workspaces that no Session uses, too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_if_empty_kills_what_a_closed_workspace_left() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env.runtime(RuntimeLimits::default()).await;
    let session = open(&rt, &env.ws1).await;
    let workspace = host(&rt, &env.ws1);
    // Closing the last Session stops the host Workspace; start it again as
    // a later user of the folder would, then shut down with no Session.
    rt.close(&session.key, false).await.unwrap();
    workspace.start().await.unwrap();
    let process = start(&workspace, &env.out, "left").await;
    assert!(rt.shutdown_if_empty().await);
    process.assert_killed("shutdown with no Session").await;
    assert_eq!(workspace.status(), SandboxStatus::Stopped);
}

/// The final net: a host process of a Workspace the Runtime does not know
/// (and that nobody drops or destroys) is killed at shutdown too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_processes_of_workspaces_it_never_loaded() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env.runtime(RuntimeLimits::default()).await;
    open(&rt, &env.ws1).await;
    let stray: Arc<dyn Workspace> = HostWorkspaceDriver
        .create(WorkspaceSpec {
            source: WorkspaceSource::HostFolder(env.ws2.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    stray.start().await.unwrap();
    let process = start(&stray, &env.out, "stray").await;
    rt.shutdown().await;
    process.assert_killed("final net").await;
    // Kept alive to here: the net killed it, not a drop.
    drop(stray);
}

/// Closing the last Session of a folder ends its processes, and the next
/// Session of that folder can start processes again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_last_session_kills_its_processes_and_the_folder_restarts() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env.runtime(RuntimeLimits::default()).await;
    let first = open(&rt, &env.ws1).await;
    let second = open(&rt, &env.ws1).await;
    let workspace = host(&rt, &env.ws1);
    let process = start(&workspace, &env.out, "closing").await;

    // Another Session still uses the folder: nothing ends.
    rt.close(&first.key, false).await.unwrap();
    process.assert_running("one Session left");
    assert_eq!(workspace.status(), SandboxStatus::Ready);

    rt.close(&second.key, false).await.unwrap();
    process.assert_killed("last Session closed").await;
    assert_eq!(workspace.status(), SandboxStatus::Stopped);

    // The folder works again with its next Session.
    open(&rt, &env.ws1).await;
    assert_eq!(host(&rt, &env.ws1).status(), SandboxStatus::Ready);
    let again = start(&host(&rt, &env.ws1), &env.out, "again").await;
    rt.shutdown().await;
    again.assert_killed("shutdown").await;
}

/// A Session unloaded to make room is the last of its folder: its
/// processes end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unloading_the_last_session_kills_its_processes() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env
        .runtime(RuntimeLimits {
            max_sessions: 1,
            max_active_turns: 1,
        })
        .await;
    open(&rt, &env.ws1).await;
    let process = start(&host(&rt, &env.ws1), &env.out, "unloaded").await;
    // The limit is one: the second Session unloads the first.
    open(&rt, &env.ws2).await;
    process.assert_killed("unloaded").await;
    rt.shutdown().await;
}

/// `workspace/reload` replaces the resources: the processes of the old
/// ones end, and the new ones run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_kills_the_processes_of_the_replaced_workspace() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env.runtime(RuntimeLimits::default()).await;
    open(&rt, &env.ws1).await;
    let old = host(&rt, &env.ws1);
    let process = start(&old, &env.out, "reloaded").await;
    RuntimeAdmin::reload(rt.as_ref(), reload_params(&env.ws1))
        .await
        .unwrap();
    process.assert_killed("reload").await;
    // The test holds the old Workspace, so a drop did not do this.
    assert_eq!(old.status(), SandboxStatus::Stopped);
    let new = host(&rt, &env.ws1);
    assert!(!Arc::ptr_eq(&old, &new));
    assert_eq!(new.status(), SandboxStatus::Ready);
    let next = start(&new, &env.out, "after-reload").await;
    rt.shutdown().await;
    next.assert_killed("shutdown").await;
}

/// A process of one workspace survives what happens to another workspace,
/// until the Runtime shuts down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_workspace_ending_leaves_a_process_alone() {
    let _only = PROCESSES.lock().await;
    let env = Env::new();
    let rt = env.runtime(RuntimeLimits::default()).await;
    open(&rt, &env.ws1).await;
    let other = open(&rt, &env.ws2).await;
    // Lives longer than the checks below take.
    let kept = start_for(&host(&rt, &env.ws1), &env.out, "kept", LIFE * 4).await;
    let lost = start(&host(&rt, &env.ws2), &env.out, "lost").await;

    // Reloading the other workspace.
    RuntimeAdmin::reload(rt.as_ref(), reload_params(&env.ws2))
        .await
        .unwrap();
    lost.assert_killed("reload of the other workspace").await;
    kept.assert_running("reload of the other workspace");

    // Closing the other workspace's Session.
    let second = start(&host(&rt, &env.ws2), &env.out, "second").await;
    rt.close(&other.key, false).await.unwrap();
    second.assert_killed("close in the other workspace").await;
    kept.assert_running("close in the other workspace");

    rt.shutdown().await;
    kept.assert_killed("shutdown").await;
}
