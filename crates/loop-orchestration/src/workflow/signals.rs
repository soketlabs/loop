//! Signal dispatch and timer management.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use super::types::Signal;
use crate::planner::task_graph::TaskId;

/// Manages signal routing to workflow tasks and timers.
pub struct SignalRouter {
    task_senders: Mutex<HashMap<TaskId, broadcast::Sender<Signal>>>,
    workflow_sender: broadcast::Sender<Signal>,
    timers: Mutex<HashMap<String, CancellationToken>>,
}

impl SignalRouter {
    /// Create a new signal router.
    pub fn new() -> Self {
        let (workflow_sender, _) = broadcast::channel(64);
        Self {
            task_senders: Mutex::new(HashMap::new()),
            workflow_sender,
            timers: Mutex::new(HashMap::new()),
        }
    }

    /// Register a task and get a receiver for signals directed at it.
    pub async fn register_task(&self, task_id: &str) -> broadcast::Receiver<Signal> {
        let mut senders = self.task_senders.lock().await;
        let entry = senders
            .entry(task_id.to_string())
            .or_insert_with(|| broadcast::channel(32).0);
        entry.subscribe()
    }

    /// Unregister a task (cleanup on completion).
    pub async fn unregister_task(&self, task_id: &str) {
        self.task_senders.lock().await.remove(task_id);
    }

    /// Send a signal to a specific task. Returns whether delivery succeeded.
    pub async fn send_to_task(&self, task_id: &str, signal: Signal) -> bool {
        let senders = self.task_senders.lock().await;
        if let Some(sender) = senders.get(task_id) {
            sender.send(signal).is_ok()
        } else {
            false
        }
    }

    /// Broadcast a signal to the workflow-level channel.
    pub fn broadcast(&self, signal: Signal) {
        let _ = self.workflow_sender.send(signal);
    }

    /// Subscribe to workflow-level signals.
    pub fn subscribe_workflow(&self) -> broadcast::Receiver<Signal> {
        self.workflow_sender.subscribe()
    }

    /// Schedule a timer that fires a `Signal::Timer` after the given duration.
    pub async fn schedule_timer(
        self: &Arc<Self>,
        name: String,
        duration: std::time::Duration,
        target_task: Option<TaskId>,
    ) {
        let cancel = CancellationToken::new();
        self.timers
            .lock()
            .await
            .insert(name.clone(), cancel.clone());

        let router = Arc::clone(self);
        let timer_name = name.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(duration) => {
                    let signal = Signal::Timer { name: timer_name.clone() };
                    if let Some(task_id) = &target_task {
                        router.send_to_task(task_id, signal).await;
                    } else {
                        router.broadcast(signal);
                    }
                    router.timers.lock().await.remove(&timer_name);
                }
                _ = cancel.cancelled() => {}
            }
        });
    }

    /// Cancel a named timer. Returns whether it existed.
    pub async fn cancel_timer(&self, name: &str) -> bool {
        if let Some(token) = self.timers.lock().await.remove(name) {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel all timers and unregister all tasks.
    pub async fn shutdown(&self) {
        let timers = std::mem::take(&mut *self.timers.lock().await);
        for (_, token) in timers {
            token.cancel();
        }
        self.task_senders.lock().await.clear();
    }
}

impl Default for SignalRouter {
    fn default() -> Self {
        Self::new()
    }
}
