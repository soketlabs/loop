//! Artifact store for tracking task outputs.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use super::worker::ArtifactAccess;
use crate::planner::task_graph::TaskId;
use crate::workflow::types::{Artifact, ArtifactKind};

/// In-memory artifact store tracking outputs per task.
pub struct ArtifactStore {
    artifacts: RwLock<HashMap<TaskId, Vec<Artifact>>>,
}

impl ArtifactStore {
    /// Create an empty artifact store.
    pub fn new() -> Self {
        Self {
            artifacts: RwLock::new(HashMap::new()),
        }
    }

    /// Store an artifact for a task.
    pub async fn store_artifact(
        &self,
        task_id: &str,
        kind: ArtifactKind,
        data: Value,
        path: Option<String>,
    ) {
        let artifact = Artifact { kind, path, data };
        self.artifacts
            .write()
            .await
            .entry(task_id.to_string())
            .or_default()
            .push(artifact);
    }

    /// Get all artifacts for a task.
    pub async fn get_artifacts(&self, task_id: &str) -> Vec<Artifact> {
        self.artifacts
            .read()
            .await
            .get(task_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Get all artifacts across all tasks.
    pub async fn all_artifacts(&self) -> HashMap<TaskId, Vec<Artifact>> {
        self.artifacts.read().await.clone()
    }
}

impl Default for ArtifactStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Scoped artifact access bound to a specific task ID.
pub struct ScopedArtifactAccess {
    store: Arc<ArtifactStore>,
    task_id: TaskId,
}

impl ScopedArtifactAccess {
    /// Create a scoped accessor for the given task.
    pub fn new(store: Arc<ArtifactStore>, task_id: TaskId) -> Self {
        Self { store, task_id }
    }
}

#[async_trait]
impl ArtifactAccess for ScopedArtifactAccess {
    async fn store(&self, _task_id: &str, kind: &str, data: Value, path: Option<String>) {
        let artifact_kind = match kind {
            "file" => ArtifactKind::File,
            "code" => ArtifactKind::Code,
            "test_result" => ArtifactKind::TestResult,
            "log" => ArtifactKind::Log,
            other => ArtifactKind::Custom(other.to_string()),
        };
        self.store
            .store_artifact(&self.task_id, artifact_kind, data, path)
            .await;
    }

    async fn get_for_task(&self, task_id: &str) -> Vec<Value> {
        self.store
            .get_artifacts(task_id)
            .await
            .into_iter()
            .map(|a| a.data)
            .collect()
    }
}
