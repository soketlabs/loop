//! Async event stream for assistant message events.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::types::{AssistantMessage, AssistantMessageEvent, StopReason, Usage};

struct SharedState<T, R> {
    queue: Vec<T>,
    done: bool,
    waker: Option<std::task::Waker>,
    final_result: Option<R>,
}

/// Generic push-based async stream with a terminal result.
pub struct EventStream<T, R> {
    state: Arc<Mutex<SharedState<T, R>>>,
    notify: Arc<Notify>,
    is_complete: fn(&T) -> bool,
    extract_result: fn(&T) -> R,
}

impl<T, R> EventStream<T, R>
where
    T: Send + 'static,
    R: Clone + Send + 'static,
{
    /// Create a new event stream.
    pub fn new(is_complete: fn(&T) -> bool, extract_result: fn(&T) -> R) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                queue: Vec::new(),
                done: false,
                waker: None,
                final_result: None,
            })),
            notify: Arc::new(Notify::new()),
            is_complete,
            extract_result,
        }
    }

    /// Push an event. Terminal events resolve [`Self::result`].
    pub fn push(&self, event: T) {
        self.handle().push(event);
    }

    /// Mark the stream ended without a terminal event (should be rare).
    pub fn end(&self) {
        self.handle().end();
    }

    /// Handle that can push events from another task.
    pub fn handle(&self) -> EventStreamHandle<T, R> {
        EventStreamHandle {
            state: Arc::clone(&self.state),
            notify: Arc::clone(&self.notify),
            is_complete: self.is_complete,
            extract_result: self.extract_result,
        }
    }

    /// Await the final result (`done` or `error`). Safe to call while iterating.
    pub fn result(&self) -> impl Future<Output = R> + '_ {
        let state = Arc::clone(&self.state);
        let notify = Arc::clone(&self.notify);
        async move {
            loop {
                {
                    let guard = state.lock();
                    if let Some(result) = guard.final_result.clone() {
                        return result;
                    }
                    if guard.done && guard.final_result.is_none() {
                        // Should not happen for assistant streams; caller gets whatever R default path.
                        drop(guard);
                        // Wait once more in case of race, then panic-free fallback via notify timeout path:
                        // For typed assistant stream we override; for generic, spin on notify.
                    }
                }
                notify.notified().await;
            }
        }
    }
}

impl EventStream<AssistantMessageEvent, AssistantMessage> {
    /// Await the final assistant message, with a fallback if the stream ended uncleanly.
    pub async fn result_message(&self) -> AssistantMessage {
        loop {
            {
                let guard = self.state.lock();
                if let Some(result) = guard.final_result.clone() {
                    return result;
                }
                if guard.done {
                    return AssistantMessage {
                        content: Vec::new(),
                        api: String::new(),
                        provider: String::new(),
                        model: String::new(),
                        response_model: None,
                        response_id: None,
                        usage: Usage::empty(),
                        stop_reason: StopReason::Error,
                        error_message: Some("stream closed without terminal event".into()),
                        raw_stop_reason: None,
                        timestamp: crate::utils::id::now_ms(),
                    };
                }
            }
            self.notify.notified().await;
        }
    }
}

impl<T, R> Stream for EventStream<T, R> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.state.lock();
        if !state.queue.is_empty() {
            return Poll::Ready(Some(state.queue.remove(0)));
        }
        if state.done {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Cloneable handle for producing events.
#[derive(Clone)]
pub struct EventStreamHandle<T, R> {
    state: Arc<Mutex<SharedState<T, R>>>,
    notify: Arc<Notify>,
    is_complete: fn(&T) -> bool,
    extract_result: fn(&T) -> R,
}

impl<T, R> EventStreamHandle<T, R> {
    /// Push an event.
    pub fn push(&self, event: T) {
        let mut state = self.state.lock();
        if state.done {
            return;
        }
        if (self.is_complete)(&event) {
            state.done = true;
            let result = (self.extract_result)(&event);
            state.final_result = Some(result);
            self.notify.notify_waiters();
        }
        state.queue.push(event);
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    /// End without a terminal event.
    pub fn end(&self) {
        let mut state = self.state.lock();
        state.done = true;
        self.notify.notify_waiters();
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}

fn assistant_is_complete(event: &AssistantMessageEvent) -> bool {
    event.is_terminal()
}

fn assistant_extract_result(event: &AssistantMessageEvent) -> AssistantMessage {
    match event {
        AssistantMessageEvent::Done { message, .. } => message.clone(),
        AssistantMessageEvent::Error { error, .. } => error.clone(),
        _ => unreachable!("non-terminal event"),
    }
}

/// Stream of [`AssistantMessageEvent`] that resolves to an [`AssistantMessage`].
pub struct AssistantMessageEventStream {
    inner: EventStream<AssistantMessageEvent, AssistantMessage>,
}

impl AssistantMessageEventStream {
    /// Create an empty stream.
    pub fn new() -> Self {
        Self {
            inner: EventStream::new(assistant_is_complete, assistant_extract_result),
        }
    }

    /// Push handle for background producers.
    pub fn handle(&self) -> AssistantMessageEventStreamHandle {
        self.inner.handle()
    }

    /// Push an event.
    pub fn push(&self, event: AssistantMessageEvent) {
        self.inner.push(event);
    }

    /// Await the final assistant message (`done` or `error`).
    pub async fn result(&self) -> AssistantMessage {
        self.inner.result_message().await
    }
}

impl Default for AssistantMessageEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

/// Handle for pushing assistant events.
pub type AssistantMessageEventStreamHandle =
    EventStreamHandle<AssistantMessageEvent, AssistantMessage>;

/// Create an assistant message event stream.
pub fn create_assistant_message_event_stream() -> AssistantMessageEventStream {
    AssistantMessageEventStream::new()
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    fn sample_message(reason: StopReason) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: reason,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn push_iterate_and_result() {
        let stream = create_assistant_message_event_stream();
        let handle = stream.handle();

        let done_msg = sample_message(StopReason::Stop);
        handle.push(AssistantMessageEvent::Start {
            partial: sample_message(StopReason::Pending),
        });
        handle.push(AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: done_msg.clone(),
        });

        let mut stream = stream;
        let first = stream.next().await.unwrap();
        assert!(matches!(first, AssistantMessageEvent::Start { .. }));
        let second = stream.next().await.unwrap();
        assert!(matches!(second, AssistantMessageEvent::Done { .. }));
        assert!(stream.next().await.is_none());

        let result = stream.result().await;
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(result.model, "m");
    }
}
