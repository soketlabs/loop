//! Workspace admin and host-only methods: the same over the wire as
//! in-process, and refused by transport authority before any service runs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::native_backend::NativeBackend;
use loop_ai::Models;
use loop_protocol::admin::*;
use loop_protocol::rpc::{methods, PeerInfo, SessionInfo};
use loop_protocol::{
    AgentBackend, BackendError, BackendResult, Capabilities, ContentBlock, HostServices,
    InteractionOutcome, InteractionRequest, SessionRef, UpdateEnvelope,
};
use loop_wire::server::{serve_with_control, ServeOptions};
use loop_wire::transport::memory;
use loop_wire::{
    Attachment, Authority, Connection, Incoming, RemoteBackend, RpcError, RuntimeAdmin,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;

fn info(name: &str) -> PeerInfo {
    PeerInfo {
        name: name.into(),
        version: "test".into(),
    }
}

fn admin_flags() -> Capabilities {
    Capabilities {
        session_skills: true,
        workflow: true,
        workspace_mcp: true,
        workspace_web_search: true,
        workspace_skills: true,
        workspace_templates: true,
        workspace_reload: true,
        workspace_trust: true,
        session_sandbox: true,
        permission_mode: true,
        host_providers: true,
        host_tracing: true,
        host_mcp_auth: true,
        host_usage: true,
        host_defaults: true,
        ..Capabilities::default()
    }
}

/// A Runtime stand-in: a native backend with every admin flag, and an admin
/// service that counts calls, so a refusal is seen to stop before it.
struct Fake {
    inner: Arc<NativeBackend>,
    calls: AtomicUsize,
    lock: Option<loop_protocol::ToolLock>,
}

#[async_trait]
impl AgentBackend for Fake {
    fn instance(&self) -> &loop_protocol::BackendInstanceId {
        self.inner.instance()
    }
    fn capabilities(&self) -> Capabilities {
        admin_flags()
    }
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.inner.subscribe()
    }
    async fn new_session(&self, spec: loop_protocol::SessionSpec) -> BackendResult<SessionRef> {
        self.inner.new_session(spec).await
    }
    async fn load_session(
        &self,
        session: &SessionRef,
        spec: loop_protocol::SessionSpec,
    ) -> BackendResult<SessionRef> {
        self.inner.load_session(session, spec).await
    }
    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<loop_protocol::TurnHandle> {
        self.inner.prompt(session, input).await
    }
    fn cancel(&self, session: &SessionRef) {
        self.inner.cancel(session)
    }
    fn is_busy(&self, session: &SessionRef) -> bool {
        self.inner.is_busy(session)
    }
    async fn shutdown(&self) {}
}

#[async_trait]
impl loop_wire::RuntimeControl for Fake {
    async fn skill(
        &self,
        p: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<loop_protocol::MutationResult> {
        assert_eq!(p.name, "inspect");
        Ok(loop_protocol::MutationResult {
            revision: 2,
            seq: 3,
            pending: false,
            applies_after_turn: None,
        })
    }
    async fn workflow(
        &self,
        p: loop_protocol::session_services::WorkflowParams,
    ) -> BackendResult<loop_protocol::session_services::WorkflowResult> {
        assert_eq!(p.goal, "inspect code");
        Ok(loop_protocol::session_services::WorkflowResult {
            workflow_id: "wf1".into(),
        })
    }

    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        Ok(vec![])
    }
    async fn close(&self, _session: &SessionRef, _force: bool) -> BackendResult<()> {
        Ok(())
    }
    async fn attach(&self, _session: &SessionRef, _after_seq: u64) -> BackendResult<Attachment> {
        Err(BackendError::Unsupported("attach".into()))
    }
    async fn stop(&self) -> BackendResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn admin(&self) -> Option<&dyn RuntimeAdmin> {
        Some(self)
    }
}

fn known(workspace_id: &str) -> BackendResult<()> {
    if workspace_id == "w1" {
        Ok(())
    } else {
        Err(BackendError::failed(
            loop_protocol::ErrorCategory::Protocol,
            "unknown workspace",
        ))
    }
}

