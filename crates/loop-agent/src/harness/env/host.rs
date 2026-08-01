//! Host (local machine) execution environment.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::harness::types::{
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileSystem, Shell,
    ShellExecOptions, ShellOutput,
};

/// Local filesystem + shell execution environment.
pub struct HostExecutionEnv {
    cwd: PathBuf,
}

impl HostExecutionEnv {
    /// Create with an absolute or relative cwd (canonicalized best-effort).
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        Self { cwd }
    }

    /// Create rooted at the process current directory.
    pub fn from_current_dir() -> Result<Self, FileError> {
        let cwd = std::env::current_dir()
            .map_err(|e| FileError::new(FileErrorCode::Io, format!("current_dir: {e}")))?;
        Ok(Self::new(cwd))
    }

    fn map_io(err: std::io::Error) -> FileError {
        let code = match err.kind() {
            std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
            std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => FileErrorCode::AlreadyExists,
            _ => FileErrorCode::Io,
        };
        FileError::new(code, err.to_string())
    }
}

#[async_trait]
impl FileSystem for HostExecutionEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.cwd.join(path))
        }
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::read_to_string(&path)
            .await
            .map_err(Self::map_io)
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::read(&path).await.map_err(Self::map_io)
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(Self::map_io)?;
        }
        tokio::fs::write(&path, data).await.map_err(Self::map_io)
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        use tokio::io::AsyncWriteExt;
        let path = self.absolute_path(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(Self::map_io)?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(Self::map_io)?;
        file.write_all(data).await.map_err(Self::map_io)
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(Self::map_io)?;
        Ok(FileInfo {
            path,
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            size: meta.len(),
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let path = self.absolute_path(path)?;
        let mut rd = tokio::fs::read_dir(&path).await.map_err(Self::map_io)?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(Self::map_io)? {
            let meta = entry.metadata().await.map_err(Self::map_io)?;
            out.push(FileInfo {
                path: entry.path(),
                is_dir: meta.is_dir(),
                is_file: meta.is_file(),
                size: meta.len(),
            });
        }
        Ok(out)
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        let path = self.absolute_path(path)?;
        Ok(tokio::fs::try_exists(&path).await.map_err(Self::map_io)?)
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(Self::map_io)
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(Self::map_io)?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&path)
                .await
                .map_err(Self::map_io)
        } else {
            tokio::fs::remove_file(&path).await.map_err(Self::map_io)
        }
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::canonicalize(&path)
            .await
            .map_err(Self::map_io)
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        let base = std::env::temp_dir();
        let name = format!("{prefix}-{}", loop_ai::new_id());
        let path = base.join(name);
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(Self::map_io)?;
        Ok(path)
    }
}

#[async_trait]
impl Shell for HostExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let cwd = options.cwd.clone().unwrap_or_else(|| self.cwd.clone());

        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if !options.inherit_env {
            cmd.env_clear();
        }
        if let Some(env) = &options.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        let child = cmd.spawn().map_err(|e| {
            ExecutionError::new(ExecutionErrorCode::SpawnFailed, e.to_string())
        })?;

        let output_fut = child.wait_with_output();
        let output = if let Some(cancel) = &options.cancel {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Aborted,
                        "operation aborted",
                    ));
                }
                res = output_fut => res.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                })?,
            }
        } else if let Some(ms) = options.timeout_ms {
            match tokio::time::timeout(Duration::from_millis(ms), output_fut).await {
                Ok(res) => res.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                })?,
                Err(_) => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::TimedOut,
                        "command timed out",
                    ));
                }
            }
        } else {
            output_fut
                .await
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?
        };

        Ok(ShellOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}
