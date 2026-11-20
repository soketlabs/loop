//! Global and project settings.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::paths::{get_project_dir, settings_path};
use super::providers::CustomProviderEntry;
use super::tracing::TracingSettings;

/// Compaction settings subset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettingsJson {
    /// Enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Reserve tokens.
    #[serde(default = "default_reserve")]
    pub reserve_tokens: u64,
    /// Keep recent tokens.
    #[serde(default = "default_keep")]
    pub keep_recent_tokens: u64,
    /// Compact once the context reaches this many tokens (unset: only near the window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_tokens: Option<u64>,
    /// Without `triggerTokens`, compact at this percentage of the model's context window.
    #[serde(default = "default_trigger_percent")]
    pub trigger_percent: u64,
}

fn default_true() -> bool {
    true
}
fn default_trigger_percent() -> u64 {
    90
}
fn default_reserve() -> u64 {
    16_384
}
fn default_keep() -> u64 {
    20_000
}
impl Default for CompactionSettingsJson {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: default_reserve(),
            keep_recent_tokens: default_keep(),
            trigger_tokens: None,
            trigger_percent: default_trigger_percent(),
        }
    }
}

/// Sandbox settings.
///
/// `isolation` and `runtime` are optional: unset means the platform default
/// (Linux: `full` and `runc`; macOS: `partial`, no container runtime). Only values
/// written in `settings.json` count as chosen by the user, so they are never filled
/// in with defaults before saving.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// `off` or `local`.
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
    /// When `mode` is `local`: `full` or `partial`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    /// When `mode` is `local`: `runc`, `crun`, `runsc`, or `krun` (Linux only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Allow network access from sandboxed commands (default off). Not yet
    /// enforced by the Linux Podman sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Extra paths sandboxed commands may write, besides the project. `~` is the
    /// home directory. Not yet enforced by the Linux Podman sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_write: Vec<String>,
    /// Deny sandboxed commands and file tools reads under the home directory,
    /// except the project, `allowWrite`, `allowRead` and git/Rust toolchain
    /// settings (default off; macOS Seatbelt only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_home_reads: Option<bool>,
    /// Paths under the home directory that stay readable with
    /// `denyHomeReads`. `~` is the home directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_read: Vec<String>,
}

fn default_sandbox_mode() -> String {
    "off".into()
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            isolation: None,
            runtime: None,
            network: None,
            allow_write: Vec::new(),
            deny_home_reads: None,
            allow_read: Vec::new(),
        }
    }
}

/// Configuration for an external MCP server (stdio or remote HTTP).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// Command to spawn a stdio MCP server. Mutually exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to the command (stdio only).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process (stdio only).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// URL for a remote streamable-HTTP MCP server. Mutually exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra HTTP headers sent with every request (remote only).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl SandboxSettings {
    /// Whether sandboxed commands may use the network.
    pub fn network(&self) -> bool {
        self.network.unwrap_or(false)
    }

    /// Whether reads under the home directory are denied.
    pub fn deny_home_reads(&self) -> bool {
        self.deny_home_reads.unwrap_or(false)
    }

    /// Human-readable short status (settings only; prefer harness `sandbox_info` for live details).
    pub fn display(&self) -> String {
        match self.mode.as_str() {
            "local" => {
                let builder = crate::sandbox_builder::SandboxBuilder::for_current_platform();
                match crate::sandbox_builder::SandboxRequest::from_settings(self)
                    .and_then(|request| builder.resolve(&request))
                {
                    Ok(plan) => format!("local {}", plan.label()),
                    Err(e) => format!("local (invalid: {e})"),
                }
            }
            other => other.to_string(),
        }
    }

    /// Overlay project sandbox settings. A project may enable the sandbox, never
    /// disable it. Isolation and
    /// runtime only when the project sets them. A project may turn network access off
    /// but not on, and cannot add writable paths. Likewise it may turn
    /// `denyHomeReads` on but not off, and cannot add readable paths.
    fn overlay_project(&mut self, project: SandboxSettings) {
        if project.deny_home_reads == Some(true) {
            self.deny_home_reads = Some(true);
        }
        if project.mode == "local" {
            self.mode = project.mode;
        }
        if project.isolation.is_some() {
            self.isolation = project.isolation;
        }
        if project.runtime.is_some() {
            self.runtime = project.runtime;
        }
        if project.network == Some(false) {
            self.network = Some(false);
        }
    }
}

