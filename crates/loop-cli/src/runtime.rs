//! Bootstrap Models, sessions, harness, and first-run auth.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use rustyline::DefaultEditor;

use loop_agent::harness::{
    create_bash_tool, create_edit_tool, create_read_tool, create_session_repository,
    create_sqlite_session_store, create_write_tool, AgentHarness, AgentHarnessOptions,
    AgentHarnessResources, HostExecutionEnv, LocalShellSandbox, Sandbox, SandboxConfig,
    SandboxMode,
};
use loop_agent::types::AgentThinkingLevel;
use loop_ai::providers::{
    custom_provider, soket_provider, CustomModelSpec, CustomProviderConfig, SOKET_API_KEY_ENVS,
    SOKET_DEFAULT_MODEL_ID, SOKET_PROVIDER_ID,
};
use loop_ai::{
    CreateModelsOptions, FileModelsStore, Models, ModelsRefreshOptions,
};

use crate::config::auth::{provider_has_key, FileCredentialStore};
use crate::config::paths::{
    auth_path, ensure_agent_dirs, get_agent_dir, models_json_path, models_store_path,
    sessions_db_path, settings_path,
};
use crate::config::settings::{load_settings, Settings};
use crate::config::trust::TrustStore;
use crate::config::paths::{keybindings_path, trust_path};
use crate::keybindings::Keybindings;
use crate::resources::{load_resources, LoadedResources};
use crate::system_prompt::{
    build_system_prompt, default_tool_snippets, load_context_files, resolve_system_prompt_files,
    BuildSystemPromptOptions,
};
use crate::theme::{theme_search_dirs, Theme};

/// Fully constructed interactive runtime.
pub struct Runtime {
    /// Agent config dir.
    pub agent_dir: PathBuf,
    /// Cwd.
    pub cwd: PathBuf,
    /// Settings.
    pub settings: Settings,
    /// Models collection.
    pub models: Arc<Models>,
    /// Credential store.
    pub credentials: Arc<FileCredentialStore>,
    /// Harness.
    pub harness: Arc<AgentHarness>,
    /// Theme.
    pub theme: Theme,
    /// Keybindings.
    pub keybindings: Keybindings,
    /// Resources.
    pub resources: LoadedResources,
    /// Project trusted.
    pub project_trusted: bool,
    /// Trust store.
    pub trust: TrustStore,
    /// Session store path.
    pub sessions_db: PathBuf,
    /// Active session id (persisted in SQLite; sent to the provider API).
    pub session_id: String,
    /// True when started with `--resume <id>` (transcript hydrated from store).
    pub resumed: bool,
    /// When true, TUI should show the first-run API key setup box.
    pub needs_api_key_setup: bool,
    /// Interactive tool approval bridge (set by the TUI).
    pub tool_approval: Option<std::sync::Arc<crate::tool_approval::ToolApprovalBridge>>,
}

/// CLI bootstrap flags affecting runtime.
pub struct BootstrapOpts {
    /// Working directory.
    pub cwd: PathBuf,
    /// Provider override.
    pub provider: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Theme override.
    pub theme: Option<String>,
    /// System prompt override.
    pub system_prompt: Option<String>,
    /// Append system prompt.
    pub append_system_prompt: Option<String>,
    /// Skip context files.
    pub no_context_files: bool,
    /// Interactive (prompt for API key).
    pub interactive: bool,
    /// Resume session id.
    pub session_id: Option<String>,
}

/// Ensure a Soket API key exists, or defer prompting to the TUI when interactive.
///
/// Returns `true` when the TUI should show the first-run setup box.
pub fn ensure_soket_api_key(
    store: &FileCredentialStore,
    interactive: bool,
) -> anyhow::Result<bool> {
    if provider_has_key(store, SOKET_PROVIDER_ID, SOKET_API_KEY_ENVS) {
        return Ok(false);
    }
    if !interactive {
        bail!(
            "Soket API key not set. Set SOKET_API_KEY / TENSORSTUDIO_API_KEY / LOOP_API_KEY or run interactively to enter a key."
        );
    }
    // Defer to the welcome/setup UI inside the TUI.
    Ok(true)
}

/// Build models with Soket + optional models.json customs.
pub fn build_models(
    agent_dir: &Path,
    credentials: Arc<FileCredentialStore>,
) -> anyhow::Result<Arc<Models>> {
    let store = Arc::new(FileModelsStore::new(models_store_path(agent_dir)));
    let models = Arc::new(Models::create(CreateModelsOptions {
        credentials: Some(credentials),
        models_store: Some(store),
    }));
    models.set_provider(soket_provider());
    load_custom_models_json(agent_dir, &models)?;
    Ok(models)
}

