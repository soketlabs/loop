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
    /// The Runtime's tool lock, fixed for the life of the process: every
    /// workspace and Session built from these resources is locked
    /// ([`crate::permissions::tool_lock`]).
    tool_lock: Option<crate::permissions::tool_lock::RuntimeToolLock>,
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

    /// These resources with the Runtime's tool lock. Call it before the
    /// resources are shared: no later call can remove the lock.
    pub fn with_tool_lock(
        mut self,
        lock: Option<crate::permissions::tool_lock::RuntimeToolLock>,
    ) -> Self {
        if self.tool_lock.is_none() {
            self.tool_lock = lock;
        }
        self
    }

    /// The tool lock in force for every Session, if any.
    pub fn tool_lock(&self) -> Option<crate::permissions::tool_lock::ToolLock> {
        self.tool_lock.as_ref().map(|lock| lock.lock())
    }

    /// The lock with its root, if any.
    pub fn tool_lock_config(&self) -> Option<&crate::permissions::tool_lock::RuntimeToolLock> {
        self.tool_lock.as_ref()
    }

    /// Refuse a Session folder outside the tool lock's root; without a lock
    /// every folder is admitted.
    pub fn admit_cwd(
        &self,
        cwd: &Path,
    ) -> Result<(), crate::permissions::tool_lock::ToolLockError> {
        match &self.tool_lock {
            Some(lock) => lock.admit(cwd).map(|_| ()),
            None => Ok(()),
        }
    }

    fn from_parts(
        agent_dir: PathBuf,
        credentials: Arc<FileCredentialStore>,
        models: Arc<Models>,
    ) -> anyhow::Result<Self> {
        let sessions_db = sessions_db_path(&agent_dir);
        let store = create_sqlite_session_store(&sessions_db)
            .map_err(|e| session_error("sqlite session store", e))?;
        Ok(Self {
            agent_dir,
            credentials,
            models,
            sessions_db,
            repository: create_session_repository(store, None),
            tool_lock: None,
        })
    }
}

/// Store construction at the edge: the Runtime opens no database itself.
impl GlobalResources {
    /// Inject a model catalog and the Session store of the shared repository,
    /// with no SQLite store opened.
    ///
    /// The injected store and `sessions_db` can differ: `sessions_db` still
    /// names the default path, and nothing is created there. The Runtime
    /// never reads `sessions_db` (it uses its injected store). Code that
    /// still reads it: the standalone Surface path of
    /// [`SessionFactory::open`] (`create_sqlite_session_store_for_resume`),
    /// [`Self::open_runtime_session_store`], and `loop_app_core::Runtime`,
    /// which copies it for the Surfaces to show. So do not use these
    /// resources for a standalone Surface.
    pub fn with_models_and_store(
        agent_dir: PathBuf,
        models: Arc<Models>,
        store: Arc<dyn loop_agent::harness::session::SessionStore>,
    ) -> anyhow::Result<Self> {
        ensure_agent_dirs(&agent_dir)?;
        let credentials = Arc::new(FileCredentialStore::open(auth_path(&agent_dir))?);
        Ok(Self {
            sessions_db: sessions_db_path(&agent_dir),
            agent_dir,
            credentials,
            models,
            repository: create_session_repository(store, None),
            tool_lock: None,
        })
    }

    /// Open the SQLite Session store at `sessions_db`, with the leases,
    /// control markers and destinations that a Runtime needs.
    pub fn open_runtime_session_store(
        &self,
    ) -> Result<
        Arc<dyn loop_agent::harness::session::RuntimeSessionStore>,
        loop_agent::harness::types::SessionError,
    > {
        Ok(Arc::new(
            loop_agent::harness::session::sqlite::SqliteSessionStore::open(&self.sessions_db)?,
        ))
    }
}

/// A workspace's sandbox, its tool env and whether the sandbox is off.
type Execution = (SandboxMode, Arc<dyn ExecutionEnv>, bool);

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
    /// The `host` driver's Workspace of this folder; `host` is its env.
    /// Every Session has it, with the sandbox on or off, for the Workspace
    /// methods (processes, transfer) that later protocol methods expose.
    pub host_workspace: Arc<dyn loop_agent::harness::sandbox::Workspace>,
    pub mcp_client: Arc<loop_mcp::McpClientManager>,
    pub startup_notices: Vec<String>,
    // Standalone prompts retain the caller's spelling and lexical ancestors;
    // cwd is separately normalized for workspace identity and execution.
    prompt_cwd: PathBuf,
    agent_dir: PathBuf,
    hooks: crate::hooks_load::LoadedHooks,
    web: crate::web_tools::WebTools,
    // Shared with the workspace's reloads, so a reload never starts a second sandbox.
    sandbox: Arc<OnceCell<Execution>>,
    standalone: bool,
}

