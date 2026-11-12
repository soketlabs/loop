//! `SessionShutdown` releases a standalone Surface's leases only once its
//! turn, with the tools, hooks and writes it runs, has finished.
#![cfg(unix)]

use loop_agent::harness::{
    create_session_repository,
    session::{sqlite::SqliteSessionStore, SessionStore},
    types::{ExecutionError, ShellOutput},
    AgentHarness, AgentHarnessOptions, AgentHarnessResources, HostExecutionEnv, KrunIsolation,
    KrunSandbox, LocalSandboxRuntime, PodmanClient, PodmanExecOpts, PodmanRunOpts, Sandbox,
    SandboxError, SandboxMode, TaggedSandbox,
};
use loop_ai::{
    providers::{faux_provider, FauxResponse, FauxScript},
    Models, ToolCall,
};
use loop_app_core::runtime::SessionShutdown;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

/// A tool that ignores cancellation and finishes only when told.
struct Stubborn {
    started: Arc<Notify>,
    finish: Arc<Notify>,
    finished: Arc<AtomicBool>,
}

fn stubborn() -> (loop_agent::AgentTool, Stubborn) {
    let parts = Stubborn {
        started: Arc::new(Notify::new()),
        finish: Arc::new(Notify::new()),
        finished: Arc::new(AtomicBool::new(false)),
    };
    let (started, finish, finished) = (
        parts.started.clone(),
        parts.finish.clone(),
        parts.finished.clone(),
    );
    let tool = loop_agent::AgentTool::simple(
        "slow",
        "slow",
        "takes a while",
        json!({"type": "object", "properties": {}}),
        move |_, _, _, _| {
            let (started, finish, finished) = (started.clone(), finish.clone(), finished.clone());
            async move {
                started.notify_one();
                finish.notified().await;
                finished.store(true, Ordering::SeqCst);
                Ok(loop_agent::AgentToolResult::text("done"))
            }
        },
    );
    (tool, parts)
}

async fn setup(
    dir: &std::path::Path,
) -> (
    Arc<AgentHarness>,
    Arc<dyn SessionStore>,
    SqliteSessionStore,
    String,
    Stubborn,
) {
    setup_with(dir, SandboxMode::Disabled).await
}

async fn setup_with(
    dir: &std::path::Path,
    sandbox: SandboxMode,
) -> (
    Arc<AgentHarness>,
    Arc<dyn SessionStore>,
    SqliteSessionStore,
    String,
    Stubborn,
) {
    let path = dir.join("sessions.db");
    let base = SqliteSessionStore::open(&path).unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(base.standalone_writer());
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    let id = session.metadata().id.clone();
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "c1".into(),
        name: "slow".into(),
        arguments: json!({}),
        thought_signature: None,
    }]));
    script.push(FauxResponse::Text("after".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let (tool, parts) = stubborn();
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        model: models.get_model("faux", "faux-model"),
        models,
        session,
        host_env: Arc::new(HostExecutionEnv::new(dir)),
        tools: vec![tool],
        system_prompt: "sys".into(),
        sandbox,
        resources: AgentHarnessResources::default(),
    }));
    (harness, store, base, id, parts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leases_stay_while_a_turn_ignores_cancellation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let (harness, store, base, id, tool) = setup(dir.path()).await;
        let turn = tokio::spawn({
            let harness = harness.clone();
            async move { harness.prompt_with_images("go", vec![]).await }
        });
        tool.started.notified().await;
        let shutdown = SessionShutdown::new(harness.clone(), Some(store))
            .with_grace(Duration::from_millis(200));
        let error = shutdown.run().await.unwrap_err().to_string();
        assert!(error.contains("did not stop"), "{error}");
        assert!(
            base.lease(&id).await.unwrap().is_some(),
            "released while the turn was still running"
        );
        // Once the turn has really finished, shutdown completes.
        tool.finish.notify_one();
        let _ = turn.await.unwrap();
        shutdown.run().await.unwrap();
        assert!(base.lease(&id).await.unwrap().is_none());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_returns_only_after_the_turn_has_finished() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let (harness, store, base, id, tool) = setup(dir.path()).await;
        let turn = tokio::spawn({
            let harness = harness.clone();
            async move { harness.prompt_with_images("go", vec![]).await }
        });
        tool.started.notified().await;
        let finish = tool.finish.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            finish.notify_one();
        });
        SessionShutdown::new(harness.clone(), Some(store))
            .run()
            .await
            .unwrap();
        assert!(
            tool.finished.load(Ordering::SeqCst),
            "released before the tool finished"
        );
        assert!(base.lease(&id).await.unwrap().is_none());
        let _ = turn.await.unwrap();
    })
    .await
    .unwrap();
}

