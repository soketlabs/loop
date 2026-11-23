//! A connection's caller (ADR-0009) reaches the Runtime: the Runtime records
//! who loaded each Session, from the connection and never from a request.

use std::sync::Arc;
use std::time::Duration;

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    AgentBackend, Caller, Principal, PrincipalKind, SessionRef, SessionSpec, Tenant,
};
use loop_runtime::{log::MemoryEventLog, Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_caller, serve_with_control, ServeOptions};
use loop_wire::transport::memory;
use loop_wire::{Authority, RemoteBackend, RuntimeControl};

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

struct Reject;
#[async_trait::async_trait]
impl loop_protocol::HostServices for Reject {
    async fn interact(
        &self,
        _: loop_protocol::InteractionRequest,
    ) -> loop_protocol::InteractionOutcome {
        loop_protocol::InteractionOutcome::Cancelled
    }
}

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "test".into(),
    }
}

fn options(authority: Authority) -> ServeOptions {
    ServeOptions {
        owns_backend: false,
        authority,
    }
}

fn edge(id: &str, tenant: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new(tenant).unwrap()).unwrap()
}

/// What the model saw of a running Subagent: the `loaded_by` of each live
/// child Session, read while the child runs (it is unloaded when it ends).
#[derive(Default)]
struct Probe {
    runtime: std::sync::OnceLock<std::sync::Weak<Runtime>>,
    children: std::sync::Mutex<Vec<Option<Caller>>>,
}

/// A shared Runtime (as the CLI runs it: `into_shared`) whose model answers
/// text, or calls the `subagent` tool once when a prompt says `SPAWN`.
async fn shared(
    dir: &std::path::Path,
    lock: bool,
    limits: RuntimeLimits,
) -> (Arc<Runtime>, Arc<Probe>) {
    use loop_ai::providers::faux::{FauxResponder, FauxResponse};
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    let probe = Arc::new(Probe::default());
    let seen = probe.clone();
    let responder: FauxResponder = Arc::new(move |context: &loop_ai::Context| {
        use loop_ai::Message;
        let text = format!("{:?}", context.messages);
        let answered = context
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(_)));
        let seen = seen.clone();
        Box::pin(async move {
            if text.contains("child work") && !text.contains("SPAWN") {
                if let Some(rt) = seen.runtime.get().and_then(std::sync::Weak::upgrade) {
                    for info in rt.list().await.unwrap() {
                        if info.parent.is_some() && info.live {
                            let loaded_by = rt.loaded_by(&info.r#ref.key);
                            seen.children.lock().unwrap().push(loaded_by);
                        }
                    }
                }
            }
            if text.contains("SPAWN") && !answered {
                FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
                    id: "call-spawn".into(),
                    name: "subagent".into(),
                    arguments: serde_json::json!({"task": "child work"}),
                    thought_signature: None,
                }])
            } else {
                FauxResponse::Text("done".into())
            }
        })
    });
    models.set_provider(loop_ai::providers::faux_provider_fn(responder));
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let globals = GlobalResources::with_models(dir.join("agent"), models)
        .unwrap()
        .with_tool_lock(lock.then(|| {
            loop_app_core::permissions::tool_lock::RuntimeToolLock::new(
                loop_protocol::ToolLock::Chat,
                &ws,
            )
            .unwrap()
        }));
    let mut opts = opts(&ws);
    opts.permission_mode = Some("full-access".into());
    let store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(
        globals,
        opts,
        Arc::new(MemoryEventLog::new()),
        store,
        limits,
    )
    .await
    .unwrap()
    .into_shared();
    let _ = probe.runtime.set(Arc::downgrade(&rt));
    (rt, probe)
}

async fn prompt(rt: &Runtime, session: &SessionRef, text: &str) {
    let turn = rt
        .prompt(session, vec![loop_protocol::ContentBlock::text(text)])
        .await
        .unwrap();
    assert!(matches!(
        turn.outcome().await,
        loop_protocol::TurnOutcome::Completed(_)
    ));
}

#[tokio::test]
async fn the_runtime_records_the_connection_s_caller_for_each_session() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        // Tool-locked, since only a locked Runtime serves edge users.
        let (rt, _) = shared(dir.path(), true, RuntimeLimits::default()).await;
        let control: Arc<dyn RuntimeControl> = rt.clone();

        // The host's own Unix-socket connection.
        let (server, client) = memory();
        let _host = serve_with_control(
            rt.clone(),
            control.clone(),
            server,
            info(),
            options(Authority::HostLocal),
        );
        let host = RemoteBackend::connect(client, info(), Arc::new(Reject))
            .await
            .unwrap();

        // Two Edge Gateway connections for different users and tenants.
        let mut edges = Vec::new();
        for caller in [edge("alice", "t-a"), edge("bob", "t-b")] {
            let (server, client) = memory();
            let _served = serve_with_caller(
                rt.clone(),
                Some(control.clone()),
                server,
                info(),
                options(Authority::Remote),
                caller.clone(),
            );
            let remote = RemoteBackend::connect(client, info(), Arc::new(Reject))
                .await
                .unwrap();
            edges.push((remote, caller));
        }

        // Interleave Session creation over all three connections.
        let (h, a, b) = tokio::join!(
            // Each in a folder of its own (the folder rule, I3).
            host.new_session(own_folder(dir.path(), "h")),
            edges[0].0.new_session(own_folder(dir.path(), "a")),
            edges[1].0.new_session(own_folder(dir.path(), "b")),
        );
        let local = Caller::host(loop_wire::local_principal());
        assert_eq!(rt.loaded_by(&h.unwrap().key), Some(local));
        assert_eq!(rt.loaded_by(&a.unwrap().key), Some(edges[0].1.clone()));
        assert_eq!(rt.loaded_by(&b.unwrap().key), Some(edges[1].1.clone()));

        // An in-process call has no connection, so no caller: there is no
        // fallback identity.
        let direct = rt.new_session(own_folder(dir.path(), "d")).await.unwrap();
        assert_eq!(rt.loaded_by(&direct.key), None);

        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// On a shared Runtime, fork, clone and import create their destination in a
