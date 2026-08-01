//! Per-key serial operation queue with a global concurrency limit.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};

type VoidFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

fn resolved_future() -> VoidFuture {
    Box::pin(async {})
}

/// Serializes operations per key while allowing concurrent operations on different keys.
pub struct KeyedOperationQueue {
    semaphore: Arc<Semaphore>,
    state: Mutex<QueueState>,
}

struct QueueState {
    tails: HashMap<String, VoidFuture>,
    barrier: VoidFuture,
}

impl KeyedOperationQueue {
    /// Create a queue with at most `max_concurrent` operations running at once.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            state: Mutex::new(QueueState {
                tails: HashMap::new(),
                barrier: resolved_future(),
            }),
        }
    }

    /// Enqueue an operation for `key`, waiting for prior operations on the same key.
    pub async fn enqueue<F, Fut, T>(&self, key: String, f: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send,
        T: Send,
    {
        let (barrier_wait, previous) = {
            let mut state = self.state.lock().await;
            let barrier_wait = std::mem::replace(&mut state.barrier, resolved_future());
            let previous = state
                .tails
                .remove(&key)
                .unwrap_or_else(resolved_future);
            (barrier_wait, previous)
        };

        barrier_wait.await;
        previous.await;

        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("keyed operation semaphore closed");
        let result = f().await;
        drop(_permit);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let tail = Box::pin(async move {
            let _ = rx.await;
        }) as VoidFuture;
        {
            let mut state = self.state.lock().await;
            state.tails.insert(key, tail);
        }
        let _ = tx.send(());

        result
    }

    /// Run `f` after all in-flight keyed operations complete.
    pub async fn enqueue_barrier<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send,
        T: Send,
    {
        let previous_barrier = {
            let mut state = self.state.lock().await;
            let tails: Vec<VoidFuture> = state.tails.drain().map(|(_, tail)| tail).collect();
            let previous_barrier = std::mem::replace(&mut state.barrier, resolved_future());
            for tail in tails {
                tail.await;
            }
            previous_barrier
        };

        previous_barrier.await;

        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("keyed operation semaphore closed");
        let result = f().await;
        drop(_permit);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let next_barrier = Box::pin(async move {
            let _ = rx.await;
        }) as VoidFuture;
        {
            let mut state = self.state.lock().await;
            state.barrier = next_barrier;
        }
        let _ = tx.send(());

        result
    }

    /// Wait until all queued operations finish.
    #[allow(dead_code)]
    pub async fn drain(&self) {
        self.enqueue_barrier(|| async {}).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn serializes_same_key() {
        let queue = Arc::new(KeyedOperationQueue::new(4));
        let order = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let queue = Arc::clone(&queue);
            let order = Arc::clone(&order);
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                queue
                    .enqueue("a".into(), || async move {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        order.lock().await.push(n);
                        n
                    })
                    .await
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(*order.lock().await, vec![0, 1, 2]);
    }
}
