//! Pub/sub notification bus for memory changes.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

use crate::planner::task_graph::TaskId;

/// Entry representing a memory change notification.
#[derive(Debug, Clone)]
pub struct MemoryChangeEvent {
    /// The key that was modified.
    pub key: String,
    /// Which task wrote the change.
    pub writer: TaskId,
    /// Unix epoch milliseconds.
    pub timestamp: i64,
}

/// Pub/sub bus for memory change notifications.
pub struct MemoryBus {
    /// Per-key subscribers.
    key_senders: RwLock<HashMap<String, broadcast::Sender<MemoryChangeEvent>>>,
    /// Global broadcast for all changes.
    global_sender: broadcast::Sender<MemoryChangeEvent>,
}

impl MemoryBus {
    /// Create a new memory bus.
    pub fn new() -> Self {
        let (global_sender, _) = broadcast::channel(256);
        Self {
            key_senders: RwLock::new(HashMap::new()),
            global_sender,
        }
    }

    /// Subscribe to changes for a specific key.
    pub async fn subscribe_key(&self, key: &str) -> broadcast::Receiver<MemoryChangeEvent> {
        let mut senders = self.key_senders.write().await;
        let sender = senders
            .entry(key.to_string())
            .or_insert_with(|| broadcast::channel(32).0);
        sender.subscribe()
    }

    /// Subscribe to all memory changes.
    pub fn subscribe_all(&self) -> broadcast::Receiver<MemoryChangeEvent> {
        self.global_sender.subscribe()
    }

    /// Publish a memory change event.
    pub async fn publish(&self, event: MemoryChangeEvent) {
        let _ = self.global_sender.send(event.clone());
        let senders = self.key_senders.read().await;
        if let Some(sender) = senders.get(&event.key) {
            let _ = sender.send(event);
        }
    }
}

impl Default for MemoryBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience constructor.
pub fn create_memory_bus() -> Arc<MemoryBus> {
    Arc::new(MemoryBus::new())
}
