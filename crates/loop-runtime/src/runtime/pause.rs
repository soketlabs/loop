//! Named pause and signal points for deterministic race tests. Outside tests
//! they hold nothing and never wait.
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::{oneshot, Notify};

/// Where an armed pause reports arrival, and how it is released.
#[cfg(test)]
type Armed = (Arc<Notify>, oneshot::Receiver<bool>);

#[derive(Default)]
pub(super) struct Pauses {
    #[cfg(test)]
    armed: std::sync::Mutex<HashMap<&'static str, Armed>>,
    #[cfg(test)]
    signals: std::sync::Mutex<HashMap<&'static str, Arc<Notify>>>,
}

impl Pauses {
    /// Stop the next caller at `point`. The returned notify fires when it
    /// arrives; sending `true` releases it with an injected failure.
    #[cfg(test)]
    pub(super) fn arm(&self, point: &'static str) -> (Arc<Notify>, oneshot::Sender<bool>) {
        let reached = Arc::new(Notify::new());
        let (release, wait) = oneshot::channel();
        self.armed
            .lock()
            .unwrap()
            .insert(point, (reached.clone(), wait));
        (reached, release)
    }
    /// Notify whenever a caller passes `point`, from now on.
    #[cfg(test)]
    pub(super) fn watch(&self, point: &'static str) -> Arc<Notify> {
        self.signals
            .lock()
            .unwrap()
            .entry(point)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }
    /// Wait at `point` if a test armed it. True asks the caller to fail there.
    pub(super) async fn wait(&self, _point: &'static str) -> bool {
        #[cfg(test)]
        {
            let armed = self.armed.lock().unwrap().remove(_point);
            if let Some((reached, release)) = armed {
                reached.notify_one();
                return release.await.unwrap_or(false);
            }
        }
        false
    }
    /// Mark `point` passed for a watching test.
    pub(super) fn signal(&self, _point: &'static str) {
        #[cfg(test)]
        if let Some(notify) = self.signals.lock().unwrap().get(_point) {
            notify.notify_one();
        }
    }
}
