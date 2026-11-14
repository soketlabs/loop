//! Workspace admin, Session sandbox and permission mode
//! (`loop_wire::RuntimeAdmin`). Host-only methods are the daemon's.
//!
//! A workspace change writes that workspace's host-owned overrides (or the
//! trust store) and reloads the workspace: new Sessions open with the new resources at
//! once, idle Sessions switch now, and busy ones when their turn or user
//! command ends.

use std::path::Path;

use loop_agent::harness::SandboxMode;
use loop_app_core::config::workspace_settings::{WorkspaceOverrides, WorkspaceSettingsStore};
use loop_app_core::config::{paths, settings::set_disabled_name, settings::set_skill_disabled};
use loop_app_core::config::{Settings, TrustStore};
use loop_protocol::admin::*;
use loop_wire::RuntimeAdmin;

use super::*;
use crate::access::{Actor, Need};

/// Tools the Runtime adds to its Sessions, kept across a reload.
const RUNTIME_TOOLS: &[&str] = &[super::subagent::TOOL];

/// One loaded version of a workspace. Its MCP connections close when the
/// last holder releases it: the workspace (while current), each Session
/// using it, and each Session that will switch to it after its work.
pub(super) struct Generation {
    pub(super) resources: Arc<WorkspaceResources>,
    /// Set once its connections are closed; shared so a watcher need not
    /// hold the generation.
    pub(super) released: Arc<AtomicBool>,
}

impl Generation {
    pub(super) fn new(resources: Arc<WorkspaceResources>) -> Arc<Self> {
        Arc::new(Self {
            resources,
            released: Arc::default(),
        })
    }
}

/// Give up one hold on `generation`; the last one closes its connections.
pub(super) async fn release(generation: Arc<Generation>) {
    if let Some(generation) = Arc::into_inner(generation) {
        generation.released.store(true, Ordering::Release);
        generation.resources.mcp_client.disconnect_all().await;
    }
}

/// Starts the sandbox a test asks for.
#[cfg(test)]
pub(super) type SandboxStarter =
    Arc<dyn Fn(&SandboxChoice) -> BackendResult<SandboxMode> + Send + Sync>;

/// A Session's sandbox choice that waits for its work to end.
pub(super) struct PendingSandbox {
    mode: SandboxMode,
    choice: SandboxChoice,
    /// The workspace's shared sandbox when it was chosen, never destroyed
    /// with a Session's own.
    shared: Option<Arc<dyn loop_agent::harness::Sandbox>>,
}

impl PendingSandbox {
    /// Destroy the started sandbox, unused.
    async fn discard(self) {
        if let SandboxMode::Enabled { sandbox } = self.mode {
            if !self
                .shared
                .is_some_and(|shared| loop_agent::harness::sandbox::same_sandbox(&shared, &sandbox))
            {
                let _ = sandbox.destroy().await;
            }
        }
    }
}

impl LiveSession {
    /// Switch the Session to `mode` and rebuild its tools in it. The caller
    /// holds the lifecycle lock and no work is running.
    async fn install_sandbox(
        &self,
        workspace: &WorkspaceResources,
        pending: PendingSandbox,
    ) -> BackendResult<()> {
        let PendingSandbox {
            mode,
            choice,
            shared,
        } = pending;
        let previous = self.opened.harness.replace_sandbox(mode).await;
        // The workspace's own sandbox serves other Sessions; keep it.
        if let SandboxMode::Enabled { sandbox } = previous {
            if !shared
                .is_some_and(|shared| loop_agent::harness::sandbox::same_sandbox(&shared, &sandbox))
            {
                let _ = sandbox.destroy().await;
            }
        }
        self.opened
            .apply_workspace(workspace, RUNTIME_TOOLS)
            .await
            .map_err(failed)?;
        let sandboxed = choice.mode == "local";
        *self.sandbox_choice.lock().unwrap() = if sandboxed {
            choice
        } else {
            Runtime::sandbox_choice(&workspace.settings, false)
        };
        // A choice that waited for the turn to end is now the effective one.
        self.opened.native.sandbox_applied();
        Ok(())
    }

