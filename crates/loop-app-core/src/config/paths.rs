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

/// Folder of the anonymous usage log read by `/usage`.
pub fn usage_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("usage")
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

/// TUI input history path.
pub fn history_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("history.json")
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

/// Create/verify the Runtime directory, private to the current user.
pub fn run_dir(agent_dir: &Path) -> std::io::Result<PathBuf> {
    let path = agent_dir.join("run");
    ensure_private_runtime_dir(&path)?;
    Ok(path)
}

/// Default Runtime socket, falling back to a short, user-owned directory.
/// Explicit socket overrides should be passed directly to the listener instead.
pub fn runtime_socket_path(agent_dir: &Path) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let candidate = std::path::absolute(agent_dir)?.join("run/runtime.sock");
        let capacity = std::mem::size_of::<libc::sockaddr_un>()
            - std::mem::offset_of!(libc::sockaddr_un, sun_path);
        if candidate.as_os_str().as_bytes().len() < capacity {
            return Ok(run_dir(agent_dir)?.join("runtime.sock"));
        }
        // SAFETY: geteuid has no arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        let fallback = PathBuf::from(format!("/tmp/loop-{uid}"));
        ensure_private_runtime_dir(&fallback)?;
        // Key on the agent dir (like the lock) so two long-path agent dirs never share a socket.
        let canonical = std::fs::canonicalize(agent_dir).unwrap_or_else(|_| {
            candidate
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default()
        });
        Ok(fallback.join(format!(
            "{:016x}.sock",
            fnv1a(canonical.as_os_str().as_bytes())
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = agent_dir;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Runtime Unix sockets are unsupported on this platform",
        ))
    }
}

/// Stable (toolchain-independent) 64-bit FNV-1a, so every client derives the same socket name.
#[cfg(unix)]
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Open the Runtime log for appending: owner-only (0600), refusing symlinks and foreign files.
/// Creates `logs/` as a private (0700) directory.
pub fn open_runtime_log(agent_dir: &Path) -> std::io::Result<(std::fs::File, PathBuf)> {
    let path = runtime_log_path(agent_dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        ensure_private_runtime_dir(&agent_dir.join("logs"))?;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or preconditions.
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Runtime log must be a regular file owned by the current user",
            ));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok((file, path))
    }
    #[cfg(not(unix))]
    {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;
        Ok((file, path))
    }
}

/// Single-instance lock identity, always tied to the agent directory.
pub fn runtime_lock_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("run/runtime.lock")
}

/// Diagnostic pid file, always tied to the agent directory.
pub fn runtime_pid_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("run/runtime.pid")
}

/// Daemon log path.
pub fn runtime_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("logs/runtime.log")
}

/// Create a private directory, rejecting symlinks and foreign ownership.
pub(crate) fn ensure_private_runtime_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        // SAFETY: geteuid has no arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_dir() || metadata.uid() != uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Runtime directory must be a directory owned by the current user: {}",
                    path.display()
                ),
            ));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Runtime directories are unsupported on this platform",
        ))
    }
}