fn load_custom_models_json(agent_dir: &Path, models: &Models) -> anyhow::Result<()> {
    let path = models_json_path(agent_dir);
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    // Support { "providers": [ { id, baseUrl, apiKeyEnv, models: ["id"] } ] }
    if let Some(arr) = value.get("providers").and_then(|v| v.as_array()) {
        for p in arr {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("custom");
            let base_url = p
                .get("baseUrl")
                .or_else(|| p.get("base_url"))
                .and_then(|v| v.as_str())
                .unwrap_or("http://localhost:11434/v1");
            let api_key_env = p
                .get("apiKeyEnv")
                .or_else(|| p.get("api_key_env"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let model_specs = p
                .get("models")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| {
                            if let Some(s) = m.as_str() {
                                Some(CustomModelSpec::new(s))
                            } else {
                                let id = m.get("id")?.as_str()?;
                                Some(CustomModelSpec::new(id))
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if model_specs.is_empty() {
                continue;
            }
            models.set_provider(custom_provider(CustomProviderConfig {
                id: id.into(),
                name: p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                base_url: base_url.into(),
                api_key_env,
                models: model_specs,
                headers: None,
            }));
        }
    }
    Ok(())
}

/// Resolve project trust (ask interactively when needed).
pub fn resolve_trust(
    trust: &mut TrustStore,
    cwd: &Path,
    settings: &Settings,
    interactive: bool,
) -> anyhow::Result<bool> {
    if let Some(v) = trust.get(cwd) {
        return Ok(v);
    }
    match settings.default_project_trust.as_str() {
        "always" => {
            trust.set(cwd, true)?;
            Ok(true)
        }
        "never" => {
            trust.set(cwd, false)?;
            Ok(false)
        }
        _ => {
            if !interactive {
                return Ok(false);
            }
            eprintln!(
                "Trust project config from {}? [y/N]",
                cwd.display()
            );
            let mut rl = DefaultEditor::new()?;
            let answer = rl.readline("> ")?;
            let yes = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
            trust.set(cwd, yes)?;
            Ok(yes)
        }
    }
}

fn parse_thinking(s: &str) -> AgentThinkingLevel {
    match s.to_lowercase().as_str() {
        "minimal" => AgentThinkingLevel::Minimal,
        "low" => AgentThinkingLevel::Low,
        "medium" => AgentThinkingLevel::Medium,
        "high" => AgentThinkingLevel::High,
        "xhigh" => AgentThinkingLevel::XHigh,
        "max" => AgentThinkingLevel::Max,
        _ => AgentThinkingLevel::Off,
    }
}

fn thinking_label(level: AgentThinkingLevel) -> &'static str {
    match level {
        AgentThinkingLevel::Off => "off",
        AgentThinkingLevel::Minimal => "minimal",
        AgentThinkingLevel::Low => "low",
        AgentThinkingLevel::Medium => "medium",
        AgentThinkingLevel::High => "high",
        AgentThinkingLevel::XHigh => "xhigh",
        AgentThinkingLevel::Max => "max",
    }
}

/// Bootstrap the full runtime.
pub async fn bootstrap(opts: BootstrapOpts) -> anyhow::Result<Runtime> {
    let agent_dir = get_agent_dir();
    ensure_agent_dirs(&agent_dir)?;
    std::env::set_var("LOOP_CODING_AGENT", "true");

    let mut trust = TrustStore::load(trust_path(&agent_dir))?;
    // Load global settings first for trust default
    let global_settings = Settings::load_file(&settings_path(&agent_dir))?;
    if !settings_path(&agent_dir).exists() {
        global_settings.save_file(&settings_path(&agent_dir))?;
    }

    let project_trusted = resolve_trust(&mut trust, &opts.cwd, &global_settings, opts.interactive)?;
    let mut settings = load_settings(&agent_dir, &opts.cwd, project_trusted)?;
    if let Some(t) = &opts.theme {
        settings.theme = t.clone();
    }
    if let Some(p) = &opts.provider {
        settings.default_provider = p.clone();
    }
    if let Some(m) = &opts.model {
        settings.default_model = m.clone();
    }

    let credentials = Arc::new(FileCredentialStore::open(auth_path(&agent_dir))?);
    let needs_api_key_setup = ensure_soket_api_key(&credentials, opts.interactive)?;

    let models = build_models(&agent_dir, Arc::clone(&credentials))?;
    if !needs_api_key_setup {
        let refresh = models
            .refresh(ModelsRefreshOptions {
                allow_network: Some(true),
                force: true,
                provider_id: Some(SOKET_PROVIDER_ID.into()),
            })
            .await;
        if !refresh.errors.is_empty() {
            for (pid, err) in &refresh.errors {
                tracing::warn!("model refresh {pid}: {err}");
            }
        }
    }

    let provider = settings.default_provider.clone();
    let model_id = settings.default_model.clone();
    let mut model = models
        .get_model(&provider, &model_id)
        .or_else(|| models.get_model(SOKET_PROVIDER_ID, SOKET_DEFAULT_MODEL_ID))
        .or_else(|| models.get_models(None).into_iter().next())
        .context("no models available")?;

    let resources = load_resources(&agent_dir, &opts.cwd, project_trusted, &settings);
    let context_files = if opts.no_context_files {
        vec![]
    } else {
        load_context_files(&opts.cwd, &agent_dir)
    };
    let (custom, append) = resolve_system_prompt_files(
        &opts.cwd,
        project_trusted,
        opts.system_prompt.as_deref(),
        opts.append_system_prompt.as_deref(),
    );
    let snippets = default_tool_snippets();
    let selected = ["read", "bash", "edit", "write"];
    let system_prompt = build_system_prompt(BuildSystemPromptOptions {
        custom_prompt: custom.as_deref(),
        append_system_prompt: append.as_deref(),
        cwd: &opts.cwd,
        selected_tools: &selected,
        tool_snippets: &snippets,
        context_files: &context_files,
        skills: &resources.skills,
    });

    let sessions_db = sessions_db_path(&agent_dir);
    let store = create_sqlite_session_store(&sessions_db)
        .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
    let repo = create_session_repository(store, None);
    let resumed = opts.session_id.is_some();
    let session = if let Some(id) = &opts.session_id {
        repo.open(id)
            .await
            .map_err(|e| anyhow::anyhow!("load session: {e}"))?
    } else {
        repo.create(Some(opts.cwd.to_string_lossy().into_owned()), None)
            .await
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?
    };

    // Restore model from the session branch when resuming (unless CLI overrides).
    if resumed {
        if let Ok(ctx) = session.build_context().await {
            if opts.provider.is_none() && opts.model.is_none() {
                if let Some((p, m)) = &ctx.model {
                    if let Some(resolved) = models.get_model(p, m) {
                        settings.default_provider = p.clone();
                        settings.default_model = m.clone();
                        model = resolved;
                    }
                }
            }
            // Thinking-level changes are recorded on the branch when present.
            if !matches!(ctx.thinking_level, AgentThinkingLevel::Off) {
                settings.default_thinking_level = thinking_label(ctx.thinking_level).into();
            }
        }
    }

    let host = Arc::new(HostExecutionEnv::new(&opts.cwd));
    let tools = vec![
        create_read_tool(Arc::clone(&host) as _),
        create_write_tool(Arc::clone(&host) as _),
        create_edit_tool(Arc::clone(&host) as _),
        create_bash_tool(Arc::clone(&host) as _),
    ];

    let sandbox = match settings.sandbox.mode.as_str() {
        "local-shell" => {
            let sb = LocalShellSandbox::new(SandboxConfig {
                workdir: opts.cwd.clone(),
                ..Default::default()
            });
            sb.start()
                .await
                .map_err(|e| anyhow::anyhow!("sandbox: {e}"))?;
            SandboxMode::Enabled {
                sandbox: Arc::new(sb),
            }
        }
        _ => SandboxMode::Disabled,
    };

    let session_id = session.metadata().id.clone();
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(&models),
        model,
        session,
        host_env: host,
        tools,
        system_prompt,
        sandbox,
        resources: AgentHarnessResources {
            skills: resources.skills.clone(),
            prompt_templates: resources.prompts.clone(),
        },
    }));
    harness
        .set_thinking_level(parse_thinking(&settings.default_thinking_level))
        .await;

    crate::hooks_load::register_json_hooks(&harness, &resources.hook_paths);
    let ext = crate::extensions::load_extensions(&resources.extension_paths);
    for n in &ext.notices {
        tracing::info!("extension: {n}");
    }

    let theme_dirs = theme_search_dirs(
        &agent_dir,
        project_trusted.then_some(crate::config::paths::get_project_dir(&opts.cwd)).as_ref().map(|p| p.as_path()),
    );
    let theme = Theme::load(&settings.theme, &theme_dirs).unwrap_or_else(|_| Theme::dark());
    let keybindings = Keybindings::load(&keybindings_path(&agent_dir))?;

    Ok(Runtime {
        agent_dir,
        cwd: opts.cwd,
        settings,
        models,
        credentials,
        harness,
        theme,
        keybindings,
        resources,
        project_trusted,
        trust,
        sessions_db,
        session_id,
        resumed,
        needs_api_key_setup,
        tool_approval: None,
    })
}
