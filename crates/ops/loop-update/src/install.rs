use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{sha256_file, Error, Result};

pub fn default_install_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("LOOP_INSTALL_DIR") {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir()
        .map(|p| p.join(".loop/bin"))
        .ok_or_else(|| Error::Invalid("Cannot find the home directory".into()))
}

/// Ownership is limited to `loop[.exe]` and the reserved `.loop-update/` folder.
/// Installers use the same layout. No config or session paths are accepted.
pub struct Installation {
    executable: PathBuf,
    state_dir: PathBuf,
    lock_dir: PathBuf,
}

impl Drop for Installation {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.lock_dir);
    }
}

#[derive(Serialize, Deserialize)]
struct Rollback {
    schema: u32,
    current_sha256: String,
    previous_sha256: String,
    previous_version: String,
}

fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(Error::Invalid(format!(
            "Expected a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Installation {
    /// Accept only the installer location. Cargo/package-manager installs must
    /// be updated by their owner rather than overwritten by this command.
    pub fn current() -> Result<Self> {
        let executable = std::env::current_exe()?;
        let dir = default_install_dir()?;
        let expected = dir.join(if cfg!(windows) { "loop.exe" } else { "loop" });
        regular(&expected).map_err(|_| {
            Error::Invalid(
                "Installer binary is missing or is a symlink; use install.sh/install.ps1".into(),
            )
        })?;
        if fs::canonicalize(&executable)? != fs::canonicalize(&expected).map_err(|_| {
            Error::Invalid("This binary is not in the installer directory. Use its package manager or install.sh/install.ps1".into())
        })? {
            return Err(Error::Invalid("This binary is not in the installer directory. Use its package manager or install.sh/install.ps1".into()));
        }
        Self::at(executable)
    }

    /// Acquire the installation lock. The caller must establish ownership of
    /// this executable; CLI callers use `current` to enforce that boundary.
    pub fn at(executable: PathBuf) -> Result<Self> {
        regular(&executable)?;
        let parent = executable
            .parent()
            .ok_or_else(|| Error::Invalid("Missing installation directory".into()))?;
        let state_dir = parent.join(".loop-update");
        match fs::symlink_metadata(&state_dir) {
            Ok(m) if !m.file_type().is_dir() => {
                return Err(Error::Invalid(
                    "Update state directory must not be a symlink".into(),
                ))
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&state_dir)?,
            Err(e) => return Err(e.into()),
        }
        let lock_dir = state_dir.join("operation.lock");
        fs::create_dir(&lock_dir).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Invalid("Another installation operation is running. If it crashed, remove .loop-update/operation.lock after checking that no installer or updater is running".into())
            } else { e.into() }
        })?;
        Ok(Self {
            executable,
            state_dir,
            lock_dir,
        })
    }

    pub fn staging(&self) -> Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(&self.state_dir)?)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    fn install_dir(&self) -> Result<&Path> {
        self.executable
            .parent()
            .ok_or_else(|| Error::Invalid("Missing installation directory".into()))
    }

    /// Validate the candidate before any replacement. Save a checked previous
    /// binary and durable rollback record, then replace in one platform step.
    pub async fn install(&self, candidate: &Path, version: &str) -> Result<()> {
        regular(candidate)?;
        let wanted = semver::Version::parse(version)?;
        let actual = binary_version(candidate).await?;
        if wanted.to_string() != actual {
            return Err(Error::Invalid(format!(
                "Staged binary reports {actual}, expected {wanted}; current binary kept"
            )));
        }
        let current_version = binary_version(&self.executable).await?;
        let record = Rollback {
            schema: 1,
            current_sha256: sha256_file(candidate)?,
            previous_sha256: sha256_file(&self.executable)?,
            previous_version: current_version,
        };
        self.replace(candidate, record)
    }

    fn backup_path(&self, hash: &str) -> PathBuf {
        self.state_dir.join(format!(
            "previous-{hash}{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
    }

    fn replace(&self, candidate: &Path, record: Rollback) -> Result<()> {
        let backup = self.backup_path(&record.previous_sha256);
        // Immutable backups allow the old record to stay valid until the new
        // record is committed. A crash never pairs one version with another hash.
        if backup.exists() {
            regular(&backup)?;
            if sha256_file(&backup)? != record.previous_sha256 {
                return Err(Error::Invalid("Previous binary checksum mismatch".into()));
            }
        } else {
            let mut temp = tempfile::NamedTempFile::new_in(&self.state_dir)?;
            std::io::copy(&mut fs::File::open(&self.executable)?, &mut temp)?;
            temp.as_file().sync_all()?;
            fs::set_permissions(temp.path(), fs::metadata(&self.executable)?.permissions())?;
            temp.persist(&backup).map_err(|e| e.error)?;
        }
        let record_path = self.state_dir.join("rollback.json");
        let old_record = fs::read(&record_path).ok();
        self.write_record(&record)?;
        if let Err(error) = self.replace_executable(candidate) {
            // Windows replacement can move the old executable before a later
            // copy/rename fails. Restore its public path before returning.
            if !self.executable.exists() {
                let mut temp = tempfile::NamedTempFile::new_in(self.install_dir()?)?;
                std::io::copy(&mut fs::File::open(&backup)?, &mut temp)?;
                fs::set_permissions(temp.path(), fs::metadata(&backup)?.permissions())?;
                temp.as_file().sync_all()?;
                temp.persist(&self.executable).map_err(|e| e.error)?;
            }
            if let Some(bytes) = old_record {
                let mut temp = tempfile::NamedTempFile::new_in(&self.state_dir)?;
                temp.write_all(&bytes)?;
                temp.as_file().sync_all()?;
                temp.persist(record_path).map_err(|e| e.error)?;
            } else {
                fs::remove_file(record_path)?;
            }
            return Err(error);
        }
        // Keep only the rollback binary. Cleanup failures do not turn a
        // completed replacement into a reported update failure.
        if let Ok(entries) = fs::read_dir(&self.state_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("previous-")
                    && entry.path() != backup
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    fn write_record(&self, record: &Rollback) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        serde_json::to_writer(&mut temp, record)?;
        temp.as_file().sync_all()?;
        temp.persist(self.state_dir.join("rollback.json"))
            .map_err(|e| e.error)?;
        #[cfg(unix)]
        fs::File::open(&self.state_dir)?.sync_all()?;
        Ok(())
    }

    fn replace_executable(&self, candidate: &Path) -> Result<()> {
        if fs::canonicalize(std::env::current_exe()?)? == fs::canonicalize(&self.executable)? {
            self_replace::self_replace(candidate)?;
        } else {
            // Used by installers and embedding callers; never copy over a
            // live executable in place. Windows fails safely if it is in use.
            let mut temp = tempfile::NamedTempFile::new_in(self.install_dir()?)?;
            std::io::copy(&mut fs::File::open(candidate)?, &mut temp)?;
            temp.as_file().sync_all()?;
            fs::set_permissions(temp.path(), fs::metadata(candidate)?.permissions())?;
            temp.persist(&self.executable).map_err(|e| e.error)?;
        }
        Ok(())
    }

    pub async fn rollback(&self) -> Result<String> {
        let record_path = self.state_dir.join("rollback.json");
        regular(&record_path).map_err(|_| Error::Invalid("No rollback record found".into()))?;
        let record: Rollback = serde_json::from_slice(&fs::read(record_path)?)?;
        if record.schema != 1
            || !valid_hash(&record.current_sha256)
            || !valid_hash(&record.previous_sha256)
        {
            return Err(Error::Invalid("Invalid rollback record".into()));
        }
        if sha256_file(&self.executable)? != record.current_sha256 {
            return Err(Error::Invalid(
                "Installed binary differs from the rollback record; refusing rollback".into(),
            ));
        }
        let previous = self.backup_path(&record.previous_sha256);
        regular(&previous)?;
        if sha256_file(&previous)? != record.previous_sha256 {
            return Err(Error::Invalid(
                "Rollback binary checksum mismatch; current binary kept".into(),
            ));
        }
        self.install(&previous, &record.previous_version).await?;
        Ok(record.previous_version)
    }

    pub fn uninstall(self) -> Result<()> {
        regular(&self.executable)?;
        if fs::canonicalize(std::env::current_exe()?)? == fs::canonicalize(&self.executable)? {
            self_replace::self_delete()?;
        } else {
            fs::remove_file(&self.executable)?;
        }
        // Drop the file lock before removing the state directory on Windows.
        let state_dir = self.state_dir.clone();
        drop(self);
        fs::remove_dir_all(state_dir)?;
        Ok(())
    }
}

async fn binary_version(path: &Path) -> Result<String> {
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| {
            Error::Invalid("Binary version check timed out; current binary kept".into())
        })??;
    let text = std::str::from_utf8(&output.stdout).unwrap_or("").trim();
    let version = text.strip_prefix("loop ").unwrap_or(text);
    if !output.status.success() || semver::Version::parse(version).is_err() {
        return Err(Error::Invalid(
            "Binary version check failed; current binary kept".into(),
        ));
    }
    Ok(version.to_string())
}