/// The started Workspace of the `host` driver for `cwd`, with reads confined
/// under the tool lock.
async fn host_workspace(
    cwd: &Path,
    confine_reads: bool,
) -> anyhow::Result<Arc<dyn loop_agent::harness::sandbox::Workspace>> {
    use loop_agent::harness::sandbox::{WorkspaceSource, WorkspaceSpec, HOST_DRIVER_ID};
    let driver = crate::sandbox_builder::platform_registry()
        .driver(HOST_DRIVER_ID)
        .ok_or_else(|| anyhow::anyhow!("the `{HOST_DRIVER_ID}` workspace driver is missing"))?;
    let workspace = driver
        .create(WorkspaceSpec {
            source: WorkspaceSource::HostFolder(cwd.to_path_buf()),
            options: serde_json::json!({ "confineReads": confine_reads }),
            ..WorkspaceSpec::default()
        })
        .await?;
    // A replaced host driver must still confine reads under the lock.
    if confine_reads && !workspace.capabilities().confines_reads {
        bail!("the `{HOST_DRIVER_ID}` workspace driver does not confine reads for the tool lock");
    }
    workspace.start().await?;
    Ok(workspace)
}

fn absolute_workspace(cwd: &Path) -> anyhow::Result<PathBuf> {
    if !cwd.is_absolute() {
        bail!("workspace cwd must be absolute: {}", cwd.display());
    }
    Ok(cwd.canonicalize()?)
}

