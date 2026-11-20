//! Running the workflows without the TUI, and installing them into Loop.
//!
//! A headless run is one `loop --print` episode with its own config directory
//! (under `~/.cache/defensive/loop/<id>`): only this crate's MCP server, and
//! Loop's own file, shell and web tools denied, so the agent works only in the
//! sandbox. `auth.json` and the model stores are linked to the user's Loop
//! config, never copied.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::mcp::server_name;
use crate::prompts;
use crate::report::to_json_pretty;
use crate::sources;
use crate::tools::Workflow;

/// Loop's config directory: `$LOOP_CODING_AGENT_DIR` or `~/.loop/agent`.
pub fn loop_agent_dir() -> PathBuf {
    std::env::var_os("LOOP_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".loop").join("agent"))
}

/// The `loop` binary: `$DEFENSIVE_LOOP_BIN`, next to this binary, or on `PATH`.
pub fn loop_bin() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("DEFENSIVE_LOOP_BIN").map(PathBuf::from) {
        return Ok(p);
    }
    if let Some(sibling) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("loop"))) {
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("loop"))
        .find(|p| p.is_file())
        .ok_or_else(|| "loop binary not found; set DEFENSIVE_LOOP_BIN".into())
}

/// The `mcpServers` entry that starts this binary for `workflow`.
pub fn server_entry(exe: &Path, workflow: Workflow) -> Value {
    let arg = match workflow {
        Workflow::Patch => "patch",
        Workflow::Audit => "audit",
    };
    let mut env = Map::new();
    if let Some(runs) = std::env::var_os("DEFENSIVE_RUNS") {
        env.insert("DEFENSIVE_RUNS".into(), runs.to_string_lossy().into_owned().into());
    }
    json!({"command": exe.to_string_lossy(), "args": ["serve", arg], "env": env})
}

/// Settings for a headless run's own config directory.
pub fn headless_settings(base: &Value, exe: &Path, workflow: Workflow, model: Option<&str>, provider: Option<&str>) -> Value {
    let server = server_name(workflow);
    let mut servers = Map::new();
    servers.insert(server.into(), server_entry(exe, workflow));
    let mut perms = Map::new();
    for tool in ["read", "write", "edit", "bash", "grep", "find", "ls", "glob", "web_fetch", "web_search"] {
        perms.insert(tool.into(), "deny".into());
    }
    perms.insert(format!("mcp__{server}__*"), "allow".into());
    let mut s = json!({
        "defaultProvider": provider.map(Value::from).or_else(|| base.get("defaultProvider").cloned()).unwrap_or_else(|| "soket".into()),
        "defaultModel": model.map(Value::from).or_else(|| base.get("defaultModel").cloned()).unwrap_or_else(|| "GLM-5.3".into()),
        "defaultThinkingLevel": base.get("defaultThinkingLevel").cloned().unwrap_or_else(|| "off".into()),
        "defaultProjectTrust": "never",
        "sandbox": {"mode": "off"},
        "mcpServers": servers,
        "toolPermissions": perms,
        "skills": [], "extensions": [], "prompts": [],
        "responseHeaderTimeoutMs": base.get("responseHeaderTimeoutMs").cloned().unwrap_or_else(|| 120_000.into()),
    });
    for key in ["tracing", "providers"] {
        if let Some(v) = base.get(key) {
            s[key] = v.clone();
        }
    }
    s
}

