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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::bus::create_memory_bus;

    #[tokio::test]
    async fn set_and_get() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(bus);
        mem.set_entry("key1", serde_json::json!("hello"), &"task_a".into()).await;
        let entry = mem.get_entry("key1").await.unwrap();
        assert_eq!(entry.value, serde_json::json!("hello"));
        assert_eq!(entry.written_by, "task_a");
        assert_eq!(entry.version, 1);
    }

    #[tokio::test]
    async fn version_increments() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(bus);
        mem.set_entry("k", serde_json::json!(1), &"t1".into()).await;
        mem.set_entry("k", serde_json::json!(2), &"t2".into()).await;
        let entry = mem.get_entry("k").await.unwrap();
        assert_eq!(entry.version, 2);
        assert_eq!(entry.written_by, "t2");
        assert_eq!(entry.value, serde_json::json!(2));
    }

    #[tokio::test]
    async fn list_by_prefix() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(bus);
        mem.set_entry("result:a", serde_json::json!(1), &"t".into()).await;
        mem.set_entry("result:b", serde_json::json!(2), &"t".into()).await;
        mem.set_entry("config:x", serde_json::json!(3), &"t".into()).await;

        let mut keys = mem.list("result:").await;
        keys.sort();
        assert_eq!(keys, vec!["result:a", "result:b"]);
    }

    #[tokio::test]
    async fn delete_entry() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(bus);
        mem.set_entry("k", serde_json::json!(1), &"t".into()).await;
        assert!(mem.delete_entry("k").await);
        assert!(mem.get_entry("k").await.is_none());
        assert!(!mem.delete_entry("k").await);
    }

    #[tokio::test]
    async fn snapshot_returns_all() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(bus);
        mem.set_entry("a", serde_json::json!(1), &"t".into()).await;
        mem.set_entry("b", serde_json::json!(2), &"t".into()).await;
        let snap = mem.snapshot().await;
        assert_eq!(snap.len(), 2);
    }

    #[tokio::test]
    async fn bus_receives_change_notifications() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(Arc::clone(&bus));

        let mut rx = bus.subscribe_key("key1").await;

        mem.set_entry("key1", serde_json::json!("v"), &"writer".into()).await;

        let event = rx.try_recv().unwrap();
        assert_eq!(event.key, "key1");
        assert_eq!(event.writer, "writer");
    }

    #[tokio::test]
    async fn shared_memory_access_trait() {
        let bus = create_memory_bus();
        let mem = SharedMemory::new(bus);
        let access: &dyn SharedMemoryAccess = &mem;

        access.set("k", serde_json::json!("val"), "w").await;
        let v = access.get("k").await.unwrap();
        assert_eq!(v, serde_json::json!("val"));

        let keys = access.list_keys("").await;
        assert_eq!(keys, vec!["k"]);

        assert!(access.delete("k").await);
        assert!(access.get("k").await.is_none());
    }
}
