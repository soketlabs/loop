//! Project trust decisions (`trust.json`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    projects: HashMap<String, bool>,
}

/// Trust store for project directories.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    projects: HashMap<String, bool>,
}

impl TrustStore {
    /// Load from disk.
    pub fn load(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let projects = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let file: TrustFile = if raw.trim().is_empty() {
                TrustFile::default()
            } else {
                serde_json::from_str(&raw)?
            };
            file.projects
        } else {
            HashMap::new()
        };
        Ok(Self { path, projects })
    }

    fn key(cwd: &Path) -> String {
        cwd.canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    /// Look up a remembered trust decision.
    pub fn get(&self, cwd: &Path) -> Option<bool> {
        self.projects.get(&Self::key(cwd)).copied()
    }

    /// Remember a trust decision.
    pub fn set(&mut self, cwd: &Path, trusted: bool) -> anyhow::Result<()> {
        self.projects.insert(Self::key(cwd), trusted);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = TrustFile {
            projects: self.projects.clone(),
        };
        std::fs::write(&self.path, serde_json::to_string_pretty(&file)?)?;
        Ok(())
    }
}
