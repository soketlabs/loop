//! Typed hook registry for harness lifecycle events.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::types::AgentMessage;

type Handler = Arc<dyn Fn(HarnessHookEvent) -> Pin<Box<dyn Future<Output = HookOutcome> + Send>> + Send + Sync>;

/// Harness hook events (pi agent-harness parity).
#[derive(Debug, Clone)]
pub enum HarnessHookEvent {
    /// Before an agent turn starts.
    BeforeAgentStart {
        /// Messages in the turn snapshot.
        messages: Vec<AgentMessage>,
    },
    /// Before session compaction.
    SessionBeforeCompact {
        /// Cut index from compaction preparation.
        preparation_cut: usize,
        /// Optional custom summarizer instructions.
        custom_instructions: Option<String>,
    },
    /// Before navigating the session tree.
    SessionBeforeTree {
        /// Target entry id.
        target_id: String,
    },
    /// Harness settled to idle after a turn.
    Settled,
    /// Steering / follow-up / next-turn queue changed.
    QueueUpdate,
    /// Shutdown was requested.
    ShutdownRequested,
}

/// Outcome from a hook handler (field-merge across handlers).
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    /// Cancel the pending operation.
    pub cancel: bool,
    /// Optional summary supplied by hook (compact / tree).
    pub summary: Option<String>,
}

/// Registry of harness hook handlers.
#[derive(Clone, Default)]
pub struct HookRegistry {
    handlers: Arc<Mutex<Vec<Handler>>>,
}

impl HookRegistry {
    /// Register a hook handler.
    pub fn on<F, Fut>(&self, handler: F)
    where
        F: Fn(HarnessHookEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookOutcome> + Send + 'static,
    {
        self.handlers.lock().push(Arc::new(move |event| {
            Box::pin(handler(event))
        }));
    }

    /// Emit an event to all handlers; cancel if any handler cancels; first summary wins.
    pub async fn emit(&self, event: HarnessHookEvent) -> HookOutcome {
        let handlers = self.handlers.lock().clone();
        let mut merged = HookOutcome::default();
        for handler in handlers {
            let outcome = handler(event.clone()).await;
            if outcome.cancel {
                merged.cancel = true;
            }
            if merged.summary.is_none() {
                merged.summary = outcome.summary;
            }
        }
        merged
    }
}