    /// Apply a workspace reload that waited for this Session's work to
    /// end. The caller holds the lifecycle lock and has cleared `busy`.
    pub(super) async fn apply_pending(&self) {
        let pending = self.pending_workspace.lock().unwrap().take();
        if let Some(next) = pending {
            if let Err(error) = self
                .opened
                .apply_workspace(&next.resources, RUNTIME_TOOLS)
                .await
            {
                eprintln!("Runtime: could not apply the reloaded workspace: {error}");
            }
            let previous = self.generation.lock().unwrap().replace(next);
            if let Some(previous) = previous {
                release(previous).await;
            }
        }
        let sandbox = self.pending_sandbox.lock().unwrap().take();
        if let Some(sandbox) = sandbox {
            let workspace = self
                .generation
                .lock()
                .unwrap()
                .as_ref()
                .map(|generation| Arc::clone(&generation.resources))
                .unwrap_or_else(|| Arc::clone(&self.opened.workspace));
            if let Err(error) = self.install_sandbox(&workspace, sandbox).await {
                eprintln!("Runtime: could not apply the chosen sandbox: {error}");
            }
        }
    }
}

impl Runtime {
    /// Before an opened Session is published: move it onto the workspace's
    /// current generation if a reload happened while it was opening, and
    /// hold that generation. The caller holds the reload gate's read side.
    pub(super) async fn adopt_generation(&self, live: &LiveSession) {
        let current = self
            .workspaces
            .lock()
            .unwrap()
            .get(&live.opened.workspace.cwd)
            .map(|state| Arc::clone(&state.generation));
        let Some(current) = current else { return };
        if !Arc::ptr_eq(&current.resources, &live.opened.workspace) {
            if let Err(error) = live
                .opened
                .apply_workspace(&current.resources, RUNTIME_TOOLS)
                .await
            {
                eprintln!("Runtime: could not apply the reloaded workspace: {error}");
            }
        }
        *live.generation.lock().unwrap() = Some(current);
    }

    /// Start, record and install (or queue) a Session's new sandbox. Runs
    /// under the Session's lifecycle lock inside `set_config`.
    async fn choose_sandbox(
        &self,
        live: Arc<LiveSession>,
        choice: SandboxChoice,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let native = &live.opened.native;
        if let Some(expected) = expected_revision {
            if expected != native.revision() {
                return Err(BackendError::Conflict {
                    revision: native.revision(),
                });
            }
        }
        let workspace = self.workspace_now(&live.opened.workspace.cwd).await?;
        let mode = self.start_sandbox(&workspace, &choice).await?;
        let shared = self
            .workspaces
            .lock()
            .unwrap()
            .get(&workspace.cwd)
            .and_then(|state| state.sandbox.clone());
        let json = serde_json::to_value(&choice).map_err(failed)?;
        let session = native.current_session().await;
        let chosen = PendingSandbox {
            mode,
            choice,
            shared,
        };
        // Whatever an earlier choice left waiting is stale now.
        let stale = live.pending_sandbox.lock().unwrap().take();
        if let Some(stale) = stale {
            stale.discard().await;
        }
        if live.busy.load(Ordering::Acquire) {
            // Running tool calls keep their sandbox; the next turn starts
            // in this one. Recorded first: a choice that cannot be saved is
            // not made.
            let recorded = native.record_sandbox(&session, json, None, true).await;
            if let Err(error) = recorded {
                chosen.discard().await;
                return Err(error);
            }
            *live.pending_sandbox.lock().unwrap() = Some(chosen);
            recorded
        } else {
            live.install_sandbox(&workspace, chosen).await?;
            native.record_sandbox(&session, json, None, false).await
        }
    }

    /// Release what admin gave a closing Session: its place in a pending
    /// reload, and a sandbox of its own (never the workspace's shared one).
    pub(super) async fn release_admin(&self, live: &LiveSession) {
        let sandbox = live.pending_sandbox.lock().unwrap().take();
        if let Some(sandbox) = sandbox {
            sandbox.discard().await;
        }
        let pending = live.pending_workspace.lock().unwrap().take();
        let current = live.generation.lock().unwrap().take();
        for held in pending.into_iter().chain(current) {
            release(held).await;
        }
        let shared = self
            .workspaces
            .lock()
            .unwrap()
            .get(&live.opened.workspace.cwd)
            .and_then(|state| state.sandbox.clone());
        let own = live
            .opened
            .harness
            .replace_sandbox(SandboxMode::Disabled)
            .await;
        if let SandboxMode::Enabled { sandbox } = own {
            if !shared
                .is_some_and(|shared| loop_agent::harness::sandbox::same_sandbox(&shared, &sandbox))
            {
                let _ = sandbox.destroy().await;
            }
        }
    }
}

