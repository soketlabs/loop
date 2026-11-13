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

/// `RuntimeControl` is implemented by the CLI; these tests need only the
/// backend methods, so a minimal stand-in forwards nothing.
struct NoControl;
#[async_trait::async_trait]
impl RuntimeControl for NoControl {
    async fn list(&self) -> loop_protocol::BackendResult<Vec<loop_protocol::rpc::SessionInfo>> {
        Ok(Vec::new())
    }
    async fn close(&self, _: &SessionRef, _: bool) -> loop_protocol::BackendResult<()> {
        Ok(())
    }
    async fn attach(
        &self,
        _: &SessionRef,
        _: u64,
    ) -> loop_protocol::BackendResult<loop_wire::Attachment> {
        Err(loop_protocol::BackendError::Unsupported("attach".into()))
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

#[tokio::test]
async fn the_runtime_records_the_connection_s_caller_for_each_session() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(Default::default()));
        let globals = GlobalResources::with_models(dir.path().join("agent"), models).unwrap();
        let rt = Arc::new(
            Runtime::new(
                globals,
                opts(dir.path()),
                Arc::new(MemoryEventLog::new()),
                RuntimeLimits::default(),
            )
            .await
            .unwrap(),
        );
        let control: Arc<dyn RuntimeControl> = Arc::new(NoControl);

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
            host.new_session(SessionSpec::default()),
            edges[0].0.new_session(SessionSpec::default()),
            edges[1].0.new_session(SessionSpec::default()),
        );
        let local = Caller::host(loop_wire::local_principal());
        assert_eq!(rt.opened_by(&h.unwrap().key), Some(local));
        assert_eq!(rt.opened_by(&a.unwrap().key), Some(edges[0].1.clone()));
        assert_eq!(rt.opened_by(&b.unwrap().key), Some(edges[1].1.clone()));

        // An in-process call has no connection, so no caller: there is no
        // fallback identity.
        let direct = rt.new_session(SessionSpec::default()).await.unwrap();
        assert_eq!(rt.opened_by(&direct.key), None);

        rt.shutdown().await;
    })
    .await
    .unwrap();
}