#[async_trait]
impl RuntimeAdmin for Fake {
    async fn providers(
        &self,
        p: loop_protocol::host::ProvidersParams,
    ) -> BackendResult<loop_protocol::host::ProvidersResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(p.action, loop_protocol::host::HostAction::List);
        Ok(loop_protocol::host::ProvidersResult {
            providers: vec![loop_protocol::host::ProviderInfo {
                id: "faux".into(),
                name: "Faux".into(),
                connected: true,
                model_count: Some(2),
            }],
            notices: vec![],
        })
    }
    async fn tracing(
        &self,
        _p: loop_protocol::host::TracingParams,
    ) -> BackendResult<loop_protocol::host::TracingResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(loop_protocol::host::TracingResult {
            enabled: false,
            backend: "otlp".into(),
            status: "disabled".into(),
        })
    }
    async fn mcp_auth(
        &self,
        p: loop_protocol::host::McpAuthParams,
    ) -> BackendResult<loop_protocol::host::McpAuthResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        known(&p.workspace_id)?;
        Ok(loop_protocol::host::McpAuthResult {
            status: "absent".into(),
            notices: vec![],
        })
    }

    fn offered(&self) -> Capabilities {
        Capabilities {
            tool_lock: self.lock,
            ..admin_flags()
        }
    }
    async fn mcp(&self, params: McpParams) -> BackendResult<McpResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        known(&params.workspace_id)?;
        if params.expected_revision.is_some_and(|r| r != 4) {
            return Err(BackendError::Conflict { revision: 4 });
        }
        Ok(McpResult {
            revision: 4,
            servers: vec![McpServerStatus {
                name: "docs".into(),
                enabled: params.action != AdminAction::Disable,
                connected: false,
                tools: 2,
                error: None,
            }],
            pending_sessions: 1,
        })
    }
    async fn reload(&self, params: ReloadParams) -> BackendResult<ReloadResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        known(&params.workspace_id)?;
        Ok(ReloadResult {
            revision: 5,
            notices: vec!["reloaded".into()],
            pending_sessions: 0,
        })
    }
    async fn trust(&self, params: TrustParams) -> BackendResult<TrustResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        known(&params.workspace_id)?;
        Ok(TrustResult {
            revision: 5,
            trusted: params.trusted.unwrap_or(false),
            pending_sessions: 0,
        })
    }
    async fn usage(&self, _params: UsageParams) -> BackendResult<UsageResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(UsageResult {
            summary: None,
            rows: vec![json!({"session_id": "a"})],
            totals: json!({"llm_calls": 1}),
            next_cursor: None,
            taken_at_ms: 1,
            log_status: "ok".into(),
        })
    }
    async fn defaults(&self, params: DefaultsParams) -> BackendResult<DefaultsResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(DefaultsResult {
            revision: 1,
            response_header_timeout_ms: params.patch.and_then(|p| p.response_header_timeout_ms),
            ..DefaultsResult::default()
        })
    }
}

struct NoHost;
#[async_trait]
impl HostServices for NoHost {
    async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Cancelled
    }
}

async fn fake() -> Arc<Fake> {
    let models = Arc::new(Models::new());
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: None,
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    Arc::new(Fake {
        inner: NativeBackend::new(harness).await,
        calls: AtomicUsize::new(0),
        lock: None,
    })
}

#[tokio::test]
async fn a_tool_lock_is_advertised_and_hides_the_shell_and_workflows() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for authority in [Authority::HostLocal, Authority::Remote] {
            let mut fake = fake().await;
            Arc::get_mut(&mut fake).unwrap().lock = Some(loop_protocol::ToolLock::Chat);
            let remote =
                RemoteBackend::connect(served(&fake, authority), info("client"), Arc::new(NoHost))
                    .await
                    .unwrap();
            let offered = remote.capabilities();
            assert_eq!(offered.tool_lock, Some(loop_protocol::ToolLock::Chat));
            assert!(!offered.local_shell && !offered.workflow);
            assert!(!offered.host_usage && !offered.host_defaults && !offered.host_providers);
            // Reads a chat client needs still work.
            remote
                .mcp(McpParams {
                    workspace_id: "w1".into(),
                    action: AdminAction::List,
                    name: None,
                    expected_revision: None,
                })
                .await
                .unwrap();
        }
        // Host-only and unknown admin methods are refused with the lock,
        // before any service runs.
        let mut locked = fake().await;
        Arc::get_mut(&mut locked).unwrap().lock = Some(loop_protocol::ToolLock::Chat);
        let conn = raw(served(&locked, Authority::HostLocal)).await;
        let before = locked.calls.load(Ordering::SeqCst);
        for method in [
            methods::RUNTIME_USAGE,
            methods::RUNTIME_PROVIDERS,
            methods::RUNTIME_TRACING,
            methods::RUNTIME_MCP_AUTH,
            methods::RUNTIME_DEFAULTS,
        ] {
            let refused = conn.request(method, json!({})).await;
            assert!(
                matches!(&refused, Err(RpcError::Remote { code, .. })
                    if *code == loop_protocol::rpc::error_codes::TOOL_LOCKED),
                "{method}: {refused:?}"
            );
        }
        assert_eq!(locked.calls.load(Ordering::SeqCst), before);
        let fake = fake().await;
        let remote = RemoteBackend::connect(
            served(&fake, Authority::HostLocal),
            info("client"),
            Arc::new(NoHost),
        )
        .await
        .unwrap();
        let offered = remote.capabilities();
        assert_eq!(offered.tool_lock, None);
        assert!(offered.workflow, "no lock: the backend's flags stand");
    })
    .await
    .expect("timed out");
}