/// Run `workflow` on `target` with `loop --print`; returns loop's exit code.
pub fn run(workflow: Workflow, target: &str, model: Option<&str>, provider: Option<&str>, timeout: Duration) -> Result<i32, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let loop_exe = loop_bin()?;
    let base_dir = loop_agent_dir();
    let base: Value = fs::read_to_string(base_dir.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(json!({}));
    let id = format!("{}-{}", server_name(workflow), chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
    let cfg = sources::cache_dir().join("loop").join(&id);
    let cwd = cfg.join("agent-cwd");
    fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
    fs::write(cfg.join("settings.json"), to_json_pretty(&headless_settings(&base, &exe, workflow, model, provider)))
        .map_err(|e| e.to_string())?;
    for name in ["auth.json", "models-store.json", "models.json"] {
        let (src, dst) = (base_dir.join(name), cfg.join(name));
        if src.exists() && fs::symlink_metadata(&dst).is_err() {
            let _ = std::os::unix::fs::symlink(&src, &dst);
        }
    }
    // A relative folder means the caller's folder, not the empty agent cwd.
    let target = match sources::classify_target(target, &std::env::current_dir().unwrap_or_default()) {
        Ok(sources::Target::Local(p)) => p.to_string_lossy().into_owned(),
        _ => target.trim().to_owned(),
    };
    let prompt = match workflow {
        Workflow::Patch => prompts::patch(&target),
        Workflow::Audit => prompts::audit(&target),
    };
    let mut child = Command::new(&loop_exe)
        .env("LOOP_CODING_AGENT_DIR", &cfg)
        .env("LOOP_CODING_AGENT_SESSION_DIR", cfg.join("sessions"))
        .args(["--cwd", &cwd.to_string_lossy(), "--no-web", "--no-context-files", "--permission-mode", "ask", "--print", &prompt])
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", loop_exe.display()))?;
    let t0 = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(status.code().unwrap_or(1));
        }
        if t0.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// What `install` changed or would change.
#[derive(Debug, Default)]
pub struct Installed {
    /// Prompt templates written.
    pub prompts: Vec<PathBuf>,
    /// Earlier files kept as backups.
    pub backups: Vec<PathBuf>,
    /// The `mcpServers` and `toolPermissions` entries for settings.json.
    pub settings_entries: Value,
    /// Whether settings.json was updated.
    pub settings_written: bool,
}

fn backup(path: &Path, new: &str, backups: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if let Ok(old) = fs::read_to_string(path) {
        if old != new {
            let bak = path.with_extension(format!(
                "{}.bak-{}",
                path.extension().and_then(|e| e.to_str()).unwrap_or(""),
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            ));
            fs::copy(path, &bak)?;
            backups.push(bak);
        }
    }
    Ok(())
}

/// Install `/patch` and `/audit` into Loop's config at `agent_dir`: the
/// prompt templates always, settings.json only with `write_settings`.
pub fn install(agent_dir: &Path, exe: &Path, write_settings: bool) -> std::io::Result<Installed> {
    let mut out = Installed::default();
    let prompts_dir = agent_dir.join("prompts");
    fs::create_dir_all(&prompts_dir)?;
    for (name, body) in [("patch.md", prompts::PATCH_TEMPLATE), ("audit.md", prompts::AUDIT_TEMPLATE)] {
        let p = prompts_dir.join(name);
        backup(&p, body, &mut out.backups)?;
        fs::write(&p, body)?;
        out.prompts.push(p);
    }
    let mut servers = Map::new();
    let mut perms = Map::new();
    for wf in [Workflow::Patch, Workflow::Audit] {
        servers.insert(server_name(wf).into(), server_entry(exe, wf));
        perms.insert(format!("mcp__{}__*", server_name(wf)), "allow".into());
    }
    out.settings_entries = json!({"mcpServers": servers, "toolPermissions": perms});
    if write_settings {
        let path = agent_dir.join("settings.json");
        let mut settings: Value = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(json!({}));
        for key in ["mcpServers", "toolPermissions"] {
            if !settings[key].is_object() {
                settings[key] = json!({});
            }
            for (k, v) in out.settings_entries[key].as_object().into_iter().flatten() {
                settings[key][k] = v.clone();
            }
        }
        let text = serde_json::to_string_pretty(&settings).unwrap_or_default() + "\n";
        backup(&path, &text, &mut out.backups)?;
        fs::write(&path, text)?;
        out.settings_written = true;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_settings_allow_only_the_sandbox() {
        let base = json!({"defaultModel": "GLM-5.3", "providers": {"x": 1}, "mcpServers": {"other": {}}});
        let s = headless_settings(&base, Path::new("/bin/loop-defensive"), Workflow::Patch, None, None);
        assert_eq!(s["mcpServers"].as_object().unwrap().len(), 1);
        assert_eq!(s["mcpServers"]["defensive"]["args"], json!(["serve", "patch"]));
        assert_eq!(s["toolPermissions"]["bash"], "deny");
        assert_eq!(s["toolPermissions"]["mcp__defensive__*"], "allow");
        assert_eq!(s["providers"]["x"], 1);
        assert_eq!(s["defaultModel"], "GLM-5.3");
        let a = headless_settings(&base, Path::new("/x"), Workflow::Audit, Some("m"), Some("p"));
        assert_eq!(a["toolPermissions"]["mcp__defensive-audit__*"], "allow");
        assert_eq!((a["defaultModel"].as_str(), a["defaultProvider"].as_str()), (Some("m"), Some("p")));
    }

    #[test]
    fn install_writes_prompts_and_keeps_backups() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("prompts")).unwrap();
        fs::write(dir.path().join("prompts/patch.md"), "old python prompt").unwrap();
        fs::write(dir.path().join("settings.json"), r#"{"defaultModel": "GLM-5.3", "mcpServers": {"defensive": {"command": "python3"}, "figma": {}}}"#).unwrap();
        let only_prompts = install(dir.path(), Path::new("/bin/loop-defensive"), false).unwrap();
        assert_eq!(only_prompts.prompts.len(), 2);
        assert_eq!(only_prompts.backups.len(), 1);
        assert!(fs::read_to_string(dir.path().join("prompts/patch.md")).unwrap().contains("mcp__defensive__prepare_task"));
        assert!(fs::read_to_string(dir.path().join("settings.json")).unwrap().contains("python3"), "settings untouched");
        let all = install(dir.path(), Path::new("/bin/loop-defensive"), true).unwrap();
        assert!(all.settings_written);
        let s: Value = serde_json::from_str(&fs::read_to_string(dir.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(s["mcpServers"]["defensive"]["command"], "/bin/loop-defensive");
        assert!(s["mcpServers"]["figma"].is_object(), "other servers kept");
        assert_eq!(s["defaultModel"], "GLM-5.3");
        assert_eq!(s["toolPermissions"]["mcp__defensive-audit__*"], "allow");
    }
}
