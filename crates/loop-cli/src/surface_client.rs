//! The TUI's typed client boundary over today's in-process native backend.
//!
//! The native owner is private. Surfaces receive typed services and immutable views;
//! neither the host nor mutable settings can be reached through this boundary.
use loop_protocol::{rpc::*, session_services::*, AgentBackend, SessionRef, SessionSpec};
use loop_wire::RuntimeControl;
use std::sync::Arc;
mod native_admin;
mod native_sessions;
use crate::theme::Theme;
use crate::{keybindings, CliRuntime, PendingAgent, StartupSessions, ToolApprovalBridge};
mod native_host;
mod remote;
use native_host::NativeHost;
pub use remote::{connect, RemoteHost};

/// A Surface's client services and local terminal state.
///
/// The client cannot be used as the application host:
/// ```compile_fail
/// fn host(client: &mut loop_cli::SurfaceClient) {
///     let _: &mut loop_app_core::Runtime = client;
/// }
/// ```
/// Nor can a child module reach a hidden host through field access:
/// ```compile_fail
/// fn host(client: &loop_cli::SurfaceClient) {
///     let _ = &client.host.inner;
/// }
/// ```
/// The removed host escape hatch is not part of the Surface API:
/// ```compile_fail
/// fn host(client: &loop_cli::SurfaceClient) { let _ = client.legacy(); }
/// ```
/// ```compile_fail
/// fn host(client: &mut loop_cli::SurfaceClient) { let _ = client.legacy_mut(); }
/// ```
pub struct SurfaceClient {
    host: NativeHost,
    remote: Option<Arc<loop_wire::RemoteBackend>>,
    remote_host: Option<Arc<RemoteHost>>,
    remote_snapshot: Option<SessionSnapshot>,
    remote_resources: Option<ResourceView>,
    remote_mcp: Option<loop_protocol::admin::McpResult>,
    remote_updates:
        std::sync::Mutex<Option<tokio::sync::broadcast::Receiver<loop_protocol::UpdateEnvelope>>>,
    remote_cursor: u64,
    remote_view: Option<String>,
    remote_redraw: bool,
    /// Tests start their own sandboxes instead of a platform one.
    #[cfg(test)]
    pub(crate) sandbox_starter: Option<native_admin::SandboxStarter>,
    confirm_timeout_default: Option<u64>,
    sessions: Arc<dyn RuntimeControl>,
    native_sessions: Arc<native_sessions::NativeSessions>,
    native_backend: Arc<dyn AgentBackend>,
    workspace_id: String,
    /// Terminal keybindings.
    pub keybindings: keybindings::Keybindings,
    /// Debug session logging enabled.
    pub debug: bool,
    /// Path to the active debug log file, if any.
    pub debug_log_path: Option<std::path::PathBuf>,
    /// External agent to start once the surface is ready.
    pub pending_agent: Option<PendingAgent>,
    /// Subscription being started (`id` or `id/model`), until it is ready or fails.
    pub starting_agent: Option<String>,
    /// Subscription being signed in to from `/login`, until it finishes.
    pub signing_in: Option<String>,
    /// Backend instances whose updates are already forwarded to the surface.
    pub forwarded: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Recoverable reasoning warnings already shown in this CLI process.
    pub(crate) reasoning_notices: std::collections::HashSet<String>,
    /// `/permissions full-access` was asked for once and needs repeating to
    /// confirm.
    pub confirm_full_access: bool,
    /// `/export` was asked to overwrite files with this path argument and
    /// needs repeating to confirm.
    pub confirm_export: Option<String>,
    /// `--resume` with no id or `--continue`: what to do once the TUI is up.
    pub startup_sessions: Option<StartupSessions>,
    /// `/usage` asked for the usage view; the chat loop opens it.
    pub usage_requested: bool,
}

/// `/default` over a Runtime: save `model` (`provider/id`) and `effort` for
/// new Sessions with the host-only `runtime/defaults`. Returns what was
/// saved, as `model · effort`. A connection without host-local authority
/// (a WebSocket) is told to run it on the Runtime host.
pub async fn save_defaults_on(
    admin: &dyn loop_wire::RuntimeAdmin,
    model: &str,
    effort: &str,
) -> anyhow::Result<String> {
    let patch = loop_protocol::admin::DefaultsPatch {
        model: Some(model.to_owned()),
        effort: Some(effort.to_owned()),
        ..Default::default()
    };
    match admin
        .defaults(loop_protocol::admin::DefaultsParams {
            patch: Some(patch),
            ..Default::default()
        })
        .await
    {
        Ok(_) => Ok(format!("{model} · {effort}")),
        // Refused by the host, or not even offered: host-only methods are
        // advertised only on a host-local connection.
        Err(
            loop_protocol::BackendError::Forbidden(_) | loop_protocol::BackendError::Unsupported(_),
        ) => anyhow::bail!("run this on the Runtime host"),
        Err(error) => Err(error.into()),
    }
}

impl From<CliRuntime> for SurfaceClient {
    fn from(runtime: CliRuntime) -> Self {
        let CliRuntime {
            inner,
            keybindings,
            debug,
            debug_log_path,
            pending_agent,
            starting_agent,
            signing_in,
            forwarded,
            reasoning_notices,
            confirm_full_access,
            confirm_export,
            startup_sessions,
            usage_requested,
        } = runtime;
        let native = Arc::new(native_sessions::NativeSessions::new(
            inner.native.clone(),
            inner.session_store.clone(),
            inner.cwd.clone(),
        ));
        let workspace_id = native.workspace_id.clone();
        Self {
            sessions: native.clone(),
            native_sessions: native.clone(),
            native_backend: native,
            workspace_id,
            host: NativeHost { inner },
            remote: None,
            remote_host: None,
            remote_snapshot: None,
            remote_resources: None,
            remote_mcp: None,
            remote_updates: Default::default(),
            remote_cursor: 0,
            remote_view: None,
            remote_redraw: false,
            #[cfg(test)]
            sandbox_starter: None,
            confirm_timeout_default: None,
            keybindings,
            debug,
            debug_log_path,
            pending_agent,
            starting_agent,
            signing_in,
            forwarded,
            reasoning_notices,
            confirm_full_access,
            confirm_export,
            startup_sessions,
            usage_requested,
        }
    }
}

