//! Worker pool: registry and dynamic dispatch of workers.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;

use super::worker::{Worker, WorkerError};

/// Manages a set of registered workers and concurrency limits.
pub struct WorkerPool {
    workers: Vec<Arc<dyn Worker>>,
    kind_index: HashMap<String, usize>,
    concurrency_limit: Arc<Semaphore>,
}

impl WorkerPool {
    /// Create a worker pool with the given maximum concurrency.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            workers: Vec::new(),
            kind_index: HashMap::new(),
            concurrency_limit: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Register a worker. Its supported task kinds are indexed for lookup.
    pub fn register(&mut self, worker: Arc<dyn Worker>) {
        let idx = self.workers.len();
        for kind in worker.supported_task_kinds() {
            self.kind_index.insert(kind.to_string(), idx);
        }
        self.workers.push(worker);
    }

    /// Find a worker capable of handling the given task kind.
    pub fn find_worker(&self, task_kind: &str) -> Result<Arc<dyn Worker>, WorkerError> {
        let idx = self
            .kind_index
            .get(task_kind)
            .ok_or_else(|| WorkerError::UnsupportedKind(task_kind.to_string()))?;
        Ok(Arc::clone(&self.workers[*idx]))
    }

    /// Acquire a concurrency permit (blocks if at limit).
    pub async fn acquire_permit(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, WorkerError> {
        Arc::clone(&self.concurrency_limit)
            .acquire_owned()
            .await
            .map_err(|_| WorkerError::Other("semaphore closed".to_string()))
    }

    /// Available permits (for monitoring).
    pub fn available_permits(&self) -> usize {
        self.concurrency_limit.available_permits()
    }
}