impl WorkspaceResources {
    /// Status of Loop's web tools in this workspace; never contains keys.
    pub fn web_status(&self) -> String {
        self.web.status()
    }

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
        // Every workspace a locked Runtime loads, for any Session, fork,
        // child, reload or admin call, passes here: refuse a folder
        // outside the root, and treat every folder as untrusted, so nothing
        // from the folder (project settings, skills, prompts, context
        // files, hooks, system prompt files) loads.
        let mut project_trusted = project_trusted;
        if globals.tool_lock.is_some() {
            globals.admit_cwd(&cwd)?;
            project_trusted = false;
            settings = load_settings(&globals.agent_dir, &cwd, false)?;
        }
        let prompt_cwd = if standalone {
            opts.cwd.clone()
        } else {
            cwd.clone()
        };
        if let Some(theme) = &opts.theme {
            settings.theme = theme.clone();
        }
        // Isolation and runtime stay as the user wrote them, so a later save never
        // turns an inherited platform default into an explicit choice.
        if settings.sandbox.mode != "local" {
            settings.sandbox.mode = "off".into();
        }
        // The workspace env exists before the loaders run: project resources
        // (skills, prompts, context files, system prompt files) are read
        // through it, global ones stay host reads.
        // Under the tool lock the host Workspace confines reads: each file
        // read is checked on the opened file itself, so a path changed into
        // a link between the gate's check and the open is refused. It also
        // starts no process and takes no upload.
        let host_workspace = host_workspace(&cwd, globals.tool_lock().is_some()).await?;
        let host: Arc<dyn ExecutionEnv> = host_workspace.env();
        let mut resources =
            load_resources_in(&*host, &globals.agent_dir, &cwd, project_trusted, &settings).await;
        let mut locked_notices = Vec::new();
        if let Some(lock) = globals.tool_lock() {
            locked_notices = lock_workspace(lock, &mut settings, &mut resources);
        }
        let hooks = crate::hooks_load::load_hooks(&resources.hook_paths);
        let context_files = if opts.no_context_files {
            vec![]
        } else if standalone || project_trusted {
            load_context_files_in(&*host, &prompt_cwd, &globals.agent_dir).await
        } else {
            crate::system_prompt::load_global_context_files(&globals.agent_dir)
        };
        let (custom_system_prompt, append_system_prompt) =
            crate::system_prompt::resolve_system_prompt_files_via(
                &*host,
                &globals.agent_dir,
                &prompt_cwd,
                project_trusted,
                opts.system_prompt.as_deref(),
                opts.append_system_prompt.as_deref(),
            )
            .await;
        let web = crate::web_tools::WebTools::from_settings(
            &settings,
            Some(Arc::clone(&globals.credentials)),
            opts.no_web,
        );
        let mcp_client = Arc::new(loop_mcp::McpClientManager::with_oauth_dir(
            globals.agent_dir.join("mcp-auth"),
        ));
        let entries = if globals.tool_lock().is_some() {
            // No MCP server starts or connects under a tool lock.
            Vec::new()
        } else {
            enabled_mcp_server_entries(&settings.mcp_servers, &settings.disabled_mcp_servers)
        };
        if !entries.is_empty() {
            for (name, result) in mcp_client.connect_all(&entries).await {
                match result {
                    Ok(count) => tracing::info!("mcp: connected to '{name}' ({count} tools)"),
                    Err(e) => tracing::warn!("mcp: failed to connect to '{name}': {e}"),
                }
            }
        }
        let mut startup_notices = settings.project_notices.clone();
        startup_notices.extend(locked_notices);
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
            host_workspace,
            web,
            mcp_client,
            startup_notices,
            prompt_cwd,
            agent_dir: globals.agent_dir.clone(),
            hooks,
            sandbox: Arc::new(OnceCell::new()),
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
                            let builder =
                                crate::sandbox_builder::SandboxBuilder::for_current_platform()
                                    .with_agent_dir(&self.agent_dir);
                            let started =
                                match crate::sandbox_builder::SandboxRequest::from_settings(
                                    &settings.sandbox,
                                ) {
                                    Ok(mut request) => {
                                        // A locked workspace never starts a
                                        // sandbox (`lock_workspace` sets the
                                        // mode off): path rules are the only
                                        // path policy that can apply here.
                                        use crate::sandbox_builder::PathPolicyUse;
                                        let rules = crate::tool_approval::permissions_from_settings(
                                            &settings.tool_permissions,
                                        );
                                        request.path_policy =
                                            if crate::permissions::has_path_rules(&rules) {
                                                PathPolicyUse::Rules
                                            } else {
                                                PathPolicyUse::None
                                            };
                                        builder.start(&self.cwd, &request).await
                                    }
                                    Err(e) => Err(e),
                                };
                            match started {
                                Ok((sb, _plan)) => {
                                    let env = sb.env();
                                    (SandboxMode::Enabled { sandbox: sb }, env)
                                }
                                Err(e) => {
                                    if !self.standalone {
                                        let platform = if cfg!(any(
                                            target_os = "linux",
                                            target_os = "macos"
                                        )) {
                                            ""
                                        } else {
                                            "local sandbox is unsupported on this platform. "
                                        };
                                        return Err(anyhow::anyhow!(
                                            "required workspace sandbox failed: {platform}{e}"
                                        ));
                                    }
                                    // The line below is the user's warning. The log keeps
                                    // the record at warn; its target lets a terminal log
                                    // layer skip it, so it is not printed twice.
                                    tracing::warn!(
                                        target: crate::USER_NOTICE_TARGET,
                                        "local sandbox disabled at startup: {e}"
                                    );
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

impl WorkspaceResources {
    /// Remove this workspace's sandbox container, if one was started. For a
    /// standalone Surface whose startup failed after the sandbox started;
    /// a running Session removes it through its harness instead.
    pub async fn destroy_sandbox(&self) {
        if let Some((SandboxMode::Enabled { sandbox }, _, _)) = self.sandbox.get() {
            if !sandbox.outlives_process() {
                return;
            }
            if let Err(error) = sandbox.destroy().await {
                tracing::warn!(%error, "could not remove the workspace sandbox");
            }
        }
    }
}

/// Builds independent Sessions over shared global and workspace resources.
pub struct SessionFactory {
    pub globals: Arc<GlobalResources>,
    workspaces: Mutex<HashMap<PathBuf, Arc<OnceCell<Arc<WorkspaceResources>>>>>,
}

/// One Session's mutable harness, gate and approval state.
/// A Session store error for people: a Session owned by another process
/// is reported as is; anything else names what failed.
fn session_error(context: &str, error: loop_agent::harness::types::SessionError) -> anyhow::Error {
    match error {
        loop_agent::harness::types::SessionError::InUse(message) => anyhow::anyhow!(message),
        other => anyhow::anyhow!("{context}: {other}"),
    }
}

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
    /// The standalone Surface's own Session store, which holds its leases.
    /// `None` when the Runtime owns ownership (and non-Unix standalone).
    pub session_store: Option<Arc<dyn loop_agent::harness::session::SessionStore>>,
    /// The workspace hooks this Session runs, replaced on reload.
    pub hooks: crate::hooks_load::HookSlot,
}

impl OpenedSession {
    /// Point this Session at `workspace`, a reload of its own: skills and
    /// templates, tools (web, bash limits), system prompt, hooks, MCP
    /// tools and timeouts, as a Session opened there now would have them.
    /// Its execution env, model, effort, permission mode and grants stay,
    /// and so do the tools named in `keep`, which its owner added.
    pub async fn apply_workspace(
        &self,
        workspace: &WorkspaceResources,
        keep: &[&str],
    ) -> anyhow::Result<Vec<String>> {
        let harness = &self.harness;
        let kept: Vec<_> = harness
            .get_tools()
            .await
            .into_iter()
            .filter(|tool| keep.contains(&tool.name.as_str()))
            .collect();
        let settings = &workspace.settings;
        harness
            .set_resources(AgentHarnessResources {
                skills: workspace.resources.skills.clone(),
                prompt_templates: workspace.resources.prompts.clone(),
            })
            .await;
        let env = harness
            .tool_env()
            .await
            .map_err(|e| anyhow::anyhow!("tool env: {e}"))?;
        let bash_limits = settings.bash.unwrap_or_default().limits();
        harness.set_bash_limits(bash_limits);
        harness
            .set_compaction_settings(settings.compaction.harness_settings())
            .await;
        let mut tools = build_tools(
            env,
            &workspace.web,
            bash_limits,
            self.tool_gate.search_filter(),
        );
        let lock = self.tool_gate.tool_lock();
        retain_locked(lock, &mut tools);
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
        tools.extend(
            loop_agent::harness::mcp::bridge::mcp_tools_to_agent_tools_async(
                workspace.mcp_client.connections(),
            )
            .await,
        );
        tools.extend(kept);
        retain_locked(lock, &mut tools);
        harness
            .set_tools(tools)
            .await
            .map_err(|e| anyhow::anyhow!("set tools: {e}"))?;
        harness.set_system_prompt(system_prompt).await;
        harness
            .set_timeouts(
                settings.timeouts.clone(),
                settings.timeout_overrides.clone(),
            )
            .await;
        self.native
            .set_default_response_timeout(settings.response_header_timeout_ms)
            .await;
        self.tool_gate.set_mcp(Arc::clone(&workspace.mcp_client));
        let mut notices = self.hooks.replace(workspace.hooks.clone(), &workspace.cwd);
        notices.extend(workspace.startup_notices.iter().cloned());
        Ok(notices)
    }
}

/// Keep only the tools `lock` offers; all of them without a lock.
fn retain_locked(
    lock: Option<crate::permissions::tool_lock::ToolLock>,
    tools: &mut Vec<loop_agent::AgentTool>,
) {
    if let Some(lock) = lock {
        tools.retain(|tool| crate::permissions::tool_lock::offers(lock, &tool.name));
    }
}

/// Take out of a workspace what can start a process or reach past the
/// lock without a tool call: hooks, Rhai extensions, skills that run code,
/// and private web addresses. MCP servers are not started (see
/// `load_resolved`). Returns the notices for the Session.
fn lock_workspace(
    lock: crate::permissions::tool_lock::ToolLock,
    settings: &mut Settings,
    resources: &mut LoadedResources,
) -> Vec<String> {
    use crate::permissions::tool_lock::skill_runs_code;
    let mut notices = vec![format!(
        "Tool lock `{lock}`: this Runtime offers only reading files in the workspace and \
         the web tools. Shell, file changes, Subagents, workflows, MCP servers and hooks are \
         off."
    )];
    // No sandbox process starts for a locked Session: no tool runs one.
    settings.sandbox.mode = "off".into();
    resources.hook_paths.clear();
    resources.extension_paths.clear();
    let before = resources.skills.len() + resources.disabled_skills.len();
    resources.skills.retain(|skill| !skill_runs_code(skill));
    resources
        .disabled_skills
        .retain(|skill| !skill_runs_code(skill));
    let dropped = before - resources.skills.len() - resources.disabled_skills.len();
    if dropped > 0 {
        notices.push(format!(
            "Tool lock `{lock}`: {dropped} skill(s) with scripts or other files that are not \
             text are off"
        ));
    }
    if !settings.mcp_servers.is_empty() {
        notices.push(format!(
            "Tool lock `{lock}`: {} MCP server(s) are not started",
            settings.mcp_servers.len()
        ));
    }
    if let Some(fetch) = settings.web_fetch.as_mut() {
        fetch.allow_private_addresses = false;
    }
    if let Some(search) = settings.web_search.as_mut() {
        search.allow_private_addresses = false;
    }
    notices
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

    /// The model a Session opened with `opts` in `workspace` starts on, and
    /// why there is none: the command-line choice, else the saved default.
    fn startup_model(
        &self,
        workspace: &WorkspaceResources,
        opts: &BootstrapOpts,
    ) -> anyhow::Result<(Option<loop_ai::Model>, Option<String>)> {
        let settings = &workspace.settings;
        let agents = crate::agents::AgentManager::new(
            crate::agents::agent_entries(&settings.agents),
            workspace.cwd.clone(),
            &self.globals.agent_dir,
        );
        // A saved subscription model is started by the surface, not Loop's agent.
        let saved = settings
            .selected_model()
            .filter(|(provider, _)| agents.entry(provider).is_none());
        let (model, note) = match crate::model_selection::resolve_startup_model(
            &self.globals.models,
            saved,
            opts.provider.as_deref(),
            opts.model.as_deref(),
        )? {
            StartupModel::Selected(model) => (Some(*model), None),
            StartupModel::NotSelected { reason } => (None, reason),
        };
        if model
            .as_ref()
            .is_some_and(|m| self.globals.credentials.is_logged_out(&m.provider))
        {
            if opts.provider.is_some() || opts.model.is_some() {
                bail!("selected provider is logged out; run /login before selecting its model");
            }
            return Ok((
                None,
                Some("saved provider is logged out; use /login or /model".into()),
            ));
        }
        Ok((model, note))
    }

    /// The model and effort a Session without choices of its own uses in
    /// `workspace`: what `open` gives it before restoring its tree.
    pub fn session_defaults(
        &self,
        workspace: &WorkspaceResources,
        opts: &BootstrapOpts,
    ) -> (Option<loop_ai::Model>, loop_agent::AgentThinkingLevel) {
        let model = self
            .startup_model(workspace, opts)
            .ok()
            .and_then(|(m, _)| m);
        let effort = loop_agent::effort::parse_thinking(&workspace.settings.default_thinking_level);
        (model, effort)
    }

    pub async fn open(
        &self,
        workspace: Arc<WorkspaceResources>,
        opts: &BootstrapOpts,
    ) -> anyhow::Result<OpenedSession> {
        self.open_with_environment(workspace, opts, None).await
    }

    /// Open with a Surface's tool environment, without changing process globals.
    pub async fn open_with_environment(
        &self,
        workspace: Arc<WorkspaceResources>,
        opts: &BootstrapOpts,
        env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> anyhow::Result<OpenedSession> {
        let mut leased = None;
        let result = self.open_leased(workspace, opts, &mut leased, env).await;
        // A standalone open that fails after taking its lease must not leave
        // it to a Drop that may never run before the process exits.
        if let (Err(_), Some(store)) = (&result, leased) {
            if let Err(error) = store.release_leases().await {
                tracing::warn!(%error, "could not release the lease of a failed Session open");
            }
        }
        result
    }

    async fn open_leased(
        &self,
        workspace: Arc<WorkspaceResources>,
        opts: &BootstrapOpts,
        leased: &mut Option<Arc<dyn loop_agent::harness::session::SessionStore>>,
        env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> anyhow::Result<OpenedSession> {
        if absolute_workspace(&opts.cwd)? != workspace.cwd {
            bail!("Session cwd does not match workspace");
        }
        self.globals.admit_cwd(&workspace.cwd)?;
        let globals = &self.globals;
        let models = Arc::clone(&globals.models);
        let credentials = Arc::clone(&globals.credentials);
        let mut settings = workspace.settings.clone();
        // A cold custom catalog must resolve an explicit choice before the
        // background refresh has a chance to finish.
        let explicit = opts.model.as_deref().and_then(|model| {
            opts.provider
                .as_deref()
                .map(|provider| (provider, model))
                .or_else(|| model.split_once('/'))
        });
        if let Some((provider, id)) = explicit {
            if settings.providers.iter().any(|entry| entry.id == provider)
                && models.get_model(provider, id).is_none()
                && !credentials.is_logged_out(provider)
            {
                let refreshed = models
                    .refresh(ModelsRefreshOptions {
                        allow_network: Some(true),
                        force: true,
                        provider_id: Some(provider.into()),
                    })
                    .await;
                if let Some(error) = refreshed.errors.get(provider) {
                    bail!("could not load models for provider {provider}: {error}");
                }
            }
        }
        let (mut model, mut model_note) = self.startup_model(&workspace, opts)?;
        if let Some(m) = &model {
            settings.set_selected_model(&m.provider, &m.id);
        }

        // Unix standalone Surfaces own their writer lease; Runtime callers manage
        // ownership separately before opening through the shared repository.
        let (standalone_repository, session_store) = if workspace.standalone {
            let store =
                loop_agent::harness::session::sqlite::create_sqlite_session_store_for_resume(
                    &globals.sessions_db,
                    opts.session_id.as_deref(),
                )
                .await
                .map_err(|e| session_error("sqlite session store", e))?;
            *leased = Some(Arc::clone(&store));
            (
                Some(create_session_repository(Arc::clone(&store), None)),
                Some(store),
            )
        } else {
            (None, None)
        };
        let repository = standalone_repository
            .as_ref()
            .unwrap_or(&globals.repository);

        let (sandbox, tool_env, disabled) = workspace.execution().await?;
        let sandbox = sandbox.clone();
        // One policy for the tools built here and for the harness's own
        // rebuilds, so a removal note is reported once per turn across both.
        let tool_env_policy =
            crate::config::tool_env::tool_env_policy(&globals.agent_dir, &settings);
        let tool_env = loop_agent::harness::SessionExecutionEnv::for_tools(
            Arc::clone(tool_env),
            env,
            &tool_env_policy,
        );
        if *disabled {
            settings.sandbox.mode = "off".into();
        }
        let resumed = opts.session_id.is_some();
        let session = if let Some(id) = &opts.session_id {
            repository
                .open(id)
                .await
                .map_err(|e| session_error("load session", e))?
        } else {
            repository
                .create(Some(workspace.cwd.to_string_lossy().into_owned()), None)
                .await
                .map_err(|e| session_error("create session", e))?
        };

        // A resumed Session gets back the model and effort it recorded: the
        // model unless the command line names one, the effort when it chose
        // one (an explicit off stays off). The saved defaults are untouched.
        let default_model = model.clone();
        let default_effort = loop_agent::effort::parse_thinking(&settings.default_thinking_level);
        let mut effort = default_effort;
        let mut recorded_mode = None;
        if resumed {
            let recorded = async {
                let reader = session.reader();
                let head = reader.read_head().await?;
                let entries = reader.read_entries(None).await?;
                Ok::<_, loop_agent::harness::types::SessionError>(
                    loop_agent::harness::session::types::config_from_entries(
                        &entries,
                        head.as_deref(),
                    ),
                )
            }
            .await;
            if let Ok(recorded) = recorded {
                if opts.provider.is_none() && opts.model.is_none() {
                    if let Some((p, m)) = &recorded.model {
                        if let Some(resolved) = models
                            .get_model(p, m)
                            .filter(|_| !credentials.is_logged_out(p))
                        {
                            model = Some(resolved);
                            model_note = None;
                        }
                    }
                }
                if let Some(level) = recorded.thinking_level {
                    effort = level;
                }
                recorded_mode = recorded
                    .permission_mode
                    .as_deref()
                    .and_then(crate::permissions::PermissionMode::parse);
            }
        }

        let bash_limits = settings.bash.unwrap_or_default().limits();
        // The gate checks Workspace paths through an env in the driver's
        // namespace: the host env for host paths (a sandbox env confines
        // its own reads and is not a resolver), the Session's env for guest
        // paths.
        use loop_agent::harness::sandbox::PathNamespace;
        let guest = matches!(&sandbox, SandboxMode::Enabled { sandbox }
            if sandbox.capabilities().path_namespace == PathNamespace::Guest);
        let gate_paths = if guest {
            if globals.tool_lock_config().is_some() {
                bail!("the tool lock needs a workspace driver whose paths are Runtime host paths");
            }
            crate::permissions::GatePaths::new(tool_env.clone(), PathNamespace::Guest)
        } else {
            crate::permissions::GatePaths::new(workspace.host.clone(), PathNamespace::Host)
        };
        let tool_gate = crate::tool_gate::ToolGate::new_for_env(
            crate::tool_approval::permissions_from_settings(&settings.tool_permissions),
            &workspace.cwd,
            gate_paths,
        )
        .await;
        if let Some(lock) = globals.tool_lock_config() {
            tool_gate.lock_tools(lock.lock());
            // The folder and root as they resolve now: later checks compare
            // against these, so a folder swapped for a link is refused.
            tool_gate.confine(crate::permissions::tool_lock::Confinement::new(
                &workspace.cwd,
                lock.root(),
            )?);
        }
        let mut tools = build_tools(
            Arc::clone(&tool_env),
            &workspace.web,
            bash_limits,
            tool_gate.search_filter(),
        );
        // Not built, so the prompt never names them; the gate's tool filter
        // and its check also remove and refuse them ([`ToolGate::available`]).
        retain_locked(globals.tool_lock(), &mut tools);

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
        harness.set_session_environment(env.cloned());
        harness.set_tool_env_policy(tool_env_policy);
        harness.set_thinking_level(effort).await;
        harness.set_bash_limits(bash_limits);
        harness
            .set_compaction_settings(settings.compaction.harness_settings())
            .await;
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
        if let Some(lock) = globals.tool_lock() {
            native.lock_tools(lock);
        }
        native.set_default_effort(default_effort);
        native.set_default_model(default_model);

        tool_gate.install(&harness);
        // Every decision is recorded. Embedded, records go to the agent
        // directory; the Runtime puts its Event Log sink in place.
        tool_gate.set_decision_sink(Some(Arc::new(
            crate::permissions::decision::JsonlDecisionSink::new(&globals.agent_dir),
        )));
        // User commands (`session/shell`, `!command`) pass the same gate.
        native.set_command_gate(tool_gate.command_gate());
        let mut startup_notices = workspace.startup_notices.clone();
        let (hook_notices, hooks) = crate::hooks_load::register_reloadable_hooks(
            &harness,
            workspace.hooks.clone(),
            &workspace.cwd,
        );
        startup_notices.extend(hook_notices);
        let (mode, mode_notice) = startup_permission_mode(
            opts.permission_mode.as_deref(),
            settings.permission_mode.as_deref(),
            &settings.file_edit_review,
            opts.interactive,
        )?;
        // A mode the Session recorded wins over the startup default.
        let mode = recorded_mode.unwrap_or(mode);
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
            retain_locked(globals.tool_lock(), &mut all_tools);
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
            session_store,
            hooks,
        })
    }

    /// Reread the model definitions and `opts.cwd`'s workspace (trust, settings, skills, templates,
    /// hooks, context, MCP connections) and cache it for new Sessions.
    /// Returns the new resources and the ones they replace, whose MCP
    /// connections the caller closes once no Session uses them. The
    /// workspace's sandbox is kept.
    pub async fn reload_workspace(
        &self,
        opts: &BootstrapOpts,
    ) -> anyhow::Result<(Arc<WorkspaceResources>, Option<Arc<WorkspaceResources>>)> {
        let cwd = absolute_workspace(&opts.cwd)?;
        let global = Settings::load_file(&settings_path(&self.globals.agent_dir))?;
        crate::runtime::reload_model_definitions(
            &self.globals.agent_dir,
            &self.globals.models,
            &global.providers,
        )
        .await?;
        let old = {
            let workspaces = self.workspaces.lock().await;
            workspaces.get(&cwd).and_then(|slot| slot.get().cloned())
        };
        let trust = TrustStore::load(trust_path(&self.globals.agent_dir))?;
        let trusted = trust.get(&cwd) == Some(true);
        let settings = load_settings(&self.globals.agent_dir, &cwd, trusted)?;
        let mut fresh =
            WorkspaceResources::load_resolved(&self.globals, opts, trusted, settings, false)
                .await?;
        if let Some(old) = &old {
            fresh.sandbox = Arc::clone(&old.sandbox);
        }
        let fresh = Arc::new(fresh);
        self.workspaces
            .lock()
            .await
            .insert(cwd, Arc::new(OnceCell::new_with(Some(Arc::clone(&fresh)))));
        Ok((fresh, old))
    }
}

#[cfg(test)]
mod tool_lock_tests {
    use super::*;

    /// Global settings cannot widen the lock: hooks, extensions, skills
    /// with scripts and private web addresses are taken out.
    #[test]
    fn a_locked_workspace_drops_what_starts_processes() {
        let dir = tempfile::tempdir().unwrap();
        let skill = |name: &str, script: bool| {
            let root = dir.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("SKILL.md"), "x").unwrap();
            if script {
                std::fs::write(root.join("run.sh"), "x").unwrap();
            }
            loop_agent::harness::types::Skill {
                name: name.into(),
                description: String::new(),
                body: String::new(),
                path: root.join("SKILL.md"),
                disable_model_invocation: false,
            }
        };
        let mut resources = LoadedResources {
            skills: vec![skill("text", false), skill("script", true)],
            disabled_skills: vec![skill("off-script", true)],
            hook_paths: vec![dir.path().join("h.json")],
            extension_paths: vec![dir.path().join("e.rhai")],
            ..Default::default()
        };
        let mut settings: Settings = serde_json::from_value(serde_json::json!({
            "webFetch": {"allowPrivateAddresses": true},
            "webSearch": {"allowPrivateAddresses": true},
            "mcpServers": {"s": {"command": "sh"}}
        }))
        .unwrap();
        let notices = lock_workspace(
            crate::permissions::tool_lock::ToolLock::Chat,
            &mut settings,
            &mut resources,
        );
        assert!(resources.hook_paths.is_empty() && resources.extension_paths.is_empty());
        let names: Vec<_> = resources.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["text"]);
        assert!(resources.disabled_skills.is_empty());
        assert!(!settings.web_fetch.unwrap().allow_private_addresses);
        assert!(!settings.web_search.unwrap().allow_private_addresses);
        assert_eq!(notices.len(), 3, "{notices:?}");
    }

