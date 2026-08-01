//! Persistent cache for dynamically refreshed model catalogs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::Model;

/// Stored catalog entry for one provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    /// Models last fetched for this provider.
    pub models: Vec<Model>,
    /// Unix ms when the catalog was last checked / written.
    pub checked_at: i64,
}

/// Errors from the models store.
#[derive(Debug, Error)]
pub enum ModelsStoreError {
    /// I/O failure.
    #[error("models store io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON failure.
    #[error("models store json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persistent (or in-memory) store for dynamic model catalogs.
#[async_trait]
pub trait ModelsStore: Send + Sync {
    /// Read a provider's cached catalog.
    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, ModelsStoreError>;
    /// Write a provider's cached catalog.
    async fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
    ) -> Result<(), ModelsStoreError>;
}

/// In-memory models store (tests).
#[derive(Debug, Default)]
pub struct InMemoryModelsStore {
    entries: RwLock<HashMap<String, ModelsStoreEntry>>,
}

impl InMemoryModelsStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelsStore for InMemoryModelsStore {
    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, ModelsStoreError> {
        Ok(self.entries.read().get(provider_id).cloned())
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
    ) -> Result<(), ModelsStoreError> {
        self.entries.write().insert(provider_id.to_string(), entry);
        Ok(())
    }
}

/// File-backed models store (`models-store.json`).
#[derive(Debug)]
pub struct FileModelsStore {
    path: PathBuf,
    cache: RwLock<Option<HashMap<String, ModelsStoreEntry>>>,
}

impl FileModelsStore {
    /// Create a store at the given path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cache: RwLock::new(None),
        }
    }

    /// Path to the store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_from_disk(&self) -> Result<HashMap<String, ModelsStoreEntry>, ModelsStoreError> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let raw = std::fs::read_to_string(&self.path)?;
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&raw)?)
    }

    fn ensure_loaded(&self) -> Result<(), ModelsStoreError> {
        if self.cache.read().is_some() {
            return Ok(());
        }
        let data = self.load_from_disk()?;
        *self.cache.write() = Some(data);
        Ok(())
    }

    fn persist(&self, data: &HashMap<String, ModelsStoreEntry>) -> Result<(), ModelsStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(data)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[async_trait]
impl ModelsStore for FileModelsStore {
    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, ModelsStoreError> {
        self.ensure_loaded()?;
        Ok(self
            .cache
            .read()
            .as_ref()
            .and_then(|m| m.get(provider_id).cloned()))
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
    ) -> Result<(), ModelsStoreError> {
        self.ensure_loaded()?;
        let mut guard = self.cache.write();
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(provider_id.to_string(), entry);
        self.persist(map)?;
        Ok(())
    }
}

/// Shared store handle.
pub type SharedModelsStore = Arc<dyn ModelsStore>;
