//! Path constants for Loop config discovery (`~/.loop/agent`, project `.loop/`).

use std::path::{Path, PathBuf};

/// Config directory name under home and project roots.
pub const CONFIG_DIR_NAME: &str = ".loop";
/// Env override for the agent config root.
pub const ENV_AGENT_DIR: &str = "LOOP_CODING_AGENT_DIR";
/// Env override for session storage directory / db parent.
pub const ENV_SESSION_DIR: &str = "LOOP_CODING_AGENT_SESSION_DIR";

/// Resolve the agent config directory: `$LOOP_CODING_AGENT_DIR` or `~/.loop/agent`.
pub fn get_agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_AGENT_DIR) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR_NAME)
        .join("agent")
}

/// Project-local `.loop` directory for `cwd`.
pub fn get_project_dir(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_DIR_NAME)
}

/// Ensure standard agent subdirectories exist.
pub fn ensure_agent_dirs(agent_dir: &Path) -> std::io::Result<()> {
    for sub in [
        "",
        "extensions",
        "hooks",
        "skills",
        "prompts",
        "themes",
        "bin",
    ] {
        let p = if sub.is_empty() {
            agent_dir.to_path_buf()
        } else {
            agent_dir.join(sub)
        };
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

/// Settings path.
pub fn settings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("settings.json")
}

/// Auth path.
pub fn auth_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("auth.json")
}

/// Custom models path.
pub fn models_json_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models.json")
}

/// Dynamic catalog cache path.
pub fn models_store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models-store.json")
}

/// Keybindings path.
pub fn keybindings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("keybindings.json")
}

/// Trust store path.
pub fn trust_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("trust.json")
}

/// Default SQLite sessions database.
pub fn sessions_db_path(agent_dir: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_SESSION_DIR) {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("sessions.db");
        }
    }
    agent_dir.join("sessions.db")
}

/// SYSTEM.md path (replace).
pub fn system_md_path(dir: &Path) -> PathBuf {
    dir.join("SYSTEM.md")
}

/// APPEND_SYSTEM.md path.
pub fn append_system_md_path(dir: &Path) -> PathBuf {
    dir.join("APPEND_SYSTEM.md")
}
