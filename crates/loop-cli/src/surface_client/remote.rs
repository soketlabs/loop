//! Remote services behind the same SurfaceClient used by native and ACP.
use super::*;
use loop_protocol::{BackendResult, HostServices, InteractionOutcome, InteractionRequest};
use loop_wire::{transport::Transport, RemoteBackend};

/// Interaction endpoint installed once the TUI owns its approval queue.
#[derive(Default)]
pub struct RemoteHost {
    queue: tokio::sync::OnceCell<loop_app_core::interaction::InteractionQueue>,
    ready: tokio::sync::Notify,
}
impl RemoteHost {
    pub fn install(&self, queue: loop_app_core::interaction::InteractionQueue) {
        let _ = self.queue.set(queue);
        self.ready.notify_waiters();
    }
}
#[async_trait::async_trait]
impl HostServices for RemoteHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if let Some(queue) = self.queue.get() {
                return queue.interact(request).await;
            }
            ready.await;
        }
    }
}

/// Connect with immutable transcript reset support and interactive approvals.
pub async fn connect(
    transport: Transport,
    host: Arc<dyn HostServices>,
) -> BackendResult<Arc<RemoteBackend>> {
    RemoteBackend::connect_with_capabilities(
        transport,
        PeerInfo {
            name: "loop-tui".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        host,
        ClientCapabilities {
            interact: true,
            branch_reset: true,
        },
    )
    .await
}

pub(super) struct RemoteSessions(pub Arc<RemoteBackend>);
#[async_trait::async_trait]
impl RuntimeControl for RemoteSessions {
    async fn config(&self, p: SessionParams) -> BackendResult<SessionSnapshot> {
        self.0.config(p).await
    }
    async fn skill(&self, p: SkillParams) -> BackendResult<loop_protocol::MutationResult> {
        self.0.skill(p).await
    }
    async fn workflow(&self, p: WorkflowParams) -> BackendResult<WorkflowResult> {
        self.0.workflow(p).await
    }
    async fn tree(&self, p: TreeParams) -> BackendResult<TreeResult> {
        self.0.tree(p).await
    }
    async fn view(&self, p: ViewParams) -> BackendResult<ViewResult> {
        self.0.view(p).await
    }
    async fn navigate(&self, p: NavigateParams) -> BackendResult<NavigateResult> {
        self.0.navigate(p).await
    }
    async fn rename(&self, p: RenameParams) -> BackendResult<MutationResult> {
        self.0.rename(p).await
    }
    async fn fork(&self, p: ForkParams) -> BackendResult<DestinationResult> {
        self.0.fork(p).await
    }
    async fn clone_session(&self, p: CloneParams) -> BackendResult<DestinationResult> {
        self.0.clone_session(p).await
    }
    async fn export(&self, p: SessionParams) -> BackendResult<ExportResult> {
        self.0.export(p).await
    }
    async fn import(&self, p: ImportParams) -> BackendResult<DestinationResult> {
        self.0.import(p).await
    }
    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        self.0.list().await
    }
    async fn close(&self, session: &SessionRef, force: bool) -> BackendResult<()> {
        self.0.close(session, force).await
    }
    async fn attach(&self, session: &SessionRef, seq: u64) -> BackendResult<loop_wire::Attachment> {
        let result = self.0.attach(session, seq).await?;
        Ok(loop_wire::Attachment {
            snapshot: result.snapshot,
            latest_seq: result.latest_seq,
            generation: result.generation,
            baseline: vec![],
            replay: Box::new(std::iter::empty()),
        })
    }
}

