//! Resource tiers shared by standalone bootstrap and the future Runtime.
use super::*;
use loop_agent::harness::{ExecutionEnv, SessionRepository};
use std::collections::HashMap;
use tokio::sync::{Mutex, OnceCell};

/// Process-wide credentials, model catalog and Session repository. Never reads stdin.
pub struct GlobalResources {
    pub agent_dir: PathBuf,
    pub credentials: Arc<FileCredentialStore>,
    pub models: Arc<Models>,
    pub sessions_db: PathBuf,
    pub repository: SessionRepository,
}

impl GlobalResources {
    /// Load cached models and optionally refresh catalogs in the background.
    pub async fn load(
        agent_dir: PathBuf,
        providers: &[crate::config::CustomProviderEntry],
        refresh_network: bool,
    ) -> anyhow::Result<Self> {
        ensure_agent_dirs(&agent_dir)?;
        let credentials = Arc::new(FileCredentialStore::open(auth_path(&agent_dir))?);
        let models = build_models(&agent_dir, Arc::clone(&credentials), providers)?;
        // Hydrate every provider from models-store.json before resolving a model. Without
        // this, `--print` races the background `/v1/models` refresh.
        let _ = models
            .refresh(ModelsRefreshOptions {
                allow_network: Some(false),
                force: false,
                provider_id: None,
            })
            .await;
        if refresh_network {
            // Network catalog refresh in the background so interactive startup
            // isn't blocked by a slow API. Cached models are already in memory.
            let bg_models = Arc::clone(&models);
            tokio::spawn(async move {
                let refresh = bg_models
                    .refresh(ModelsRefreshOptions {
                        allow_network: Some(true),
                        force: true,
                        // Providers without a key stay on their cache (see the fetcher).
                        provider_id: None,
                    })
                    .await;
                for (pid, err) in &refresh.errors {
                    tracing::warn!("model refresh {pid}: {err}");
                }
            });
        }

        Self::from_parts(agent_dir, credentials, models)
    }

    /// Inject a model catalog, including a faux provider, without network refresh.
    pub fn with_models(agent_dir: PathBuf, models: Arc<Models>) -> anyhow::Result<Self> {
        ensure_agent_dirs(&agent_dir)?;
        let credentials = Arc::new(FileCredentialStore::open(auth_path(&agent_dir))?);
        Self::from_parts(agent_dir, credentials, models)
    }

    fn from_parts(
        agent_dir: PathBuf,
        credentials: Arc<FileCredentialStore>,
        models: Arc<Models>,
    ) -> anyhow::Result<Self> {
        let sessions_db = sessions_db_path(&agent_dir);
        let store = create_sqlite_session_store(&sessions_db)
            .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
        Ok(Self {
            agent_dir,
            credentials,
            models,
            sessions_db,
            repository: create_session_repository(store, None),
        })
    }
}

/// Immutable workspace definitions and shared connections, cached by absolute cwd.
/// Gates, hooks registries, selected tools and grants belong to each Session.
pub struct WorkspaceResources {
    pub cwd: PathBuf,
    pub project_trusted: bool,
    pub settings: Settings,
    pub resources: LoadedResources,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub context_files: Vec<ContextFile>,
    pub host: Arc<dyn ExecutionEnv>,
    pub mcp_client: Arc<loop_mcp::McpClientManager>,
    pub startup_notices: Vec<String>,
    // Standalone prompts retain the caller's spelling and lexical ancestors;
    // cwd is separately normalized for workspace identity and execution.
    prompt_cwd: PathBuf,
    hooks: crate::hooks_load::LoadedHooks,
    web: crate::web_tools::WebTools,
    sandbox: OnceCell<(SandboxMode, Arc<dyn ExecutionEnv>, bool)>,
    standalone: bool,
}

fn absolute_workspace(cwd: &Path) -> anyhow::Result<PathBuf> {
    if !cwd.is_absolute() {
        bail!("workspace cwd must be absolute: {}", cwd.display());
    }
    Ok(cwd.canonicalize()?)
}

