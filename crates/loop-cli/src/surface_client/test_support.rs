use super::*;
use loop_agent::harness::{
    create_session_repository, AgentHarness, AgentHarnessOptions, HostExecutionEnv, SandboxMode,
};
use loop_agent::native_backend::NativeBackend;
use loop_ai::providers::{faux_provider, FauxScript};
use std::path::Path;
pub(crate) async fn fixture(dir: &Path) -> SurfaceClient {
    fixture_with(dir, true).await
}

/// A fixture whose standalone Session store is `None` when `with_store` is
/// false, as on platforms without writer leases.
pub(crate) async fn fixture_with(dir: &Path, with_store: bool) -> SurfaceClient {
    let models = Arc::new(loop_ai::Models::new());
    let provider = faux_provider(FauxScript::new());
    let mut model = provider.model("faux-model").unwrap();
    model.reasoning = true;
    let mut second = model.clone();
    second.id = "second".into();
    provider.set_models(vec![model.clone(), second]);
    models.set_provider(provider);
    let store: Arc<dyn loop_agent::harness::session::SessionStore> = Arc::new(
        loop_agent::harness::session::sqlite::SqliteSessionStore::open(dir.join("sessions.db"))
            .unwrap()
            .standalone_writer(),
    );
    let session = create_session_repository(store.clone(), None)
        .create(Some(dir.display().to_string()), None)
        .await
        .unwrap();
    let session_id = session.metadata().id.clone();
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models: models.clone(),
        model: Some(model.clone()),
        session,
        host_env: Arc::new(HostExecutionEnv::new(dir)),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    let native = NativeBackend::new(harness.clone()).await;
    let inner = loop_app_core::Runtime {
        agent_dir: dir.into(),
        cwd: dir.into(),
        custom_system_prompt: None,
        append_system_prompt: None,
        context_files: vec![],
        settings: Default::default(),
        models,
        credentials: Arc::new(
            crate::config::auth::FileCredentialStore::open(dir.join("auth.json")).unwrap(),
        ),
        harness,
        backend: native.clone(),
        native,
        agents: Arc::new(loop_app_core::agents::AgentManager::new(
            vec![],
            dir.into(),
            dir,
        )),
        active_agent: None,
        agent_model: None,
        agent_effort: None,
        theme: Theme::dark(),
        resources: Default::default(),
        project_trusted: false,
        trust: crate::config::trust::TrustStore::load(dir.join("trust.json")).unwrap(),
        sessions_db: dir.join("sessions.db"),
        session_id,
        resumed: false,
        needs_provider_setup: false,
        selected_model: Some(model),
        model_note: None,
        tool_approval: None,
        tool_gate: loop_app_core::tool_gate::ToolGate::new(Default::default()),
        startup_notices: vec![],
        mcp_client: Arc::new(loop_mcp::McpClientManager::new()),
        active_skills: vec![],
        web_disabled_for_run: true,
        #[cfg(feature = "telemetry")]
        telemetry: None,
        session_store: with_store.then_some(store),
    };
    crate::CliRuntime {
        inner,
        keybindings: Default::default(),
        debug: false,
        debug_log_path: None,
        pending_agent: None,
        starting_agent: None,
        signing_in: None,
        forwarded: Default::default(),
        reasoning_notices: Default::default(),
        confirm_full_access: false,
        confirm_export: None,
        startup_sessions: None,
        usage_requested: false,
    }
    .into()
}

pub(crate) fn pause_provider(
    client: &SurfaceClient,
    started: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
) {
    let provider = loop_ai::providers::faux_provider_fn(Arc::new(move |_| {
        let (started, finish) = (started.clone(), finish.clone());
        Box::pin(async move {
            started.notify_one();
            finish.notified().await;
            loop_ai::providers::FauxResponse::Text("finished".into())
        })
    }));
    provider.set_models(client.host.inner.models.models(Some("faux")));
    client.host.inner.models.set_provider(provider);
}

pub(crate) fn settings(client: &SurfaceClient) -> &crate::config::Settings {
    &client.host.inner.settings
}
pub(crate) fn settings_mut(client: &mut SurfaceClient) -> &mut crate::config::Settings {
    &mut client.host.inner.settings
}
pub(crate) fn resources_mut(client: &mut SurfaceClient) -> &mut crate::resources::LoadedResources {
    &mut client.host.inner.resources
}
pub(crate) async fn sync_resources(client: &SurfaceClient) {
    client.host.inner.sync_harness_resources().await;
}
pub(crate) fn logout(client: &SurfaceClient, provider: &str) {
    client.host.inner.credentials.logout(provider).unwrap();
}
pub(crate) async fn append_label(client: &SurfaceClient, label: String) {
    client.host.inner.harness.append_label(label).await.unwrap();
}