/// A Podman stand-in that only records which containers were removed.
/// With `run_gate`, `podman run` waits for a permit (an image pull).
#[derive(Default)]
struct RecordingPodman {
    removed: parking_lot::Mutex<Vec<String>>,
    run_gate: Option<Arc<tokio::sync::Semaphore>>,
}

#[async_trait::async_trait]
impl PodmanClient for RecordingPodman {
    async fn preflight(&self, _runtime: &str) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn run(&self, opts: PodmanRunOpts) -> Result<String, SandboxError> {
        if let Some(gate) = &self.run_gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(format!("id-{}", opts.name))
    }
    async fn exec(&self, _opts: PodmanExecOpts) -> Result<ShellOutput, ExecutionError> {
        Ok(ShellOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            truncated: false,
        })
    }
    async fn stop(&self, _container: &str) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn rm(&self, container: &str) -> Result<(), SandboxError> {
        self.removed.lock().push(container.to_string());
        Ok(())
    }
}

async fn started_sandbox(dir: &std::path::Path) -> (SandboxMode, Arc<RecordingPodman>) {
    let client = Arc::new(RecordingPodman::default());
    let sandbox = KrunSandbox::with_client(
        KrunSandbox::config_for(
            dir.to_path_buf(),
            KrunIsolation::Full,
            LocalSandboxRuntime::Runc,
        ),
        client.clone(),
    );
    sandbox.start().await.unwrap();
    (
        SandboxMode::Enabled {
            sandbox: Arc::new(sandbox),
        },
        client,
    )
}

async fn sandbox_on(harness: &AgentHarness) -> bool {
    harness
        .sandbox_info()
        .await
        .fields
        .contains(&("Mode".into(), "on".into()))
}

/// Standalone `loop --print` and the TUI exit through `SessionShutdown`: it
/// removes the sandbox container, once, also when the turn did not stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_removes_the_sandbox_container() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let (mode, podman) = started_sandbox(dir.path()).await;
        let (harness, store, base, id, _tool) = setup_with(dir.path(), mode).await;
        assert!(sandbox_on(&harness).await);
        let shutdown = SessionShutdown::new(harness.clone(), Some(store));
        shutdown.run().await.unwrap();
        assert_eq!(podman.removed.lock().len(), 1, "container not removed");
        assert!(!sandbox_on(&harness).await);
        assert!(base.lease(&id).await.unwrap().is_none());
        // Every exit path runs it; a repeat removes nothing more.
        shutdown.run().await.unwrap();
        assert_eq!(podman.removed.lock().len(), 1);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_removes_the_sandbox_when_the_turn_does_not_stop() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let (mode, podman) = started_sandbox(dir.path()).await;
        let (harness, store, _base, _id, tool) = setup_with(dir.path(), mode).await;
        let turn = tokio::spawn({
            let harness = harness.clone();
            async move { harness.prompt_with_images("go", vec![]).await }
        });
        tool.started.notified().await;
        let error = SessionShutdown::new(harness.clone(), Some(store))
            .with_grace(Duration::from_millis(200))
            .run()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not stop"), "{error}");
        assert_eq!(podman.removed.lock().len(), 1, "container not removed");
        tool.finish.notify_one();
        let _ = turn.await.unwrap();
    })
    .await
    .unwrap();
}

/// A sandbox that ends with the process (Seatbelt on macOS) is left as it
/// is: shutdown removes only a sandbox that would outlive it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_leaves_a_sandbox_that_ends_with_the_process() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let tagged = TaggedSandbox::new("plain");
        let mode = SandboxMode::Enabled {
            sandbox: Arc::new(tagged.clone()),
        };
        let (harness, store, _base, _id, _tool) = setup_with(dir.path(), mode).await;
        SessionShutdown::new(harness.clone(), Some(store))
            .run()
            .await
            .unwrap();
        assert_eq!(tagged.destroy_count(), 0);
        assert!(sandbox_on(&harness).await);
    })
    .await
    .unwrap();
}

/// An exit while `podman run` still runs: the container it then gets is
/// removed, and the start reports failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_destroyed_while_starting_removes_its_container() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let client = Arc::new(RecordingPodman {
            run_gate: Some(gate.clone()),
            ..Default::default()
        });
        let sandbox = Arc::new(KrunSandbox::with_client(
            KrunSandbox::config_for(
                dir.path().to_path_buf(),
                KrunIsolation::Full,
                LocalSandboxRuntime::Runc,
            ),
            client.clone(),
        ));
        let starting = tokio::spawn({
            let sandbox = sandbox.clone();
            async move { sandbox.start().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        sandbox.destroy().await.unwrap();
        assert!(client.removed.lock().is_empty(), "no container yet");
        gate.add_permits(1);
        assert!(starting.await.unwrap().is_err(), "start must fail");
        assert_eq!(client.removed.lock().len(), 1, "container left running");
        assert!(
            sandbox.start().await.is_err(),
            "a removed sandbox restarted"
        );
    })
    .await
    .unwrap();
}
