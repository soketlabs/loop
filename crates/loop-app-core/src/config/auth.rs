//! File-backed auth.json credential store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use loop_ai::{Credential, CredentialStore};

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    providers: HashMap<String, StoredCred>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum StoredCred {
    #[serde(rename = "apiKey")]
    ApiKey { key: String },
}

impl From<StoredCred> for Credential {
    fn from(value: StoredCred) -> Self {
        match value {
            StoredCred::ApiKey { key } => Credential::api_key(key),
        }
    }
}

fn stored_from_credential(value: &Credential) -> Option<StoredCred> {
    match value {
        Credential::ApiKey { key } => Some(StoredCred::ApiKey { key: key.clone() }),
        Credential::OAuth { .. } => None,
    }
}

/// Credential store persisted to `auth.json` (mode 0600).
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    cache: Arc<Mutex<HashMap<String, Credential>>>,
}

impl FileCredentialStore {
    /// Load (or create empty) store at path.
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let cache = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let file: AuthFile = if raw.trim().is_empty() {
                AuthFile::default()
            } else {
                serde_json::from_str(&raw)?
            };
            file.providers
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            cache: Arc::new(Mutex::new(cache)),
        })
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let guard = self.cache.lock();
        let mut file = AuthFile::default();
        for (k, v) in guard.iter() {
            if let Some(stored) = stored_from_credential(v) {
                file.providers.insert(k.clone(), stored);
            }
        }
        drop(guard);
        let json = serde_json::to_string_pretty(&file)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&self.path, perms)?;
        }
        Ok(())
    }

    /// Path to auth.json.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl CredentialStore for FileCredentialStore {
    fn get(&self, provider_id: &str) -> Option<Credential> {
        self.cache.lock().get(provider_id).cloned()
    }

    fn set(&self, provider_id: &str, credential: Credential) {
        self.cache
            .lock()
            .insert(provider_id.to_string(), credential);
        let _ = self.persist();
    }

    fn remove(&self, provider_id: &str) {
        self.cache.lock().remove(provider_id);
        let _ = self.persist();
    }

    fn list(&self) -> Vec<String> {
        self.cache.lock().keys().cloned().collect()
    }
}

/// Whether Soket (or named provider) has a usable key from env or store.
pub fn provider_has_key(store: &dyn CredentialStore, provider_id: &str, envs: &[&str]) -> bool {
    if store.get(provider_id).is_some() {
        return true;
    }
    for env in envs {
        if let Ok(v) = std::env::var(env) {
            if !v.trim().is_empty() {
                return true;
            }
        }
    }
    false
}