    #[test]
    fn only_offered_tools_are_built() {
        let mut tools = vec![
            loop_agent::harness::tools::create_todo_tool(),
            loop_agent::harness::tools::create_exit_plan_tool(),
        ];
        retain_locked(None, &mut tools);
        assert_eq!(tools.len(), 2);
        retain_locked(
            Some(crate::permissions::tool_lock::ToolLock::Chat),
            &mut tools,
        );
        assert!(tools.is_empty());
    }
}

#[cfg(all(test, unix))]
mod lease_tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_standalone_open_releases_the_lease_it_took() {
        let dir = tempfile::tempdir().unwrap();
        let models = Arc::new(Models::create(Default::default()));
        let globals =
            Arc::new(GlobalResources::with_models(dir.path().join("agent"), models).unwrap());
        let opts = BootstrapOpts {
            cwd: dir.path().to_path_buf(),
            provider: None,
            model: None,
            theme: None,
            system_prompt: None,
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: None,
            no_web: true,
            // Refused only after the Session was created and leased.
            permission_mode: Some("invalid".into()),
            agent: None,
        };
        let workspace = Arc::new(
            WorkspaceResources::load_resolved(&globals, &opts, false, Settings::default(), true)
                .await
                .unwrap(),
        );
        let factory = SessionFactory::new(Arc::clone(&globals));
        assert!(factory.open(workspace, &opts).await.is_err());
        let store =
            loop_agent::harness::session::sqlite::SqliteSessionStore::open(&globals.sessions_db)
                .unwrap();
        for meta in globals.repository.list(None).await.unwrap() {
            assert_eq!(store.lease(&meta.id).await.unwrap(), None, "{}", meta.id);
        }
    }

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
        // Only the sentence meant for people: no store or error-kind prefix.
        assert_eq!(
            error,
            format!(
                "Session {id} is in use by Runtime {}. Use `loop session attach {id}` instead.",
                std::process::id()
            )
        );
    }
}