/// Remote model identity for display when the Surface has no matching catalog.
/// Execution metadata and credentials remain on the owner.
pub(super) fn model_view(spec: &str, name: &str) -> loop_ai::Model {
    let (provider, id) = spec.split_once('/').unwrap_or(("", spec));
    loop_ai::Model {
        id: id.into(),
        provider: provider.into(),
        name: name.into(),
        api: String::new(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![loop_ai::InputModality::Text],
        cost: Default::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

impl SurfaceClient {
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<loop_protocol::UpdateEnvelope> {
        self.remote_updates
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| self.backend().subscribe())
    }
    pub fn accept_update(&mut self, seq: u64) -> bool {
        if !self.is_remote() {
            return true;
        }
        if seq <= self.remote_cursor {
            return false;
        }
        self.remote_cursor = seq;
        true
    }
    pub fn take_transcript_redraw(&mut self) -> bool {
        std::mem::take(&mut self.remote_redraw)
    }
    pub async fn branch_transcript(
        &mut self,
        view_id: &str,
    ) -> anyhow::Result<Vec<loop_protocol::SessionUpdate>> {
        let mut updates = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .sessions
                .view(ViewParams {
                    session: self.session_ref(),
                    view_id: view_id.into(),
                    cursor,
                    limit: Some(1000),
                })
                .await?;
            self.remote_cursor = self.remote_cursor.max(page.at_seq);
            updates.extend(page.updates);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(updates)
    }
    pub async fn transcript(&mut self) -> anyhow::Result<Vec<loop_protocol::SessionUpdate>> {
        if let Some(view_id) = self.remote_view.take() {
            return self.branch_transcript(&view_id).await;
        }
        Ok(self.backend().transcript(&self.session_ref()).await?)
    }
    pub async fn reset_transcript(
        &mut self,
        view_id: &str,
    ) -> anyhow::Result<Vec<loop_protocol::SessionUpdate>> {
        let updates = self.branch_transcript(view_id).await?;
        self.remote_redraw = true;
        Ok(updates)
    }
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }
    /// Connected over WebSocket: credentials and tracing belong to that host.
    pub fn is_remote_host(&self) -> bool {
        self.remote.as_ref().is_some_and(|r| r.is_websocket())
    }
    pub fn interaction_queue(
        &self,
    ) -> (
        loop_app_core::interaction::InteractionQueue,
        tokio::sync::mpsc::UnboundedReceiver<loop_app_core::interaction::PendingInteraction>,
    ) {
        if self.is_remote() {
            loop_app_core::interaction::InteractionQueue::new_detaching()
        } else {
            loop_app_core::interaction::InteractionQueue::new()
        }
    }
    pub fn connection_closed(
        &self,
    ) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        self.remote
            .clone()
            .map(|remote| async move { remote.closed().await })
    }
    pub async fn finish(&self) -> anyhow::Result<()> {
        if let Some(remote) = &self.remote {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                remote.detach(&self.session_ref()),
            )
            .await;
            remote.disconnect();
            match result {
                Ok(Err(loop_protocol::BackendError::Failed(info)))
                    if info.category == loop_protocol::ErrorCategory::Network => {}
                Ok(Err(error)) => return Err(error.into()),
                Ok(Ok(())) | Err(_) => {}
            }
        }
        self.shutdown().run().await
    }
    pub async fn refresh_remote(&mut self) -> anyhow::Result<()> {
        let Some(remote) = self.remote.clone() else {
            return Ok(());
        };
        let snapshot = remote
            .config(SessionParams {
                session: self.session_ref(),
            })
            .await?;
        let refresh_workspace = self.remote_snapshot.as_ref().is_none_or(|old| {
            old.workspace_id != snapshot.workspace_id
                || old.workspace_revision != snapshot.workspace_revision
        }) || self.remote_resources.is_none();
        self.adopt_snapshot(snapshot);
        if refresh_workspace {
            use loop_protocol::admin::*;
            use loop_wire::RuntimeAdmin;
            let workspace_id = self.workspace_id.clone();
            let mut view = ResourceView {
                active_skills: self.host.inner.active_skills.clone(),
                ..Default::default()
            };
            if remote.capabilities().workspace_skills {
                view.skills = remote
                    .skills(SkillsParams {
                        workspace_id: workspace_id.clone(),
                        action: AdminAction::List,
                        name: None,
                        expected_revision: None,
                    })
                    .await?
                    .skills;
            }
            if remote.capabilities().workspace_templates {
                view.templates = remote
                    .templates(TemplatesParams {
                        workspace_id: workspace_id.clone(),
                        name: None,
                        args: vec![],
                    })
                    .await?
                    .templates
                    .unwrap_or_default();
            }
            if remote.capabilities().workspace_mcp {
                self.remote_mcp = Some(
                    remote
                        .mcp(McpParams {
                            workspace_id: workspace_id.clone(),
                            action: AdminAction::List,
                            name: None,
                            expected_revision: None,
                        })
                        .await?,
                );
            }
            if remote.capabilities().workspace_trust {
                self.host.inner.project_trusted = remote
                    .trust(TrustParams {
                        workspace_id: workspace_id.clone(),
                        trusted: None,
                        expected_revision: None,
                    })
                    .await?
                    .trusted;
            }
            self.remote_resources = Some(view);
        }
        Ok(())
    }
    fn adopt_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.workspace_id = snapshot.workspace_id.clone();
        self.host.inner.session_id = snapshot.session.native_id.clone();
        self.host.inner.active_skills = snapshot.config.active_skills.clone();
        self.host.inner.selected_model = snapshot.config.model.as_deref().and_then(|spec| {
            let (provider, id) = spec.split_once('/')?;
            self.host
                .inner
                .models
                .get_model(provider, id)
                .or_else(|| Some(model_view(spec, id)))
        });
        if let Some(view) = &mut self.remote_resources {
            view.active_skills = snapshot.config.active_skills.clone();
        }
        self.remote_snapshot = Some(snapshot);
    }
    pub async fn attach_remote(&mut self, session: SessionRef) -> anyhow::Result<()> {
        let remote = self.remote.as_ref().unwrap().clone();
        let attached = remote.attach(&session, 0).await?;
        let old = self.session_ref();
        if old != session {
            remote.detach(&old).await?;
        }
        self.host.inner.session_id = session.native_id;
        self.remote_cursor = 0;
        if let Some(snapshot) = attached.snapshot {
            self.remote_view = snapshot.view_id.clone();
            self.adopt_snapshot(snapshot);
        }
        self.refresh_remote().await?;
        Ok(())
    }

    /// Build Surface-local preferences and catalogs with an inert in-memory
    /// native placeholder. It owns no database lease, tools or host execution.
    pub async fn connected(
        remote: Arc<RemoteBackend>,
        host: Arc<RemoteHost>,
        opts: &crate::BootstrapOpts,
        session: SessionRef,
        resumed: bool,
    ) -> anyhow::Result<Self> {
        Self::connected_at(
            remote,
            host,
            opts,
            session,
            resumed,
            crate::config::paths::get_agent_dir(),
        )
        .await
    }

    async fn connected_at(
        remote: Arc<RemoteBackend>,
        host: Arc<RemoteHost>,
        opts: &crate::BootstrapOpts,
        session: SessionRef,
        resumed: bool,
        agent_dir: std::path::PathBuf,
    ) -> anyhow::Result<Self> {
        use loop_agent::harness::{
            create_in_memory_session_store, create_session_repository, AgentHarness,
            AgentHarnessOptions, HostExecutionEnv, SandboxMode,
        };
        let mut settings =
            crate::config::Settings::load_file(&crate::config::paths::settings_path(&agent_dir))?;
        if let Some(theme) = &opts.theme {
            settings.theme = theme.clone();
        }
        let credentials = Arc::new(crate::config::auth::FileCredentialStore::open(
            crate::config::paths::auth_path(&agent_dir),
        )?);
        let models = crate::build_models(&agent_dir, credentials.clone(), &settings.providers)?;
        let dummy = create_session_repository(create_in_memory_session_store(), None)
            .create(None, None)
            .await?;
        let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
            models: models.clone(),
            model: None,
            session: dummy,
            host_env: Arc::new(HostExecutionEnv::new(&opts.cwd)),
            tools: vec![],
            system_prompt: String::new(),
            sandbox: SandboxMode::Disabled,
            resources: Default::default(),
        }));
        let native = loop_agent::native_backend::NativeBackend::new(harness.clone()).await;
        let inner = loop_app_core::Runtime {
            agent_dir: agent_dir.clone(),
            cwd: opts.cwd.clone(),
            custom_system_prompt: None,
            append_system_prompt: None,
            context_files: vec![],
            models,
            credentials,
            harness,
            backend: remote.clone(),
            native,
            agents: Arc::new(loop_app_core::agents::AgentManager::new(
                vec![],
                opts.cwd.clone(),
                &agent_dir,
            )),
            active_agent: None,
            agent_model: None,
            agent_effort: None,
            theme: Theme::load(
                &settings.theme,
                &crate::theme::theme_search_dirs(&agent_dir, None),
            )
            .unwrap_or_else(|_| Theme::dark()),
            resources: Default::default(),
            project_trusted: false,
            trust: crate::config::trust::TrustStore::load(crate::config::paths::trust_path(
                &agent_dir,
            ))?,
            sessions_db: crate::config::paths::sessions_db_path(&agent_dir),
            session_id: session.native_id.clone(),
            resumed,
            needs_provider_setup: false,
            selected_model: None,
            model_note: None,
            tool_approval: None,
            tool_gate: loop_app_core::tool_gate::ToolGate::new(Default::default()),
            startup_notices: vec![],
            mcp_client: Arc::new(loop_mcp::McpClientManager::new()),
            active_skills: vec![],
            web_disabled_for_run: false,
            #[cfg(feature = "telemetry")]
            telemetry: None,
            session_store: None,
            settings,
        };
        let mut client: SurfaceClient = crate::CliRuntime {
            keybindings: keybindings::Keybindings::load(&crate::config::paths::keybindings_path(
                &agent_dir,
            ))?,
            inner,
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
        .into();
        *client.remote_updates.lock().unwrap() = Some(remote.subscribe());
        client.native_backend = remote.clone();
        client.sessions = Arc::new(RemoteSessions(remote.clone()));
        client
            .host
            .inner
            .startup_notices
            .extend(remote.take_connection_notices());
        client.remote = Some(remote);
        client.remote_host = Some(host);
        client.attach_remote(session).await?;
        if resumed {
            client.host.inner.startup_notices.push("Attached Session keeps its original cwd and tool environment; after a Runtime restart it uses the Runtime's environment.".into());
        }
        if let Some(snapshot) = &client.remote_snapshot {
            client
                .host
                .inner
                .startup_notices
                .extend(snapshot.notices.clone());
        }
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_protocol::{ContentBlock, SessionUpdate};
    use loop_wire::server::{serve_with_control, ServeOptions};
    use std::time::Duration;
    fn opts(dir: &std::path::Path) -> crate::BootstrapOpts {
        crate::BootstrapOpts {
            cwd: dir.canonicalize().unwrap(),
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
    async fn runtime(dir: &std::path::Path) -> Arc<loop_runtime::Runtime> {
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider_fn(Arc::new(|context| {
            let answered = context
                .messages
                .iter()
                .any(|m| matches!(m, loop_ai::Message::Assistant(_)));
            Box::pin(async move {
                if answered {
                    return loop_ai::providers::FauxResponse::Text("finished".into());
                }
                loop_ai::providers::FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
                    id: "call".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"echo done"}),
                    thought_signature: None,
                }])
            })
        })));
        let agent = dir.join("agent");
        let mut settings = crate::config::Settings::default();
        settings.sandbox.mode = "off".into();
        settings
            .save_file(&crate::config::paths::settings_path(&agent))
            .unwrap();
        let globals = loop_app_core::runtime::GlobalResources::with_models(agent, models).unwrap();
        {
            let store = globals.open_runtime_session_store().unwrap();
            loop_runtime::Runtime::new(
                globals,
                opts(dir),
                Arc::new(loop_runtime::log::MemoryEventLog::new()),
                store,
                Default::default(),
            )
        }
        .await
        .unwrap()
        .into_shared()
    }
    async fn remote(
        rt: &Arc<loop_runtime::Runtime>,
        version: &str,
    ) -> (Arc<RemoteBackend>, Arc<RemoteHost>) {
        let (server, client) = loop_wire::transport::memory();
        let _ = serve_with_control(
            rt.clone(),
            rt.clone(),
            server,
            PeerInfo {
                name: "loop".into(),
                version: version.into(),
            },
            ServeOptions {
                owns_backend: false,
                authority: loop_wire::Authority::HostLocal,
            },
        );
        let host = Arc::new(RemoteHost::default());
        (connect(client, host.clone()).await.unwrap(), host)
    }
    #[tokio::test]
    async fn two_surfaces_observe_config_changes_and_detach_without_closing() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let rt = runtime(dir.path()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (r1, h1) = remote(&rt, env!("CARGO_PKG_VERSION")).await;
            let (r2, h2) = remote(&rt, env!("CARGO_PKG_VERSION")).await;
            let one = SurfaceClient::connected_at(
                r1,
                h1,
                &opts(dir.path()),
                session.clone(),
                false,
                dir.path().join("agent"),
            )
            .await
            .unwrap();
            let two = SurfaceClient::connected_at(
                r2,
                h2,
                &opts(dir.path()),
                session.clone(),
                true,
                dir.path().join("agent"),
            )
            .await
            .unwrap();
            let mut updates1 = one.backend().subscribe();
            let mut updates2 = two.backend().subscribe();
            one.rename_session("shared name").await.unwrap();
            for updates in [&mut updates1, &mut updates2] {
                loop {
                    if let SessionUpdate::ConfigChanged(changed) =
                        updates.recv().await.unwrap().update
                    {
                        assert_eq!(changed.metadata.name.as_deref(), Some("shared name"));
                        break;
                    }
                }
            }
            one.finish().await.unwrap();
            assert!(!rt.is_idle().await);
            two.finish().await.unwrap();
            assert!(rt.is_idle().await);
            assert_eq!(rt.status().loaded, 1);
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn detach_keeps_a_parked_approval_for_the_next_surface() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let rt = runtime(dir.path()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (remote, host) = remote(&rt, env!("CARGO_PKG_VERSION")).await;
            let mut one = SurfaceClient::connected_at(
                remote.clone(),
                host,
                &opts(dir.path()),
                session.clone(),
                false,
                dir.path().join("agent"),
            )
            .await
            .unwrap();
            let (queue, mut requests) = one.interaction_queue();
            one.install_interactions(queue).await.unwrap();
            let turn = rt
                .prompt(&session, vec![ContentBlock::text("park")])
                .await
                .unwrap();
            let request = requests.recv().await.unwrap();
            one.finish().await.unwrap();
            drop(request);
            drop(requests);
            assert_eq!(rt.status().busy, 1);
            assert!(!rt.is_idle().await);
            let (second, second_host) = self::remote(&rt, env!("CARGO_PKG_VERSION")).await;
            let (queue, mut requests) =
                loop_app_core::interaction::InteractionQueue::new_detaching();
            second_host.install(queue);
            second.attach(&session, 0).await.unwrap();
            let _request = requests.recv().await.unwrap();
            assert_eq!(rt.status().busy, 1);
            rt.cancel(&session);
            tokio::time::timeout(Duration::from_secs(5), turn.outcome())
                .await
                .expect("cancel did not settle the parked turn");
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn version_mismatch_allows_restart_only_without_live_sessions() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let rt = runtime(dir.path()).await;
            let (remote, _) = remote(&rt, "0.0.1").await;
            assert!(
                crate::runtime_launch::compatible(&remote).await.is_err(),
                "explicit owners must not be replaced"
            );
            rt.new_session(SessionSpec::default()).await.unwrap();
            let error = crate::runtime_launch::compatible(&remote)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("Runtime is v0.0.1, this loop is v"));
            assert!(error.contains("loop runtime restart"));
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn immutable_attach_view_discards_replay_overlap_and_resets_the_transcript() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let rt = runtime(dir.path()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (remote, host) = remote(&rt, env!("CARGO_PKG_VERSION")).await;
            let mut client = SurfaceClient::connected_at(
                remote,
                host,
                &opts(dir.path()),
                session.clone(),
                false,
                dir.path().join("agent"),
            )
            .await
            .unwrap();
            let mut updates = client.subscribe();
            let _ = client.transcript().await.unwrap();
            client.rename_session("after attach").await.unwrap();
            loop {
                let envelope = updates.recv().await.unwrap();
                if !client.accept_update(envelope.seq) {
                    continue;
                }
                if matches!(envelope.update, SessionUpdate::ConfigChanged(_)) {
                    break;
                }
            }
            assert!(!client.accept_update(client.remote_cursor));
            let snapshot = client
                .remote
                .as_ref()
                .unwrap()
                .attach(&session, 0)
                .await
                .unwrap()
                .snapshot
                .unwrap();
            let _ = client
                .reset_transcript(snapshot.view_id.as_deref().unwrap())
                .await
                .unwrap();
            assert!(client.take_transcript_redraw());
            assert!(!client.take_transcript_redraw());
            client.finish().await.unwrap();
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }
}
