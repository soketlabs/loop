//! OS single-instance guard for a Runtime, independent of its socket location.

use std::{io, path::Path};

/// Holds the agent directory's exclusive OS file lock until dropped.
/// The lock inode is never unlinked, so all starters contend on the same file.
///
/// The caller must hold this lock before probing, unlinking, or binding the Runtime
/// socket (`loop_wire::socket::unix_listen`); the socket path alone does not serialize starters.
#[derive(Debug)]
pub struct RuntimeInstanceLock {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(unix)]
    pid_path: std::path::PathBuf,
}

impl RuntimeInstanceLock {
    /// Acquire without waiting and publish this process's diagnostic pid.
    pub fn acquire(agent_dir: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use crate::config::paths::{run_dir, runtime_lock_path, runtime_pid_path};
            use std::io::Write;
            use std::os::fd::AsRawFd;

            run_dir(agent_dir)?;
            let file = private_file(&runtime_lock_path(agent_dir))?;
            // SAFETY: the fd is valid for this call; flock does not retain it.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(error);
                }
                let pid = std::fs::read_to_string(runtime_pid_path(agent_dir))
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "unknown".into());
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("a Runtime is already running (pid {pid})"),
                ));
            }
            let pid_path = runtime_pid_path(agent_dir);
            let mut pid_file = private_file(&pid_path)?;
            pid_file.set_len(0)?;
            writeln!(pid_file, "{}", std::process::id())?;
            Ok(Self {
                _file: file,
                pid_path,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = agent_dir;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Runtime single-instance locking is unsupported on this platform",
            ))
        }
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Runtime lock and pid files must be regular files owned by the current user",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

impl Drop for RuntimeInstanceLock {
    fn drop(&mut self) {
        // Remove the pid before closing the fd releases the lock.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.pid_path);
    }
}
