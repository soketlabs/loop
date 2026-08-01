//! Serialize file mutations per path key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

struct QueueState {
    locks: HashMap<String, Arc<AsyncMutex<()>>>,
}

static STATES: Mutex<Option<QueueState>> = Mutex::new(None);

fn lock_for(path: &Path) -> Arc<AsyncMutex<()>> {
    let key = path.to_string_lossy().into_owned();
    let mut guard = STATES.lock();
    if guard.is_none() {
        *guard = Some(QueueState {
            locks: HashMap::new(),
        });
    }
    let st = guard.as_mut().unwrap();
    st.locks
        .entry(key)
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// Acquire the mutation lock for `path`, then run `f`.
pub async fn with_file_mutation_queue<T, F, Fut>(path: PathBuf, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T> + Send,
    T: Send,
{
    let lock = lock_for(&path);
    let _guard = lock.lock().await;
    f().await
}