impl WorkspaceResources {
    /// Load without prompting. Only an explicitly remembered trust decision enables project input.
    pub async fn load(globals: &GlobalResources, opts: &BootstrapOpts) -> anyhow::Result<Self> {
        let cwd = absolute_workspace(&opts.cwd)?;
        let trust = TrustStore::load(trust_path(&globals.agent_dir))?;
        let trusted = trust.get(&cwd) == Some(true);
        let settings = load_settings(&globals.agent_dir, &cwd, trusted)?;
        Self::load_resolved(globals, opts, trusted, settings, false).await
    }

    pub(super) async fn load_resolved(
        globals: &GlobalResources,
        opts: &BootstrapOpts,
        project_trusted: bool,
        mut settings: Settings,
        standalone: bool,
    ) -> anyhow::Result<Self> {
        let cwd = if standalone {
            opts.cwd.canonicalize()?
        } else {
            absolute_workspace(&opts.cwd)?
        };
        let prompt_cwd = if standalone {
            opts.cwd.clone()
        } else {
            cwd.clone()
        };
        if let Some(theme) = &opts.theme {
            settings.theme = theme.clone();
        }
        if settings.sandbox.mode == "local" {
            settings.sandbox.isolation = KrunIsolation::parse(&settings.sandbox.isolation)
                .unwrap_or(KrunIsolation::Full)
                .as_str()
                .into();
            settings.sandbox.runtime = LocalSandboxRuntime::parse(&settings.sandbox.runtime)
                .unwrap_or(LocalSandboxRuntime::Runc)
                .as_str()
                .into();
        } else {
            settings.sandbox.mode = "off".into();
        }
        let resources = load_resources(&globals.agent_dir, &cwd, project_trusted, &settings);
        let hooks = crate::hooks_load::load_hooks(&resources.hook_paths);
        let context_files = if opts.no_context_files {
            vec![]
        } else if standalone || project_trusted {
            load_context_files(&prompt_cwd, &globals.agent_dir)
        } else {
            crate::system_prompt::load_global_context_files(&globals.agent_dir)
        };
        let (custom_system_prompt, append_system_prompt) =
            crate::system_prompt::resolve_system_prompt_files_in(
                &globals.agent_dir,
                &prompt_cwd,
                project_trusted,
                opts.system_prompt.as_deref(),
                opts.append_system_prompt.as_deref(),
            );
        let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&cwd));
        let web = crate::web_tools::WebTools::from_settings(
            &settings,
            Some(Arc::clone(&globals.credentials)),
            opts.no_web,
        );
        let mcp_client = Arc::new(loop_mcp::McpClientManager::with_oauth_dir(
            globals.agent_dir.join("mcp-auth"),
        ));
        let entries =
            enabled_mcp_server_entries(&settings.mcp_servers, &settings.disabled_mcp_servers);
        if !entries.is_empty() {
            for (name, result) in mcp_client.connect_all(&entries).await {
                match result {
                    Ok(count) => tracing::info!("mcp: connected to '{name}' ({count} tools)"),
                    Err(e) => tracing::warn!("mcp: failed to connect to '{name}': {e}"),
                }
            }
        }
        let mut startup_notices = Vec::new();
        if !standalone && !project_trusted {
            startup_notices.push(format!(
                "Workspace {} has no recorded trust; using global settings and resources only",
                cwd.display()
            ));
        }
        Ok(Self {
            cwd,
            project_trusted,
            settings,
            resources,
            custom_system_prompt,
            append_system_prompt,
            context_files,
            host,
            web,
            mcp_client,
            startup_notices,
            prompt_cwd,
            hooks,
            sandbox: OnceCell::new(),
            standalone,
        })
    }

    async fn execution(&self) -> anyhow::Result<&(SandboxMode, Arc<dyn ExecutionEnv>, bool)> {
        self.sandbox
            .get_or_try_init(|| async {
                let mut settings = self.settings.clone();
                let (sandbox, tool_env): (SandboxMode, Arc<dyn loop_agent::harness::ExecutionEnv>) =
                    match settings.sandbox.mode.as_str() {
                        "local" => {
                            let isolation = KrunIsolation::parse(&settings.sandbox.isolation)
                                .unwrap_or(KrunIsolation::Full);
                            let oci_runtime = LocalSandboxRuntime::parse(&settings.sandbox.runtime)
                                .unwrap_or(LocalSandboxRuntime::Runc);
                            settings.sandbox.isolation = isolation.as_str().into();
                            settings.sandbox.runtime = oci_runtime.as_str().into();
                            let sb = KrunSandbox::new(KrunSandbox::config_for(
                                self.cwd.clone(),
                                isolation,
                                oci_runtime,
                            ));
                            match sb.start().await {
                                Ok(()) => {
                                    let env = sb.env();
                                    (
                                        SandboxMode::Enabled {
                                            sandbox: Arc::new(sb),
                                        },
                                        env,
                                    )
                                }
                                Err(e) => {
                                    if !self.standalone {
                                        return Err(anyhow::anyhow!(
                                            "required workspace sandbox failed: {e}"
                                        ));
                                    }
                                    tracing::warn!("local sandbox disabled at startup: {e}");
                                    eprintln!("warning: local sandbox not enabled:\n{e}");
                                    settings.sandbox.mode = "off".into();
                                    (SandboxMode::Disabled, Arc::clone(&self.host))
                                }
                            }
                        }
                        _ => {
                            settings.sandbox.mode = "off".into();
                            (SandboxMode::Disabled, Arc::clone(&self.host))
                        }
                    };

                let disabled = settings.sandbox.mode == "off";
                Ok((sandbox, tool_env, disabled))
            })
            .await
    }
}