fn served(fake: &Arc<Fake>, authority: Authority) -> loop_wire::Transport {
    let (server_end, client_end) = memory();
    let _ = serve_with_control(
        fake.clone(),
        fake.clone(),
        server_end,
        info("server"),
        ServeOptions {
            owns_backend: false,
            authority,
        },
    );
    client_end
}

#[tokio::test]
async fn admin_is_the_same_over_the_wire_on_a_host_local_connection() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fake = fake().await;
        let remote = RemoteBackend::connect(
            served(&fake, Authority::HostLocal),
            info("client"),
            Arc::new(NoHost),
        )
        .await
        .unwrap();
        let offered = remote.capabilities();
        assert!(offered.workspace_mcp && offered.workspace_trust);
        assert!(offered.host_usage && offered.host_defaults);
        let params = McpParams {
            workspace_id: "w1".into(),
            action: AdminAction::Disable,
            name: Some("docs".into()),
            expected_revision: Some(4),
        };
        assert_eq!(
            remote.mcp(params.clone()).await.unwrap(),
            fake.mcp(params.clone()).await.unwrap()
        );
        let stale = McpParams {
            expected_revision: Some(1),
            ..params
        };
        assert_eq!(
            remote.mcp(stale).await.unwrap_err(),
            BackendError::Conflict { revision: 4 }
        );
        let reload = ReloadParams {
            workspace_id: "w1".into(),
            expected_revision: None,
        };
        assert_eq!(
            remote.reload(reload.clone()).await.unwrap(),
            fake.reload(reload).await.unwrap()
        );
        // An unknown workspace id is an error, not a path.
        assert!(remote
            .reload(ReloadParams {
                workspace_id: "/etc".into(),
                expected_revision: None,
            })
            .await
            .is_err());
        // Trust changes and host-only methods are served host-locally.
        let trusted = remote
            .trust(TrustParams {
                workspace_id: "w1".into(),
                trusted: Some(true),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert!(trusted.trusted);
        assert_eq!(
            remote.usage(UsageParams::default()).await.unwrap().rows,
            vec![json!({"session_id": "a"})]
        );
        let saved = remote
            .defaults(DefaultsParams {
                patch: Some(DefaultsPatch {
                    response_header_timeout_ms: Some(0),
                    ..DefaultsPatch::default()
                }),
                ..DefaultsParams::default()
            })
            .await
            .unwrap();
        assert_eq!(saved.response_header_timeout_ms, Some(0));
        // Offered host methods validate their typed parameters.
        let conn = raw(served(&fake, Authority::HostLocal)).await;
        for method in [
            methods::RUNTIME_PROVIDERS,
            methods::RUNTIME_MCP_AUTH,
            methods::RUNTIME_TRACING,
        ] {
            assert!(
                matches!(conn.request(method, json!({})).await, Err(RpcError::Remote { code, .. })
                    if code == -32602),
                "{method}"
            );
        }
        use loop_protocol::host::*;
        assert_eq!(
            remote
                .providers(ProvidersParams {
                    action: HostAction::List,
                    provider: None,
                    login: None
                })
                .await
                .unwrap()
                .providers[0]
                .id,
            "faux"
        );
        assert_eq!(
            remote
                .tracing(TracingParams {
                    action: HostAction::Status,
                    config: None
                })
                .await
                .unwrap()
                .status,
            "disabled"
        );
        assert_eq!(
            remote
                .mcp_auth(McpAuthParams {
                    workspace_id: "w1".into(),
                    action: HostAction::Logout,
                    name: "test".into()
                })
                .await
                .unwrap()
                .status,
            "absent"
        );
        // A service the owner does not serve answers Unsupported.
        assert!(matches!(
            remote
                .skills(SkillsParams {
                    workspace_id: "w1".into(),
                    action: AdminAction::List,
                    name: None,
                    expected_revision: None,
                })
                .await,
            Err(BackendError::Unsupported(_))
        ));
    })
    .await
    .expect("timed out");
}