/// User / project settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Default provider id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Selected model id; `None` until the user picks one with `/model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Default thinking level (`off`, `low`, …).
    #[serde(default = "default_thinking")]
    pub default_thinking_level: String,
    /// Theme name.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Compaction.
    #[serde(default)]
    pub compaction: CompactionSettingsJson,
    /// Models enabled for Ctrl+P cycling: exact `id` or `provider/id`, no wildcards.
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// Extra skill paths / globs.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Skill names hidden from the model and from `/skill:` commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    /// Extra extension paths.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Extra prompt paths.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Extra theme paths.
    #[serde(default)]
    pub themes: Vec<String>,
    /// Enable `/skill:name` commands.
    #[serde(default = "default_true")]
    pub enable_skill_commands: bool,
    /// Quiet startup banner.
    #[serde(default)]
    pub quiet_startup: bool,
    /// Hide thinking blocks in UI.
    #[serde(default)]
    pub hide_thinking_block: bool,
    /// Double-escape action: `tree`, `fork`, `none`.
    #[serde(default = "default_double_escape")]
    pub double_escape_action: String,
    /// UI mode: `normal` or `fullscreen`.
    #[serde(default = "default_ui_mode")]
    pub ui_mode: String,
    /// Sandbox.
    #[serde(default)]
    pub sandbox: SandboxSettings,
    /// `web_fetch` tool (`webFetch` in settings.json).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<loop_agent::harness::WebFetchConfig>,
    /// Web tools switch and `web_search` backend (`webSearch` in
    /// settings.json). Absent means on, with the Soket backend; the search
    /// tool is only registered when the backend's API key resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<loop_agent::harness::WebSearchConfig>,
    /// `bash` time limits (`bash.defaultTimeoutMs`, `bash.maxTimeoutMs`).
    /// Global only: a project file cannot change them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash: Option<BashSettings>,
    /// Permission mode at startup: `read-only`, `ask` (default),
    /// `accept-edits` or `full-access`. A project file may only choose a
    /// stricter mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// When to prompt for `ask`-mode tools: `newSession`, `always`, or `never`.
    #[serde(default = "default_file_edit_review")]
    pub file_edit_review: String,
    /// Diff editor binary (`cursor`, `code`, or absolute path). Auto-detected when empty.
    #[serde(default)]
    pub diff_editor: Option<String>,
    /// Per-tool defaults: `ask` | `allow` | `deny` (stored in `~/.loop/agent/settings.json`).
    /// Interactive workflows currently exist for `write`, `edit`, and `bash`.
    #[serde(default = "default_tool_permissions_settings")]
    pub tool_permissions: BTreeMap<String, String>,
    /// Default project trust: `ask`, `always`, `never` (global only).
    #[serde(default = "default_trust")]
    pub default_project_trust: String,
    /// External MCP servers keyed by name.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Configured MCP server names that are not connected.
    ///
    /// Saved in the global `settings.json` as `disabledMcpServers`. A project
    /// settings file can add more names; they are combined with the global list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcp_servers: Vec<String>,
    /// Max wait for the model HTTP response headers (time-to-first-byte), in ms.
    ///
    /// Default `120000`. Set to `0` to wait indefinitely (useful for long-running
    /// workflows / slow models). Esc still aborts the turn.
    #[serde(default = "default_response_header_timeout_ms")]
    pub response_header_timeout_ms: u64,
    /// Native streaming deadline overrides.
    #[serde(default)]
    pub timeouts: loop_ai::TimeoutPolicy,
    /// Provider and model streaming deadline overrides.
    #[serde(default)]
    pub timeout_overrides: loop_ai::TimeoutOverrides,
    /// Langfuse tracing (global only; see [`TracingSettings`]).
    #[serde(default)]
    pub tracing: TracingSettings,
    /// Custom OpenAI-compatible providers added with `/login` (global only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<CustomProviderEntry>,
    /// External ACP agents (global only: a project file must not add
    /// commands Loop runs). Entries replace built-in presets with the same id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<crate::agents::AgentEntry>,
    /// Limits of `loop runtime start` (global only: one Runtime serves many
    /// folders). A command-line flag overrides each.
    #[serde(default, skip_serializing_if = "RuntimeSettings::is_empty")]
    pub runtime: RuntimeSettings,
    /// Top-level keys the file this value was loaded from names. `None` for a
    /// value built in code or parsed from a string: `merge_project` then
    /// treats a key as set when it differs from its default.
    /// Public only so `..Settings::default()` keeps working in other crates.
    #[serde(skip)]
    #[doc(hidden)]
    pub explicit_keys: Option<BTreeSet<String>>,
}

/// `runtime` settings. Values are kept as written and checked when the
/// Runtime starts, so a bad one is reported by its key instead of making
/// the whole file unreadable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettings {
    /// Additional environment variable names forwarded to WebSocket Sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_env: Vec<String>,
    /// Idle minutes for auto-started owners; zero disables shutdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_shutdown_minutes: Option<u64>,
    /// Most Sessions loaded at once (default 16).
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_sessions: Option<serde_json::Value>,
    /// Most turns running at once (default 8).
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_active_turns: Option<serde_json::Value>,
}

/// Keep an explicit `null`, so it can be refused rather than ignored.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}

impl RuntimeSettings {
    fn is_empty(&self) -> bool {
        self.max_sessions.is_none()
            && self.max_active_turns.is_none()
            && self.idle_shutdown_minutes.is_none()
            && self.forward_env.is_empty()
    }
    /// `runtime.maxSessions`, when set.
    pub fn max_sessions(&self) -> anyhow::Result<Option<usize>> {
        limit("maxSessions", self.max_sessions.as_ref())
    }
    /// `runtime.maxActiveTurns`, when set.
    pub fn max_active_turns(&self) -> anyhow::Result<Option<usize>> {
        limit("maxActiveTurns", self.max_active_turns.as_ref())
    }
}

fn limit(key: &str, value: Option<&serde_json::Value>) -> anyhow::Result<Option<usize>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.as_u64().and_then(|n| usize::try_from(n).ok()) {
        Some(n) if n >= 1 => Ok(Some(n)),
        _ => anyhow::bail!(
            "settings.json: `runtime.{key}` must be a whole number of at least 1, not {value}"
        ),
    }
}