fn protocol(message: impl Into<String>) -> BackendError {
    BackendError::failed(ErrorCategory::Protocol, message)
}

impl Runtime {
    fn workspace_opts(&self, cwd: &Path) -> BootstrapOpts {
        let mut opts = self.session_opts();
        opts.cwd = cwd.to_path_buf();
        opts
    }

    /// The workspace an id names: one the Runtime has loaded, or its
    /// startup workspace. Never read as a path.
    ///
    /// An outside user names only the folder of a loaded Session it may
    /// read; any other id is unknown to it, with the same error.
    fn resolve_workspace(&self, id: &str) -> BackendResult<PathBuf> {
        let known: Vec<PathBuf> = match self.readable_folders(&Actor::current()) {
            Some(folders) => folders,
            None => {
                let mut known: Vec<PathBuf> =
                    self.workspaces.lock().unwrap().keys().cloned().collect();
                if let Ok(cwd) = self.defaults.cwd.canonicalize() {
                    known.push(cwd);
                }
                known
            }
        };
        known
            .into_iter()
            .find(|cwd| tree::workspace_id(cwd) == id)
            .ok_or_else(|| protocol(format!("unknown workspace {id}")))
    }

    pub(super) fn workspace_revision(&self, cwd: &Path) -> u64 {
        self.workspaces
            .lock()
            .unwrap()
            .get(cwd)
            .map_or(0, |state| state.revision)
    }

    fn check_workspace(&self, cwd: &Path, expected: Option<u64>) -> BackendResult<()> {
        let revision = self.workspace_revision(cwd);
        match expected {
            Some(expected) if expected != revision => Err(BackendError::Conflict { revision }),
            _ => Ok(()),
        }
    }

    async fn workspace_now(&self, cwd: &Path) -> BackendResult<Arc<WorkspaceResources>> {
        self.factory
            .workspace(&self.workspace_opts(cwd))
            .await
            .map_err(failed)
    }

    /// Change `cwd`'s host-owned overrides, starting from its effective
    /// settings. Never the global defaults or a project file. Caller holds
    /// `admin`.
    fn edit_workspace(
        &self,
        cwd: &Path,
        current: &Settings,
        edit: impl FnOnce(&mut WorkspaceOverrides, &Settings),
    ) -> BackendResult<()> {
        // Every workspace setter writes through here: under a tool lock none
        // may change what a Session loads.
        self.refuse_if_locked("changing workspace settings")?;
        let mut store =
            WorkspaceSettingsStore::load(&self.factory.globals.agent_dir).map_err(failed)?;
        let mut overrides = store.get(cwd);
        edit(&mut overrides, current);
        store.set(cwd, overrides).map_err(failed)
    }