/// spawned task; the caller still reaches it. A reload after an unload takes
/// the reloading caller: `loaded_by` is the last loader, not an owner.
#[tokio::test]
async fn every_way_a_shared_runtime_loads_a_session_records_its_caller() {
    use loop_protocol::rpc::{CloneParams, ForkParams, ImportParams, TreeParams};
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = shared(dir.path(), false, RuntimeLimits::default()).await;
        // Only callers that may use a Session (I3): the host's user, on its
        // socket and on its token connection, take turns.
        let (alice, bob, carol, dave, erin, frank) = (
            token_user(),
            host_user(),
            token_user(),
            host_user(),
            host_user(),
            host_user(),
        );

        let source = alice
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        assert_eq!(rt.loaded_by(&source.key), Some(alice.clone()));
        prompt(&rt, &source, "hello").await;

        let tree = RuntimeControl::tree(
            rt.as_ref(),
            TreeParams {
                session: source.clone(),
                cursor: None,
                limit: Some(1),
            },
        )
        .await
        .unwrap();
        let fork = bob
            .clone()
            .scope(RuntimeControl::fork(
                rt.as_ref(),
                ForkParams {
                    session: source.clone(),
                    before_entry_id: tree.fork_points[0].entry_id.clone(),
                    name: None,
                    request_id: "fork".into(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(rt.loaded_by(&fork.session.key), Some(bob));

        let clone = carol
            .clone()
            .scope(RuntimeControl::clone_session(
                rt.as_ref(),
                CloneParams {
                    session: source.clone(),
                    name: None,
                    request_id: "clone".into(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(rt.loaded_by(&clone.session.key), Some(carol));

        let payload = rt.export(&source).await.unwrap();
        let imported = dave
            .clone()
            .scope(RuntimeControl::import(
                rt.as_ref(),
                ImportParams {
                    workspace_id: fork.workspace_id.clone(),
                    jsonl: payload.jsonl,
                    request_id: "import".into(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(rt.loaded_by(&imported.session.key), Some(dave));

        // Closed, then loaded again by another caller: it is that caller's
        // now, which is why `loaded_by` is not an owner.
        rt.close(&source.key, false).await.unwrap();
        assert_eq!(rt.loaded_by(&source.key), None);
        erin.clone()
            .scope(rt.load_session(&source, SessionSpec::default()))
            .await
            .unwrap();
        assert_eq!(rt.loaded_by(&source.key), Some(erin));
        rt.shutdown().await;

        // Unloaded to make room (a one-Session Runtime), then reloaded by a
        // prompt of another caller.
        let dir = tempfile::tempdir().unwrap();
        let one = RuntimeLimits {
            max_sessions: 1,
            ..RuntimeLimits::default()
        };
        let (rt, _) = shared(dir.path(), false, one).await;
        let first = alice
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        let second = bob_again()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        assert_eq!(rt.loaded_by(&first.key), None, "unloaded to make room");
        assert_eq!(rt.loaded_by(&second.key), Some(bob_again()));
        frank.clone().scope(prompt(&rt, &first, "again")).await;
        assert_eq!(rt.loaded_by(&first.key), Some(frank));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

fn bob_again() -> Caller {
    host_user()
}

fn host_user() -> Caller {
    Caller::host(loop_wire::local_principal())
}

/// The host's user over the token WebSocket: another caller (`via`) with
/// the same access to `local` Sessions.
fn token_user() -> Caller {
    Caller::local_token(loop_wire::local_principal())
}

fn own_folder(dir: &std::path::Path, name: &str) -> SessionSpec {
    let cwd = dir.join("ws").join(name);
    std::fs::create_dir_all(&cwd).unwrap();
    SessionSpec {
        cwd: Some(cwd),
        ..SessionSpec::default()
    }
}

/// A Subagent copies its parent's `loaded_by`, whoever runs the child.
#[tokio::test]
async fn a_subagent_copies_its_parent_s_loaded_by() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, probe) = shared(dir.path(), false, RuntimeLimits::default()).await;
        let alice = edge("alice", "t-a");
        let parent = alice
            .clone()
            .scope(rt.new_session(own_folder(dir.path(), "alice")))
            .await
            .unwrap();
        // Prompted with no caller in scope (and the turn runs on the
        // Runtime's own task): the child still copies the parent's.
        prompt(&rt, &parent, "SPAWN a child").await;
        assert_eq!(
            probe.children.lock().unwrap().as_slice(),
            [Some(alice)],
            "one live child, seen while it ran"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