fn default_thinking() -> String {
    "off".into()
}
fn default_theme() -> String {
    "dark".into()
}
fn default_double_escape() -> String {
    "tree".into()
}
fn default_ui_mode() -> String {
    "normal".into()
}
fn default_trust() -> String {
    "ask".into()
}
fn default_file_edit_review() -> String {
    "newSession".into()
}
fn default_tool_permissions_settings() -> BTreeMap<String, String> {
    crate::tool_approval::default_tool_permissions()
}
fn default_response_header_timeout_ms() -> u64 {
    120_000
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_provider: None,
            default_model: None,
            default_thinking_level: default_thinking(),
            theme: default_theme(),
            compaction: CompactionSettingsJson::default(),
            enabled_models: Vec::new(),
            skills: Vec::new(),
            disabled_skills: Vec::new(),
            extensions: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
            enable_skill_commands: true,
            quiet_startup: false,
            hide_thinking_block: false,
            double_escape_action: default_double_escape(),
            ui_mode: default_ui_mode(),
            sandbox: SandboxSettings::default(),
            web_fetch: None,
            web_search: None,
            bash: None,
            permission_mode: None,
            file_edit_review: default_file_edit_review(),
            diff_editor: None,
            tool_permissions: default_tool_permissions_settings(),
            default_project_trust: default_trust(),
            mcp_servers: BTreeMap::new(),
            disabled_mcp_servers: Vec::new(),
            response_header_timeout_ms: default_response_header_timeout_ms(),
            timeouts: Default::default(),
            timeout_overrides: Default::default(),
            tracing: TracingSettings::default(),
            providers: Vec::new(),
            agents: Vec::new(),
            runtime: RuntimeSettings::default(),
            explicit_keys: None,
        }
    }
}

/// `bash` settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashSettings {
    /// Timeout when a call names none, in ms (default 120000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
    /// The most a call may ask for, in ms (default 600000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout_ms: Option<u64>,
}

impl BashSettings {
    /// The limits these settings ask for, over the built-in defaults. The
    /// maximum is never below the default.
    pub fn limits(&self) -> loop_agent::harness::BashLimits {
        let base = loop_agent::harness::BashLimits::default();
        let default_timeout_ms = self.default_timeout_ms.unwrap_or(base.default_timeout_ms);
        loop_agent::harness::BashLimits {
            default_timeout_ms,
            max_timeout_ms: self
                .max_timeout_ms
                .unwrap_or(base.max_timeout_ms)
                .max(default_timeout_ms),
        }
    }
}

impl Settings {
    /// The saved `(provider, model id)`, when both are set.
    pub fn selected_model(&self) -> Option<(&str, &str)> {
        Some((
            self.default_provider.as_deref()?,
            self.default_model.as_deref()?,
        ))
    }

    /// `provider/model id` of the saved selection.
    pub fn selected_model_spec(&self) -> Option<String> {
        self.selected_model().map(|(p, m)| format!("{p}/{m}"))
    }

    /// Remember a model selection.
    pub fn set_selected_model(&mut self, provider: &str, model: &str) {
        self.default_provider = Some(provider.into());
        self.default_model = Some(model.into());
    }

    /// Forget the model selection.
    pub fn clear_selected_model(&mut self) {
        self.default_provider = None;
        self.default_model = None;
    }

    /// Load from a JSON file, or defaults if missing.
    pub fn load_file(path: &Path) -> anyhow::Result<Self> {
        let none_set = || Self {
            explicit_keys: Some(BTreeSet::new()),
            ..Self::default()
        };
        if !path.exists() {
            return Ok(none_set());
        }
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(none_set());
        }
        let mut settings: Self = serde_json::from_str(&raw)?;
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        settings.explicit_keys = Some(
            value
                .as_object()
                .map(|object| object.keys().cloned().collect())
                .unwrap_or_default(),
        );
        Ok(settings)
    }

    /// Whether this value, read as a project layer, sets `key` (its camelCase
    /// JSON name). A key the file omits is filled in by serde with its
    /// default and must not override the global value.
    fn sets(&self, key: &str) -> bool {
        if let Some(keys) = &self.explicit_keys {
            return keys.contains(key);
        }
        let field = |settings: &Self| {
            serde_json::to_value(settings)
                .ok()
                .and_then(|value| value.get(key).cloned())
        };
        field(self) != field(&Self::default())
    }

    /// Save to disk.
    pub fn save_file(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Deep-merge project settings over global (arrays replace).
    pub fn merge_project(&mut self, project: Settings) {
        // Only keys the project file names override global values (see
        // `sets`); the security rules in `project_overlay` still apply.
        *self = project_overlay(self.clone(), project);
    }
}

/// Keys whose value `project_overlay` copies only when the project file names
/// them (every one needs a `sets(..)` check there).
const PROJECT_OVERRIDABLE_KEYS: [&str; 9] = [
    "defaultThinkingLevel",
    "theme",
    "compaction",
    "enableSkillCommands",
    "quietStartup",
    "hideThinkingBlock",
    "doubleEscapeAction",
    "uiMode",
    "responseHeaderTimeoutMs",
];