    /// Reload `cwd`'s workspace and move its live Sessions onto it: idle
    /// ones now, busy ones when their work ends. Returns the workspace
    /// revision, notices and how many Sessions wait. Caller holds `admin`.
    pub(super) async fn reload_workspace(
        &self,
        cwd: &Path,
    ) -> BackendResult<(Arc<WorkspaceResources>, u64, Vec<String>, u32)> {
        // Opening Sessions publish under the read side, so none escapes.
        let _publishing = self.reload_gate.write().await;
        let (fresh, _) = self
            .factory
            .reload_workspace(&self.workspace_opts(cwd))
            .await
            .map_err(failed)?;
        let generation = Generation::new(Arc::clone(&fresh));
        let (revision, previous) = {
            let mut workspaces = self.workspaces.lock().unwrap();
            let state = workspaces
                .entry(cwd.to_path_buf())
                .or_insert_with(|| WorkspaceState {
                    resources: Arc::clone(&fresh),
                    generation: Arc::clone(&generation),
                    sandbox: None,
                    revision: 0,
                });
            state.resources = Arc::clone(&fresh);
            state.revision += 1;
            let previous = std::mem::replace(&mut state.generation, Arc::clone(&generation));
            (state.revision, previous)
        };
        let live: Vec<Arc<LiveSession>> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|live| live.opened.workspace.cwd == cwd)
            .cloned()
            .collect();
        let mut notices: Vec<String> = Vec::new();
        let mut pending = 0;
        let by = request_caller();
        for live in live {
            let _lifecycle = live.lifecycle.lock().await;
            if live.closing.load(Ordering::Acquire) {
                continue;
            }
            // The reload's config changes are this request's.
            let _claim = live.claim_changes(&by);
            if live.busy.load(Ordering::Acquire) {
                pending += 1;
                // A generation it was only going to switch to is not in use.
                let replaced = live
                    .pending_workspace
                    .lock()
                    .unwrap()
                    .replace(Arc::clone(&generation));
                if let Some(replaced) = replaced {
                    release(replaced).await;
                }
                continue;
            }
            match live.opened.apply_workspace(&fresh, RUNTIME_TOOLS).await {
                Ok(applied) => {
                    for notice in applied {
                        if !notices.contains(&notice) {
                            notices.push(notice);
                        }
                    }
                }
                Err(error) => notices.push(format!("{}: {error}", live.reference.key)),
            }
            let used = live
                .generation
                .lock()
                .unwrap()
                .replace(Arc::clone(&generation));
            if let Some(used) = used {
                release(used).await;
            }
        }
        release(previous).await;
        if notices.is_empty() {
            notices.extend(fresh.startup_notices.iter().cloned());
        }
        Ok((fresh, revision, notices, pending))
    }

    async fn mcp_servers(workspace: &WorkspaceResources) -> Vec<McpServerStatus> {
        let connected: HashMap<String, usize> = workspace
            .mcp_client
            .list_connections()
            .await
            .into_iter()
            .collect();
        let settings = &workspace.settings;
        settings
            .mcp_servers
            .keys()
            .map(|name| {
                let enabled = !settings.disabled_mcp_servers.contains(name);
                let tools = connected.get(name).copied();
                McpServerStatus {
                    name: name.clone(),
                    enabled,
                    connected: tools.is_some(),
                    tools: tools.unwrap_or(0) as u32,
                    error: (enabled && tools.is_none()).then(|| "not connected".into()),
                }
            })
            .collect()
    }

    fn skills_of(workspace: &WorkspaceResources) -> Vec<SkillStatus> {
        let resources = &workspace.resources;
        let mut skills: Vec<SkillStatus> = resources
            .skills
            .iter()
            .map(|s| (s, true))
            .chain(resources.disabled_skills.iter().map(|s| (s, false)))
            .map(|(skill, enabled)| SkillStatus {
                name: skill.name.clone(),
                description: skill.description.clone(),
                enabled,
            })
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// The sandbox a Session runs in, as it would be chosen.
    pub(super) fn sandbox_choice(
        settings: &loop_app_core::config::Settings,
        sandboxed: bool,
    ) -> SandboxChoice {
        if sandboxed {
            SandboxChoice {
                mode: "local".into(),
                isolation: settings.sandbox.isolation.clone(),
                runtime: settings.sandbox.runtime.clone(),
            }
        } else {
            SandboxChoice {
                mode: "off".into(),
                ..SandboxChoice::default()
            }
        }
    }

    /// Put a reopened Session back in the sandbox it last chose. Nothing has
    /// run in it yet, so the swap needs no waiting.
    pub(super) async fn restore_sandbox(
        &self,
        opened: &OpenedSession,
        choice: &SandboxChoice,
    ) -> BackendResult<()> {
        let mode = self.start_sandbox(&opened.workspace, choice).await?;
        let shared = self
            .workspaces
            .lock()
            .unwrap()
            .get(&opened.workspace.cwd)
            .and_then(|state| state.sandbox.clone());
        let previous = opened.harness.replace_sandbox(mode).await;
        if let SandboxMode::Enabled { sandbox } = previous {
            if !shared
                .is_some_and(|shared| loop_agent::harness::sandbox::same_sandbox(&shared, &sandbox))
            {
                let _ = sandbox.destroy().await;
            }
        }
        opened
            .apply_workspace(&opened.workspace, RUNTIME_TOOLS)
            .await
            .map_err(failed)?;
        Ok(())
    }

    /// Start the sandbox `choice` asks for, validated by the platform builder.
    async fn start_sandbox(
        &self,
        workspace: &WorkspaceResources,
        choice: &SandboxChoice,
    ) -> BackendResult<SandboxMode> {
        // No sandbox process starts under a tool lock, also not to restore
        // the one a Session chose before.
        if choice.mode != "off" && choice.mode != "disabled" {
            self.refuse_if_locked("starting a sandbox")?;
        }
        use loop_app_core::sandbox_builder::{
            parse_local_sandbox_flags, SandboxBuilder, SandboxRequest,
        };
        #[cfg(test)]
        {
            let starter = self.sandbox_starter.lock().unwrap().clone();
            if let Some(starter) = starter {
                return starter(choice);
            }
        }
        match choice.mode.as_str() {
            "off" | "disabled" => Ok(SandboxMode::Disabled),
            "local" => {
                let args: Vec<String> = [&choice.isolation, &choice.runtime]
                    .into_iter()
                    .flatten()
                    .map(|flag| format!("--{flag}"))
                    .collect();
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let flags = parse_local_sandbox_flags(&args).map_err(protocol)?;
                let builder = SandboxBuilder::for_current_platform()
                    .with_agent_dir(&self.factory.globals.agent_dir);
                let request = SandboxRequest::for_flags(&workspace.settings.sandbox, flags);
                builder
                    .resolve(&request)
                    .map_err(|e| protocol(e.to_string()))?;
                let (sandbox, _plan) = builder
                    .start(&workspace.cwd, &request)
                    .await
                    .map_err(|e| failed(format!("sandbox: {e}")))?;
                Ok(SandboxMode::Enabled { sandbox })
            }
            other => Err(protocol(format!(
                "unknown sandbox mode `{other}`: use off or local"
            ))),
        }
    }
}

