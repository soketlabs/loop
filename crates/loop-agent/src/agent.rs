//! Stateful agent wrapping the low-level loop with queues and awaited subscribers.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{run_agent_loop, run_agent_loop_continue};
use crate::messages::user_message_with_images;
use crate::stream_fn::StreamFn;
use crate::types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentEvent, AgentEventSink,
    AgentLoopConfig, AgentMessage, AgentState, AgentThinkingLevel, AgentTool, BeforeToolCallContext,
    BeforeToolCallResult, PromptInput, QueueMode, ToolExecutionMode,
};

type Subscriber =
    Arc<dyn Fn(AgentEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

type BeforeHook = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>
        + Send
        + Sync,
>;

type AfterHook = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;

/// Options for constructing an [`Agent`].
pub struct AgentOptions {
    /// Initial state.
    pub initial_state: AgentState,
    /// Required stream function.
    pub stream_fn: StreamFn,
    /// Convert agent messages to LLM messages.
    pub convert_to_llm: Option<crate::types::ConvertToLlmFn>,
    /// Optional context transform.
    pub transform_context: Option<crate::types::TransformContextFn>,
    /// Steering queue mode.
    pub steering_mode: QueueMode,
    /// Follow-up queue mode.
    pub follow_up_mode: QueueMode,
    /// Tool execution mode.
    pub tool_execution: ToolExecutionMode,
    /// Session id for provider caching.
    pub session_id: Option<String>,
    /// Thinking budgets.
    pub thinking_budgets: Option<loop_ai::ThinkingBudgets>,
}

impl AgentOptions {
    /// Create options with state and stream fn.
    pub fn new(initial_state: AgentState, stream_fn: StreamFn) -> Self {
        Self {
            initial_state,
            stream_fn,
            convert_to_llm: None,
            transform_context: None,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            tool_execution: ToolExecutionMode::Parallel,
            session_id: None,
            thinking_budgets: None,
        }
    }
}

struct AgentHooks {
    before_tool_call: Option<BeforeHook>,
    after_tool_call: Option<AfterHook>,
}

/// Stateful agent with tool execution and event streaming.
pub struct Agent {
    state: Arc<RwLock<AgentState>>,
    stream_fn: StreamFn,
    convert_to_llm: crate::types::ConvertToLlmFn,
    transform_context: Option<crate::types::TransformContextFn>,
    steering_mode: Mutex<QueueMode>,
    follow_up_mode: Mutex<QueueMode>,
    tool_execution: Mutex<ToolExecutionMode>,
    session_id: Mutex<Option<String>>,
    thinking_budgets: Mutex<Option<loop_ai::ThinkingBudgets>>,
    steering_queue: Arc<Mutex<VecDeque<AgentMessage>>>,
    follow_up_queue: Arc<Mutex<VecDeque<AgentMessage>>>,
    subscribers: Mutex<Vec<Subscriber>>,
    cancel: Mutex<Option<CancellationToken>>,
    idle: Notify,
    busy: Mutex<bool>,
    hooks: Mutex<AgentHooks>,
}

impl Agent {
    /// Create a new agent.
    pub fn new(options: AgentOptions) -> Self {
        let convert_to_llm = options.convert_to_llm.unwrap_or_else(|| {
            Arc::new(|messages| {
                Box::pin(async move { crate::types::default_convert_to_llm(&messages) })
            })
        });
        Self {
            state: Arc::new(RwLock::new(options.initial_state)),
            stream_fn: options.stream_fn,
            convert_to_llm,
            transform_context: options.transform_context,
            steering_mode: Mutex::new(options.steering_mode),
            follow_up_mode: Mutex::new(options.follow_up_mode),
            tool_execution: Mutex::new(options.tool_execution),
            session_id: Mutex::new(options.session_id),
            thinking_budgets: Mutex::new(options.thinking_budgets),
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            subscribers: Mutex::new(Vec::new()),
            cancel: Mutex::new(None),
            idle: Notify::new(),
            busy: Mutex::new(false),
            hooks: Mutex::new(AgentHooks {
                before_tool_call: None,
                after_tool_call: None,
            }),
        }
    }

    /// Snapshot of current state.
    pub async fn state(&self) -> AgentState {
        self.state.read().await.clone()
    }

    /// Update system prompt.
    pub async fn set_system_prompt(&self, prompt: impl Into<String>) {
        self.state.write().await.system_prompt = prompt.into();
    }

    /// Update model.
    pub async fn set_model(&self, model: loop_ai::Model) {
        self.state.write().await.model = model;
    }

    /// Update thinking level.
    pub async fn set_thinking_level(&self, level: AgentThinkingLevel) {
        self.state.write().await.thinking_level = level;
    }

    /// Update tools (copies top-level vec).
    pub async fn set_tools(&self, tools: Vec<AgentTool>) {
        self.state.write().await.set_tools(tools);
    }

    /// Update messages (copies top-level vec).
    pub async fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.state.write().await.set_messages(messages);
    }

    /// Set tool execution mode.
    pub fn set_tool_execution(&self, mode: ToolExecutionMode) {
        *self.tool_execution.lock() = mode;
    }

    /// Set session id.
    pub fn set_session_id(&self, id: Option<String>) {
        *self.session_id.lock() = id;
    }

    /// Set before-tool-call hook.
    pub fn set_before_tool_call(&self, hook: Option<BeforeHook>) {
        self.hooks.lock().before_tool_call = hook;
    }

    /// Set after-tool-call hook.
    pub fn set_after_tool_call(&self, hook: Option<AfterHook>) {
        self.hooks.lock().after_tool_call = hook;
    }

    /// Subscribe to events (awaited in registration order).
    pub fn subscribe<F, Fut>(&self, handler: F) -> usize
    where
        F: Fn(AgentEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler: Subscriber = Arc::new(move |event| Box::pin(handler(event)));
        self.subscribers.lock().push(handler);
        self.subscribers.lock().len() - 1
    }

    /// Remove a subscriber by index returned from [`subscribe`](Self::subscribe).
    pub fn unsubscribe(&self, index: usize) {
        let mut subs = self.subscribers.lock();
        if index < subs.len() {
            subs.remove(index);
        }
    }

    /// Wait until the agent is idle (including awaited agent_end listeners).
    pub async fn wait_for_idle(&self) {
        loop {
            if !*self.busy.lock() {
                return;
            }
            self.idle.notified().await;
        }
    }

    /// Abort the current operation.
    pub fn abort(&self) {
        if let Some(token) = self.cancel.lock().as_ref() {
            token.cancel();
        }
    }

    /// Reset messages and queues.
    pub async fn reset(&self) {
        self.abort();
        self.clear_all_queues();
        let mut state = self.state.write().await;
        state.set_messages(Vec::new());
        state.error_message = None;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
    }

    /// Steer with a message while tools are running.
    pub fn steer(&self, message: AgentMessage) {
        self.steering_queue.lock().push_back(message);
    }

    /// Queue a follow-up after the agent would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up_queue.lock().push_back(message);
    }

    /// Clear steering queue.
    pub fn clear_steering_queue(&self) {
        self.steering_queue.lock().clear();
    }

    /// Clear follow-up queue.
    pub fn clear_follow_up_queue(&self) {
        self.follow_up_queue.lock().clear();
    }

    /// Clear all queues.
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// Whether any queue has messages.
    pub fn has_queued_messages(&self) -> bool {
        !self.steering_queue.lock().is_empty() || !self.follow_up_queue.lock().is_empty()
    }

    /// Prompt the agent.
    pub async fn prompt(&self, input: impl Into<PromptInput>) -> Result<(), AgentError> {
        if *self.busy.lock() {
            return Err(AgentError::Busy);
        }
        let prompts = Self::prompt_to_messages(input.into());
        self.run(Some(prompts)).await
    }

    /// Continue from current context.
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        if *self.busy.lock() {
            return Err(AgentError::Busy);
        }
        {
            let state = self.state.read().await;
            if let Some(last) = state.messages().last() {
                if last.role() == "assistant" && !self.has_queued_messages() {
                    return Err(AgentError::CannotContinue(
                        "last message is assistant".into(),
                    ));
                }
            } else {
                return Err(AgentError::CannotContinue("no messages".into()));
            }
        }
        self.run(None).await
    }

    fn prompt_to_messages(input: PromptInput) -> Vec<AgentMessage> {
        match input {
            PromptInput::Text(text) => vec![AgentMessage::user_text(text)],
            PromptInput::TextWithImages { text, images } => {
                vec![user_message_with_images(text, images)]
            }
            PromptInput::Message(m) => vec![m],
            PromptInput::Messages(ms) => ms,
        }
    }

    async fn run(&self, prompts: Option<Vec<AgentMessage>>) -> Result<(), AgentError> {
        *self.busy.lock() = true;
        let token = CancellationToken::new();
        *self.cancel.lock() = Some(token.clone());

        let state_snapshot = self.state.read().await.clone();
        let mut config = AgentLoopConfig::new(state_snapshot.model.clone());
        config.convert_to_llm = Arc::clone(&self.convert_to_llm);
        config.transform_context = self.transform_context.clone();
        config.tool_execution = *self.tool_execution.lock();
        config.thinking_budgets = self.thinking_budgets.lock().clone();
        config.stream_options.reasoning = state_snapshot.thinking_level.to_reasoning();
        if let Some(sid) = self.session_id.lock().clone() {
            config.stream_options.base.session_id = Some(sid);
        }
        {
            let hooks = self.hooks.lock();
            config.before_tool_call = hooks.before_tool_call.clone();
            config.after_tool_call = hooks.after_tool_call.clone();
        }

        let steering_mode = *self.steering_mode.lock();
        let follow_up_mode = *self.follow_up_mode.lock();
        let steering_q = Arc::clone(&self.steering_queue);
        let follow_q = Arc::clone(&self.follow_up_queue);

        config.get_steering_messages = Some({
            let q = Arc::clone(&steering_q);
            Arc::new(move || {
                let q = Arc::clone(&q);
                Box::pin(async move {
                    let mut guard = q.lock();
                    match steering_mode {
                        QueueMode::All => guard.drain(..).collect(),
                        QueueMode::OneAtATime => guard.pop_front().into_iter().collect(),
                    }
                })
            })
        });

        config.get_follow_up_messages = Some({
            let q = Arc::clone(&follow_q);
            Arc::new(move || {
                let q = Arc::clone(&q);
                Box::pin(async move {
                    let mut guard = q.lock();
                    match follow_up_mode {
                        QueueMode::All => guard.drain(..).collect(),
                        QueueMode::OneAtATime => guard.pop_front().into_iter().collect(),
                    }
                })
            })
        });

        let context = AgentContext {
            system_prompt: state_snapshot.system_prompt.clone(),
            messages: state_snapshot.messages().to_vec(),
            tools: Some(state_snapshot.tools().to_vec()),
        };

        let state = Arc::clone(&self.state);
        let subscribers = self.subscribers.lock().clone();

        let emit: AgentEventSink = Arc::new(move |event| {
            let state = Arc::clone(&state);
            let subscribers = subscribers.clone();
            Box::pin(async move {
                {
                    let mut s = state.write().await;
                    match &event {
                        AgentEvent::MessageUpdate { message, .. } => {
                            s.streaming_message = Some(message.clone());
                            if let Some(last) = s.messages_mut().last_mut() {
                                if last.role() == "assistant" {
                                    *last = message.clone();
                                }
                            }
                        }
                        AgentEvent::MessageEnd { message } => {
                            s.streaming_message = None;
                            match message.role() {
                                "assistant" => {
                                    if s.messages()
                                        .last()
                                        .map(|m| m.role() == "assistant")
                                        .unwrap_or(false)
                                    {
                                        *s.messages_mut().last_mut().unwrap() = message.clone();
                                    } else {
                                        s.messages_mut().push(message.clone());
                                    }
                                    if let Some(a) = message.as_assistant() {
                                        if matches!(
                                            a.stop_reason,
                                            loop_ai::StopReason::Error
                                                | loop_ai::StopReason::Aborted
                                        ) {
                                            s.error_message = a.error_message.clone();
                                        }
                                    }
                                }
                                "user" | "toolResult" => {
                                    let already = s.messages().iter().any(|m| m == message);
                                    if !already {
                                        s.messages_mut().push(message.clone());
                                    }
                                }
                                _ => {
                                    s.messages_mut().push(message.clone());
                                }
                            }
                        }
                        AgentEvent::MessageStart { message }
                            if matches!(message.role(), "user" | "toolResult") =>
                        {
                            let already = s.messages().iter().any(|m| m == message);
                            if !already {
                                s.messages_mut().push(message.clone());
                            }
                        }
                        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                            s.pending_tool_calls.insert(tool_call_id.clone());
                        }
                        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                            s.pending_tool_calls.remove(tool_call_id);
                        }
                        _ => {}
                    }
                }

                for sub in &subscribers {
                    sub(event.clone()).await;
                }
            })
        });

        let result = if let Some(prompts) = prompts {
            {
                let mut s = self.state.write().await;
                s.is_streaming = true;
            }
            let ctx = AgentContext {
                system_prompt: state_snapshot.system_prompt.clone(),
                messages: state_snapshot.messages().to_vec(),
                tools: Some(state_snapshot.tools().to_vec()),
            };
            run_agent_loop(
                prompts,
                ctx,
                config,
                emit,
                Some(token),
                Some(Arc::clone(&self.stream_fn)),
            )
            .await
            .map(|_| ())
            .map_err(|e| AgentError::Loop(e.to_string()))
        } else {
            {
                let mut s = self.state.write().await;
                s.is_streaming = true;
            }
            run_agent_loop_continue(
                context,
                config,
                emit,
                Some(token),
                Some(Arc::clone(&self.stream_fn)),
            )
            .await
            .map(|_| ())
            .map_err(|e| AgentError::Loop(e.to_string()))
        };

        {
            let mut s = self.state.write().await;
            s.is_streaming = false;
            s.streaming_message = None;
            s.pending_tool_calls.clear();
        }
        *self.busy.lock() = false;
        *self.cancel.lock() = None;
        self.idle.notify_waiters();
        result
    }
}

/// Agent errors.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Agent is busy.
    #[error("agent is busy")]
    Busy,
    /// Cannot continue.
    #[error("cannot continue: {0}")]
    CannotContinue(String),
    /// Loop error.
    #[error("agent loop error: {0}")]
    Loop(String),
}
