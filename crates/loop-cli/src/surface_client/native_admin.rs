//! Native implementation of the same typed admin requests used by a Runtime.
use super::*;
use loop_protocol::host::*;
use loop_protocol::{admin::*, BackendError, BackendResult, Capabilities, ErrorCategory};
use loop_wire::RuntimeAdmin;

pub(super) struct NativeAdmin<'a>(
    pub tokio::sync::Mutex<&'a mut loop_app_core::Runtime>,
    pub Option<&'a loop_wire::RemoteBackend>,
);
fn failed(e: impl std::fmt::Display) -> BackendError {
    BackendError::failed(ErrorCategory::Other, e.to_string())
}
impl NativeAdmin<'_> {
    fn workspace(runtime: &loop_app_core::Runtime, id: &str) -> BackendResult<()> {
        if id
            != loop_runtime::workspace_id(
                &runtime
                    .cwd
                    .canonicalize()
                    .unwrap_or_else(|_| runtime.cwd.clone()),
            )
        {
            return Err(failed("unknown workspace"));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl RuntimeAdmin for NativeAdmin<'_> {
    fn offered(&self) -> Capabilities {
        if let Some(remote) = self.1 {
            return remote.offered();
        }
        Capabilities {
            host_providers: true,
            host_mcp_auth: true,
            host_tracing: cfg!(feature = "telemetry"),
            host_defaults: true,
            workspace_web_search: true,
            workspace_mcp: true,
            workspace_settings: true,
            runtime_sessions: true,
            host_usage: cfg!(feature = "usage"),
            workspace_skills: true,
            workspace_templates: true,
            workspace_reload: true,
            workspace_trust: true,
            permission_mode: true,
            ..Default::default()
        }
    }
    async fn providers(&self, p: ProvidersParams) -> BackendResult<ProvidersResult> {
        if let Some(remote) = self.1 {
            return remote.providers(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        let mut result = ProvidersResult::default();
        match p.action {
            HostAction::List => {
                result.providers = r
                    .connected_providers()
                    .into_iter()
                    .map(|id| ProviderInfo {
                        name: id.clone(),
                        id,
                        connected: true,
                        model_count: None,
                    })
                    .collect();
            }
            HostAction::Connect => {
                let login: crate::config::ProviderLoginRequest = serde_json::from_value(
                    p.login.ok_or_else(|| failed("missing provider login"))?,
                )
                .map_err(failed)?;
                let (id, name, count) = match &login {
                    crate::config::ProviderLoginRequest::WebSearch { id, api_key } => (
                        id.clone(),
                        r.connect_web_search(id, api_key).await.map_err(failed)?,
                        None,
                    ),
                    _ => {
                        let connected = r.connect_provider(&login).await.map_err(failed)?;
                        (
                            login.provider_id(),
                            connected.name,
                            Some(connected.model_count),
                        )
                    }
                };
                result.providers.push(ProviderInfo {
                    id,
                    name,
                    connected: true,
                    model_count: count,
                });
            }
            HostAction::Disconnect => {
                let id = p.provider.ok_or_else(|| failed("missing provider name"))?;
                let name = r.logout_provider(&id).await.map_err(failed)?;
                result.providers.push(ProviderInfo {
                    id,
                    name,
                    connected: false,
                    model_count: None,
                });
                result.notices.push(r.web_status());
            }
            _ => return Err(failed("use list, connect or disconnect")),
        }
        Ok(result)
    }
    #[cfg(feature = "telemetry")]
    async fn tracing(&self, p: TracingParams) -> BackendResult<TracingResult> {
        if let Some(remote) = self.1 {
            return remote.tracing(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        let state = match p.action {
            HostAction::Status => r
                .tracing_status()
                .ok_or_else(|| failed("tracing is not available in this mode"))?,
            HostAction::Enable => r.set_tracing_enabled(true).map_err(failed)?,
            HostAction::Disable => r.set_tracing_enabled(false).map_err(failed)?,
            HostAction::Setup => {
                let setup = serde_json::from_value(
                    p.config.ok_or_else(|| failed("missing tracing setup"))?,
                )
                .map_err(failed)?;
                r.setup_tracing(&setup).map_err(failed)?
            }
            _ => return Err(failed("use status, enable, disable or setup")),
        };
        Ok(TracingResult {
            enabled: r.settings.tracing.enabled,
            backend: format!("{:?}", r.settings.tracing.backend),
            status: crate::config::describe_tracing_status(&state),
        })
    }
    async fn mcp_auth(&self, p: McpAuthParams) -> BackendResult<McpAuthResult> {
        if let Some(remote) = self.1 {
            return remote.mcp_auth(p).await;
        }
        let guard = self.0.lock().await;
        let r = &**guard;
        Self::workspace(r, &p.workspace_id)?;
        let cfg =
            r.settings.mcp_servers.get(&p.name).ok_or_else(|| {
                failed(format!("mcp server not found: {} (see /mcp list)", p.name))
            })?;
        let status = match p.action {
            HostAction::Logout => {
                if r.mcp_client
                    .clear_oauth_credentials(&p.name)
                    .await
                    .map_err(failed)?
                {
                    r.mcp_client.disconnect(&p.name).await;
                    r.refresh_mcp_tools().await.map_err(failed)?;
                    "removed"
                } else {
                    "absent"
                }
            }
            HostAction::Login => {
                let url = cfg
                    .url
                    .as_deref()
                    .ok_or_else(|| failed("local command servers do not use OAuth"))?;
                let result = r
                    .mcp_client
                    .oauth_login(&p.name, url, |url| {
                        let _ = crate::app::open_link(url);
                    })
                    .await
                    .map_err(failed)?;
                match result {
                    loop_mcp::OAuthLoginOutcome::Authorized => "authorized",
                    loop_mcp::OAuthLoginOutcome::NotRequired => "not_required",
                }
            }
            _ => return Err(failed("use login or logout")),
        };
        Ok(McpAuthResult {
            status: status.into(),
            notices: vec![],
        })
    }
    async fn web_search(&self, p: WebSearchParams) -> BackendResult<WebSearchResult> {
        if let Some(remote) = self.1 {
            return remote.web_search(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        let status = if let Some(provider) = p.provider {
            r.set_web_provider(&provider).await.map_err(failed)?
        } else if let Some(enabled) = p.enabled {
            r.set_web_enabled(enabled).await.map_err(failed)?
        } else {
            r.web_status()
        };
        Ok(WebSearchResult {
            enabled: !r.web_disabled_for_run,
            provider: r
                .settings
                .web_search
                .as_ref()
                .and_then(|w| w.provider.clone()),
            status,
            ..Default::default()
        })
    }
    async fn defaults(&self, p: DefaultsParams) -> BackendResult<DefaultsResult> {
        if let Some(remote) = self.1 {
            return remote.defaults(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        if let Some(id) = p.workspace_id {
            Self::workspace(r, &id)?;
        }
        let path = crate::config::paths::settings_path(&r.agent_dir);
        let mut global = crate::config::Settings::load_file(&path).map_err(failed)?;
        if let Some(patch) = p.patch {
            let mut native_model = None;
            if let Some(spec) = &patch.model {
                let (provider, id) = spec
                    .split_once('/')
                    .ok_or_else(|| failed("choose a model as provider/model"))?;
                if r.is_subscription(provider) {
                    if !r.agents.models(provider).iter().any(|m| m.id == id) {
                        return Err(failed("subscription model not found"));
                    }
                } else {
                    use loop_ai::CredentialStore;
                    if r.credentials.is_logged_out(provider) {
                        return Err(failed(format!(
                            "{provider} is logged out; run /login {provider}"
                        )));
                    }
                    native_model = Some(
                        r.models
                            .get_model(provider, id)
                            .ok_or_else(|| failed("model not found"))?,
                    );
                }
                global.set_selected_model(provider, id);
            }
            if let Some(effort) = &patch.effort {
                if r.active_agent.is_none()
                    && !loop_agent::effort::NATIVE_EFFORTS.contains(&effort.as_str())
                {
                    return Err(failed("unknown effort"));
                }
                global.default_thinking_level = effort.clone();
            }
            if let Some(ms) = patch.response_header_timeout_ms {
                global.response_header_timeout_ms = ms;
            }
            global.save_file(&path).map_err(failed)?;
            r.settings.default_provider = global.default_provider.clone();
            r.settings.default_model = global.default_model.clone();
            r.settings.default_thinking_level = global.default_thinking_level.clone();
            r.settings.response_header_timeout_ms = global.response_header_timeout_ms;
            if let Some(model) = native_model {
                r.native.set_default_model(Some(model));
            }
            if let Some(effort) = patch.effort {
                if let Some(agent) = &r.active_agent {
                    if !effort.is_empty() {
                        r.agents.remember_effort(agent, &effort).map_err(failed)?;
                    }
                } else {
                    r.native
                        .set_default_effort(loop_agent::effort::parse_thinking(&effort));
                }
            }
            if let Some(ms) = patch.response_header_timeout_ms {
                r.native.set_default_response_timeout(ms).await;
            }
        }
        Ok(DefaultsResult {
            model: global.selected_model().map(|(p, m)| format!("{p}/{m}")),
            effort: Some(global.default_thinking_level),
            response_header_timeout_ms: Some(global.response_header_timeout_ms),
            ..Default::default()
        })
    }
    async fn settings(&self, p: SettingsParams) -> BackendResult<SettingsResult> {
        if let Some(remote) = self.1 {
            return remote.settings(p).await;
        }
        let guard = self.0.lock().await;
        let r = &**guard;
        Self::workspace(r, &p.workspace_id)?;
        Ok(SettingsResult {
            effective: r.settings.public_view(),
            enabled_models: r.settings.enabled_models.clone(),
            resource_counts: std::collections::BTreeMap::from([
                ("skills".into(), r.resources.skills.len() as u32),
                ("templates".into(), r.resources.prompts.len() as u32),
            ]),
            ..Default::default()
        })
    }
    async fn sessions(&self) -> BackendResult<RuntimeSessionsResult> {
        if let Some(remote) = self.1 {
            return remote.sessions().await;
        }
        let guard = self.0.lock().await;
        let r = &**guard;
        let mut sessions = vec![];
        let native = r.native.current_session().await;
        let active = r.session_ref();
        let mut loaded: Vec<(Arc<dyn AgentBackend>, SessionRef)> =
            vec![(r.native.clone(), native.clone())];
        if active != native {
            loaded.push((r.backend.clone(), active));
        }
        let mut totals = RuntimeSessionTotals::default();
        for (backend, session) in loaded {
            let stats = match backend.session_stats() {
                Some(s) => s.stats(&session).await.ok(),
                None => None,
            };
            let busy = backend.is_busy(&session);
            let tokens = stats.as_ref().map(|s| s.total_tokens);
            let state = stats.and_then(|s| s.turn_state).unwrap_or_else(|| {
                if busy {
                    "busy".into()
                } else {
                    "idle".into()
                }
            });
            totals.sessions += 1;
            totals.busy += u32::from(busy);
            if let Some(tokens) = tokens {
                totals.tokens += tokens;
            } else {
                totals.unknown_tokens += 1;
            }
            let model = if session == native {
                r.native.config().config.model
            } else {
                r.active_agent
                    .as_ref()
                    .zip(r.agent_model.as_ref())
                    .map(|(agent, model)| format!("{agent}/{model}"))
            };
            sessions.push(RuntimeSessionRow {
                session,
                workspace_id: loop_runtime::workspace_id(
                    &r.cwd.canonicalize().unwrap_or_else(|_| r.cwd.clone()),
                ),
                state,
                model,
                tokens,
            });
        }
        Ok(RuntimeSessionsResult {
            sessions,
            totals,
            taken_at_ms: chrono::Utc::now().timestamp_millis() as u64,
        })
    }
    #[cfg(feature = "usage")]
    async fn usage(&self, p: UsageParams) -> BackendResult<UsageResult> {
        if let Some(remote) = self.1 {
            return remote.usage(p).await;
        }
        let guard = self.0.lock().await;
        let r = &**guard;
        r.flush_usage_log();
        crate::runtime_cmd::usage_page(&crate::config::paths::usage_dir(&r.agent_dir), &p)
    }
    async fn skills(&self, p: SkillsParams) -> BackendResult<SkillsResult> {
        if let Some(remote) = self.1 {
            return remote.skills(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        match p.action {
            AdminAction::Enable | AdminAction::Disable => {
                let name = p.name.ok_or_else(|| failed("missing skill name"))?;
                if p.action == AdminAction::Disable && r.active_skills.contains(&name) {
                    r.native
                        .skill(loop_protocol::session_services::SkillParams {
                            session: r.native.current_session().await,
                            name: name.clone(),
                            active: false,
                            expected_revision: None,
                        })
                        .await?;
                }
                if !r
                    .set_skill_enabled(&name, p.action == AdminAction::Enable)
                    .await
                    .map_err(failed)?
                {
                    return Err(failed(format!("skill not found: {name}")));
                }
            }
            AdminAction::Reload => return Err(failed("use workspace/reload")),
            AdminAction::List => {}
        }
        Ok(SkillsResult {
            skills: r
                .resources
                .all_skills()
                .into_iter()
                .map(|(s, enabled)| SkillStatus {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    enabled,
                })
                .collect(),
            ..Default::default()
        })
    }
    async fn templates(&self, p: TemplatesParams) -> BackendResult<TemplatesResult> {
        if let Some(remote) = self.1 {
            return remote.templates(p).await;
        }
        let guard = self.0.lock().await;
        let r = &**guard;
        Self::workspace(r, &p.workspace_id)?;
        match p.name {
            Some(name) => {
                let template = r
                    .resources
                    .prompts
                    .iter()
                    .find(|t| t.name == name)
                    .ok_or_else(|| failed(format!("template not found: {name}")))?;
                Ok(TemplatesResult {
                    text: Some(loop_agent::harness::prompt_templates::substitute_args(
                        &template.body,
                        &p.args,
                    )),
                    ..Default::default()
                })
            }
            None => Ok(TemplatesResult {
                templates: Some(
                    r.resources
                        .prompts
                        .iter()
                        .map(|p| TemplateInfo {
                            name: p.name.clone(),
                            description: p.argument_hint.as_ref().map_or_else(
                                || "prompt template".into(),
                                |h| format!("{h} — prompt template"),
                            ),
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
        }
    }
    async fn reload(&self, p: ReloadParams) -> BackendResult<ReloadResult> {
        if let Some(remote) = self.1 {
            return remote.reload(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        let _ = r
            .models
            .refresh(loop_ai::ModelsRefreshOptions {
                allow_network: Some(true),
                force: true,
                provider_id: None,
            })
            .await;
        // The loader of a fresh open: project folders through the workspace env.
        let env = loop_agent::harness::HostExecutionEnv::new(&r.cwd);
        r.resources = crate::resources::load_resources_in(
            &env,
            &r.agent_dir,
            &r.cwd,
            r.project_trusted,
            &r.settings,
        )
        .await;
        r.sync_harness_resources().await;
        Ok(ReloadResult::default())
    }
    async fn mcp(&self, p: McpParams) -> BackendResult<McpResult> {
        if let Some(remote) = self.1 {
            return remote.mcp(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        let mut errors = std::collections::HashMap::new();
        match p.action {
            AdminAction::Enable | AdminAction::Disable => {
                let name = p
                    .name
                    .as_deref()
                    .ok_or_else(|| failed("missing MCP server name"))?;
                match r
                    .set_mcp_server_enabled(name, p.action == AdminAction::Enable)
                    .await
                    .map_err(failed)?
                {
                    loop_app_core::runtime::McpToggleOutcome::Missing => {
                        return Err(failed(format!(
                            "mcp server not found: {name} (see /mcp list)"
                        )))
                    }
                    loop_app_core::runtime::McpToggleOutcome::ConnectFailed(error) => {
                        errors.insert(name.to_string(), error);
                    }
                    loop_app_core::runtime::McpToggleOutcome::Updated => {}
                }
            }
            AdminAction::Reload => {
                r.mcp_client.disconnect_all().await;
                let entries = loop_app_core::runtime::enabled_mcp_server_entries(
                    &r.settings.mcp_servers,
                    &r.settings.disabled_mcp_servers,
                );
                for (name, result) in r.mcp_client.connect_all(&entries).await {
                    if let Err(error) = result {
                        errors.insert(name, error);
                    }
                }
                r.refresh_mcp_tools().await.map_err(failed)?;
            }
            AdminAction::List => {}
        }
        let connected: std::collections::HashMap<_, _> =
            r.mcp_client.list_connections().await.into_iter().collect();
        Ok(McpResult {
            servers: r
                .settings
                .mcp_servers
                .keys()
                .map(|name| McpServerStatus {
                    name: name.clone(),
                    enabled: !r.settings.disabled_mcp_servers.contains(name),
                    connected: connected.contains_key(name),
                    tools: connected.get(name).copied().unwrap_or(0) as u32,
                    error: errors.remove(name),
                })
                .collect(),
            ..Default::default()
        })
    }
    async fn permission_mode(
        &self,
        p: PermissionModeParams,
    ) -> BackendResult<PermissionModeResult> {
        if let Some(remote) = self.1 {
            return remote.permission_mode(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        if p.session != r.session_ref() {
            return Err(failed("unknown Session"));
        }
        let mut mutation = None;
        if let Some(mode) = p.mode {
            let mode = loop_app_core::permissions::PermissionMode::parse(&mode)
                .ok_or_else(|| failed("unknown permission mode"))?;
            if r.is_native() {
                mutation = Some(
                    r.native
                        .record_permission_mode(&p.session, mode.as_str(), p.expected_revision)
                        .await?,
                );
            }
            r.set_permission_mode(mode).await;
        }
        let mode = r.permission_mode();
        Ok(PermissionModeResult {
            mode: mode.as_str().into(),
            effective_policy: mode.description().into(),
            revision: r.native.config().revision,
            seq: mutation.as_ref().map(|m| m.seq),
            pending: mutation.as_ref().is_some_and(|m| m.pending),
        })
    }
    async fn trust(&self, p: TrustParams) -> BackendResult<TrustResult> {
        if let Some(remote) = self.1 {
            return remote.trust(p).await;
        }
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        if let Some(trusted) = p.trusted {
            r.trust.set(&r.cwd, trusted).map_err(failed)?;
            r.project_trusted = trusted;
            let env = loop_agent::harness::HostExecutionEnv::new(&r.cwd);
            r.resources = crate::resources::load_resources_in(
                &env,
                &r.agent_dir,
                &r.cwd,
                trusted,
                &r.settings,
            )
            .await;
            r.sync_harness_resources().await;
        }
        Ok(TrustResult {
            trusted: r.project_trusted,
            ..Default::default()
        })
    }
}

/// Starts the sandbox a test asks for.
#[cfg(test)]
pub(crate) type SandboxStarter =
    Arc<dyn Fn(&SandboxChoice) -> BackendResult<loop_agent::harness::SandboxMode> + Send + Sync>;

/// An owned sandbox service lets the terminal keep draining input during startup.
pub(super) struct NativeSandbox {
    pub native: Arc<loop_agent::native_backend::NativeBackend>,
    pub harness: Arc<loop_agent::harness::AgentHarness>,
    pub cwd: std::path::PathBuf,
    pub agent_dir: std::path::PathBuf,
    pub settings: crate::config::settings::SandboxSettings,
    pub web: loop_app_core::web_tools::WebTools,
    pub search: loop_agent::harness::SearchFilter,
    pub bash: loop_agent::harness::BashLimits,
    /// Tests start their own sandboxes instead of a platform one.
    #[cfg(test)]
    pub starter: Option<SandboxStarter>,
}
#[async_trait::async_trait]
impl RuntimeAdmin for NativeSandbox {
    fn offered(&self) -> Capabilities {
        Capabilities {
            session_sandbox: true,
            ..Default::default()
        }
    }
    async fn sandbox(&self, p: SandboxParams) -> BackendResult<SandboxResult> {
        use loop_agent::harness::SandboxMode;
        if self.native.current_session().await != p.session {
            return Err(BackendError::SessionNotFound(p.session.key));
        }
        self.native.check_session_revision(p.expected_revision)?;
        // The Session's own choice when it made one, else the settings'.
        let mut choice = self
            .native
            .recorded_sandbox()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_else(|| SandboxChoice {
                mode: self.settings.mode.clone(),
                isolation: self.settings.isolation.clone(),
                runtime: self.settings.runtime.clone(),
            });
        let mut note = String::new();
        let mut mutation = None;
        if p.action == SandboxAction::Set {
            choice = p
                .config
                .ok_or_else(|| failed("missing sandbox configuration"))?;
            let mode = self.start(&choice).await?;
            // A running turn and its tool calls keep the sandbox they
            // started with; the next turn starts in the new one.
            let slot = Arc::new(std::sync::Mutex::new(Some(mode)));
            let deferred = Arc::clone(&slot);
            let harness = Arc::clone(&self.harness);
            let native = Arc::clone(&self.native);
            let web = self.web.clone();
            let search = self.search.clone();
            let bash = self.bash;
            let job: loop_agent::native_backend::NextTurnJob = Box::new(move |apply| {
                Box::pin(async move {
                    let mode = deferred.lock().unwrap().take();
                    match mode {
                        // A failed rebuild is rolled back inside; nobody
                        // is waiting to hear about it.
                        Some(mode) if apply => {
                            if install_sandbox(&harness, &web, bash, search, mode)
                                .await
                                .is_ok()
                            {
                                native.sandbox_applied();
                            }
                        }
                        Some(SandboxMode::Enabled { sandbox }) => {
                            let _ = sandbox.destroy().await;
                        }
                        _ => {}
                    }
                })
            });
            let json = serde_json::to_value(&choice).map_err(failed)?;
            let deferred = self
                .native
                .defer_sandbox(&p.session, SANDBOX_JOB, json.clone(), None, job)
                .await;
            let deferred = match deferred {
                Ok(deferred) => deferred,
                Err(error) => {
                    // Nothing was queued: release what was started.
                    let started = slot.lock().unwrap().take();
                    if let Some(SandboxMode::Enabled { sandbox }) = started {
                        let _ = sandbox.destroy().await;
                    }
                    return Err(error);
                }
            };
            match deferred {
                Ok(queued) => {
                    mutation = Some(queued);
                    note = "\nThe new sandbox applies from the next turn.".into();
                }
                Err(_idle_job) => {
                    let mode = slot.lock().unwrap().take().expect("the job did not run");
                    install_sandbox(
                        &self.harness,
                        &self.web,
                        self.bash,
                        self.search.clone(),
                        mode,
                    )
                    .await?;
                    mutation = Some(
                        self.native
                            .record_sandbox(&p.session, json, None, false)
                            .await?,
                    );
                }
            }
        }
        Ok(SandboxResult {
            config: choice,
            info: format!("{}{note}", self.harness.sandbox_info().await.format_box()),
            revision: mutation
                .as_ref()
                .map_or_else(|| self.native.revision(), |m| m.revision),
            seq: mutation.map(|m| m.seq),
        })
    }
}

impl NativeSandbox {
    /// Start the sandbox `choice` asks for.
    async fn start(
        &self,
        choice: &SandboxChoice,
    ) -> BackendResult<loop_agent::harness::SandboxMode> {
        use loop_agent::harness::SandboxMode;
        use loop_app_core::sandbox_builder::{SandboxBuilder, SandboxRequest};
        #[cfg(test)]
        if let Some(starter) = self.starter.as_ref() {
            return starter(choice);
        }
        match choice.mode.as_str() {
            "off" => Ok(SandboxMode::Disabled),
            "local" => {
                let mut settings = self.settings.clone();
                settings.mode = choice.mode.clone();
                settings.isolation = choice.isolation.clone();
                settings.runtime = choice.runtime.clone();
                let request = SandboxRequest::from_settings(&settings).map_err(failed)?;
                let builder =
                    SandboxBuilder::for_current_platform().with_agent_dir(&self.agent_dir);
                let (sandbox, _) = builder.start(&self.cwd, &request).await.map_err(failed)?;
                Ok(SandboxMode::Enabled { sandbox })
            }
            _ => Err(failed("unknown sandbox mode")),
        }
    }

    /// Put a reopened Session back in the sandbox it last chose, if that is
    /// not the one it runs in. Nothing has run in it yet.
    pub(super) async fn restore(&self) -> BackendResult<()> {
        let Some(choice) = self
            .native
            .recorded_sandbox()
            .and_then(|v| serde_json::from_value::<SandboxChoice>(v).ok())
        else {
            return Ok(());
        };
        let default = SandboxChoice {
            mode: self.settings.mode.clone(),
            isolation: self.settings.isolation.clone(),
            runtime: self.settings.runtime.clone(),
        };
        if choice == default {
            return Ok(());
        }
        let mode = self.start(&choice).await?;
        install_sandbox(
            &self.harness,
            &self.web,
            self.bash,
            self.search.clone(),
            mode,
        )
        .await
    }
}

/// Key of the sandbox change waiting for the next turn.
const SANDBOX_JOB: &str = "sandbox";

/// Switch the harness to `mode` and rebuild its tools in it. A failed rebuild
/// puts the previous sandbox back.
async fn install_sandbox(
    harness: &loop_agent::harness::AgentHarness,
    web: &loop_app_core::web_tools::WebTools,
    bash: loop_agent::harness::BashLimits,
    search: loop_agent::harness::SearchFilter,
    mode: loop_agent::harness::SandboxMode,
) -> BackendResult<()> {
    use loop_agent::harness::SandboxMode;
    let previous = harness.replace_sandbox(mode).await;
    let tools = async {
        let env = harness.tool_env().await.map_err(failed)?;
        harness
            .set_tools(crate::build_tools(env, web, bash, search))
            .await
            .map_err(failed)
    }
    .await;
    if let Err(error) = tools {
        let rejected = harness.replace_sandbox(previous).await;
        if let SandboxMode::Enabled { sandbox } = rejected {
            let _ = sandbox.destroy().await;
        }
        return Err(error);
    }
    if let SandboxMode::Enabled { sandbox } = previous {
        let _ = sandbox.destroy().await;
    }
    Ok(())
}
