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
    lock_dir: Option<PathBuf>,
}

impl Drop for Installation {
    fn drop(&mut self) {
        if let Some(path) = self.lock_dir.take() {
            let _ = fs::remove_dir(path);
        }
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
    /// Check ownership without taking a lock or creating state. A real marker
    /// supports custom directories without a persistent environment variable.
    pub fn is_managed() -> bool {
        Self::managed_executable().is_ok()
    }

    pub fn managed_executable() -> Result<PathBuf> {
        let executable = std::env::current_exe()?;
        let dir = default_install_dir()?;
        let expected = dir.join(if cfg!(windows) { "loop.exe" } else { "loop" });
        if !managed_at(&executable, &expected) {
            return Err(Error::Invalid(format!(
                "Cannot self-update {}: no installer ownership marker (.loop-update/). Use its package manager or install.sh/install.ps1. LOOP_INSTALL_DIR currently resolves to {}",
                executable.display(), dir.display()
            )));
        }
        Ok(executable)
    }

    pub fn current() -> Result<Self> {
        Self::at(Self::managed_executable()?)
    }

    pub fn notification_cache_path() -> Result<PathBuf> {
        let executable = Self::managed_executable()?;
        let state = executable
            .parent()
            .ok_or_else(|| Error::Invalid("Missing install directory".into()))?
            .join(".loop-update");
        if !state.exists() {
            fs::create_dir(&state)?;
        }
        if !fs::symlink_metadata(&state)?.file_type().is_dir() {
            return Err(Error::Invalid(
                "Update marker must be a real directory".into(),
            ));
        }
        Ok(state.join("update-check.json"))
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
        let installation = Self {
            executable,
            state_dir,
            lock_dir: Some(lock_dir),
        };
        installation.prune_retired();
        Ok(installation)
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

    fn prune_retired(&self) {
        if let Ok(entries) = fs::read_dir(&self.state_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("retired-")
                    && entry.path().extension().is_some_and(|ext| ext == "exe")
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
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
            let failures =
                self.restore_after_failed_replace(&backup, &record_path, old_record, &record);
            if failures.is_empty() {
                return Err(error);
            }
            return Err(Error::Invalid(format!(
                "{error}; additionally failed to restore: {}",
                failures.join("; ")
            )));
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

    /// Best-effort recovery after a failed replacement. Every step runs even
    /// if an earlier one fails, so the caller can still report the original
    /// error; the failed steps are returned for that report.
    fn restore_after_failed_replace(
        &self,
        backup: &Path,
        record_path: &Path,
        old_record: Option<Vec<u8>>,
        record: &Rollback,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        // Windows replacement can move the old executable before a later
        // copy/rename fails. Restore its public path before returning.
        if !self.executable.exists() {
            let restore = || -> Result<()> {
                let mut temp = tempfile::NamedTempFile::new_in(self.install_dir()?)?;
                std::io::copy(&mut fs::File::open(backup)?, &mut temp)?;
                fs::set_permissions(temp.path(), fs::metadata(backup)?.permissions())?;
                temp.as_file().sync_all()?;
                temp.persist(&self.executable).map_err(|e| e.error)?;
                Ok(())
            };
            if let Err(e) = restore() {
                failures.push(format!("previous binary: {e}"));
            }
        }
        // The record must describe the binary actually on disk. If the
        // candidate landed despite the error, the new record is correct.
        if sha256_file(&self.executable).ok().as_deref() == Some(&record.current_sha256) {
            return failures;
        }
        let restore = || -> Result<()> {
            match old_record {
                Some(bytes) => {
                    let mut temp = tempfile::NamedTempFile::new_in(&self.state_dir)?;
                    temp.write_all(&bytes)?;
                    temp.as_file().sync_all()?;
                    temp.persist(record_path).map_err(|e| e.error)?;
                }
                None => fs::remove_file(record_path)?,
            }
            Ok(())
        };
        if let Err(e) = restore() {
            failures.push(format!("rollback record: {e}"));
        }
        failures
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

    pub fn uninstall(mut self) -> Result<()> {
        regular(&self.executable)?;
        if fs::canonicalize(std::env::current_exe()?)? == fs::canonicalize(&self.executable)? {
            self_replace::self_delete()?;
        } else {
            fs::remove_file(&self.executable)?;
        }
        // Keep the mkdir lock until every other entry has been removed. A
        // recursive deletion of the whole directory could delete the lock first.
        for entry in fs::read_dir(&self.state_dir)? {
            let entry = entry?;
            if self.lock_dir.as_ref() == Some(&entry.path()) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        if let Some(lock) = self.lock_dir.take() {
            fs::remove_dir(lock)?;
        }
        // A new installer may start now. Never recursively delete its files.
        match fs::remove_dir(&self.state_dir) {
            Ok(()) => (),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

fn managed_at(executable: &Path, default: &Path) -> bool {
    if regular(executable).is_err()
        || executable.file_name()
            != Some(std::ffi::OsStr::new(if cfg!(windows) {
                "loop.exe"
            } else {
                "loop"
            }))
    {
        return false;
    }
    let marker = executable.parent().map(|p| p.join(".loop-update"));
    let has_marker =
        marker.is_some_and(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir()));
    has_marker
        || (regular(default).is_ok()
            && fs::canonicalize(executable).ok() == fs::canonicalize(default).ok())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_lock_free_and_supports_custom_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp
            .path()
            .join(if cfg!(windows) { "loop.exe" } else { "loop" });
        let default = tmp.path().join("other/loop");
        fs::write(&exe, "fixture").unwrap();
        assert!(!managed_at(&exe, &default));
        fs::create_dir(tmp.path().join(".loop-update")).unwrap();
        assert!(managed_at(&exe, &default));
        assert!(!tmp.path().join(".loop-update/operation.lock").exists());
        assert!(
            managed_at(&exe, &exe),
            "legacy default location remains managed"
        );
    }

    #[test]
    fn failed_replace_keeps_original_error_and_record() {
        for old_record in [Some(b"{\"previous\":true}".to_vec()), None] {
            let tmp = tempfile::tempdir().unwrap();
            let exe = tmp
                .path()
                .join(if cfg!(windows) { "loop.exe" } else { "loop" });
            fs::write(&exe, "old").unwrap();
            let installation = Installation::at(exe.clone()).unwrap();
            let record_path = installation.state_dir.join("rollback.json");
            if let Some(bytes) = &old_record {
                fs::write(&record_path, bytes).unwrap();
            }
            let record = Rollback {
                schema: 1,
                current_sha256: "a".repeat(64),
                previous_sha256: sha256_file(&exe).unwrap(),
                previous_version: "0.0.1".into(),
            };

            let error = installation
                .replace(&tmp.path().join("missing-candidate"), record)
                .unwrap_err();

            assert!(
                matches!(&error, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
                "original error should be returned unchanged, got {error}"
            );
            assert_eq!(fs::read(&exe).unwrap(), b"old");
            assert_eq!(fs::read(&record_path).ok(), old_record);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_markers_do_not_establish_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("loop");
        fs::write(&exe, "fixture").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join(".loop-update")).unwrap();
        assert!(!managed_at(&exe, &tmp.path().join("other")));
    }
}