fn project_overlay(mut base: Settings, project: Settings) -> Settings {
    let set_keys: Vec<&str> = PROJECT_OVERRIDABLE_KEYS
        .into_iter()
        .filter(|key| project.sets(key))
        .collect();
    let sets = |key: &str| set_keys.contains(&key);
    // Prefer project values when the project settings file was loaded (non-default merge).
    if let (Some(provider), Some(model)) = (project.default_provider, project.default_model) {
        base.default_provider = Some(provider);
        base.default_model = Some(model);
    }
    // Keys a project file omits come back from serde as defaults; only the
    // ones it names override the global value.
    if sets("defaultThinkingLevel") {
        base.default_thinking_level = project.default_thinking_level.clone();
    }
    if sets("theme") {
        base.theme = project.theme.clone();
    }
    if sets("compaction") {
        base.compaction = project.compaction.clone();
    }
    if !project.enabled_models.is_empty() {
        base.enabled_models = project.enabled_models;
    }
    if !project.skills.is_empty() {
        base.skills = project.skills;
    }
    for name in project.disabled_skills {
        if !base.disabled_skills.contains(&name) {
            base.disabled_skills.push(name);
        }
    }
    if !project.extensions.is_empty() {
        base.extensions = project.extensions;
    }
    if !project.prompts.is_empty() {
        base.prompts = project.prompts;
    }
    if !project.themes.is_empty() {
        base.themes = project.themes;
    }
    if sets("enableSkillCommands") {
        base.enable_skill_commands = project.enable_skill_commands;
    }
    if sets("quietStartup") {
        base.quiet_startup = project.quiet_startup;
    }
    if sets("hideThinkingBlock") {
        base.hide_thinking_block = project.hide_thinking_block;
    }
    if sets("doubleEscapeAction") {
        base.double_escape_action = project.double_escape_action.clone();
    }
    if sets("uiMode") {
        base.ui_mode = project.ui_mode.clone();
    }
    base.sandbox.overlay_project(project.sandbox);
    if let Some(project_fetch) = project.web_fetch {
        let global = base.web_fetch.unwrap_or_default();
        base.web_fetch = Some(loop_agent::harness::WebFetchConfig {
            allow_private_addresses: global.allow_private_addresses,
            provider: project_fetch.provider,
        });
    }
    if let Some(project_search) = project.web_search {
        // A project may pick the backend or turn the web tools off, but not
        // change where keys are sent or which env vars are read: `baseUrl`,
        // `apiKeyEnv` and `allowPrivateAddresses` stay global-only.
        let global = base.web_search.take().unwrap_or_default();
        base.web_search = Some(loop_agent::harness::WebSearchConfig {
            enabled: global.enabled && project_search.enabled,
            provider: project_search.provider.or(global.provider),
            ..global
        });
    }
    // A project may tighten the permission mode, never loosen it.
    if let Some(project_mode) = project
        .permission_mode
        .as_deref()
        .and_then(crate::permissions::PermissionMode::parse)
    {
        let global = base
            .permission_mode
            .as_deref()
            .and_then(crate::permissions::PermissionMode::parse)
            .unwrap_or_default();
        if project_mode < global {
            base.permission_mode = Some(project_mode.as_str().to_owned());
        }
    }
    // A project may make edit review stricter (`always`), never looser. A
    // project file without the key parses as the default, which changes
    // nothing.
    if project.file_edit_review.eq_ignore_ascii_case("always") {
        base.file_edit_review = project.file_edit_review;
    }
    if project.diff_editor.is_some() {
        base.diff_editor = project.diff_editor;
    }
    // A project may add `ask` and `deny` rules, never `allow`, and never
    // loosen a global rule. A project file without the key parses as exactly
    // the defaults, which changes nothing.
    let defaults = crate::tool_approval::default_tool_permissions();
    let project_rules = if project.tool_permissions == defaults {
        BTreeMap::new()
    } else {
        project.tool_permissions
    };
    for (key, value) in project_rules {
        let strictness = |v: &str| crate::permissions::ToolPermission::parse(v).strictness();
        let global = base
            .tool_permissions
            .get(&key)
            .or_else(|| defaults.get(&key))
            .map_or(0, |v| strictness(v));
        if strictness(&value) > global {
            base.tool_permissions.insert(key, value);
        } else if strictness(&value) < global {
            tracing::warn!(
                "project settings cannot loosen toolPermissions `{key}` to `{value}`; ignored"
            );
        }
    }
    if !project.mcp_servers.is_empty() {
        for (k, v) in project.mcp_servers {
            base.mcp_servers.insert(k, v);
        }
    }
    for name in project.disabled_mcp_servers {
        if !base.disabled_mcp_servers.contains(&name) {
            base.disabled_mcp_servers.push(name);
        }
    }
    base.timeouts.overlay(&project.timeouts);
    for (key, value) in project.timeout_overrides.providers {
        base.timeout_overrides
            .providers
            .entry(key)
            .or_default()
            .overlay(&value);
    }
    for (key, value) in project.timeout_overrides.models {
        base.timeout_overrides
            .models
            .entry(key)
            .or_default()
            .overlay(&value);
    }
    if sets("responseHeaderTimeoutMs") {
        base.response_header_timeout_ms = project.response_header_timeout_ms;
    }
    base
}