impl Runtime {
    /// `runtime/defaults`: read, or save new-Session defaults in the global
    /// settings and reload every loaded workspace. The revision check, the
    /// read-modify-write and the reloads run under the admin lock, so they
    /// never interleave with each other or with a workspace change.
    pub async fn update_defaults(&self, params: DefaultsParams) -> BackendResult<DefaultsResult> {
        // A change writes the global settings; reading them is fine.
        if params.patch.is_some() {
            self.refuse_if_locked("runtime/defaults")?;
        }
        let _admin = self.admin.lock().await;
        let path = paths::settings_path(&self.factory.globals.agent_dir);
        let mut settings = Settings::load_file(&path).map_err(failed)?;
        if let Some(patch) = params.patch {
            let revision = self.defaults_revision.load(Ordering::Acquire);
            if params.expected_revision.is_some_and(|e| e != revision) {
                return Err(BackendError::Conflict { revision });
            }
            // Every field is checked before anything is written.
            let model = match &patch.model {
                Some(spec) => {
                    let (provider, id) = spec.split_once('/').ok_or_else(|| {
                        protocol(format!("choose a model as provider/model, not `{spec}`"))
                    })?;
                    let models = &self.factory.globals.models;
                    if models.credentials().is_logged_out(provider) {
                        return Err(protocol(format!(
                            "{provider} is logged out; run /login {provider}"
                        )));
                    }
                    models
                        .get_model(provider, id)
                        .ok_or_else(|| protocol(format!("model not found: {spec}")))?;
                    Some((provider.to_owned(), id.to_owned()))
                }
                None => None,
            };
            if let Some(effort) = &patch.effort {
                if !loop_agent::effort::NATIVE_EFFORTS.contains(&effort.as_str()) {
                    return Err(protocol(format!(
                        "unknown effort `{effort}`; choose one of: {}",
                        loop_agent::effort::NATIVE_EFFORTS.join(", ")
                    )));
                }
            }
            if let Some((provider, id)) = &model {
                settings.set_selected_model(provider, id);
            }
            if let Some(effort) = patch.effort {
                settings.default_thinking_level = effort;
            }
            if let Some(ms) = patch.response_header_timeout_ms {
                settings.response_header_timeout_ms = ms;
            }
            settings.save_file(&path).map_err(failed)?;
            self.defaults_revision.fetch_add(1, Ordering::AcqRel);
            // Loaded workspaces cache settings; new Sessions read them there.
            let loaded: Vec<PathBuf> = self.workspaces.lock().unwrap().keys().cloned().collect();
            for cwd in loaded {
                self.reload_workspace(&cwd).await?;
            }
        }
        Ok(DefaultsResult {
            revision: self.defaults_revision.load(Ordering::Acquire),
            model: settings
                .selected_model()
                .map(|(provider, model)| format!("{provider}/{model}")),
            effort: Some(settings.default_thinking_level.clone()),
            mode: settings.permission_mode.clone(),
            response_header_timeout_ms: Some(settings.response_header_timeout_ms),
        })
    }
}

impl Runtime {
    /// What `mode` lets a Session do, with the tool lock named when there is
    /// one: the lock caps every mode.
    fn effective_policy(&self, mode: loop_app_core::permissions::PermissionMode) -> String {
        match self.factory.globals.tool_lock() {
            Some(lock) => format!(
                "{} The Runtime's `{lock}` tool lock caps it: only reading files in the \
                 Session's folder and the web tools.",
                mode.description()
            ),
            None => mode.description().into(),
        }
    }
}

