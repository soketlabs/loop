//! Global and project settings.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::paths::{get_project_dir, settings_path};
use loop_ai::providers::{SOKET_DEFAULT_MODEL_ID, SOKET_PROVIDER_ID};

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
}

fn default_true() -> bool {
    true
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
        }
    }
}

/// Sandbox settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// `off` or `local-shell`.
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
}

fn default_sandbox_mode() -> String {
    "off".into()
}

/// User / project settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Default provider id.
    #[serde(default = "default_provider")]
    pub default_provider: String,
    /// Default model id.
    #[serde(default = "default_model")]
    pub default_model: String,
    /// Default thinking level (`off`, `low`, …).
    #[serde(default = "default_thinking")]
    pub default_thinking_level: String,
    /// Theme name.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Compaction.
    #[serde(default)]
    pub compaction: CompactionSettingsJson,
    /// Models enabled for Ctrl+P cycling (globs).
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// Extra skill paths / globs.
    #[serde(default)]
    pub skills: Vec<String>,
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
    /// Default project trust: `ask`, `always`, `never` (global only).
    #[serde(default = "default_trust")]
    pub default_project_trust: String,
}

fn default_provider() -> String {
    SOKET_PROVIDER_ID.into()
}
fn default_model() -> String {
    SOKET_DEFAULT_MODEL_ID.into()
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            default_model: default_model(),
            default_thinking_level: default_thinking(),
            theme: default_theme(),
            compaction: CompactionSettingsJson::default(),
            enabled_models: Vec::new(),
            skills: Vec::new(),
            extensions: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
            enable_skill_commands: true,
            quiet_startup: false,
            hide_thinking_block: false,
            double_escape_action: default_double_escape(),
            ui_mode: default_ui_mode(),
            sandbox: SandboxSettings::default(),
            default_project_trust: default_trust(),
        }
    }
}

impl Settings {
    /// Load from a JSON file, or defaults if missing.
    pub fn load_file(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&raw)?)
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
        // Scalars / objects from project always override when present in file —
        // we treat the deserialized project file as authoritative for set fields
        // by replacing known top-level keys that differ from defaults only if
        // the project file existed. Callers pass fully loaded project Settings.
        *self = project_overlay(self.clone(), project);
    }
}

fn project_overlay(mut base: Settings, project: Settings) -> Settings {
    // Prefer project values when the project settings file was loaded (non-default merge).
    base.default_provider = project.default_provider;
    base.default_model = project.default_model;
    base.default_thinking_level = project.default_thinking_level;
    base.theme = project.theme;
    base.compaction = project.compaction;
    if !project.enabled_models.is_empty() {
        base.enabled_models = project.enabled_models;
    }
    if !project.skills.is_empty() {
        base.skills = project.skills;
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
    base.enable_skill_commands = project.enable_skill_commands;
    base.quiet_startup = project.quiet_startup;
    base.hide_thinking_block = project.hide_thinking_block;
    base.double_escape_action = project.double_escape_action;
    base.ui_mode = project.ui_mode;
    base.sandbox = project.sandbox;
    base
}

/// Load global settings then overlay trusted project settings.
pub fn load_settings(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> anyhow::Result<Settings> {
    let mut settings = Settings::load_file(&settings_path(agent_dir))?;
    if project_trusted {
        let project_settings = get_project_dir(cwd).join("settings.json");
        if project_settings.exists() {
            let project = Settings::load_file(&project_settings)?;
            settings.merge_project(project);
        }
    }
    Ok(settings)
}