/// Load global settings then overlay trusted project settings.
pub fn load_settings(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> anyhow::Result<Settings> {
    let mut settings = Settings::load_file(&settings_path(agent_dir))?;
    crate::tool_approval::merge_default_tool_permissions(&mut settings.tool_permissions);
    if project_trusted {
        let project_settings = get_project_dir(cwd).join("settings.json");
        if project_settings.exists() {
            settings.merge_project(Settings::load_file(&project_settings)?);
        }
    }
    // The host's own choices for this workspace win over project files.
    crate::config::workspace_settings::WorkspaceSettingsStore::load(agent_dir)?
        .get(cwd)
        .apply(&mut settings);
    Ok(settings)
}

/// Add (`enabled = false`) or remove (`enabled = true`) `name` in a disabled-name list.
pub fn set_disabled_name(list: &mut Vec<String>, name: &str, enabled: bool) {
    if enabled {
        list.retain(|n| n != name);
    } else if !list.iter().any(|n| n == name) {
        list.push(name.to_string());
    }
}

/// Add (`enabled = false`) or remove (`enabled = true`) `name` in a disabled-skills list.
pub fn set_skill_disabled(list: &mut Vec<String>, name: &str, enabled: bool) {
    set_disabled_name(list, name, enabled);
}

impl Settings {
    /// Public effective values for workspace/settings. Credentials and telemetry destinations are excluded.
    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({ "defaultThinkingLevel": self.default_thinking_level, "sandbox": self.sandbox, "permissionMode": self.permission_mode, "toolPermissions": self.tool_permissions, "diffEditor": self.diff_editor, "responseHeaderTimeoutMs": self.response_header_timeout_ms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_settings_old_files_still_parse() {
        let settings: Settings = serde_json::from_str(
            r#"{"sandbox":{"mode":"local","isolation":"partial","runtime":"krun"}}"#,
        )
        .unwrap();
        assert_eq!(settings.sandbox.mode, "local");
        assert_eq!(settings.sandbox.isolation.as_deref(), Some("partial"));
        assert_eq!(settings.sandbox.runtime.as_deref(), Some("krun"));
        assert!(!settings.sandbox.network());
        assert!(settings.sandbox.allow_write.is_empty());
    }

    #[test]
    fn sandbox_settings_missing_keys_are_inherited() {
        let settings: Settings = serde_json::from_str(r#"{"sandbox":{"mode":"local"}}"#).unwrap();
        assert_eq!(settings.sandbox.isolation, None);
        assert_eq!(settings.sandbox.runtime, None);
        assert_eq!(settings.sandbox.network, None);
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.sandbox.mode, "off");
        assert_eq!(settings.sandbox.isolation, None);
    }

    #[test]
    fn sandbox_settings_round_trip_network_and_allow_write() {
        let settings: Settings = serde_json::from_str(
            r#"{"sandbox":{"mode":"local","network":true,"allowWrite":["~/.cargo","/opt/cache"]}}"#,
        )
        .unwrap();
        assert!(settings.sandbox.network());
        assert_eq!(settings.sandbox.allow_write, ["~/.cargo", "/opt/cache"]);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["sandbox"]["network"], true);
        assert_eq!(
            json["sandbox"]["allowWrite"],
            serde_json::json!(["~/.cargo", "/opt/cache"])
        );
        let again: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(again.sandbox.allow_write, settings.sandbox.allow_write);
        assert_eq!(again.sandbox.network, Some(true));
    }

    #[test]
    fn saving_default_sandbox_settings_writes_no_inherited_values() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        Settings::default().save_file(&path).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["sandbox"], serde_json::json!({"mode": "off"}));
    }

    #[test]
    fn project_may_tighten_home_reads_but_never_loosen_them() {
        let mut global: Settings =
            serde_json::from_str(r#"{"sandbox":{"denyHomeReads":true,"allowRead":["~/.npm"]}}"#)
                .unwrap();
        global.merge_project(
            serde_json::from_str(r#"{"sandbox":{"denyHomeReads":false,"allowRead":["~/.ssh"]}}"#)
                .unwrap(),
        );
        assert!(global.sandbox.deny_home_reads());
        assert_eq!(global.sandbox.allow_read, ["~/.npm"]);
        let mut global = Settings::default();
        assert!(!global.sandbox.deny_home_reads());
        global
            .merge_project(serde_json::from_str(r#"{"sandbox":{"denyHomeReads":true}}"#).unwrap());
        assert!(global.sandbox.deny_home_reads());
    }

    #[test]
    fn project_cannot_disable_a_required_sandbox() {
        for project in [
            r#"{}"#,
            r#"{"sandbox":{"mode":"off"}}"#,
            r#"{"sandbox":{"mode":"unknown"}}"#,
        ] {
            let mut global: Settings =
                serde_json::from_str(r#"{"sandbox":{"mode":"local"}}"#).unwrap();
            global.merge_project(serde_json::from_str(project).unwrap());
            assert_eq!(global.sandbox.mode, "local");
        }
        let mut global = Settings::default();
        global.merge_project(serde_json::from_str(r#"{"sandbox":{"mode":"local"}}"#).unwrap());
        assert_eq!(global.sandbox.mode, "local");
    }

    #[test]
    fn project_sandbox_settings_overlay_field_by_field() {
        let mut global: Settings = serde_json::from_str(
            r#"{"sandbox":{"mode":"local","runtime":"krun","network":true,"allowWrite":["~/.cargo"]}}"#,
        )
        .unwrap();
        let project: Settings = serde_json::from_str(
            r#"{"sandbox":{"mode":"local","isolation":"partial","allowWrite":["/"]}}"#,
        )
        .unwrap();
        global.merge_project(project);
        assert_eq!(global.sandbox.isolation.as_deref(), Some("partial"));
        assert_eq!(global.sandbox.runtime.as_deref(), Some("krun"));
        assert!(global.sandbox.network());
        assert_eq!(global.sandbox.allow_write, ["~/.cargo"]);

        let project: Settings =
            serde_json::from_str(r#"{"sandbox":{"mode":"local","network":false}}"#).unwrap();
        global.merge_project(project);
        assert!(!global.sandbox.network());

        let mut global = Settings::default();
        let project: Settings =
            serde_json::from_str(r#"{"sandbox":{"mode":"local","network":true}}"#).unwrap();
        global.merge_project(project);
        assert!(!global.sandbox.network());
    }

    fn runtime_limits(json: &str) -> anyhow::Result<(Option<usize>, Option<usize>)> {
        let settings: Settings = serde_json::from_str(json)?;
        Ok((
            settings.runtime.max_sessions()?,
            settings.runtime.max_active_turns()?,
        ))
    }

    #[test]
    fn runtime_limits_are_read_from_settings() {
        assert_eq!(runtime_limits("{}").unwrap(), (None, None));
        assert_eq!(
            runtime_limits(r#"{"runtime":{"maxSessions":3,"maxActiveTurns":2}}"#).unwrap(),
            (Some(3), Some(2))
        );
        assert_eq!(
            runtime_limits(r#"{"runtime":{"maxActiveTurns":1}}"#).unwrap(),
            (None, Some(1))
        );
    }

    #[test]
    fn invalid_runtime_limits_name_their_key() {
        for (json, key) in [
            (r#"{"runtime":{"maxSessions":0}}"#, "runtime.maxSessions"),
            (r#"{"runtime":{"maxSessions":-2}}"#, "runtime.maxSessions"),
            (r#"{"runtime":{"maxSessions":null}}"#, "runtime.maxSessions"),
            (r#"{"runtime":{"maxSessions":2.5}}"#, "runtime.maxSessions"),
            (
                r#"{"runtime":{"maxActiveTurns":"8"}}"#,
                "runtime.maxActiveTurns",
            ),
            (
                r#"{"runtime":{"maxActiveTurns":0}}"#,
                "runtime.maxActiveTurns",
            ),
        ] {
            let error = runtime_limits(json).unwrap_err().to_string();
            assert!(
                error.contains(key) && error.contains("at least 1"),
                "{json}: {error}"
            );
        }
    }

    #[test]
    fn runtime_limits_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"runtime":{"maxSessions":4,"maxActiveTurns":null}}"#,
        )
        .unwrap();
        Settings::load_file(&path)
            .unwrap()
            .save_file(&path)
            .unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["runtime"],
            serde_json::json!({"maxSessions":4,"maxActiveTurns":null})
        );
        // Absent stays absent.
        Settings::default().save_file(&path).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.get("runtime").is_none(), "{saved}");
    }

    #[test]
    fn disabled_mcp_servers_round_trip_and_merge() {
        let mut settings = Settings::default();
        assert!(!serde_json::to_string(&settings)
            .unwrap()
            .contains("disabledMcpServers"));
        set_disabled_name(&mut settings.disabled_mcp_servers, "filesystem", false);
        set_disabled_name(&mut settings.disabled_mcp_servers, "filesystem", false);
        assert_eq!(settings.disabled_mcp_servers, ["filesystem"]);
        let raw = serde_json::to_string(&settings).unwrap();
        assert!(raw.contains("\"disabledMcpServers\":[\"filesystem\"]"));
        set_disabled_name(&mut settings.disabled_mcp_servers, "filesystem", true);
        assert!(settings.disabled_mcp_servers.is_empty());

        let mut global = Settings {
            disabled_mcp_servers: vec!["filesystem".into()],
            ..Default::default()
        };
        let project = Settings {
            disabled_mcp_servers: vec!["filesystem".into(), "remote".into()],
            ..Default::default()
        };
        global.merge_project(project);
        assert_eq!(global.disabled_mcp_servers, ["filesystem", "remote"]);
    }

    #[test]
    fn a_project_can_only_tighten_tool_permissions() {
        let mut global = Settings::default();
        global
            .tool_permissions
            .insert("read".into(), "allow".into());
        let project: Settings = serde_json::from_str(
            r#"{"toolPermissions": {"bash": "allow", "write": "deny",
                "bash(rm *)": "deny", "bash(git *)": "allow", "read": "ask"}}"#,
        )
        .unwrap();
        global.merge_project(project);
        let p = &global.tool_permissions;
        assert_eq!(p["bash"], "ask", "cannot loosen bash");
        assert_eq!(p["write"], "deny");
        assert_eq!(p["bash(rm *)"], "deny");
        assert!(!p.contains_key("bash(git *)"), "no new allow rules");
        assert_eq!(p["read"], "ask");

        let mut global = Settings::default();
        global
            .tool_permissions
            .insert("bash".into(), "allow".into());
        global.merge_project(Settings::default());
        assert_eq!(
            global.tool_permissions["bash"], "allow",
            "a project file without toolPermissions keeps the global rules"
        );
    }

    #[test]
    fn a_project_can_only_make_edit_review_stricter() {
        let mut global = Settings {
            file_edit_review: "always".into(),
            ..Default::default()
        };
        global.merge_project(Settings::default());
        assert_eq!(global.file_edit_review, "always", "absent key keeps global");
        let never: Settings = serde_json::from_str(r#"{"fileEditReview": "never"}"#).unwrap();
        global.merge_project(never);
        assert_eq!(global.file_edit_review, "always");

        let mut global = Settings {
            file_edit_review: "never".into(),
            ..Default::default()
        };
        let always: Settings = serde_json::from_str(r#"{"fileEditReview": "always"}"#).unwrap();
        global.merge_project(always);
        assert_eq!(global.file_edit_review, "always");
    }

    #[test]
    fn bash_limits_are_global_only() {
        let mut global: Settings =
            serde_json::from_str(r#"{"bash": {"defaultTimeoutMs": 300000}}"#).unwrap();
        let limits = global.bash.unwrap().limits();
        assert_eq!(limits.default_timeout_ms, 300_000);
        assert_eq!(limits.max_timeout_ms, 600_000);
        let project: Settings =
            serde_json::from_str(r#"{"bash": {"maxTimeoutMs": 99999999}}"#).unwrap();
        global.merge_project(project);
        assert_eq!(global.bash.unwrap().limits().max_timeout_ms, 600_000);

        let low_max = BashSettings {
            default_timeout_ms: Some(900_000),
            max_timeout_ms: Some(1_000),
        };
        assert_eq!(low_max.limits().max_timeout_ms, 900_000);
    }

    #[test]
    fn a_project_can_only_tighten_the_permission_mode() {
        let with_mode = |mode: &str| Settings {
            permission_mode: Some(mode.into()),
            ..Default::default()
        };
        let mut global = with_mode("accept-edits");
        global.merge_project(with_mode("full-access"));
        assert_eq!(global.permission_mode.as_deref(), Some("accept-edits"));
        global.merge_project(with_mode("read-only"));
        assert_eq!(global.permission_mode.as_deref(), Some("read-only"));

        let mut default = Settings::default();
        default.merge_project(with_mode("accept-edits"));
        assert_eq!(default.permission_mode, None, "looser than the default ask");
        default.merge_project(Settings::default());
        assert_eq!(default.permission_mode, None);
    }

    #[test]
    fn project_web_search_cannot_redirect_keys() {
        use loop_agent::harness::WebSearchConfig;
        let mut global = Settings {
            web_search: Some(WebSearchConfig {
                provider: Some("tavily".into()),
                api_key_env: vec!["MY_TAVILY_KEY".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let project: Settings = serde_json::from_str(
            r#"{"webSearch": {"provider": "brave", "baseUrl": "https://attacker.example",
                "apiKeyEnv": ["AWS_SECRET_ACCESS_KEY"], "allowPrivateAddresses": true}}"#,
        )
        .unwrap();
        global.merge_project(project);
        let merged = global.web_search.clone().unwrap();
        assert_eq!(merged.provider.as_deref(), Some("brave"));
        assert_eq!(merged.base_url, None);
        assert_eq!(merged.api_key_env, ["MY_TAVILY_KEY"]);
        assert!(!merged.allow_private_addresses);
        assert!(merged.enabled);

        // A project may turn the web tools off, but not back on.
        let off: Settings = serde_json::from_str(r#"{"webSearch": {"enabled": false}}"#).unwrap();
        global.merge_project(off);
        let merged = global.web_search.clone().unwrap();
        assert!(!merged.enabled);
        assert_eq!(merged.provider.as_deref(), Some("brave"), "provider kept");
        let on: Settings = serde_json::from_str(r#"{"webSearch": {"enabled": true}}"#).unwrap();
        global.merge_project(on);
        assert!(!global.web_search.unwrap().enabled);
    }

    #[test]
    fn project_web_fetch_cannot_enable_private_addresses_or_reset_global() {
        use loop_agent::harness::{WebFetchConfig, WebFetchProvider};

        let mut global = Settings {
            web_fetch: Some(WebFetchConfig {
                allow_private_addresses: false,
                provider: WebFetchProvider::Soket,
            }),
            ..Default::default()
        };
        let project: Settings = serde_json::from_str(
            r#"{"webFetch":{"allowPrivateAddresses":true,"provider":"direct"}}"#,
        )
        .unwrap();
        global.merge_project(project);
        let merged = global.web_fetch.unwrap();
        assert!(!merged.allow_private_addresses);
        assert_eq!(merged.provider, WebFetchProvider::Direct);

        let mut global = Settings {
            web_fetch: Some(WebFetchConfig {
                allow_private_addresses: false,
                provider: WebFetchProvider::Soket,
            }),
            ..Default::default()
        };
        global.merge_project(Settings::default());
        assert_eq!(
            global.web_fetch,
            Some(WebFetchConfig {
                allow_private_addresses: false,
                provider: WebFetchProvider::Soket,
            })
        );
    }

    #[test]
    fn saved_partial_permissions_gain_new_defaults() {
        let agent_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = settings_path(agent_dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"toolPermissions":{"bash":"allow"}}"#).unwrap();

        let loaded = load_settings(agent_dir.path(), cwd.path(), false).unwrap();
        assert_eq!(
            loaded.tool_permissions.get("bash").map(String::as_str),
            Some("allow")
        );
        assert_eq!(
            loaded.tool_permissions.get("web_fetch").map(String::as_str),
            Some("allow")
        );
        assert_eq!(
            loaded
                .tool_permissions
                .get("web_search")
                .map(String::as_str),
            Some("allow")
        );
    }

    /// Keys a project file may set freely: global value, project value, and
    /// the key's default (the project value equals it in the last case).
    fn plain_overlay_cases() -> Vec<(&'static str, serde_json::Value, serde_json::Value)> {
        use serde_json::json;
        vec![
            ("defaultThinkingLevel", json!("high"), json!("low")),
            ("theme", json!("light"), json!("solarized")),
            (
                "compaction",
                json!({"enabled": false, "reserveTokens": 1000, "keepRecentTokens": 2000, "triggerPercent": 80}),
                json!({"enabled": true, "reserveTokens": 5000, "keepRecentTokens": 6000, "triggerPercent": 85}),
            ),
            ("enableSkillCommands", json!(false), json!(true)),
            ("quietStartup", json!(true), json!(false)),
            ("hideThinkingBlock", json!(true), json!(false)),
            ("doubleEscapeAction", json!("fork"), json!("none")),
            ("uiMode", json!("fullscreen"), json!("normal")),
            ("responseHeaderTimeoutMs", json!(30_000), json!(45_000)),
        ]
    }

    fn overlay_from_files(global: &str, project: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let global_path = dir.path().join("global.json");
        let project_path = dir.path().join("project.json");
        std::fs::write(&global_path, global).unwrap();
        std::fs::write(&project_path, project).unwrap();
        let mut settings = Settings::load_file(&global_path).unwrap();
        settings.merge_project(Settings::load_file(&project_path).unwrap());
        serde_json::to_value(&settings).unwrap()
    }

    #[test]
    fn project_file_without_a_key_keeps_the_global_value() {
        for (key, global_value, _) in plain_overlay_cases() {
            let global = serde_json::json!({ key: global_value }).to_string();
            let merged = overlay_from_files(&global, r#"{"mcpServers":{}}"#);
            assert_eq!(merged[key], global_value, "{key} reset by a project file");
        }
        // An empty project file changes nothing either.
        let all: serde_json::Map<_, _> = plain_overlay_cases()
            .into_iter()
            .map(|(key, value, _)| (key.to_owned(), value))
            .collect();
        let global = serde_json::Value::Object(all.clone()).to_string();
        let merged = overlay_from_files(&global, "");
        for (key, value) in all {
            assert_eq!(merged[&key], value, "{key} reset by an empty project file");
        }
    }

    #[test]
    fn project_file_sets_only_the_keys_it_names() {
        for (key, global_value, project_value) in plain_overlay_cases() {
            let global = serde_json::json!({ key: global_value }).to_string();
            let project = serde_json::json!({ key: project_value }).to_string();
            let merged = overlay_from_files(&global, &project);
            assert_eq!(merged[key], project_value, "{key} not taken from project");
        }
    }

    #[test]
    fn project_may_set_a_key_to_its_default_value() {
        let merged = overlay_from_files(
            r#"{"theme":"light","uiMode":"fullscreen","responseHeaderTimeoutMs":30000}"#,
            r#"{"theme":"dark","uiMode":"normal","responseHeaderTimeoutMs":120000}"#,
        );
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["uiMode"], "normal");
        assert_eq!(merged["responseHeaderTimeoutMs"], 120_000);
    }

    #[test]
    fn load_settings_keeps_global_values_a_project_does_not_set() {
        let agent_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(agent_dir.path()),
            r#"{"theme":"light","defaultThinkingLevel":"high","uiMode":"fullscreen","responseHeaderTimeoutMs":30000}"#,
        )
        .unwrap();
        std::fs::create_dir_all(get_project_dir(cwd.path())).unwrap();
        std::fs::write(
            get_project_dir(cwd.path()).join("settings.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let loaded = load_settings(agent_dir.path(), cwd.path(), true).unwrap();
        assert_eq!(loaded.theme, "light");
        assert_eq!(loaded.default_thinking_level, "high");
        assert_eq!(loaded.ui_mode, "fullscreen");
        assert_eq!(loaded.response_header_timeout_ms, 30_000);
        assert!(loaded.mcp_servers.contains_key("fs"));
    }

    #[test]
    fn programmatic_project_settings_override_only_non_default_values() {
        let mut global: Settings =
            serde_json::from_str(r#"{"theme":"light","uiMode":"fullscreen"}"#).unwrap();
        let project: Settings = serde_json::from_str(r#"{"theme":"solarized"}"#).unwrap();
        global.merge_project(project);
        assert_eq!(global.theme, "solarized");
        assert_eq!(global.ui_mode, "fullscreen");
    }

    #[test]
    fn enabled_mcp_entries_skip_disabled_names() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "filesystem".into(),
            McpServerConfig {
                command: Some("npx".into()),
                args: vec!["server".into()],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
            },
        );
        servers.insert(
            "remote".into(),
            McpServerConfig {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some("http://127.0.0.1:3100/mcp".into()),
                headers: BTreeMap::new(),
            },
        );
        let entries = crate::runtime::enabled_mcp_server_entries(&servers, &["remote".into()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "filesystem");
    }
}
