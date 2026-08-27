//! Task-scoped memory: isolated per-task key-value store.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::planner::task_graph::TaskId;
use crate::scheduler::worker::TaskMemoryAccess;

/// Per-task isolated memory with local conversation history.
pub struct TaskMemory {
    /// Which task this memory belongs to.
    task_id: TaskId,
    /// Key-value store.
    store: RwLock<HashMap<String, Value>>,
    /// Task-local conversation history (serialized as JSON values).
    messages: RwLock<Vec<Value>>,
}

impl TaskMemory {
    /// Create a new task memory for the given task.
    pub fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            store: RwLock::new(HashMap::new()),
            messages: RwLock::new(Vec::new()),
        }
    }

    /// Get the task ID this memory belongs to.
    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// Get a value by key.
    pub async fn get_value(&self, key: &str) -> Option<Value> {
        self.store.read().await.get(key).cloned()
    }

    /// Set a value by key.
    pub async fn set_value(&self, key: &str, value: Value) {
        self.store.write().await.insert(key.to_string(), value);
    }

    /// List all keys.
    pub async fn keys(&self) -> Vec<String> {
        self.store.read().await.keys().cloned().collect()
    }

    /// Delete a key.
    pub async fn delete_value(&self, key: &str) -> bool {
        self.store.write().await.remove(key).is_some()
    }

    /// Append a message (serialized as JSON) to task-local conversation history.
    pub async fn push_message(&self, message: Value) {
        self.messages.write().await.push(message);
    }

    /// Get the task-local conversation history.
    pub async fn messages(&self) -> Vec<Value> {
        self.messages.read().await.clone()
    }

    /// Clear the conversation history.
    pub async fn clear_messages(&self) {
        self.messages.write().await.clear();
    }

    /// Initialize task memory with data from dependency results.
    pub async fn seed_from_dependencies(&self, deps: &HashMap<TaskId, Value>) {
        let mut store = self.store.write().await;
        for (dep_id, value) in deps {
            store.insert(format!("dep:{dep_id}"), value.clone());
        }
    }
}

#[async_trait]
impl TaskMemoryAccess for TaskMemory {
    async fn get(&self, key: &str) -> Option<Value> {
        self.get_value(key).await
    }

    async fn set(&self, key: &str, value: Value) {
        self.set_value(key, value).await;
    }

    async fn list_keys(&self) -> Vec<String> {
        self.keys().await
    }

    async fn delete(&self, key: &str) -> bool {
        self.delete_value(key).await
    }
}
