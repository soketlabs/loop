//! A Runtime and wire clients for the file and terminal tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    Caller, HostServices, InteractionOutcome, InteractionRequest, SessionRef, SessionSpec,
};
use loop_runtime::log::MemoryEventLog;
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_caller, ServeOptions};
use loop_wire::{Authority, RemoteBackend, RuntimeControl};
use serde_json::Value;

pub struct Fixture {
    pub dir: tempfile::TempDir,
    /// The Session's folder (canonical).
    pub ws: PathBuf,
    pub rt: Arc<Runtime>,
    pub log: Arc<MemoryEventLog>,
}

pub struct Options {
    pub mode: &'static str,
    pub settings: Value,
    pub lock: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: "full-access",
            settings: serde_json::json!({}),
            lock: false,
        }
    }
}

pub async fn runtime(options: Options) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ws = ws.canonicalize().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("settings.json"), options.settings.to_string()).unwrap();
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider_fn(Arc::new(|_| {
        Box::pin(std::future::ready(
            loop_ai::providers::faux::FauxResponse::Text("ok".into()),
        ))
    })));
    let mut globals = GlobalResources::with_models(agent, models).unwrap();
    if options.lock {
        globals = globals.with_tool_lock(Some(
            loop_app_core::permissions::tool_lock::RuntimeToolLock::new(
                loop_protocol::ToolLock::Chat,
                dir.path(),
            )
            .unwrap(),
        ));
    }
    let opts = BootstrapOpts {
        cwd: ws.clone(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some(options.mode.into()),
        agent: None,
    };
    let log = Arc::new(MemoryEventLog::new());
    let store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(
        globals,
        opts,
        log.clone(),
        store,
        RuntimeLimits {
            max_sessions: 16,
            max_active_turns: 4,
        },
    )
    .await
    .unwrap()
    .into_shared();
    Fixture { dir, ws, rt, log }
}

struct NoHost;

#[async_trait]
impl HostServices for NoHost {
    async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Cancelled
    }
}

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "0".into(),
    }
}

/// A client on a memory transport, as `caller` (the host's user by
/// default, on a host-local connection).
pub async fn client(rt: &Arc<Runtime>, caller: Option<Caller>) -> Arc<RemoteBackend> {
    let (server, client) = loop_wire::transport::memory();
    serve(rt, server, caller);
    RemoteBackend::connect(client, info(), Arc::new(NoHost))
        .await
        .unwrap()
}

/// Serve `rt` on `transport` (for a raw client).
pub fn serve_raw(rt: &Arc<Runtime>, transport: loop_wire::Transport) {
    serve(rt, transport, None);
}

fn serve(rt: &Arc<Runtime>, transport: loop_wire::Transport, caller: Option<Caller>) {
    let control: Arc<dyn RuntimeControl> = rt.clone();
    let options = ServeOptions {
        owns_backend: false,
        authority: if caller.as_ref().is_none_or(Caller::is_host_user) {
            Authority::HostLocal
        } else {
            Authority::Remote
        },
    };
    let caller = caller.unwrap_or_else(|| Caller::host(loop_wire::server::local_principal()));
    let _ = serve_with_caller(
        rt.clone(),
        Some(control),
        transport,
        info(),
        options,
        caller,
    );
}

pub async fn new_session(client: &RemoteBackend, cwd: &Path) -> SessionRef {
    use loop_protocol::AgentBackend;
    client
        .new_session(SessionSpec {
            cwd: Some(cwd.to_path_buf()),
            ..SessionSpec::default()
        })
        .await
        .unwrap()
}

/// The code of a workspace error, or the error's text.
pub fn code(error: &loop_protocol::BackendError) -> String {
    match error {
        loop_protocol::BackendError::Workspace { code, .. } => code.as_str().to_owned(),
        loop_protocol::BackendError::ToolLocked { .. } => "tool_locked".into(),
        loop_protocol::BackendError::Unsupported(_) => "unsupported".into(),
        loop_protocol::BackendError::SessionNotFound(_) => "session_not_found".into(),
        loop_protocol::BackendError::Forbidden(_) => "forbidden".into(),
        other => format!("{other:?}"),
    }
}

/// Every record of the Event Log, as text.
pub async fn log_text(log: &MemoryEventLog) -> String {
    use loop_runtime::log::EventLog;
    let records = log
        .read_runtime_after(
            loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
            0,
        )
        .await
        .unwrap();
    format!("{records:?}")
}

/// A raw JSON-RPC client, as `caller`, after `initialize`.
pub async fn raw_client(rt: &Arc<Runtime>, caller: Option<Caller>) -> Arc<loop_wire::Connection> {
    let (server, client) = loop_wire::transport::memory();
    serve(rt, server, caller);
    let (conn, mut incoming) = loop_wire::Connection::new(client);
    let resolver = Arc::clone(&conn);
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let loop_wire::Incoming::Response { id, result } = message {
                resolver.resolve(id, result).await;
            }
        }
    });
    conn.request(
        loop_protocol::rpc::methods::INITIALIZE,
        serde_json::json!({"protocol_version": 1, "client_info": {"name": "raw", "version": "0"}}),
    )
    .await
    .unwrap();
    conn
}

/// A request on a raw client; a remote error as the backend error it
/// stands for.
pub async fn raw(
    conn: &loop_wire::Connection,
    method: &str,
    params: Value,
) -> Result<Value, loop_protocol::BackendError> {
    match conn.request(method, params).await {
        Ok(value) => Ok(value),
        Err(loop_wire::RpcError::Remote {
            code,
            message,
            data,
        }) => Err(loop_protocol::rpc::RpcErrorBody {
            code,
            message,
            data,
        }
        .into_backend_error()),
        Err(other) => panic!("{other}"),
    }
}

/// The capabilities `initialize` gave `client`.
pub fn caps(client: &RemoteBackend) -> loop_protocol::Capabilities {
    loop_protocol::AgentBackend::capabilities(client)
}