#[async_trait]
impl RuntimeAdmin for Runtime {
    fn offered(&self) -> Capabilities {
        Capabilities {
            workspace_settings: true,
            workspace_mcp: true,
            workspace_web_search: true,
            workspace_skills: true,
            workspace_templates: true,
            workspace_reload: true,
            workspace_trust: true,
            session_sandbox: true,
            permission_mode: true,
            runtime_sessions: true,
            tool_lock: self.factory.globals.tool_lock(),
            ..Capabilities::default()
        }
    }

    async fn settings(&self, p: SettingsParams) -> BackendResult<SettingsResult> {
        let cwd = self.resolve_workspace(&p.workspace_id)?;
        let workspace = self.workspace_now(&cwd).await?;
        Ok(SettingsResult {
            revision: self.workspace_revision(&cwd),
            effective: workspace.settings.public_view(),
            enabled_models: workspace.settings.enabled_models.clone(),
            resource_counts: std::collections::BTreeMap::from([
                ("skills".into(), workspace.resources.skills.len() as u32),
                ("templates".into(), workspace.resources.prompts.len() as u32),
                ("hooks".into(), workspace.resources.hook_paths.len() as u32),
                (
                    "extensions".into(),
                    workspace.resources.extension_paths.len() as u32,
                ),
            ]),
        })
    }
    async fn sessions(&self) -> BackendResult<RuntimeSessionsResult> {
        // Only the Sessions this caller may list; the totals count only them.
        let actor = Actor::current();
        let live: Vec<Arc<LiveSession>> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|live| Self::may_live(&actor, live, Need::List))
            .cloned()
            .collect();
        let mut rows = Vec::with_capacity(live.len());
        let mut totals = RuntimeSessionTotals::default();
        for live in live {
            let busy = live.busy.load(Ordering::Acquire);
            let state = if busy && live.escalations.has_pending().await {
                "parked"
            } else if busy {
                "busy"
            } else {
                "idle"
            };
            // A row the caller may list but not read (the host's user on
            // another tenant's Session) shows no model and no tokens.
            let readable = Self::may_live(&actor, &live, Need::Read);
            let tokens = if readable {
                SessionStats::stats(self, &live.reference)
                    .await
                    .ok()
                    .map(|s| s.total_tokens)
            } else {
                None
            };
            totals.sessions += 1;
            totals.busy += u32::from(busy);
            match tokens {
                Some(tokens) => totals.tokens += tokens,
                None => totals.unknown_tokens += 1,
            }
            rows.push(RuntimeSessionRow {
                session: live.reference.clone(),
                workspace_id: tree::workspace_id(&live.opened.workspace.cwd),
                state: state.into(),
                model: readable
                    .then(|| live.announced.lock().unwrap().latest.config.model.clone())
                    .flatten(),
                tokens,
            });
        }
        rows.sort_by(|a, b| a.session.key.0.cmp(&b.session.key.0));
        Ok(RuntimeSessionsResult {
            sessions: rows,
            totals,
            taken_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        })
    }

    async fn mcp(&self, params: McpParams) -> BackendResult<McpResult> {
        if matches!(params.action, AdminAction::Enable | AdminAction::Disable) {
            self.refuse_if_locked("workspace/mcp changes")?;
        }
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        // A reload changes every Session in the folder (as `reload`).
        if params.action == AdminAction::Reload && !self.may_change_folder(&Actor::current(), &cwd)
        {
            return Err(protocol(format!(
                "unknown workspace {}",
                params.workspace_id
            )));
        }
        let _admin = self.admin.lock().await;
        let mut pending_sessions = 0;
        let workspace = match params.action {
            AdminAction::List => self.workspace_now(&cwd).await?,
            AdminAction::Reload => {
                self.check_workspace(&cwd, params.expected_revision)?;
                let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
                pending_sessions = pending;
                fresh
            }
            action => {
                self.check_workspace(&cwd, params.expected_revision)?;
                let name = params
                    .name
                    .ok_or_else(|| protocol("name the MCP server to enable or disable"))?;
                let current = self.workspace_now(&cwd).await?;
                if !current.settings.mcp_servers.contains_key(&name) {
                    return Err(protocol(format!("no MCP server is named `{name}`")));
                }
                let enabled = action == AdminAction::Enable;
                self.edit_workspace(&cwd, &current.settings, |o, s| {
                    let mut list = s.disabled_mcp_servers.clone();
                    set_disabled_name(&mut list, &name, enabled);
                    o.disabled_mcp_servers = Some(list);
                })?;
                let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
                pending_sessions = pending;
                fresh
            }
        };
        Ok(McpResult {
            revision: self.workspace_revision(&cwd),
            servers: Self::mcp_servers(&workspace).await,
            pending_sessions,
        })
    }

    async fn web_search(&self, params: WebSearchParams) -> BackendResult<WebSearchResult> {
        if params.enabled.is_some() || params.provider.is_some() {
            self.refuse_if_locked("workspace/web_search changes")?;
        }
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let mut pending_sessions = 0;
        let workspace = if params.enabled.is_none() && params.provider.is_none() {
            self.workspace_now(&cwd).await?
        } else {
            self.check_workspace(&cwd, params.expected_revision)?;
            let provider = params.provider.map(|p| p.trim().to_ascii_lowercase());
            if let Some(provider) = &provider {
                if loop_agent::harness::web_search_backend(provider).is_none() {
                    let ids: Vec<&str> = loop_agent::harness::WEB_SEARCH_BACKENDS
                        .iter()
                        .map(|b| b.id)
                        .collect();
                    return Err(protocol(format!(
                        "unknown search provider `{provider}`; choose one of: {}",
                        ids.join(", ")
                    )));
                }
            }
            let current = self.workspace_now(&cwd).await?;
            self.edit_workspace(&cwd, &current.settings, |o, s| {
                let mut web = s.web_search.clone().unwrap_or_default();
                if let Some(enabled) = params.enabled {
                    web.enabled = enabled;
                }
                if provider.is_some() {
                    web.provider = provider;
                }
                o.web_search = Some(web);
            })?;
            let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
            pending_sessions = pending;
            fresh
        };
        let config = workspace.settings.web_search.clone().unwrap_or_default();
        Ok(WebSearchResult {
            revision: self.workspace_revision(&cwd),
            enabled: config.enabled,
            provider: config.provider,
            status: workspace.web_status(),
            pending_sessions,
        })
    }

    async fn skills(&self, params: SkillsParams) -> BackendResult<SkillsResult> {
        if matches!(params.action, AdminAction::Enable | AdminAction::Disable) {
            self.refuse_if_locked("workspace/skills changes")?;
        }
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let mut pending_sessions = 0;
        let workspace = match params.action {
            AdminAction::List => self.workspace_now(&cwd).await?,
            AdminAction::Reload => {
                return Err(protocol("skills are reloaded with workspace/reload"))
            }
            action => {
                self.check_workspace(&cwd, params.expected_revision)?;
                let name = params
                    .name
                    .ok_or_else(|| protocol("name the skill to enable or disable"))?;
                let current = self.workspace_now(&cwd).await?;
                if !Self::skills_of(&current).iter().any(|s| s.name == name) {
                    return Err(protocol(format!("no skill is named `{name}`")));
                }
                let enabled = action == AdminAction::Enable;
                self.edit_workspace(&cwd, &current.settings, |o, s| {
                    let mut list = s.disabled_skills.clone();
                    set_skill_disabled(&mut list, &name, enabled);
                    o.disabled_skills = Some(list);
                })?;
                let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
                pending_sessions = pending;
                fresh
            }
        };
        Ok(SkillsResult {
            revision: self.workspace_revision(&cwd),
            skills: Self::skills_of(&workspace),
            pending_sessions,
        })
    }

    async fn templates(&self, params: TemplatesParams) -> BackendResult<TemplatesResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let workspace = self.workspace_now(&cwd).await?;
        let prompts = &workspace.resources.prompts;
        let revision = self.workspace_revision(&cwd);
        if let Some(name) = params.name {
            let template = prompts
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| protocol(format!("no prompt template is named `{name}`")))?;
            let text = loop_agent::harness::prompt_templates::substitute_args(
                &template.body,
                &params.args,
            );
            return Ok(TemplatesResult {
                revision,
                templates: None,
                text: Some(text),
            });
        }
        let templates = prompts
            .iter()
            .map(|t| TemplateInfo {
                name: t.name.clone(),
                description: t.argument_hint.clone().unwrap_or_else(|| {
                    t.body
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect()
                }),
            })
            .collect();
        Ok(TemplatesResult {
            revision,
            templates: Some(templates),
            text: None,
        })
    }

    async fn reload(&self, params: ReloadParams) -> BackendResult<ReloadResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        // A reload changes every Session in the folder: an outside user
        // reloads only a folder whose loaded Sessions are all its own.
        if !self.may_change_folder(&Actor::current(), &cwd) {
            return Err(protocol(format!(
                "unknown workspace {}",
                params.workspace_id
            )));
        }
        let _admin = self.admin.lock().await;
        self.check_workspace(&cwd, params.expected_revision)?;
        let (_, revision, notices, pending_sessions) = self.reload_workspace(&cwd).await?;
        Ok(ReloadResult {
            revision,
            notices,
            pending_sessions,
        })
    }

    async fn trust(&self, params: TrustParams) -> BackendResult<TrustResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let path = paths::trust_path(&self.factory.globals.agent_dir);
        let mut pending_sessions = 0;
        if let Some(trusted) = params.trusted {
            // A locked Runtime treats every folder as untrusted.
            self.refuse_if_locked("workspace/trust")?;
            // The wire refuses this without host-local authority.
            self.check_workspace(&cwd, params.expected_revision)?;
            let mut store = TrustStore::load(&path).map_err(failed)?;
            store.set(&cwd, trusted).map_err(failed)?;
            pending_sessions = self.reload_workspace(&cwd).await?.3;
        }
        let trusted = self.factory.globals.tool_lock().is_none()
            && TrustStore::load(&path).map_err(failed)?.get(&cwd) == Some(true);
        Ok(TrustResult {
            revision: self.workspace_revision(&cwd),
            trusted,
            pending_sessions,
        })
    }

    async fn sandbox(&self, params: SandboxParams) -> BackendResult<SandboxResult> {
        let need = if params.action == SandboxAction::Set {
            Need::Write
        } else {
            Need::Read
        };
        self.check(&params.session, need).await?;
        let live = self.loaded(&params.session).await?;
        let mut mutation = None;
        if params.action == SandboxAction::Set {
            self.refuse_if_locked("session/sandbox")?;
            let choice = params
                .config
                .ok_or_else(|| protocol("give the sandbox config to set"))?;
            let target = Arc::clone(&live);
            mutation = Some(
                self.set_config(&params.session, false, |_| {
                    self.choose_sandbox(target, choice, params.expected_revision)
                })
                .await?,
            );
        }
        let mut info = live
            .opened
            .harness
            .sandbox_info()
            .await
            .format_box()
            .to_string();
        let waiting = live
            .pending_sandbox
            .lock()
            .unwrap()
            .as_ref()
            .map(|pending| pending.choice.clone());
        if waiting.is_some() {
            info.push_str("\nThe new sandbox applies from the next turn.");
        }
        let config = match waiting {
            Some(choice) => choice,
            None => live.sandbox_choice.lock().unwrap().clone(),
        };
        Ok(SandboxResult {
            config,
            info,
            revision: mutation
                .as_ref()
                .map_or_else(|| live.opened.native.revision(), |m| m.revision),
            seq: mutation.map(|m| m.seq),
        })
    }

    async fn permission_mode(
        &self,
        params: PermissionModeParams,
    ) -> BackendResult<PermissionModeResult> {
        use loop_app_core::permissions::PermissionMode;
        let session = &params.session;
        let need = if params.mode.is_some() {
            Need::Write
        } else {
            Need::Read
        };
        self.check(session, need).await?;
        let Some(name) = params.mode else {
            let live = self.loaded(session).await?;
            let mode = live.opened.tool_gate.mode();
            return Ok(PermissionModeResult {
                mode: mode.as_str().into(),
                effective_policy: self.effective_policy(mode),
                revision: live.opened.native.revision(),
                seq: None,
                pending: false,
            });
        };
        let mode = PermissionMode::parse(&name).ok_or_else(|| {
            protocol(format!(
                "unknown permission mode `{name}`: use read-only, ask, accept-edits, full-access or plan"
            ))
        })?;
        let expected = params.expected_revision;
        let result = self
            .set_config(session, false, |live| async move {
                let result = live
                    .opened
                    .native
                    .record_permission_mode(session, mode.as_str(), expected)
                    .await?;
                // The gate checks every tool call, so the mode applies at once.
                live.opened.tool_gate.set_mode(mode);
                Ok(result)
            })
            .await?;
        Ok(PermissionModeResult {
            mode: mode.as_str().into(),
            effective_policy: self.effective_policy(mode),
            revision: result.revision,
            seq: Some(result.seq),
            pending: false,
        })
    }
}
