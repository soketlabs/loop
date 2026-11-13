//! A Session watched by any connection stays loaded at the Session cap, and
//! an attach that races unloading is never acknowledged for a Session it
//! cannot keep loaded. Real wire, in memory.

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::{PeerInfo, RpcErrorBody};
use loop_protocol::{BackendError, SessionKey, SessionRef};
use loop_runtime::log::{EventLog, LoggedEvent, MemoryEventLog};
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_control, ServeOptions, Served};
use loop_wire::{transport::memory, Connection, Incoming, RpcError};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::Notify;

const LIMIT: Duration = Duration::from_secs(60);

fn opts(cwd: &std::path::Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
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
    }
}

/// A Runtime that holds one Session at a time.
async fn runtime(dir: &std::path::Path, log: Arc<dyn EventLog>) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(
            globals,
            opts(dir),
            log,
            store,
            RuntimeLimits {
                max_sessions: 1,
                max_active_turns: 1,
            },
        )
    }
    .await
    .unwrap()
    .into_shared()
}

/// A raw client; `interact` is what it tells `initialize`.
struct Client {
    conn: Arc<Connection>,
    _served: Served,
}
impl Client {
    async fn connect(rt: &Arc<Runtime>, interact: bool) -> Self {
        let (server, client) = memory();
        let served = serve_with_control(
            rt.clone(),
            rt.clone(),
            server,
            PeerInfo {
                name: "test".into(),
                version: "1".into(),
            },
            ServeOptions {
                owns_backend: false,
                authority: loop_wire::Authority::HostLocal,
            },
        );
        let (conn, mut incoming) = Connection::new(client);
        let reader = conn.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Response { id, result } = message {
                    reader.resolve(id, result).await;
                }
            }
        });
        conn.request(
            "initialize",
            json!({"protocol_version": 1, "client_info": {"name": "t", "version": "1"},
                "client_capabilities": {"interact": interact}}),
        )
        .await
        .unwrap();
        Self {
            conn,
            _served: served,
        }
    }
    async fn call(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        self.conn
            .request(method, params)
            .await
            .map_err(|e| match e {
                RpcError::Remote {
                    code,
                    message,
                    data,
                } => RpcErrorBody {
                    code,
                    message,
                    data,
                }
                .into_backend_error(),
                other => panic!("{method}: {other:?}"),
            })
    }
    async fn new_session(&self) -> Result<SessionRef, BackendError> {
        self.call("session/new", json!({}))
            .await
            .map(|v| serde_json::from_value(v).unwrap())
    }
    async fn attach(&self, session: &SessionRef) -> Result<Value, BackendError> {
        self.call("session/attach", json!({ "session": session }))
            .await
    }
    async fn detach(&self, session: &SessionRef) {
        self.call("session/detach", json!({ "session": session }))
            .await
            .unwrap();
    }
}

async fn loaded(rt: &Runtime) -> Vec<SessionKey> {
    rt.list()
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.live)
        .map(|s| s.r#ref.key)
        .collect()
}

/// Wait until `key` has `watching` watchers. A connection registers the
/// Sessions it creates, and forgets a disconnected one, on its own task.
/// Attach and detach register before they reply and need no wait.
async fn watched(rt: &Runtime, key: &SessionKey, watching: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while rt.watcher_count(key) != watching {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{key:?} never had {watching} watcher(s)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_keeps_its_session_loaded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
        let observer = Client::connect(&rt, false).await;
        let a = observer.new_session().await.unwrap();
        observer.attach(&a).await.unwrap();
        let other = Client::connect(&rt, true).await;
        assert_eq!(other.new_session().await, Err(BackendError::Busy));
        assert_eq!(loaded(&rt).await, std::slice::from_ref(&a.key));
        // Once it detaches, A can make room.
        observer.detach(&a).await;
        assert_eq!(rt.watcher_count(&a.key), 0);
        let b = other.new_session().await.unwrap();
        assert_eq!(loaded(&rt).await, [b.key]);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disconnected_observer_no_longer_keeps_its_session() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
        let observer = Client::connect(&rt, false).await;
        let a = observer.new_session().await.unwrap();
        watched(&rt, &a.key, 1).await;
        observer.conn.close();
        watched(&rt, &a.key, 0).await;
        let other = Client::connect(&rt, true).await;
        let b = other.new_session().await.unwrap();
        assert_eq!(loaded(&rt).await, [b.key]);
        rt.shutdown().await;
        let _ = a;
    })
    .await
    .unwrap();
}

/// Holds `read_after` (the attach snapshot) while `held` is set.
#[derive(Default)]
struct GatedLog {
    inner: MemoryEventLog,
    held: AtomicBool,
    reading: Notify,
    release: Notify,
}
#[async_trait::async_trait]
impl EventLog for GatedLog {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        key: &SessionKey,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        if self.held.swap(false, Ordering::AcqRel) {
            self.reading.notify_one();
            self.release.notified().await;
        }
        self.inner.read_after(key, seq).await
    }
    async fn read_runtime_after(&self, seq: u64) -> loop_runtime::log::Result<Vec<LoggedEvent>> {
        self.inner.read_runtime_after(seq).await
    }
    async fn latest_seq(&self, key: &SessionKey) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(key).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        ms: i64,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.allocate_generation(owner, pid, ms).await
    }
    async fn finish_generation(&self, generation: u64, ms: i64) -> loop_runtime::log::Result<()> {
        self.inner.finish_generation(generation, ms).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_protects_its_session_before_its_snapshot() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(GatedLog::default());
        let rt = runtime(dir.path(), log.clone()).await;
        let maker = Client::connect(&rt, true).await;
        let a = maker.new_session().await.unwrap();
        maker.detach(&a).await;
        // A's attach is paused in its snapshot...
        let watcher = Arc::new(Client::connect(&rt, true).await);
        log.held.store(true, Ordering::Release);
        let attaching = tokio::spawn({
            let (watcher, a) = (watcher.clone(), a.clone());
            async move { watcher.attach(&a).await }
        });
        log.reading.notified().await;
        // ...so B cannot unload A, and gets Busy.
        let other = Client::connect(&rt, true).await;
        assert_eq!(other.new_session().await, Err(BackendError::Busy));
        log.release.notify_one();
        attaching.await.unwrap().unwrap();
        assert_eq!(loaded(&rt).await, std::slice::from_ref(&a.key));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_that_cannot_load_its_unloaded_session_fails() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new())).await;
        let maker = Client::connect(&rt, true).await;
        let a = maker.new_session().await.unwrap();
        maker.detach(&a).await;
        // B unloads A and stays watched by its maker.
        let other = Client::connect(&rt, true).await;
        let b = other.new_session().await.unwrap();
        watched(&rt, &b.key, 1).await;
        assert_eq!(loaded(&rt).await, std::slice::from_ref(&b.key));
        // A cannot come back while B is watched: the attach says so.
        let watcher = Client::connect(&rt, true).await;
        assert_eq!(watcher.attach(&a).await, Err(BackendError::Busy));
        // With room again, the same attach loads A.
        other.detach(&b).await;
        watcher.attach(&a).await.unwrap();
        assert_eq!(loaded(&rt).await, std::slice::from_ref(&a.key));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