impl SurfaceClient {
    pub fn backend(&self) -> &Arc<dyn AgentBackend> {
        if self.is_native() {
            &self.native_backend
        } else {
            &self.host.inner.backend
        }
    }
    pub fn agent_dir(&self) -> &std::path::PathBuf {
        &self.host.inner.agent_dir
    }
    pub fn cwd(&self) -> &std::path::PathBuf {
        &self.host.inner.cwd
    }
    pub fn theme(&self) -> &Theme {
        &self.host.inner.theme
    }
    pub fn session_id(&self) -> &String {
        &self.host.inner.session_id
    }
    pub fn selected_model(&self) -> &Option<loop_ai::Model> {
        &self.host.inner.selected_model
    }
    pub fn model_note(&self) -> &Option<String> {
        &self.host.inner.model_note
    }
    pub fn needs_provider_setup(&self) -> bool {
        self.host.inner.needs_provider_setup
    }
    pub fn resumed(&self) -> bool {
        self.host.inner.resumed
    }
    pub fn sessions_db(&self) -> &std::path::PathBuf {
        &self.host.inner.sessions_db
    }
    pub fn active_agent(&self) -> &Option<String> {
        &self.host.inner.active_agent
    }
    pub fn agent_model(&self) -> &Option<String> {
        &self.host.inner.agent_model
    }
    pub fn agent_effort(&self) -> &Option<String> {
        &self.host.inner.agent_effort
    }
    pub fn take_startup_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.host.inner.startup_notices)
    }
    pub fn set_theme(&mut self, theme: Theme) {
        self.host.inner.theme = theme;
    }
    pub fn set_resumed(&mut self, resumed: bool) {
        self.host.inner.resumed = resumed;
    }
    pub fn route_agent_replay(&mut self, id: &str) {
        self.host.inner.session_id = id.into();
    }
    /// Ask the native harness to stop work as the TUI exits.
    pub fn request_shutdown(&self) {
        self.host.inner.harness.request_shutdown();
    }
    pub fn shutdown(&self) -> loop_app_core::runtime::SessionShutdown {
        self.host.inner.session_shutdown()
    }
    pub fn is_native(&self) -> bool {
        self.host.inner.is_native()
    }
    pub fn is_subscription(&self, provider: &str) -> bool {
        self.host.inner.is_subscription(provider)
    }
    pub fn active_model_label(&self) -> Option<String> {
        self.host.inner.active_model_label()
    }
    pub fn native_effort(&self) -> String {
        self.remote_snapshot
            .as_ref()
            .and_then(|s| s.config.effort.clone())
            .unwrap_or_else(|| self.host.inner.native_effort())
    }
    pub fn selected_model_spec(&self) -> Option<String> {
        self.host.inner.selected_model_spec()
    }
    pub fn session_ref(&self) -> loop_protocol::SessionRef {
        if let Some(remote) = &self.remote {
            return SessionRef {
                instance: remote.instance().clone(),
                key: self.session_id().clone().into(),
                native_id: self.session_id().clone(),
            };
        }
        self.host.inner.session_ref()
    }
    pub fn note_prompt(&self, prompt: &str) {
        self.host.inner.note_prompt(prompt)
    }
    pub fn adopt_agent(
        &mut self,
        id: &str,
        backend: Arc<dyn loop_protocol::AgentBackend>,
        session: loop_protocol::SessionRef,
        model: Option<String>,
    ) {
        self.host.inner.adopt_agent(id, backend, session, model)
    }
    pub async fn sync_agent_effort(&mut self) {
        self.host.inner.sync_agent_effort().await
    }
    pub async fn use_native(&mut self) {
        if self.is_remote() {
            let _ = self.refresh_remote().await;
            return;
        }
        self.host.inner.use_native().await
    }
    /// The model catalog, for tests that compare the picker with it.
    #[cfg(test)]
    pub(crate) fn catalog(&self) -> &Arc<loop_ai::Models> {
        &self.host.inner.models
    }
    /// S3.3's typed Session control service. No filesystem paths cross it.
    pub fn sessions(&self) -> &dyn RuntimeControl {
        self.sessions.as_ref()
    }

    pub async fn list_sessions(
        &self,
    ) -> anyhow::Result<Vec<loop_app_core::sessions::SessionChoice>> {
        let mut choices: Vec<_> = self
            .sessions
            .list()
            .await?
            .into_iter()
            // Preserve the TUI's message-only resume picker, while the service
            // also supplies empty related Sessions to /tree.
            .filter(|info| info.summary.message_count.unwrap_or(0) > 0)
            .map(|info| loop_app_core::sessions::SessionChoice {
                id: info.r#ref.native_id,
                title: info
                    .name
                    .or(info.summary.first_prompt)
                    .unwrap_or_else(|| "(no title)".into()),
                source: info.summary.selected_model,
                message_count: info.summary.message_count.map(|n| n as usize),
                updated_at: info.last_active_ms as i64,
                agent: None,
            })
            .collect();
        choices.extend(loop_app_core::sessions::merge(
            vec![],
            self.host.inner.agents.sessions_in(self.cwd()),
        ));
        choices.sort_by_key(|choice| std::cmp::Reverse(choice.updated_at));
        Ok(choices)
    }

    pub async fn open_native_session(&mut self, id: &str) -> anyhow::Result<()> {
        let session = SessionRef {
            instance: self.native_backend.instance().clone(),
            key: id.into(),
            native_id: id.into(),
        };
        if self.is_remote() {
            self.attach_remote(session).await?;
            self.host.inner.resumed = true;
            return Ok(());
        }
        self.native_backend
            .load_session(&session, SessionSpec::default())
            .await?;
        self.host.inner.use_native().await;
        self.host.inner.resumed = true;
        self.host.inner.active_skills.clear();
        self.refresh_native_model();
        self.host.restore_session_approvals().await;
        self.restore_sandbox().await;
        Ok(())
    }

    fn refresh_native_model(&mut self) {
        let config = self.host.inner.native.config();
        self.host.inner.selected_model = config.config.model.as_deref().and_then(|spec| {
            let (provider, id) = spec.split_once('/')?;
            self.host.inner.models.get_model(provider, id)
        });
        self.host.inner.model_note = None;
        self.host.inner.active_skills = config.config.active_skills;
    }

    pub async fn new_session(&mut self) -> anyhow::Result<()> {
        if self.is_remote() {
            let session = self
                .native_backend
                .new_session(SessionSpec {
                    cwd: Some(self.cwd().clone()),
                    env: Some(crate::runtime_launch::session_environment(
                        self.remote.as_ref().unwrap(),
                        self.agent_dir(),
                    )?),
                    ..Default::default()
                })
                .await?;
            self.attach_remote(session).await?;
        } else if self.is_native() {
            let session = self
                .native_backend
                .new_session(SessionSpec {
                    cwd: Some(self.cwd().clone()),
                    name: None,
                    ..Default::default()
                })
                .await?;
            self.host.inner.session_id = session.native_id;
            self.refresh_native_model();
        } else {
            self.host.inner.new_session().await?;
        }
        self.host.inner.resumed = false;
        self.host.inner.active_skills.clear();
        self.host.clear_session_approvals();
        Ok(())
    }

    pub async fn rename_session(&self, name: &str) -> anyhow::Result<()> {
        if !self.is_native() {
            self.host.inner.agents.rename(self.session_id(), name)?;
            return Ok(());
        }
        self.sessions
            .rename(RenameParams {
                session: self.session_ref(),
                name: name.into(),
                expected_revision: None,
            })
            .await?;
        Ok(())
    }

    pub async fn session_tree(&self) -> anyhow::Result<TreeResult> {
        let mut tree = self
            .sessions
            .tree(TreeParams {
                session: self.session_ref(),
                cursor: None,
                limit: Some(1000),
            })
            .await?;
        while let Some(cursor) = tree.next_cursor.take() {
            let page = self
                .sessions
                .tree(TreeParams {
                    session: self.session_ref(),
                    cursor: Some(cursor),
                    limit: Some(1000),
                })
                .await?;
            if page.branch_revision != tree.branch_revision {
                anyhow::bail!("tree changed; open /tree again");
            }
            tree.entries.extend(page.entries);
            tree.fork_points.extend(page.fork_points);
            tree.next_cursor = page.next_cursor;
        }
        Ok(tree)
    }

    pub async fn session_family(&self, tree: &TreeResult) -> anyhow::Result<Vec<(String, String)>> {
        let infos = self.sessions.list().await?;
        Ok(tree
            .family
            .iter()
            .map(|session| {
                let info = infos.iter().find(|info| info.r#ref == *session);
                let title = info
                    .and_then(|info| info.name.as_ref().or(info.summary.first_prompt.as_ref()))
                    .unwrap_or(&session.native_id);
                let child = info
                    .and_then(|info| info.parent.as_ref())
                    .is_some_and(|parent| parent.native_id == *self.session_id());
                (
                    session.native_id.clone(),
                    format!("{} · {title}", if child { "fork" } else { "forked from" }),
                )
            })
            .collect())
    }

    pub async fn navigate(&mut self, target_entry_id: Option<String>) -> anyhow::Result<()> {
        self.sessions
            .navigate(NavigateParams {
                session: self.session_ref(),
                target_entry_id,
                summarize: false,
                expected_revision: None,
            })
            .await?;
        if self.is_remote() {
            self.refresh_remote().await?;
        } else {
            self.refresh_native_model();
        }
        Ok(())
    }

    /// Create first, then load the returned destination. Source copies never use
    /// the old mutating harness fork helper.
    pub async fn fork_session(
        &mut self,
        before_entry_id: Option<&str>,
        name: Option<String>,
    ) -> anyhow::Result<DestinationResult> {
        let session = self.session_ref();
        if self.backend().is_busy(&session) {
            anyhow::bail!("wait for the current turn to finish, then fork or clone");
        }
        let request_id = loop_ai::new_id();
        let destination = match before_entry_id {
            Some(before) => {
                self.sessions
                    .fork(ForkParams {
                        session,
                        before_entry_id: before.into(),
                        name,
                        request_id,
                    })
                    .await?
            }
            None => {
                self.sessions
                    .clone_session(CloneParams {
                        session,
                        name,
                        request_id,
                    })
                    .await?
            }
        };
        if let Err(error) = self
            .open_native_session(&destination.session.native_id)
            .await
        {
            if self.is_remote() {
                return Err(error);
            }
            self.native_sessions
                .discard_destination(&destination.session)
                .await
                .map_err(|cleanup| {
                    anyhow::anyhow!(
                        "{error:#}; the unused copy {} could not be removed ({cleanup}); \
                         delete it from /sessions",
                        destination.session.native_id
                    )
                })?;
            return Err(error);
        }
        Ok(destination)
    }

    pub async fn export_payload(&self) -> anyhow::Result<ExportResult> {
        if !self.is_native() {
            anyhow::bail!(
                "/export saves Loop's own sessions; this one runs in a subscription agent"
            );
        }
        Ok(self
            .sessions
            .export(SessionParams {
                session: self.session_ref(),
            })
            .await?)
    }

    pub async fn import_payload(&self, jsonl: String) -> anyhow::Result<DestinationResult> {
        Ok(self
            .sessions
            .import(ImportParams {
                workspace_id: self.workspace_id.clone(),
                jsonl,
                request_id: loop_ai::new_id(),
            })
            .await?)
    }

    fn native_control(&self) -> anyhow::Result<&dyn loop_protocol::ModelControl> {
        self.native_backend
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("Loop's own agent offers no model choice here"))
    }

    /// The native Session's model choices, in picker order.
    pub async fn available_models(&self) -> anyhow::Result<Vec<loop_ai::Model>> {
        let session = self.session_ref();
        let choices = self.native_control()?.models(&session).await?;
        let models = choices
            .models
            .into_iter()
            .filter_map(|choice| {
                let (provider, id) = choice.id.split_once('/')?;
                self.host.inner.models.get_model(provider, id).or_else(|| {
                    self.is_remote()
                        .then(|| remote::model_view(&choice.id, &choice.name))
                })
            })
            .collect();
        Ok(crate::config::providers::sort_models_for_picker(models))
    }

    pub async fn cycle_models(&self) -> anyhow::Result<Vec<loop_ai::Model>> {
        let models = self.available_models().await?;
        let enabled = &self.host.inner.settings.enabled_models;
        if enabled.is_empty() {
            return Ok(models);
        }
        Ok(models
            .into_iter()
            .filter(|model| {
                enabled
                    .iter()
                    .any(|id| id == &model.id || id == &format!("{}/{}", model.provider, model.id))
            })
            .collect())
    }

    pub async fn resolve_model(&self, spec: &str) -> anyhow::Result<loop_ai::Model> {
        if self.is_remote() {
            let matches: Vec<_> = self
                .available_models()
                .await?
                .into_iter()
                .filter(|m| spec == m.id || spec == format!("{}/{}", m.provider, m.id))
                .collect();
            return match matches.as_slice() {
                [model] => Ok(model.clone()),
                [] => anyhow::bail!("unknown model `{spec}`"),
                _ => anyhow::bail!("ambiguous model `{spec}`; use provider/model"),
            };
        }
        // Preserve bare ids, provider/model ids and the existing ambiguity help.
        loop_app_core::model_selection::resolve_model_spec(&self.host.inner.models, spec)
    }

    pub async fn select_model(
        &mut self,
        provider: &str,
        id: &str,
    ) -> anyhow::Result<(loop_ai::Model, bool)> {
        if !self.is_remote()
            && loop_ai::CredentialStore::is_logged_out(
                self.host.inner.credentials.as_ref(),
                provider,
            )
        {
            anyhow::bail!("{provider} is logged out; run /login {provider}");
        }
        let model = self.resolve_model(&format!("{provider}/{id}")).await?;
        let session = self.session_ref();
        let changed = self
            .native_control()?
            .set_model(&session, &format!("{provider}/{id}"), None)
            .await?;
        self.host.inner.selected_model = Some(model.clone());
        self.host.inner.model_note = None;
        Ok((model, changed.pending))
    }

    pub async fn efforts(&self) -> anyhow::Result<(Vec<String>, String)> {
        let choices = self
            .backend()
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("this subscription has no reasoning effort setting"))?
            .efforts(&self.session_ref())
            .await?;
        Ok((
            choices.models.into_iter().map(|choice| choice.id).collect(),
            choices.current.unwrap_or_default(),
        ))
    }

    pub async fn set_effort(&mut self, level: &str) -> anyhow::Result<String> {
        let level = level.trim().to_lowercase();
        // Checked here for every backend, so an agent's refusal reads the same.
        let (levels, _) = self.efforts().await?;
        if !levels.contains(&level) {
            anyhow::bail!(
                "unknown effort `{level}`; choose one of: {}",
                levels.join(", ")
            );
        }
        self.backend()
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("this subscription has no reasoning effort setting"))?
            .set_effort(&self.session_ref(), &level, None)
            .await?;
        if !self.is_native() {
            self.host.inner.agent_effort = Some(level.clone());
        }
        Ok(level)
    }

    /// The configured effort of the current Session (not limited by the
    /// model), as `/default` saves it.
    async fn session_effort(&self) -> anyhow::Result<String> {
        Ok(self.efforts().await?.1)
    }

    /// Whether the current Session already runs on the saved defaults for
    /// new Sessions, so there is nothing for `/default` to save.
    pub async fn on_defaults(&self) -> bool {
        let settings = &self.host.inner.settings;
        let effort = self.session_effort().await.unwrap_or_default();
        match self.active_agent() {
            None => {
                let model = self
                    .selected_model()
                    .as_ref()
                    .map(|m| (m.provider.clone(), m.id.clone()));
                let saved = settings
                    .selected_model()
                    .map(|(p, m)| (p.to_owned(), m.to_owned()));
                model == saved && effort == settings.default_thinking_level
            }
            Some(id) => {
                let saved = settings.selected_model();
                saved.is_some_and(|(p, m)| p == id && Some(m) == self.agent_model().as_deref())
                    && self.host.inner.agents.effort(id).unwrap_or_default() == effort
            }
        }
    }

    /// `/default`: save the current Session's model and effort as the
    /// defaults for new Sessions. Returns what was saved, as `model · effort`.
    ///
    /// Today this writes the host settings in process. Over a Runtime it is
    /// `runtime/defaults` ([`save_defaults_on`]), which is host-only.
    pub async fn save_defaults(&mut self) -> anyhow::Result<String> {
        use loop_wire::RuntimeAdmin;
        let effort = self.session_effort().await?;
        let model = match self.active_agent() {
            Some(id) => self.agent_model().as_ref().map(|m| format!("{id}/{m}")),
            None => self.selected_model_spec(),
        };
        if model.is_none() && self.active_agent().is_none() {
            anyhow::bail!("choose a model first: /model");
        }
        self.admin()
            .defaults(loop_protocol::admin::DefaultsParams {
                patch: Some(loop_protocol::admin::DefaultsPatch {
                    model: model.clone(),
                    effort: Some(effort.clone()),
                    response_header_timeout_ms: None,
                }),
                ..Default::default()
            })
            .await?;
        let label = model
            .or_else(|| self.active_agent().clone())
            .unwrap_or_default();
        Ok(if effort.is_empty() {
            label
        } else {
            format!("{label} · {effort}")
        })
    }

    pub async fn cycle_effort(&mut self) -> anyhow::Result<String> {
        let (mut levels, current) = self.efforts().await?;
        if self.is_native() {
            levels.retain(|level| loop_agent::effort::CYCLE_EFFORTS.contains(&level.as_str()));
        }
        let next = levels
            .iter()
            .position(|level| level == &current)
            .map_or(0, |i| (i + 1) % levels.len());
        let level = levels
            .get(next)
            .ok_or_else(|| anyhow::anyhow!("no reasoning effort levels on offer"))?;
        self.set_effort(level).await
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod sandbox_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use loop_agent::harness::session::{sqlite::SqliteSessionStore, SessionFile, SessionStore};
    use loop_protocol::{ContentBlock, TurnOutcome};
    use std::time::Duration;
    use test_support::{fixture, pause_provider};

    /// Connected to a Runtime, the footer shows the Runtime Session's mode,
    /// not the local gate's.
    #[tokio::test]
    async fn the_permission_mode_is_the_runtimes_when_connected() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let mut client = fixture(dir.path()).await;
            let local = client.permission_mode();
            assert_ne!(local.as_str(), "plan");
            let session = client.session_ref();
            let snapshot = |mode: Option<&str>| {
                Some(loop_protocol::rpc::SessionSnapshot {
                    session: session.clone(),
                    workspace_id: String::new(),
                    at_seq: 0,
                    revision: 0,
                    branch_revision: 0,
                    view_id: None,
                    metadata: Default::default(),
                    config: loop_protocol::SessionConfig {
                        mode: mode.map(str::to_owned),
                        ..Default::default()
                    },
                    effective: Default::default(),
                    pending: false,
                    capabilities: Default::default(),
                    turn_state: None,
                    stats: serde_json::Value::Null,
                    notices: vec![],
                    workspace_revision: 0,
                })
            };
            client.remote_snapshot = snapshot(Some("plan"));
            assert_eq!(client.permission_mode().as_str(), "plan");
            client.remote_snapshot = snapshot(None);
            assert_eq!(client.permission_mode(), local, "no Runtime mode yet");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn copy_and_export_capture_committed_output_without_switching_or_cancelling_source() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = fixture(dir.path()).await;
            let session = client.session_ref();
            client
                .backend()
                .prompt(&session, vec![ContentBlock::text("committed")])
                .await
                .unwrap()
                .outcome()
                .await;
            let started = Arc::new(tokio::sync::Notify::new());
            let finish = Arc::new(tokio::sync::Notify::new());
            pause_provider(&client, started.clone(), finish.clone());
            let turn = client
                .backend()
                .prompt(&session, vec![ContentBlock::text("unfinished")])
                .await
                .unwrap();
            started.notified().await;
            let exported = client.export_payload().await.unwrap();
            assert!(exported.markdown.contains("committed"));
            assert!(!exported.markdown.contains("unfinished"));
            let destination = client
                .sessions()
                .clone_session(CloneParams {
                    session: session.clone(),
                    name: None,
                    request_id: "copy-once".into(),
                })
                .await
                .unwrap();
            assert_eq!(client.session_ref(), session);
            assert!(client.backend().is_busy(&session));
            let store = SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap();
            assert!(store
                .lease(&destination.session.native_id)
                .await
                .unwrap()
                .is_some());
            let reader = store.load(&destination.session.native_id).await.unwrap();
            let file = SessionFile::from_reader(reader.as_ref()).await.unwrap();
            assert!(file.markdown().contains("committed"));
            assert!(!file.markdown().contains("unfinished"));
            finish.notify_one();
            assert!(matches!(
                turn.outcome().await,
                TurnOutcome::Completed { .. }
            ));
            client.shutdown().run().await.unwrap();
            assert!(store
                .lease(&destination.session.native_id)
                .await
                .unwrap()
                .is_none());
            assert!(store.lease(&session.native_id).await.unwrap().is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn import_rejects_bad_links_and_oversize_payloads_before_creating_a_destination() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = fixture(dir.path()).await;
            let source = client.session_ref();
            client
                .backend()
                .prompt(&source, vec![ContentBlock::text("hello")])
                .await
                .unwrap()
                .outcome()
                .await;
            let before = client.list_sessions().await.unwrap();
            let payload = client.export_payload().await.unwrap();
            let mut lines: Vec<_> = payload.jsonl.lines().map(str::to_owned).collect();
            let mut entry: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
            entry["parent_id"] = "missing".into();
            lines[1] = entry.to_string();
            assert!(client
                .import_payload(lines.join("\n"))
                .await
                .unwrap_err()
                .to_string()
                .contains("parent links"));
            assert!(client
                .import_payload("x".repeat(SESSION_TRANSFER_LIMIT + 1))
                .await
                .unwrap_err()
                .to_string()
                .contains("8 MiB"));
            assert_eq!(client.list_sessions().await.unwrap(), before);
            assert_eq!(client.session_ref(), source);
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    #[tokio::test]
    async fn a_tree_cursor_fails_when_new_turn_output_changes_the_rows() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = test_support::fixture(dir.path()).await;
            let session = client.session_ref();
            client
                .backend()
                .prompt(&session, vec![loop_protocol::ContentBlock::text("first")])
                .await
                .unwrap()
                .outcome()
                .await;
            let first = client
                .sessions()
                .tree(TreeParams {
                    session: session.clone(),
                    cursor: None,
                    limit: Some(1),
                })
                .await
                .unwrap();
            client
                .backend()
                .prompt(&session, vec![loop_protocol::ContentBlock::text("second")])
                .await
                .unwrap()
                .outcome()
                .await;
            let result = client
                .sessions()
                .tree(TreeParams {
                    session,
                    cursor: first.next_cursor,
                    limit: Some(1),
                })
                .await;
            assert!(result.unwrap_err().to_string().contains("tree changed"));
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod destination_retry_tests {
    use super::*;
    #[tokio::test]
    async fn retrying_a_fork_after_source_navigation_returns_the_original_destination() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let mut client = test_support::fixture(dir.path()).await;
            let session = client.session_ref();
            client
                .backend()
                .prompt(&session, vec![loop_protocol::ContentBlock::text("draft")])
                .await
                .unwrap()
                .outcome()
                .await;
            let tree = client.session_tree().await.unwrap();
            let params = ForkParams {
                session,
                before_entry_id: tree.fork_points[0].entry_id.clone(),
                name: None,
                request_id: "same-fork".into(),
            };
            let destination = client.sessions().fork(params.clone()).await.unwrap();
            client.navigate(None).await.unwrap();
            assert_eq!(client.sessions().fork(params).await.unwrap(), destination);
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod transfer_limit_tests {
    use super::*;
    #[tokio::test]
    async fn export_applies_the_response_limit_to_json_escaping_as_well_as_jsonl() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let client = test_support::fixture(dir.path()).await;
            let text = "\u{0001}".repeat(1_300_000);
            client
                .backend()
                .prompt(
                    &client.session_ref(),
                    vec![loop_protocol::ContentBlock::text(text)],
                )
                .await
                .unwrap()
                .outcome()
                .await;
            let result = client.export_payload().await;
            assert!(result.is_err(), "an encoded export above 16 MiB must fail");
            let error = result.unwrap_err().to_string();
            assert!(error.contains("16 MiB"), "{error}");
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use loop_protocol::{ContentBlock, TurnOutcome};
    use std::time::Duration;

    #[tokio::test]
    async fn closing_a_session_keeps_it_listed_and_loadable() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let mut client = test_support::fixture(dir.path()).await;
            let first = client.session_ref();
            client
                .backend()
                .prompt(&first, vec![ContentBlock::text("keep me")])
                .await
                .unwrap()
                .outcome()
                .await;
            client.new_session().await.unwrap();
            client.sessions().close(&first, false).await.unwrap();
            let listed = client.list_sessions().await.unwrap();
            assert!(listed.iter().any(|c| c.id == first.native_id), "{listed:?}");
            client.open_native_session(&first.native_id).await.unwrap();
            assert_eq!(client.session_ref(), first);
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prompts_work_without_a_session_store() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = test_support::fixture_with(dir.path(), false).await;
            let turn = client
                .backend()
                .prompt(&client.session_ref(), vec![ContentBlock::text("hello")])
                .await
                .unwrap();
            assert!(matches!(
                turn.outcome().await,
                TurnOutcome::Completed { .. }
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_workspace_id_is_the_hosts_stable_one() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let one = test_support::fixture(dir.path()).await;
            let expected = loop_runtime::workspace_id(&dir.path().canonicalize().unwrap());
            assert_eq!(one.workspace_id, expected);
            one.shutdown().run().await.unwrap();
            let two = test_support::fixture(dir.path()).await;
            assert_eq!(two.workspace_id, expected);
            two.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod runtime_default_tests {
    use super::*;
    use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
    use loop_wire::server::{serve_with_control, ServeOptions};
    use loop_wire::{Authority, RemoteBackend};
    use std::time::Duration;

    struct NoHost;
    #[async_trait::async_trait]
    impl loop_protocol::HostServices for NoHost {
        async fn interact(
            &self,
            _: loop_protocol::InteractionRequest,
        ) -> loop_protocol::InteractionOutcome {
            loop_protocol::InteractionOutcome::Cancelled
        }
    }

    async fn connect(dir: &std::path::Path, authority: Authority) -> Arc<RemoteBackend> {
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider(
            loop_ai::providers::FauxScript::new(),
        ));
        let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
        let opts = BootstrapOpts {
            cwd: dir.into(),
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
        };
        let log = Arc::new(loop_runtime::log::MemoryEventLog::new());
        let rt = {
            let store = globals.open_runtime_session_store().unwrap();
            loop_runtime::Runtime::new(globals, opts, log, store, Default::default())
        }
        .await
        .unwrap()
        .into_shared();
        struct Host(Arc<loop_runtime::Runtime>);
        #[async_trait::async_trait]
        impl RuntimeControl for Host {
            async fn list(&self) -> loop_protocol::BackendResult<Vec<SessionInfo>> {
                self.0.list().await
            }
            async fn close(&self, s: &SessionRef, f: bool) -> loop_protocol::BackendResult<()> {
                RuntimeControl::close(self.0.as_ref(), s, f).await
            }
            async fn attach(
                &self,
                s: &SessionRef,
                after: u64,
            ) -> loop_protocol::BackendResult<loop_wire::Attachment> {
                RuntimeControl::attach(self.0.as_ref(), s, after).await
            }
            fn admin(&self) -> Option<&dyn loop_wire::RuntimeAdmin> {
                Some(self)
            }
        }
        #[async_trait::async_trait]
        impl loop_wire::RuntimeAdmin for Host {
            fn offered(&self) -> loop_protocol::Capabilities {
                loop_protocol::Capabilities {
                    host_defaults: true,
                    ..Default::default()
                }
            }
            async fn defaults(
                &self,
                p: loop_protocol::admin::DefaultsParams,
            ) -> loop_protocol::BackendResult<loop_protocol::admin::DefaultsResult> {
                self.0.update_defaults(p).await
            }
        }
        let (server, client) = loop_wire::transport::memory();
        let _ = serve_with_control(
            rt.clone(),
            Arc::new(Host(rt)),
            server,
            PeerInfo {
                name: "t".into(),
                version: "t".into(),
            },
            ServeOptions {
                owns_backend: false,
                authority,
            },
        );
        RemoteBackend::connect(
            client,
            PeerInfo {
                name: "t".into(),
                version: "t".into(),
            },
            Arc::new(NoHost),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn default_over_a_runtime_saves_host_defaults_only_host_locally() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let remote = connect(dir.path(), Authority::Remote).await;
            let refused = save_defaults_on(remote.as_ref(), "faux/faux-model", "high")
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(refused, "run this on the Runtime host");

            let local = connect(dir.path(), Authority::HostLocal).await;
            let saved = save_defaults_on(local.as_ref(), "faux/faux-model", "high")
                .await
                .unwrap();
            assert_eq!(saved, "faux/faux-model · high");
            let settings: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(dir.path().join("agent/settings.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(settings["defaultProvider"], "faux");
            assert_eq!(settings["defaultModel"], "faux-model");
            assert_eq!(settings["defaultThinkingLevel"], "high");
        })
        .await
        .unwrap();
    }
}

impl SurfaceClient {
    /// S3.4 admin request service for the native owner. The borrow prevents the
    /// UI from changing host state during an administrative operation.
    pub fn admin(&mut self) -> impl loop_wire::RuntimeAdmin + '_ {
        native_admin::NativeAdmin(
            tokio::sync::Mutex::new(&mut self.host.inner),
            self.remote.as_deref(),
        )
    }
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
    pub fn permission_mode(&self) -> loop_app_core::permissions::PermissionMode {
        self.remote_snapshot
            .as_ref()
            .and_then(|s| s.config.mode.as_deref())
            .and_then(loop_app_core::permissions::PermissionMode::parse)
            .unwrap_or_else(|| self.host.inner.permission_mode())
    }
    pub async fn set_permission_mode(
        &mut self,
        mode: loop_app_core::permissions::PermissionMode,
    ) -> anyhow::Result<()> {
        use loop_wire::RuntimeAdmin;
        let params = loop_protocol::admin::PermissionModeParams {
            session: self.session_ref(),
            mode: Some(mode.as_str().into()),
            expected_revision: None,
        };
        self.admin().permission_mode(params).await?;
        Ok(())
    }
    pub async fn sync_agent_mode(&mut self) -> Option<String> {
        self.host.inner.sync_agent_mode().await
    }
    pub fn project_trusted(&self) -> bool {
        self.host.inner.project_trusted
    }
    pub async fn set_trust(&mut self, trusted: bool) -> anyhow::Result<()> {
        use loop_wire::RuntimeAdmin;
        let params = loop_protocol::admin::TrustParams {
            workspace_id: self.workspace_id.clone(),
            trusted: Some(trusted),
            expected_revision: None,
        };
        self.admin().trust(params).await?;
        if self.is_remote() {
            self.host.inner.project_trusted = trusted;
        }
        Ok(())
    }
    pub fn supports_sandbox(&self) -> bool {
        self.is_native()
    }
    pub fn supports_ttfb(&self) -> bool {
        self.backend().capabilities().session_ttfb && self.backend().model_control().is_some()
    }
    pub fn sandbox_service(&self) -> Arc<dyn loop_wire::RuntimeAdmin> {
        if let Some(remote) = &self.remote {
            return remote.clone();
        }
        Arc::new(self.native_sandbox())
    }
    fn native_sandbox(&self) -> native_admin::NativeSandbox {
        let r = &self.host.inner;
        native_admin::NativeSandbox {
            native: r.native.clone(),
            harness: r.harness.clone(),
            cwd: r.cwd.clone(),
            agent_dir: r.agent_dir.clone(),
            settings: r.settings.sandbox.clone(),
            web: r.web_tools(),
            search: r.tool_gate.search_filter(),
            bash: r.bash_limits(),
            #[cfg(test)]
            starter: self.sandbox_starter.clone(),
        }
    }
    /// Put the opened Session back in the sandbox it last chose. A failure
    /// is shown with the other startup notices; the Session keeps the
    /// sandbox it has.
    pub async fn restore_sandbox(&mut self) {
        if !self.is_native() {
            return;
        }
        if let Err(error) = self.native_sandbox().restore().await {
            tracing::warn!("could not restore the Session's sandbox: {error}");
            self.host
                .inner
                .startup_notices
                .push(format!("Could not restore this Session's sandbox: {error}"));
        }
    }
    pub fn clear_session_approvals(&self) {
        self.host.clear_session_approvals();
    }
    pub async fn install_interactions(
        &mut self,
        host: loop_app_core::interaction::InteractionQueue,
    ) -> anyhow::Result<()> {
        if let Some(remote) = &self.remote_host {
            remote.install(host);
            return Ok(());
        }
        let r = &mut self.host.inner;
        let env = r.harness.tool_env().await?;
        let grants = r
            .harness
            .read_session_entries()
            .await
            .map(|entries| crate::tool_approval::auto_approve_from_entries(&entries))
            .unwrap_or_default();
        r.agents.set_host(Arc::new(host.clone()));
        let bridge = Arc::new(ToolApprovalBridge::with_host(
            env,
            r.settings.diff_editor.clone(),
            grants,
            Arc::new(host),
        ));
        let harness = r.harness.clone();
        bridge.set_persist(Arc::new(move |label| {
            let harness = harness.clone();
            Box::pin(async move {
                if let Err(error) = harness.append_label(label).await {
                    tracing::warn!(%error, "persist tool approval label");
                }
            })
        }));
        r.tool_gate.set_approver(Some(bridge.clone()));
        r.tool_gate
            .set_mode_recorder(Some(loop_app_core::tool_gate::mode_recorder(&r.native)));
        r.tool_approval = Some(bridge.clone());
        Ok(())
    }
    pub fn review_grants(&self) -> std::collections::HashSet<String> {
        self.host
            .inner
            .tool_approval
            .as_ref()
            .map(|b| b.auto_approve_groups())
            .unwrap_or_default()
    }
    pub fn mcp_configured(&self) -> bool {
        if let Some(mcp) = &self.remote_mcp {
            return !mcp.servers.is_empty();
        }
        !self.host.inner.settings.mcp_servers.is_empty()
    }
    pub fn mcp_enabled(&self, name: &str) -> Option<bool> {
        if let Some(mcp) = &self.remote_mcp {
            return mcp
                .servers
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.enabled);
        }
        self.host
            .inner
            .settings
            .mcp_servers
            .contains_key(name)
            .then(|| {
                !self
                    .host
                    .inner
                    .settings
                    .disabled_mcp_servers
                    .iter()
                    .any(|n| n == name)
            })
    }
    pub fn mcp_choices(&self) -> Vec<(String, String)> {
        if let Some(mcp) = &self.remote_mcp {
            return mcp
                .servers
                .iter()
                .map(|s| (s.name.clone(), format!("{} tools", s.tools)))
                .collect();
        }
        self.host
            .inner
            .settings
            .mcp_servers
            .iter()
            .map(|(name, cfg)| {
                let hint = cfg
                    .url
                    .clone()
                    .or_else(|| {
                        cfg.command.as_ref().map(|command| {
                            format!("{command} {}", cfg.args.join(" "))
                                .trim()
                                .to_string()
                        })
                    })
                    .unwrap_or_else(|| "not configured".into());
                (name.clone(), hint)
            })
            .collect()
    }
    pub async fn mcp(
        &mut self,
        action: loop_protocol::admin::AdminAction,
        name: Option<String>,
    ) -> anyhow::Result<loop_protocol::admin::McpResult> {
        use loop_wire::RuntimeAdmin;
        let params = loop_protocol::admin::McpParams {
            workspace_id: self.workspace_id.clone(),
            action,
            name,
            expected_revision: None,
        };
        let result = self.admin().mcp(params).await?;
        if self.is_remote() {
            self.remote_mcp = Some(result.clone());
        }
        Ok(result)
    }
    pub async fn mcp_tools(&self) -> Vec<(String, Vec<String>)> {
        self.host.inner.mcp_client.list_connection_tools().await
    }
}

impl SurfaceClient {
    pub fn note_sandbox(&mut self, mode: &str, isolation: Option<String>, runtime: Option<String>) {
        let settings = &mut self.host.inner.settings.sandbox;
        settings.mode = mode.into();
        settings.isolation = isolation;
        settings.runtime = runtime;
    }
    pub fn mcp_login(
        &self,
        name: &str,
        on_url: impl FnOnce(&str) + Send + 'static,
    ) -> anyhow::Result<
        futures::future::BoxFuture<'static, Result<loop_mcp::OAuthLoginOutcome, String>>,
    > {
        if self.is_remote() {
            anyhow::bail!("run MCP login on the Runtime host");
        }
        let r = &self.host.inner;
        let cfg = r
            .settings
            .mcp_servers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("mcp server not found: {name} (see /mcp list)"))?;
        let url = cfg.url.clone().ok_or_else(|| {
            anyhow::anyhow!("mcp `{name}` is a local command server and does not use OAuth")
        })?;
        if r.mcp_client
            .oauth_credential_path(name)
            .map_err(|e| anyhow::anyhow!(e))?
            .is_none()
        {
            anyhow::bail!("OAuth credential storage is not configured for this session");
        }
        let client = r.mcp_client.clone();
        let name = name.to_string();
        Ok(Box::pin(async move {
            client.oauth_login(&name, &url, on_url).await
        }))
    }
    pub async fn finish_mcp_login(&self, name: &str) -> anyhow::Result<Option<usize>> {
        if self.is_remote() {
            anyhow::bail!("run MCP login on the Runtime host");
        }
        let r = &self.host.inner;
        if self.mcp_enabled(name) == Some(false) {
            return Ok(None);
        }
        let cfg = r
            .settings
            .mcp_servers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("it is no longer configured"))?;
        let entry = loop_app_core::runtime::mcp_server_entries(&std::collections::BTreeMap::from(
            [(name.into(), cfg.clone())],
        ))
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("it is no longer configured"))?;
        r.mcp_client.disconnect(name).await;
        let count = r
            .mcp_client
            .connect(&entry)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        r.refresh_mcp_tools().await?;
        Ok(Some(count))
    }
    pub async fn mcp_logout(&self, name: &str) -> anyhow::Result<bool> {
        if self.is_remote() {
            anyhow::bail!("run MCP logout on the Runtime host");
        }
        let r = &self.host.inner;
        if self.mcp_enabled(name).is_none() {
            anyhow::bail!("mcp server not found: {name} (see /mcp list)");
        }
        let removed = r
            .mcp_client
            .clear_oauth_credentials(name)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if removed {
            r.mcp_client.disconnect(name).await;
            r.refresh_mcp_tools().await?;
        }
        Ok(removed)
    }
}

impl SurfaceClient {
    pub fn sandbox_plan(
        &self,
        flags: loop_app_core::sandbox_builder::LocalSandboxFlags,
    ) -> anyhow::Result<(loop_protocol::admin::SandboxChoice, String)> {
        use loop_app_core::sandbox_builder::{SandboxBuilder, SandboxRequest};
        let request = SandboxRequest::for_flags(&self.host.inner.settings.sandbox, flags);
        let plan = SandboxBuilder::for_current_platform()
            .with_agent_dir(self.agent_dir())
            .resolve(&request)?;
        Ok((
            loop_protocol::admin::SandboxChoice {
                mode: "local".into(),
                isolation: flags.isolation.map(|i| i.as_str().into()),
                runtime: flags.runtime.map(|r| r.as_str().into()),
            },
            plan.label(),
        ))
    }
}

/// Immutable workspace definitions for local pickers and autocomplete.
#[derive(Default, Clone)]
pub struct ResourceView {
    pub skills: Vec<loop_protocol::admin::SkillStatus>,
    pub templates: Vec<loop_protocol::admin::TemplateInfo>,
    pub active_skills: Vec<String>,
    pub extensions: usize,
    pub hooks: usize,
}
impl SurfaceClient {
    pub fn resource_view(&self) -> ResourceView {
        if let Some(view) = &self.remote_resources {
            return view.clone();
        }
        let r = &self.host.inner;
        ResourceView {
            skills: r
                .resources
                .all_skills()
                .into_iter()
                .map(|(skill, enabled)| loop_protocol::admin::SkillStatus {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    enabled,
                })
                .collect(),
            templates: r
                .resources
                .prompts
                .iter()
                .map(|p| loop_protocol::admin::TemplateInfo {
                    name: p.name.clone(),
                    description: p.argument_hint.as_ref().map_or_else(
                        || "prompt template".into(),
                        |h| format!("{h} — prompt template"),
                    ),
                })
                .collect(),
            active_skills: r.active_skills.clone(),
            extensions: r.resources.extension_paths.len(),
            hooks: r.resources.hook_paths.len(),
        }
    }
    pub async fn set_skill_enabled(&mut self, name: &str, enabled: bool) -> anyhow::Result<bool> {
        use loop_protocol::admin::*;
        use loop_wire::RuntimeAdmin;
        if !self.resource_view().skills.iter().any(|s| s.name == name) {
            return Ok(false);
        }
        let params = SkillsParams {
            workspace_id: self.workspace_id.clone(),
            action: if enabled {
                AdminAction::Enable
            } else {
                AdminAction::Disable
            },
            name: Some(name.into()),
            expected_revision: None,
        };
        let result = self.admin().skills(params).await?;
        if let Some(view) = &mut self.remote_resources {
            view.skills = result.skills;
        }
        Ok(true)
    }
    pub async fn render_template(
        &mut self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<Option<String>> {
        use loop_protocol::admin::*;
        use loop_wire::RuntimeAdmin;
        if !self
            .resource_view()
            .templates
            .iter()
            .any(|s| s.name == name)
        {
            return Ok(None);
        }
        let params = TemplatesParams {
            workspace_id: self.workspace_id.clone(),
            name: Some(name.into()),
            args: loop_agent::harness::prompt_templates::parse_command_args(args),
        };
        Ok(self.admin().templates(params).await?.text)
    }
    pub async fn reload(&mut self) -> anyhow::Result<()> {
        use loop_wire::RuntimeAdmin;
        let params = loop_protocol::admin::ReloadParams {
            workspace_id: self.workspace_id.clone(),
            expected_revision: None,
        };
        self.admin().reload(params).await?;
        Ok(())
    }
    pub async fn activate_skill(&mut self, name: &str) -> anyhow::Result<bool> {
        if !self
            .resource_view()
            .skills
            .iter()
            .any(|s| s.name == name && s.enabled)
        {
            return Ok(false);
        }
        self.sessions
            .skill(SkillParams {
                session: self.session_ref(),
                name: name.into(),
                active: true,
                expected_revision: None,
            })
            .await?;
        if !self.host.inner.active_skills.iter().any(|n| n == name) {
            self.host.inner.active_skills.push(name.into());
        }
        Ok(true)
    }
    #[cfg(feature = "orchestration")]
    pub async fn workflow(
        &self,
        goal: &str,
        concurrency: Option<usize>,
    ) -> anyhow::Result<WorkflowResult> {
        if !self.is_native() {
            anyhow::bail!(
                "workflows run with Loop's own agent · pick a Loop model with /model first"
            );
        }
        Ok(self
            .sessions
            .workflow(WorkflowParams {
                session: self.session_ref(),
                goal: goal.into(),
                concurrency: concurrency.map(|n| n as u32),
                request_id: loop_ai::new_id(),
            })
            .await?)
    }
}

impl SurfaceClient {
    pub fn skill_source(&self, name: &str) -> Option<String> {
        self.host
            .inner
            .resources
            .all_skills()
            .into_iter()
            .find(|(s, _)| s.name == name)
            .map(|(s, _)| s.path.display().to_string())
    }
}

impl SurfaceClient {
    pub async fn effective_settings(
        &mut self,
    ) -> anyhow::Result<loop_protocol::admin::SettingsResult> {
        use loop_wire::RuntimeAdmin;
        let p = loop_protocol::admin::SettingsParams {
            workspace_id: self.workspace_id.clone(),
        };
        Ok(self.admin().settings(p).await?)
    }
    pub async fn session_config(&self) -> anyhow::Result<SessionSnapshot> {
        Ok(self
            .sessions
            .config(SessionParams {
                session: self.session_ref(),
            })
            .await?)
    }
    pub async fn stats(&self) -> anyhow::Result<loop_protocol::StatsSummary> {
        let service = self
            .backend()
            .session_stats()
            .ok_or_else(|| anyhow::anyhow!("this agent does not report session statistics"))?;
        Ok(service.stats(&self.session_ref()).await?)
    }
    #[cfg(feature = "usage")]
    pub async fn usage_history(
        &mut self,
        days: Option<u32>,
    ) -> anyhow::Result<loop_protocol::admin::UsageResult> {
        use loop_wire::RuntimeAdmin;
        Ok(self
            .admin()
            .usage(loop_protocol::admin::UsageParams {
                days,
                ..Default::default()
            })
            .await?)
    }
    pub async fn loaded_sessions(
        &mut self,
    ) -> anyhow::Result<loop_protocol::admin::RuntimeSessionsResult> {
        use loop_wire::RuntimeAdmin;
        Ok(self.admin().sessions().await?)
    }
}

impl SurfaceClient {
    pub fn agent_label(&self, id: &str) -> Option<String> {
        self.host
            .inner
            .agents
            .entry(id)
            .map(|e| e.display_name().to_string())
    }
    pub fn web_label(&self) -> String {
        self.host.inner.web_tools().status()
    }
    #[cfg(feature = "telemetry")]
    pub fn usage_log_status(&self) -> Option<(String, Option<String>)> {
        self.host
            .inner
            .usage_log()
            .map(|log| (log.path.display().to_string(), log.last_error))
    }
    pub async fn settings_report(&mut self) -> anyhow::Result<String> {
        let view = self.effective_settings().await?;
        let config = self.session_config().await?;
        let settings: crate::config::Settings = serde_json::from_value(view.effective)?;
        let mut perms = String::new();
        for (k, v) in &settings.tool_permissions {
            perms.push_str(&format!("\n    {k}: {v}"));
        }
        Ok(format!("settings ({})\n  model: {}\n  theme: {}\n  thinking: {}\n  sandbox: {}\n  permissionMode: {}\n  toolPermissions:{perms}\n  diffEditor: {}\n  responseHeaderTimeoutMs: {}{}\n  Theme and UI preferences are Surface-local.", crate::config::paths::settings_path(self.agent_dir()).display(), config.config.model.unwrap_or_else(|| "none".into()), self.theme().name, config.config.effort.unwrap_or(settings.default_thinking_level), settings.sandbox.display(), self.permission_mode().as_str(), settings.diff_editor.as_deref().unwrap_or("(auto)"), settings.response_header_timeout_ms, if settings.response_header_timeout_ms == 0 { " (off)" } else { "" }))
    }
}

/// Surface-owned preferences, kept apart from effective host settings.
#[derive(Clone)]
pub struct UiPreferences {
    pub hide_thinking: bool,
    pub double_escape_action: String,
}
impl SurfaceClient {
    pub fn local_preferences(&self) -> UiPreferences {
        UiPreferences {
            hide_thinking: self.host.inner.settings.hide_thinking_block,
            double_escape_action: self.host.inner.settings.double_escape_action.clone(),
        }
    }
    pub fn save_theme_preference(&mut self, name: &str) -> anyhow::Result<()> {
        let path = crate::config::paths::settings_path(self.agent_dir());
        let mut local = crate::config::Settings::load_file(&path)?;
        local.theme = name.into();
        local.save_file(&path)?;
        self.host.inner.settings.theme = name.into();
        Ok(())
    }
    pub fn agent_signed_in(&self, id: &str) -> bool {
        self.host.inner.agents.is_signed_in(id)
    }
    pub fn active_agent_name(&self) -> Option<&str> {
        self.active_agent()
            .as_deref()
            .and_then(|id| self.host.inner.agents.entry(id))
            .map(|e| e.display_name())
    }
    pub fn subscription_models(&self) -> Vec<String> {
        let agents = &self.host.inner.agents;
        agents
            .entries()
            .iter()
            .filter(|e| agents.is_signed_in(&e.id))
            .flat_map(|e| {
                agents
                    .models(&e.id)
                    .into_iter()
                    .map(move |m| format!("{}/{}", e.id, m.id))
            })
            .collect()
    }
    pub fn login_agents(&self) -> Vec<crate::provider_setup::AgentRow> {
        let agents = &self.host.inner.agents;
        agents
            .entries()
            .iter()
            .filter(|e| e.needs_sign_in())
            .map(|e| crate::provider_setup::AgentRow {
                id: e.id.clone(),
                label: e.login_label().into(),
                configured: agents.is_signed_in(&e.id),
            })
            .collect()
    }
    pub fn connected_providers(&self) -> Vec<String> {
        self.host.inner.connected_providers()
    }
    pub fn logout_choices(&self) -> Vec<String> {
        use loop_ai::CredentialStore;
        let mut connected = self.connected_providers();
        if let Some(provider) = self
            .host
            .inner
            .settings
            .web_search
            .as_ref()
            .and_then(|w| w.provider.clone())
            .filter(|p| !self.host.inner.credentials.is_logged_out(p))
        {
            if !connected.contains(&provider) {
                connected.push(provider);
            }
        }
        connected
    }
    pub fn subscription_start(
        &self,
        pending: PendingAgent,
        forward: impl FnOnce(&dyn AgentBackend) + Send + 'static,
    ) -> SubscriptionStart {
        let agents = self.host.inner.agents.clone();
        let forwarded = self.forwarded.clone();
        Box::pin(async move {
            let id = match &pending {
                PendingAgent::New { id, .. } => id.clone(),
                PendingAgent::Resume(r) => r.agent.clone(),
            };
            let backend = agents.backend(&id).await.map_err(|e| format!("{e:#}"))?;
            if forwarded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.clone())
            {
                forward(backend.as_ref());
            }
            let result = match pending {
                PendingAgent::New { model, .. } => agents.new_session(&id, model.as_deref()).await,
                PendingAgent::Resume(r) => agents.resume(&r).await,
            };
            result
                .map(|(backend, session)| (backend as Arc<dyn AgentBackend>, session))
                .map_err(|e| format!("{e:#}"))
        })
    }
    pub fn subscription_login(
        &self,
        id: String,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<usize>> {
        let agents = self.host.inner.agents.clone();
        Box::pin(async move { agents.sign_in(&id).await })
    }
    pub async fn subscription_logout(&mut self, id: &str) -> anyhow::Result<()> {
        if self.active_agent().as_deref() == Some(id) {
            self.use_native().await;
        }
        self.host.inner.agents.sign_out(id).await
    }
    pub async fn connect_provider(
        &mut self,
        request: &crate::config::ProviderLoginRequest,
    ) -> anyhow::Result<loop_protocol::host::ProviderInfo> {
        use loop_wire::RuntimeAdmin;
        let p = loop_protocol::host::ProvidersParams {
            action: loop_protocol::host::HostAction::Connect,
            provider: None,
            login: Some(serde_json::to_value(request)?),
        };
        self.admin()
            .providers(p)
            .await?
            .providers
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("provider returned no connection state"))
    }
    pub async fn logout_provider(&mut self, id: &str) -> anyhow::Result<(String, String)> {
        use loop_wire::RuntimeAdmin;
        let p = loop_protocol::host::ProvidersParams {
            action: loop_protocol::host::HostAction::Disconnect,
            provider: Some(id.into()),
            login: None,
        };
        let result = self.admin().providers(p).await?;
        Ok((
            result
                .providers
                .into_iter()
                .next()
                .map_or_else(|| id.into(), |p| p.name),
            result.notices.join(" · "),
        ))
    }
    pub async fn web_search(&mut self, arg: Option<&str>) -> anyhow::Result<String> {
        use loop_wire::RuntimeAdmin;
        let p = loop_protocol::admin::WebSearchParams {
            workspace_id: self.workspace_id.clone(),
            enabled: match arg {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            },
            provider: arg
                .filter(|a| *a != "on" && *a != "off")
                .map(str::to_string),
            expected_revision: None,
        };
        Ok(self.admin().web_search(p).await?.status)
    }
    #[cfg(feature = "telemetry")]
    pub fn tracing_settings(&self) -> crate::config::TracingSettings {
        self.host.inner.settings.tracing.clone()
    }
    #[cfg(feature = "telemetry")]
    pub async fn tracing(
        &mut self,
        action: loop_protocol::host::HostAction,
        config: Option<&crate::config::TracingSetupRequest>,
    ) -> anyhow::Result<String> {
        use loop_wire::RuntimeAdmin;
        let p = loop_protocol::host::TracingParams {
            action,
            config: config.map(serde_json::to_value).transpose()?,
        };
        Ok(self.admin().tracing(p).await?.status)
    }
    pub async fn ttfb(
        &self,
        change: Option<Option<u64>>,
    ) -> anyhow::Result<loop_protocol::rpc::TtfbResult> {
        let control = self
            .backend()
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("this agent does not offer response-timeout control"))?;
        Ok(control.ttfb(&self.session_ref(), change, None).await?)
    }
    pub async fn timeout_default(&mut self, value: Option<u64>) -> anyhow::Result<u64> {
        use loop_wire::RuntimeAdmin;
        let p = loop_protocol::admin::DefaultsParams {
            patch: value.map(|ms| loop_protocol::admin::DefaultsPatch {
                response_header_timeout_ms: Some(ms),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(self
            .admin()
            .defaults(p)
            .await?
            .response_header_timeout_ms
            .unwrap_or(0))
    }
    pub async fn review_report(&mut self) -> anyhow::Result<String> {
        let view = self.effective_settings().await?;
        let settings: crate::config::Settings = serde_json::from_value(view.effective)?;
        let groups = self.review_grants();
        let grants = [
            crate::tool_approval::GROUP_FILE,
            crate::tool_approval::GROUP_BASH,
            crate::tool_approval::GROUP_WEB,
        ]
        .into_iter()
        .filter(|g| groups.contains(*g))
        .collect::<Vec<_>>();
        let grants = if grants.is_empty() {
            "(none)".into()
        } else {
            grants.join(", ")
        };
        let mut perms = String::new();
        for (k, v) in settings.tool_permissions {
            perms.push_str(&format!("\n    {k}: {v}"));
        }
        Ok(format!("permission mode: {} (change it with /permissions)\n  session auto-approve: {grants}\n  settings.toolPermissions:{perms}\n  settings.diffEditor: {}", self.permission_mode().as_str(), settings.diff_editor.as_deref().unwrap_or("(auto: cursor|code)")))
    }
}

impl SurfaceClient {
    pub fn confirm_timeout_default(&mut self, ms: u64) -> bool {
        if self.confirm_timeout_default.take() == Some(ms) {
            true
        } else {
            self.confirm_timeout_default = Some(ms);
            false
        }
    }
}

/// An in-process subscription backend and its opened Session.
pub type SubscriptionSession = (Arc<dyn AgentBackend>, SessionRef);
/// Subscription admission, with updates forwarded before the Session opens.
pub type SubscriptionStart =
    futures::future::BoxFuture<'static, Result<SubscriptionSession, String>>;

/// The application UI imports this view namespace. It deliberately exposes no
/// native owner, bootstrap Runtime, credential store, or mutable host settings.
/// ```compile_fail
/// use loop_cli::surface_client::ui_api as loop_app_core;
/// fn host(_: loop_app_core::Runtime) {}
/// ```
/// ```compile_fail
/// use loop_cli::surface_client::ui_api;
/// fn host(_: ui_api::CliRuntime) {}
/// ```
pub mod ui_api {
    pub use loop_app_core::{interaction, permissions, sessions};
}