#[cfg(feature = "orchestration")]
pub(crate) fn plan_shell_task(client: &SurfaceClient) {
    let provider = loop_ai::providers::faux_provider_fn(Arc::new(|_| {
        Box::pin(async {
            loop_ai::providers::FauxResponse::Text(serde_json::json!({"tasks":[{"id":"inspect","description":"Inspect","kind":"shell_command","command":"printf \"workflow-output:$LOOP_WORKFLOW_ENV\""}]}).to_string())
        })
    }));
    provider.set_models(client.host.inner.models.models(Some("faux")));
    client.host.inner.models.set_provider(provider);
}

#[cfg(feature = "orchestration")]
pub(crate) fn set_workflow_environment(client: &SurfaceClient) {
    client
        .host
        .inner
        .harness
        .set_session_environment(Some(std::collections::BTreeMap::from([(
            "LOOP_WORKFLOW_ENV".into(),
            "caller".into(),
        )])));
}

/// A subscription-like backend that reports usage through updates, without stats
/// or native sandbox/timeout services.
pub(crate) struct SubscriptionBackend {
    instance: loop_protocol::BackendInstanceId,
    updates: tokio::sync::broadcast::Sender<loop_protocol::UpdateEnvelope>,
}
impl SubscriptionBackend {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            instance: "subscription".into(),
            updates: tokio::sync::broadcast::channel(16).0,
        })
    }
    pub(crate) fn session(&self) -> SessionRef {
        SessionRef {
            instance: self.instance.clone(),
            key: "subscription-session".into(),
            native_id: "subscription-session".into(),
        }
    }
    /// Only the `/usage` dashboard tests (feature `usage`) push usage updates.
    #[cfg(feature = "usage")]
    pub(crate) fn usage(&self, usage: loop_protocol::Usage) {
        self.updates
            .send(loop_protocol::UpdateEnvelope::live(
                self.instance.clone(),
                1,
                self.session().key,
                None,
                1,
                loop_protocol::SessionUpdate::Usage(usage),
            ))
            .unwrap();
    }
}
#[async_trait::async_trait]
impl AgentBackend for SubscriptionBackend {
    fn instance(&self) -> &loop_protocol::BackendInstanceId {
        &self.instance
    }
    fn capabilities(&self) -> loop_protocol::Capabilities {
        Default::default()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<loop_protocol::UpdateEnvelope> {
        self.updates.subscribe()
    }
    async fn new_session(&self, _: SessionSpec) -> loop_protocol::BackendResult<SessionRef> {
        Ok(self.session())
    }
    async fn load_session(
        &self,
        _: &SessionRef,
        _: SessionSpec,
    ) -> loop_protocol::BackendResult<SessionRef> {
        Ok(self.session())
    }
    async fn prompt(
        &self,
        _: &SessionRef,
        _: Vec<loop_protocol::ContentBlock>,
    ) -> loop_protocol::BackendResult<loop_protocol::TurnHandle> {
        Err(loop_protocol::BackendError::Unsupported(
            "test prompt".into(),
        ))
    }
    fn cancel(&self, _: &SessionRef) {}
    fn is_busy(&self, _: &SessionRef) -> bool {
        false
    }
    async fn shutdown(&self) {}
}

pub(crate) fn capture_system_prompt(
    client: &SurfaceClient,
    observed: Arc<std::sync::Mutex<String>>,
) {
    let provider = loop_ai::providers::faux_provider_fn(Arc::new(move |request| {
        *observed.lock().unwrap() = format!("{request:?}");
        Box::pin(async { loop_ai::providers::FauxResponse::Text("done".into()) })
    }));
    provider.set_models(client.host.inner.models.models(Some("faux")));
    client.host.inner.models.set_provider(provider);
}

pub(crate) async fn install_read_tool(client: &SurfaceClient) {
    let env = client.host.inner.harness.tool_env().await.unwrap();
    client
        .host
        .inner
        .harness
        .set_tools(vec![loop_agent::harness::tools::create_read_tool(env)])
        .await
        .unwrap();
}
