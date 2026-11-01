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

/// Exclusive lock on `runtime.lock` beside a Session database, held for the
/// Runtime's lifetime. Holding it proves no other Runtime owns that database, so
/// `owner = "runtime"` leases recorded there belong to a dead Runtime. This is
/// the only way to obtain that proof.
#[derive(Debug)]
pub struct SessionDatabaseLock {
    sessions_db: std::path::PathBuf,
    #[cfg(unix)]
    _file: std::fs::File,
}

impl SessionDatabaseLock {
    /// Lock without waiting. Fails when another Runtime owns the database.
    pub fn acquire(sessions_db: &Path) -> io::Result<Self> {
        let sessions_db = std::path::absolute(sessions_db)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let dir = sessions_db.parent().unwrap_or(Path::new("/"));
            std::fs::create_dir_all(dir)?;
            let file = private_file(&dir.join("runtime.lock"))?;
            // SAFETY: the fd is valid for this call; flock does not retain it.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(error);
                }
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "another Runtime already owns the Session database at {}",
                        sessions_db.display()
                    ),
                ));
            }
            Ok(Self {
                sessions_db,
                _file: file,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = sessions_db;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Session database locking is unsupported on this platform",
            ))
        }
    }

    /// The absolute path of the locked Session database.
    pub fn sessions_db(&self) -> &Path {
        &self.sessions_db
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