async fn raw(transport: loop_wire::Transport) -> Arc<Connection> {
    let (conn, mut incoming) = Connection::new(transport);
    let resolver = Arc::clone(&conn);
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Response { id, result } = message {
                resolver.resolve(id, result).await;
            }
        }
    });
    conn.request(
        methods::INITIALIZE,
        json!({"protocol_version": 1, "client_info": {"name": "raw", "version": "0"}}),
    )
    .await
    .unwrap();
    conn
}

fn forbidden(result: Result<Value, RpcError>) -> bool {
    matches!(result, Err(RpcError::Remote { code, .. })
        if code == loop_protocol::rpc::error_codes::FORBIDDEN)
}

#[tokio::test]
async fn a_remote_connection_is_refused_host_only_methods_before_any_service_runs() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fake = fake().await;
        let remote = RemoteBackend::connect(
            served(&fake, Authority::Remote),
            info("client"),
            Arc::new(NoHost),
        )
        .await
        .unwrap();
        // Host-only flags are not advertised remotely; workspace ones are.
        let offered = remote.capabilities();
        assert!(
            !offered.host_usage
                && !offered.host_defaults
                && !offered.host_providers
                && !offered.host_tracing
                && !offered.host_mcp_auth
        );
        assert!(offered.workspace_reload && offered.workspace_trust);

        let conn = raw(served(&fake, Authority::Remote)).await;
        // Refused whatever the params, even malformed or for reserved methods.
        for method in methods::HOST_ONLY {
            for params in [json!({}), json!({"garbage": [1, 2]}), json!(null)] {
                assert!(
                    forbidden(conn.request(method, params).await),
                    "{method} was not refused"
                );
            }
        }
        assert!(forbidden(
            conn.request(
                methods::WORKSPACE_TRUST,
                json!({"workspace_id": "w1", "trusted": true}),
            )
            .await
        ));
        assert!(forbidden(
            conn.request(
                methods::WORKSPACE_TRUST,
                json!({"workspace_id": "w1", "trusted": false}),
            )
            .await
        ));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "a service ran");

        // Reading trust and other workspace admin are served remotely.
        let read = remote
            .trust(TrustParams {
                workspace_id: "w1".into(),
                trusted: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert!(!read.trusted);
        remote
            .reload(ReloadParams {
                workspace_id: "w1".into(),
                expected_revision: None,
            })
            .await
            .unwrap();
        // The typed client sees Forbidden, distinct from an auth failure.
        let error = remote
            .trust(TrustParams {
                workspace_id: "w1".into(),
                trusted: Some(true),
                expected_revision: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            BackendError::Forbidden(methods::WORKSPACE_TRUST.into())
        );
        assert_eq!(
            remote.stop().await.unwrap_err(),
            BackendError::Forbidden(methods::RUNTIME_STOP.into())
        );
    })
    .await
    .expect("timed out");
}

#[tokio::test]
async fn a_server_without_a_runtime_offers_no_admin() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fake = fake().await;
        let (server_end, client_end) = memory();
        let _served = loop_wire::server::serve_with_options(
            fake.clone(),
            server_end,
            info("server"),
            ServeOptions {
                owns_backend: false,
                authority: Authority::HostLocal,
            },
        );
        let remote = RemoteBackend::connect(client_end, info("client"), Arc::new(NoHost))
            .await
            .unwrap();
        assert!(!remote.capabilities().workspace_mcp);
        assert!(!remote.capabilities().host_usage);
        let conn = raw({
            let (server_end, client_end) = memory();
            let _ = loop_wire::server::serve_with_options(
                fake.clone(),
                server_end,
                info("server"),
                ServeOptions::default(),
            );
            client_end
        })
        .await;
        let refused = conn
            .request(methods::WORKSPACE_RELOAD, json!({"workspace_id": "w1"}))
            .await;
        assert!(matches!(refused, Err(RpcError::Remote { code, .. })
            if code == loop_protocol::rpc::error_codes::UNSUPPORTED));
    })
    .await
    .expect("timed out");
}

#[tokio::test]
async fn skill_and_workflow_services_are_available_over_session_authority() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fake = fake().await;
        let remote = RemoteBackend::connect(
            served(&fake, Authority::Remote),
            info("client"),
            Arc::new(NoHost),
        )
        .await
        .unwrap();
        let session = fake.inner.current_session().await;
        let skill = remote
            .skill(loop_protocol::session_services::SkillParams {
                session: session.clone(),
                name: "inspect".into(),
                active: true,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!((skill.revision, skill.seq), (2, 3));
        let workflow = remote
            .workflow(loop_protocol::session_services::WorkflowParams {
                session,
                goal: "inspect code".into(),
                concurrency: Some(2),
                request_id: "once".into(),
            })
            .await
            .unwrap();
        assert_eq!(workflow.workflow_id, "wf1");
    })
    .await
    .unwrap();
}