/// Builds independent Sessions over shared global and workspace resources.
pub struct SessionFactory {
    pub globals: Arc<GlobalResources>,
    workspaces: Mutex<HashMap<PathBuf, Arc<OnceCell<Arc<WorkspaceResources>>>>>,
}

/// One Session's mutable harness, gate and approval state.
pub struct OpenedSession {
    pub workspace: Arc<WorkspaceResources>,
    pub harness: Arc<AgentHarness>,
    pub native: Arc<loop_agent::native_backend::NativeBackend>,
    pub tool_gate: Arc<crate::tool_gate::ToolGate>,
    pub approval_bridge: Arc<crate::tool_approval::ToolApprovalBridge>,
    pub approval_prompts:
        tokio::sync::mpsc::UnboundedReceiver<crate::tool_approval::ApprovalPrompt>,
    pub settings: Settings,
    pub session_id: String,
    pub selected_model: Option<loop_ai::Model>,
    pub model_note: Option<String>,
    pub startup_notices: Vec<String>,
}

impl SessionFactory {
    pub fn new(globals: Arc<GlobalResources>) -> Self {
        Self {
            globals,
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    /// Cache by normalized cwd. Workspace flags are fixed by the first load;
    /// model, permission mode and Session selection remain per call to `open`.
    pub async fn workspace(&self, opts: &BootstrapOpts) -> anyhow::Result<Arc<WorkspaceResources>> {
        let cwd = absolute_workspace(&opts.cwd)?;
        let slot = {
            let mut workspaces = self.workspaces.lock().await;
            Arc::clone(workspaces.entry(cwd).or_default())
        };
        let workspace = slot
            .get_or_try_init(|| async {
                WorkspaceResources::load(&self.globals, opts)
                    .await
                    .map(Arc::new)
            })
            .await?;
        Ok(Arc::clone(workspace))
    }

    pub async fn open(
        &self,
        workspace: Arc<WorkspaceResources>,
        opts: &BootstrapOpts,
    ) -> anyhow::Result<OpenedSession> {
        if absolute_workspace(&opts.cwd)? != workspace.cwd {
            bail!("Session cwd does not match workspace");
        }
        let globals = &self.globals;
        let models = Arc::clone(&globals.models);
        let credentials = Arc::clone(&globals.credentials);
        let mut settings = workspace.settings.clone();
        let agents = crate::agents::AgentManager::new(
            crate::agents::agent_entries(&settings.agents),
            workspace.cwd.clone(),
            &globals.agent_dir,
        );
        // A saved subscription model is started by the surface, not Loop's agent.
        let saved = settings
            .selected_model()
            .filter(|(provider, _)| agents.entry(provider).is_none());
        let (mut model, mut model_note) = match crate::model_selection::resolve_startup_model(
            &models,
            saved,
            opts.provider.as_deref(),
            opts.model.as_deref(),
        )? {
            StartupModel::Selected(model) => (Some(*model), None),
            StartupModel::NotSelected { reason } => (None, reason),
        };
        if model
            .as_ref()
            .is_some_and(|m| credentials.is_logged_out(&m.provider))
        {
            if opts.provider.is_some() || opts.model.is_some() {
                bail!("selected provider is logged out; run /login before selecting its model");
            }
            model = None;
            model_note = Some("saved provider is logged out; use /login or /model".into());
        }
        if let Some(m) = &model {
            settings.set_selected_model(&m.provider, &m.id);
        }

        // Standalone Surfaces own their writer lease; Runtime callers manage
        // ownership separately before opening through the shared repository.
        let standalone_repository = if workspace.standalone {
            let store =
                loop_agent::harness::session::sqlite::create_sqlite_session_store_for_resume(
                    &globals.sessions_db,
                    opts.session_id.as_deref(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
            Some(create_session_repository(store, None))
        } else {
            None
        };
        let repository = standalone_repository
            .as_ref()
            .unwrap_or(&globals.repository);

        let (sandbox, tool_env, disabled) = workspace.execution().await?;
        let sandbox = sandbox.clone();
        let tool_env = Arc::clone(tool_env);
        if *disabled {
            settings.sandbox.mode = "off".into();
        }
        let resumed = opts.session_id.is_some();
        let session = if let Some(id) = &opts.session_id {
            repository
                .open(id)
                .await
                .map_err(|e| anyhow::anyhow!("load session: {e}"))?
        } else {
            repository
                .create(Some(workspace.cwd.to_string_lossy().into_owned()), None)
                .await
                .map_err(|e| anyhow::anyhow!("create session: {e}"))?
        };

        // Restore model from the session branch when resuming (unless CLI overrides).
        if resumed {
            if let Ok(ctx) = session.build_context().await {
                if opts.provider.is_none() && opts.model.is_none() {
                    if let Some((p, m)) = &ctx.model {
                        if let Some(resolved) = models
                            .get_model(p, m)
                            .filter(|_| !credentials.is_logged_out(p))
                        {
                            settings.set_selected_model(p, m);
                            model = Some(resolved);
                            model_note = None;
                        }
                    }
                }
                // Thinking-level changes are recorded on the branch when present.
                if !matches!(ctx.thinking_level, AgentThinkingLevel::Off) {
                    settings.default_thinking_level = thinking_label(ctx.thinking_level).into();
                }
            }
        }

        let bash_limits = settings.bash.unwrap_or_default().limits();
        let tool_gate = crate::tool_gate::ToolGate::new_for_workspace(
            crate::tool_approval::permissions_from_settings(&settings.tool_permissions),
            &workspace.cwd,
        );
        let tools = build_tools(
            Arc::clone(&tool_env),
            &workspace.web,
            bash_limits,
            tool_gate.search_filter(),
        );

        // The prompt lists the tools that were actually built (web_search may be
        // unconfigured), so it can never promise a tool the agent cannot call.
        let snippets = default_tool_snippets();
        let selected: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let system_prompt = build_system_prompt(BuildSystemPromptOptions {
            custom_prompt: workspace.custom_system_prompt.as_deref(),
            append_system_prompt: workspace.append_system_prompt.as_deref(),
            cwd: &workspace.prompt_cwd,
            selected_tools: &selected,
            tool_snippets: &snippets,
            context_files: &workspace.context_files,
        });

        let session_id = session.metadata().id.clone();
        let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
            models: Arc::clone(&models),
            model: model.clone(),
            session,
            host_env: Arc::clone(&workspace.host),
            tools,
            system_prompt,
            sandbox,
            resources: AgentHarnessResources {
                skills: workspace.resources.skills.clone(),
                prompt_templates: workspace.resources.prompts.clone(),
            },
        }));
        harness
            .set_thinking_level(parse_thinking(&settings.default_thinking_level))
            .await;
        harness.set_bash_limits(bash_limits);
        harness
            .set_response_header_timeout_ms(settings.response_header_timeout_ms)
            .await;
        harness
            .set_timeouts(
                settings.timeouts.clone(),
                settings.timeout_overrides.clone(),
            )
            .await;

        let native = loop_agent::native_backend::NativeBackend::new(Arc::clone(&harness)).await;

        tool_gate.install(&harness);
        let mut startup_notices = workspace.startup_notices.clone();
        startup_notices.extend(crate::hooks_load::register_workspace_hooks(
            &harness,
            workspace.hooks.clone(),
            &workspace.cwd,
        ));
        let (mode, mode_notice) = startup_permission_mode(
            opts.permission_mode.as_deref(),
            settings.permission_mode.as_deref(),
            &settings.file_edit_review,
            opts.interactive,
        )?;
        tool_gate.set_mode(mode);
        startup_notices.extend(mode_notice);
        if mode == crate::permissions::PermissionMode::FullAccess {
            startup_notices.push(
                "⚠ full access: every tool call runs without asking, including shell commands \
             and file deletion (deny rules and hooks still apply)"
                    .into(),
            );
        }
        tool_gate.set_mcp(Arc::clone(&workspace.mcp_client));
        let mcp_tools = loop_agent::harness::mcp::bridge::mcp_tools_to_agent_tools_async(
            workspace.mcp_client.connections(),
        )
        .await;
        if !mcp_tools.is_empty() {
            let mut all_tools = harness.get_tools().await;
            all_tools.extend(mcp_tools);
            harness
                .set_tools(all_tools)
                .await
                .map_err(|e| anyhow::anyhow!("set MCP tools: {e}"))?;
        }
        let grants = harness
            .read_session_entries()
            .await
            .map(|entries| crate::tool_approval::auto_approve_from_entries(&entries))
            .unwrap_or_default();
        let (bridge, approval_prompts) = crate::tool_approval::ToolApprovalBridge::new(
            tool_env,
            settings.diff_editor.clone(),
            grants,
        );
        let approval_bridge = Arc::new(bridge);
        let persist_harness = Arc::downgrade(&harness);
        approval_bridge.set_persist(Arc::new(move |label| {
            let harness = persist_harness.upgrade();
            Box::pin(async move {
                if let Some(harness) = harness {
                    if let Err(e) = harness.append_label(label).await {
                        tracing::warn!("persist tool approval label: {e}");
                    }
                }
            })
        }));
        tool_gate.set_approver(Some(Arc::clone(&approval_bridge)));
        Ok(OpenedSession {
            workspace,
            harness,
            native,
            tool_gate,
            approval_bridge,
            approval_prompts,
            settings,
            session_id,
            selected_model: model,
            model_note,
            startup_notices,
        })
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    #[tokio::test]
    async fn standalone_factory_refuses_runtime_lease_before_opening_session() {
        let dir = tempfile::tempdir().unwrap();
        let models = Arc::new(Models::create(Default::default()));
        let globals =
            Arc::new(GlobalResources::with_models(dir.path().join("agent"), models).unwrap());
        let source = globals.repository.create(None, None).await.unwrap();
        let id = source.metadata().id.clone();
        let store =
            loop_agent::harness::session::sqlite::SqliteSessionStore::open(&globals.sessions_db)
                .unwrap();
        store
            .acquire_lease(
                &id,
                &loop_agent::harness::session::sqlite::LeaseIdentity {
                    owner: "runtime".into(),
                    pid: std::process::id(),
                    generation: 1,
                },
            )
            .await
            .unwrap();
        let opts = BootstrapOpts {
            cwd: dir.path().to_path_buf(),
            provider: None,
            model: None,
            theme: None,
            system_prompt: None,
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: Some(id.clone()),
            no_web: true,
            permission_mode: Some("ask".into()),
            agent: None,
        };
        let workspace = Arc::new(
            WorkspaceResources::load_resolved(&globals, &opts, false, Settings::default(), true)
                .await
                .unwrap(),
        );
        let factory = SessionFactory::new(globals);
        let error = match factory.open(workspace, &opts).await {
            Ok(_) => panic!("standalone resume accepted an owned Session"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(&format!(
                "Session {id} is in use by Runtime {}. Use `loop attach {id}` instead.",
                std::process::id()
            )),
            "{error}"
        );
    }
}
