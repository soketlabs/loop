//! Workflow-scoped shared memory accessible by all workers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use super::bus::{MemoryBus, MemoryChangeEvent};
use crate::planner::task_graph::TaskId;
use crate::scheduler::worker::SharedMemoryAccess;

/// A single entry in shared memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// The stored value.
    pub value: Value,
    /// Task that last wrote this entry.
    pub written_by: TaskId,
    /// Unix epoch milliseconds of last write.
    pub timestamp: i64,
    /// Monotonically increasing version counter.
    pub version: u64,
}

/// Workflow-scoped key-value store with change notification.
pub struct SharedMemory {
    /// Underlying storage.
    store: RwLock<HashMap<String, MemoryEntry>>,
    /// Change notification bus.
    bus: Arc<MemoryBus>,
}

impl SharedMemory {
    /// Create a new shared memory instance with the given bus.
    pub fn new(bus: Arc<MemoryBus>) -> Self {
        Self {
            store: RwLock::new(HashMap::new()),
            bus,
        }
    }

    /// Get a memory entry by key.
    pub async fn get_entry(&self, key: &str) -> Option<MemoryEntry> {
        self.store.read().await.get(key).cloned()
    }

    /// Set a memory entry, incrementing version if it exists.
    pub async fn set_entry(&self, key: &str, value: Value, writer: &TaskId) {
        let mut store = self.store.write().await;
        let version = store.get(key).map(|e| e.version + 1).unwrap_or(1);
        let entry = MemoryEntry {
            value,
            written_by: writer.clone(),
            timestamp: now_ms(),
            version,
        };
        store.insert(key.to_string(), entry);
        drop(store);

        self.bus
            .publish(MemoryChangeEvent {
                key: key.to_string(),
                writer: writer.clone(),
                timestamp: now_ms(),
            })
            .await;
    }

    /// List keys matching a prefix.
    pub async fn list(&self, prefix: &str) -> Vec<String> {
        self.store
            .read()
            .await
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Delete a key, returns whether it existed.
    pub async fn delete_entry(&self, key: &str) -> bool {
        self.store.write().await.remove(key).is_some()
    }

    /// Get a snapshot of all entries.
    pub async fn snapshot(&self) -> HashMap<String, MemoryEntry> {
        self.store.read().await.clone()
    }

    /// Get the memory bus for subscribing to changes.
    pub fn bus(&self) -> &Arc<MemoryBus> {
        &self.bus
    }
}

#[async_trait]
impl SharedMemoryAccess for SharedMemory {
    async fn get(&self, key: &str) -> Option<Value> {
        self.get_entry(key).await.map(|e| e.value)
    }

    async fn set(&self, key: &str, value: Value, writer: &str) {
        self.set_entry(key, value, &writer.to_string()).await;
    }

    async fn list_keys(&self, prefix: &str) -> Vec<String> {
        self.list(prefix).await
    }

    async fn delete(&self, key: &str) -> bool {
        self.delete_entry(key).await
    }
}
