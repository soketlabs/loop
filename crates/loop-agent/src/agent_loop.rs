//! Low-level agent loop: turns, tool execution, and event emission.

use std::sync::Arc;

use futures::StreamExt;
use loop_ai::{
    now_ms, validate_tool_arguments, AssistantContent, AssistantMessage, AssistantMessageEvent,
    Context, Message, StopReason, TextContent, Tool, ToolCall, ToolResultContent,
    ToolResultMessage,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::harness::compaction::{
    context_tokens, fixed_overhead, fixed_overhead_estimate, is_context_overflow_error,
    should_compact, AutoCompactionError, CompactionTrigger,
};
use crate::stream_fn::{resolve_stream_fn, StreamFn};
use crate::telemetry::{
    preflight_span, record_tool_result, tool_span, turn_span, GenerationObserver, RunObserver,
};
use crate::types::{
    AfterToolCallContext, AgentContext, AgentEvent, AgentEventSink, AgentLoopConfig,
    AgentLoopTurnUpdate, AgentMessage, AgentTool, AgentToolResult, AgentToolUpdateCallback,
    BeforeToolCallContext, BeforeToolCallResult, CompactionRequest, LoopCompaction,
    ToolExecutionMode,
};

/// Observational event stream (Unpin).
pub type AgentEventStream = tokio_stream::wrappers::UnboundedReceiverStream<AgentEvent>;

/// Start an observational agent loop with new prompt messages.
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    cancel: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> AgentEventStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stream_fn = resolve_stream_fn(stream_fn);
    tokio::spawn(
        async move {
            let emit: AgentEventSink = Arc::new(move |event| {
                let tx = tx.clone();
                Box::pin(async move {
                    let _ = tx.send(event);
                })
            });
            let _ = run_agent_loop(prompts, context, config, emit, cancel, Some(stream_fn)).await;
        }
        .in_current_span(),
    );
    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}

/// Continue an observational loop from existing context (no new prompt).
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    cancel: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> AgentEventStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stream_fn = resolve_stream_fn(stream_fn);
    tokio::spawn(
        async move {
            let emit: AgentEventSink = Arc::new(move |event| {
                let tx = tx.clone();
                Box::pin(async move {
                    let _ = tx.send(event);
                })
            });
            let _ = run_agent_loop_continue(context, config, emit, cancel, Some(stream_fn)).await;
        }
        .in_current_span(),
    );
    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}

/// Run the agent loop with an awaited event sink.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    cancel: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    let stream_fn = resolve_stream_fn(stream_fn);
    let mut new_messages = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt,
        messages: {
            let mut m = context.messages;
            m.extend(prompts.iter().cloned());
            m
        },
        tools: context.tools,
    };

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await;
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .await;
    }

    run_loop_observed(
        &prompts,
        &mut current_context,
        &mut new_messages,
        config,
        cancel.as_ref(),
        &emit,
        &stream_fn,
    )
    .await?;
    Ok(new_messages)
}

/// Continue with an awaited sink.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    cancel: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    if context.messages.is_empty() {
        return Err(AgentLoopError::CannotContinue(
            "no messages in context".into(),
        ));
    }
    if context.messages.last().map(|m| m.role()) == Some("assistant") {
        return Err(AgentLoopError::CannotContinue(
            "cannot continue from message role: assistant".into(),
        ));
    }

    let stream_fn = resolve_stream_fn(stream_fn);
    let mut new_messages = Vec::new();
    let mut current_context = context;

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;

    let resumed_from: Vec<AgentMessage> = current_context
        .messages
        .last()
        .cloned()
        .into_iter()
        .collect();
    run_loop_observed(
        &resumed_from,
        &mut current_context,
        &mut new_messages,
        config,
        cancel.as_ref(),
        &emit,
        &stream_fn,
    )
    .await?;
    Ok(new_messages)
}

/// Errors from the agent loop control plane (not stream-encoded LLM failures).
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    /// Continue validation failed.
    #[error("cannot continue: {0}")]
    CannotContinue(String),
}

async fn emit_ev(emit: &AgentEventSink, event: AgentEvent) {
    emit(event).await;
}

/// [`run_loop`] inside a `loop.run` observation; `input` is what started the run.
async fn run_loop_observed(
    input: &[AgentMessage],
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> Result<(), AgentLoopError> {
    let run = RunObserver::start(
        &config.model,
        input,
        config.stream_options.base.session_id.as_deref(),
        &config.run_labels,
    );
    let mut options = config.stream_options.base.clone();
    options.resolve_timeouts(&config.model);
    let budget = options.timeouts.run_budget_ms.unwrap_or(0);
    let child = cancel
        .map(CancellationToken::child_token)
        .unwrap_or_default();
    let model = config.model.clone();
    let result = {
        let work = run_loop(
            current_context,
            new_messages,
            config,
            Some(&child),
            emit,
            stream_fn,
        )
        .instrument(run.span().clone());
        if budget == 0 {
            Some(work.await)
        } else {
            tokio::time::timeout(std::time::Duration::from_millis(budget), work)
                .await
                .ok()
        }
    };
    let result = match result {
        Some(result) => result,
        None => {
            child.cancel();
            let mut message = current_context
                .messages
                .last()
                .and_then(|m| m.as_assistant())
                .filter(|m| m.stop_reason == StopReason::Pending)
                .cloned()
                .unwrap_or_else(|| AssistantMessage::pending(&model));
            message.stop_reason = StopReason::Error;
            message.error_message = Some(format!("run_budget timeout after {budget}ms"));
            let message = AgentMessage::assistant(message);
            if current_context
                .messages
                .last()
                .and_then(|m| m.as_assistant())
                .is_some_and(|m| m.stop_reason == StopReason::Pending)
            {
                *current_context
                    .messages
                    .last_mut()
                    .expect("pending message") = message.clone();
            } else {
                current_context.messages.push(message.clone());
            }
            new_messages.push(message.clone());
            emit_ev(
                emit,
                AgentEvent::MessageEnd {
                    message: message.clone(),
                },
            )
            .await;
            emit_ev(
                emit,
                AgentEvent::TurnEnd {
                    message,
                    tool_results: vec![],
                },
            )
            .await;
            emit_ev(
                emit,
                AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                },
            )
            .await;
            Ok(())
        }
    };
    run.finish(new_messages);
    result
}

/// How many times one prompt may ask the model to continue after it stopped
/// on its output limit with no tool call.
const MAX_LENGTH_CONTINUATIONS: u32 = 3;

/// The user turn sent after a response stopped on the output limit.
pub const LENGTH_CONTINUE_PROMPT: &str = "Your last response stopped at the output token limit. \
Continue from where you stopped. Do not repeat what you already wrote.";

/// Whether a length stop means the context window is full, not that the
/// answer was long: asking to continue would only fail again.
fn context_full(model: &loop_ai::Model, message: &AssistantMessage) -> bool {
    message.usage.output == 0
        || loop_ai::is_context_overflow(model, message)
        || message.raw_stop_reason.as_deref() == Some("model_context_window_exceeded")
}

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> Result<(), AgentLoopError> {
    let mut first_turn = true;
    let mut turn_index = 0u64;
    let mut length_continuations = 0u32;
    let mut compaction = CompactionState {
        usage_valid_from: config.compaction.as_ref().map_or(0, |c| c.usage_valid_from),
        threshold_failed: false,
        compactions: 0,
        overhead_estimate: fixed_overhead_estimate(
            &current_context.system_prompt,
            &current_context
                .tools
                .as_ref()
                .map(|ts| ts.iter().map(|t| t.to_llm_tool()).collect::<Vec<_>>())
                .unwrap_or_default(),
        ),
    };
    let mut pending_messages = if let Some(getter) = &config.get_steering_messages {
        getter().await
    } else {
        Vec::new()
    };

    loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            turn_index += 1;
            let turn = turn_span(turn_index);
            if !first_turn {
                emit_ev(emit, AgentEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit_ev(
                        emit,
                        AgentEvent::MessageStart {
                            message: message.clone(),
                        },
                    )
                    .await;
                    emit_ev(
                        emit,
                        AgentEvent::MessageEnd {
                            message: message.clone(),
                        },
                    )
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message = request_with_compaction(
                current_context,
                &config,
                &mut compaction,
                cancel,
                emit,
                stream_fn,
            )
            .instrument(turn.clone())
            .await;
            new_messages.push(AgentMessage::assistant(message.clone()));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                emit_ev(
                    emit,
                    AgentEvent::TurnEnd {
                        message: AgentMessage::assistant(message),
                        tool_results: vec![],
                    },
                )
                .await;
                emit_ev(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(());
            }

            let tool_calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = async {
                    if message.stop_reason == StopReason::Length {
                        fail_tool_calls_from_truncated(&tool_calls, emit).await
                    } else {
                        execute_tool_calls(current_context, &message, &config, cancel, emit).await
                    }
                }
                .instrument(turn.clone())
                .await;
                tool_results = batch.messages;
                has_more_tool_calls = !batch.terminate;

                for result in &tool_results {
                    let msg = AgentMessage::tool_result(result.clone());
                    current_context.messages.push(msg.clone());
                    new_messages.push(msg);
                }
            }

            emit_ev(
                emit,
                AgentEvent::TurnEnd {
                    message: AgentMessage::assistant(message.clone()),
                    tool_results: tool_results.clone(),
                },
            )
            .await;

            let next_ctx = crate::types::ShouldStopAfterTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            };

            if let Some(prepare) = &config.prepare_next_turn {
                if let Some(AgentLoopTurnUpdate {
                    context,
                    model,
                    thinking_level,
                }) = prepare(next_ctx.clone()).await
                {
                    if let Some(ctx) = context {
                        *current_context = ctx;
                        // Usage on the replaced messages may not describe them.
                        compaction.usage_valid_from = current_context.messages.len();
                    }
                    if let Some(model) = model {
                        config.model = model;
                    }
                    if let Some(level) = thinking_level {
                        config.stream_options.reasoning = level.to_reasoning();
                    }
                }
            }

            if let Some(should_stop) = &config.should_stop_after_turn {
                if should_stop(next_ctx).await {
                    emit_ev(
                        emit,
                        AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        },
                    )
                    .await;
                    return Ok(());
                }
            }

            pending_messages = if let Some(getter) = &config.get_steering_messages {
                getter().await
            } else {
                Vec::new()
            };

            // A response cut at the output limit with no tool call would end
            // the run with a half answer: ask the model to go on, a few times.
            if message.stop_reason == StopReason::Length
                && tool_calls.is_empty()
                && pending_messages.is_empty()
                && length_continuations < MAX_LENGTH_CONTINUATIONS
                && !cancel.is_some_and(CancellationToken::is_cancelled)
                && !context_full(&config.model, &message)
            {
                length_continuations += 1;
                pending_messages.push(AgentMessage::user_text(LENGTH_CONTINUE_PROMPT));
            }
        }

        let follow_up = if let Some(getter) = &config.get_follow_up_messages {
            getter().await
        } else {
            Vec::new()
        };
        if !follow_up.is_empty() {
            length_continuations = 0;
            pending_messages = follow_up;
            continue;
        }
        break;
    }

    emit_ev(
        emit,
        AgentEvent::AgentEnd {
            messages: new_messages.clone(),
        },
    )
    .await;
    Ok(())
}

/// Automatic compaction state of one run.
struct CompactionState {
    /// Assistant usage before this context index predates a compaction.
    usage_valid_from: usize,
    /// Compaction before a request cannot help in this run (it failed, or
    /// the window is too small): later requests do not try again. An
    /// overflow still gets its one recovery.
    threshold_failed: bool,
    /// Automatic compactions in this run.
    compactions: u32,
    /// Estimated tokens of the system prompt and tool definitions.
    overhead_estimate: u64,
}

/// Most automatic compactions in one run, so a context that compaction
/// cannot shrink never pays for summaries without end.
pub const MAX_COMPACTIONS_PER_RUN: u32 = 8;

/// One model request, with automatic compaction around it:
///
/// - before the request, compact when the context is within the reserve of
///   the window (a failure is a notice; the request is still sent);
/// - when the provider refuses the request as too long, compact once and
///   send it once more. If that compaction fails, or the retry overflows
///   too, the turn ends with the error.
///
/// The response's `MessageEnd` is emitted only for the response that is
/// kept, so a refused request never reaches the Session tree.
async fn request_with_compaction(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    state: &mut CompactionState,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> AssistantMessage {
    let Some(compaction) = config.compaction.as_ref().filter(|c| c.settings.enabled) else {
        return stream_assistant_response(context, config, cancel, emit, stream_fn).await;
    };
    let window = config.model.context_window;

    // An unknown window (0) has no threshold; overflow recovery still works.
    if window > 0 && !state.threshold_failed && state.compactions < MAX_COMPACTIONS_PER_RUN {
        let tokens = context_tokens(
            &context.messages,
            state.usage_valid_from,
            &context.system_prompt,
        );
        if should_compact(tokens, window, &compaction.settings) {
            match compact_context(
                context,
                config,
                compaction,
                state,
                cancel,
                emit,
                CompactionTrigger::Threshold,
            )
            .await
            {
                Ok(()) => {}
                Err(AutoCompactionError::Cancelled) => {
                    return finish_response(
                        context,
                        aborted_response(&config.model),
                        false,
                        false,
                        emit,
                    )
                    .await;
                }
                // Nothing older to cut yet: later rounds may have some.
                Err(AutoCompactionError::NothingToCompact) => {}
                Err(error) => {
                    state.threshold_failed = true;
                    tracing::warn!(%error, "automatic compaction failed");
                    emit_ev(
                        emit,
                        AgentEvent::Progress {
                            message: format!("automatic compaction is off for this turn: {error}"),
                        },
                    )
                    .await;
                }
            }
        }
    }

    let (message, pushed, announced) =
        stream_attempt(context, config, cancel, emit, stream_fn).await;
    if !is_context_overflow_error(&config.model, &message)
        || cancel.is_some_and(CancellationToken::is_cancelled)
    {
        return finish_response(context, message, pushed, announced, emit).await;
    }

    // The refused request leaves no trace in the context.
    if pushed {
        context.messages.pop();
    }
    let failure = if state.compactions >= MAX_COMPACTIONS_PER_RUN {
        let reason = message.error_message.clone().unwrap_or_default();
        format!(
            "The context is too long for the model and this turn already compacted {MAX_COMPACTIONS_PER_RUN} times: {reason}"
        )
    } else {
        match compact_context(
            context,
            config,
            compaction,
            state,
            cancel,
            emit,
            CompactionTrigger::Overflow,
        )
        .await
        {
            Ok(()) => {
                let (retry, pushed, announced) =
                    stream_attempt(context, config, cancel, emit, stream_fn).await;
                if !is_context_overflow_error(&config.model, &retry) {
                    return finish_response(context, retry, pushed, announced, emit).await;
                }
                let reason = retry.error_message.clone().unwrap_or_default();
                if pushed {
                    context.messages.pop();
                }
                format!(
                    "The context is still too long for the model after automatic compaction: {reason}"
                )
            }
            Err(AutoCompactionError::Cancelled) => {
                return finish_response(
                    context,
                    aborted_response(&config.model),
                    false,
                    false,
                    emit,
                )
                .await;
            }
            Err(error) => {
                let reason = message.error_message.clone().unwrap_or_default();
                format!(
                    "The context is too long for the model and automatic compaction failed ({error}): {reason}"
                )
            }
        }
    };
    let mut failed = message;
    failed.stop_reason = StopReason::Error;
    failed.error_message = Some(failure);
    finish_response(context, failed, false, announced, emit).await
}

/// Compact `context` through the run's [`CompactContextFn`] and report it.
async fn compact_context(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    compaction: &LoopCompaction,
    state: &mut CompactionState,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    trigger: CompactionTrigger,
) -> Result<(), AutoCompactionError> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err(AutoCompactionError::Cancelled);
    }
    let overhead = fixed_overhead(
        &context.messages,
        state.usage_valid_from,
        state.overhead_estimate,
    );
    let window = config.model.context_window;
    let reserve = compaction.settings.reserve_tokens;
    if trigger == CompactionTrigger::Threshold && window > 0 && window <= reserve + overhead {
        return Err(AutoCompactionError::WindowTooSmall {
            window,
            reserve,
            overhead,
        });
    }
    let outcome = (compaction.compact)(CompactionRequest {
        messages: context.messages.clone(),
        trigger,
        model: config.model.clone(),
        cancel: cancel.cloned(),
        overhead_tokens: overhead,
        emit: Some(Arc::clone(emit)),
    })
    .await?;
    state.compactions += 1;
    context.messages = outcome.messages;
    state.usage_valid_from = context.messages.len();
    emit_ev(
        emit,
        AgentEvent::Progress {
            message: format!(
                "context compacted: ~{} to ~{} tokens",
                outcome.tokens_before + overhead,
                outcome.tokens_after + overhead
            ),
        },
    )
    .await;
    Ok(())
}

/// The response of a request that a cancel stopped before it was sent.
fn aborted_response(model: &loop_ai::Model) -> AssistantMessage {
    let mut message = AssistantMessage::pending(model);
    message.stop_reason = StopReason::Aborted;
    message.error_message = Some("aborted".into());
    message
}

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> AssistantMessage {
    let (message, pushed, announced) =
        stream_attempt(context, config, cancel, emit, stream_fn).await;
    finish_response(context, message, pushed, announced, emit).await
}

/// Record the final response: in the context (replacing its streamed
/// partial when `pushed`) and as `MessageEnd`, after a `MessageStart` unless
/// one was `announced`.
async fn finish_response(
    context: &mut AgentContext,
    final_message: AssistantMessage,
    pushed: bool,
    announced: bool,
    emit: &AgentEventSink,
) -> AssistantMessage {
    if pushed {
        if let Some(last) = context.messages.last_mut() {
            *last = AgentMessage::assistant(final_message.clone());
        }
    } else {
        context
            .messages
            .push(AgentMessage::assistant(final_message.clone()));
    }
    if !announced {
        emit_ev(
            emit,
            AgentEvent::MessageStart {
                message: AgentMessage::assistant(final_message.clone()),
            },
        )
        .await;
    }
    emit_ev(
        emit,
        AgentEvent::MessageEnd {
            message: AgentMessage::assistant(final_message.clone()),
        },
    )
    .await;
    final_message
}

/// Send one request and stream its response. Returns the final message,
/// whether its partial was pushed to the context, and whether its
/// `MessageStart` was sent. `MessageStart` waits for the first content, so a
/// request refused before any content (a context-length error) shows no
/// message; [`finish_response`] completes it.
async fn stream_attempt(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> (AssistantMessage, bool, bool) {
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(messages, cancel.cloned()).await;
    }

    let llm_messages = (config.convert_to_llm)(messages).await;
    let tools: Option<Vec<Tool>> = context
        .tools
        .as_ref()
        .map(|ts| ts.iter().map(|t| t.to_llm_tool()).collect());

    let llm_context = Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages: llm_messages,
        tools,
    };

    let mut options = config.stream_options.clone();
    if let Some(get_key) = &config.get_api_key {
        if let Some(key) = get_key(config.model.provider.clone()).await {
            options.base.api_key = Some(key);
        }
    }
    if options.thinking_budgets.is_none() {
        options.thinking_budgets = config.thinking_budgets.clone();
    }
    if let Some(c) = cancel {
        options.base.cancel = Some(c.clone());
    }

    emit_ev(
        emit,
        AgentEvent::Progress {
            message: "waiting for model…".into(),
        },
    )
    .await;

    let mut generation = GenerationObserver::start(&config.model, &llm_context, &options);
    let response = stream_fn(config.model.clone(), llm_context, options).await;
    let mut stream = response;
    let mut partial_message: Option<AssistantMessage> = None;
    let mut added_partial = false;
    let mut announced = false;

    while let Some(event) = stream.next().await {
        generation.on_event(&event);
        match &event {
            AssistantMessageEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context
                    .messages
                    .push(AgentMessage::assistant(partial.clone()));
                added_partial = true;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => {
                if let Some(started) = partial_message.as_ref().filter(|_| !announced) {
                    announced = true;
                    emit_ev(
                        emit,
                        AgentEvent::MessageStart {
                            message: AgentMessage::assistant(started.clone()),
                        },
                    )
                    .await;
                }
                if partial_message.is_some() {
                    partial_message = Some(partial.clone());
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::assistant(partial.clone());
                    }
                    emit_ev(
                        emit,
                        AgentEvent::MessageUpdate {
                            message: AgentMessage::assistant(partial.clone()),
                            assistant_message_event: event.clone(),
                        },
                    )
                    .await;
                }
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => break,
        }
    }

    let final_message = stream.result().await;
    generation.finish(&final_message);
    (final_message, added_partial, announced)
}

struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

struct FinalizedToolCall {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

async fn fail_tool_calls_from_truncated(
    tool_calls: &[ToolCall],
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages = Vec::new();
    for tool_call in tool_calls {
        emit_ev(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;
        let finalized = FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let tool_calls: Vec<ToolCall> = assistant_message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    let has_sequential = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .as_ref()
            .and_then(|tools| tools.iter().find(|t| t.name == tc.name))
            .and_then(|t| t.execution_mode)
            == Some(ToolExecutionMode::Sequential)
    });

    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            emit,
        )
        .await
    }
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls = Vec::new();
    let mut messages = Vec::new();

    for tool_call in tool_calls {
        emit_ev(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            cancel,
        )
        .await;
        let finalized = match preparation {
            PrepOutcome::Immediate { result, is_error } => FinalizedToolCall {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            PrepOutcome::Prepared {
                tool_call,
                tool,
                args,
            } => {
                let executed = execute_prepared_tool_call(
                    &tool_call,
                    &tool,
                    args.clone(),
                    cancel,
                    emit,
                    config,
                )
                .await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &tool_call,
                    &args,
                    executed,
                    config,
                    cancel,
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        finalized_calls.push(finalized);
        messages.push(tool_result_message);

        if cancel.map(|c| c.is_cancelled()).unwrap_or(false) {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    // Sequential preflight; concurrent execute; end events as each finishes;
    // toolResult messages in assistant source order.
    enum Entry {
        Done(FinalizedToolCall),
        Pending {
            tool_call: ToolCall,
            tool: AgentTool,
            args: serde_json::Value,
        },
    }

    let mut entries = Vec::new();
    for tool_call in tool_calls {
        emit_ev(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            cancel,
        )
        .await;
        match preparation {
            PrepOutcome::Immediate { result, is_error } => {
                let finalized = FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await;
                entries.push(Entry::Done(finalized));
            }
            PrepOutcome::Prepared {
                tool_call,
                tool,
                args,
            } => {
                entries.push(Entry::Pending {
                    tool_call,
                    tool,
                    args,
                });
            }
        }
        if cancel.map(|c| c.is_cancelled()).unwrap_or(false) {
            break;
        }
    }

    // Execute pending concurrently; emit end as each completes.
    let mut pending_futs = Vec::new();
    let mut index_map = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Entry::Pending {
            tool_call,
            tool,
            args,
        } = entry
        {
            index_map.push(i);
            let tool_call = tool_call.clone();
            let tool = tool.clone();
            let args = args.clone();
            let cancel = cancel.cloned();
            let emit = Arc::clone(emit);
            let current_context = current_context.clone();
            let assistant_message = assistant_message.clone();
            let config = config.clone();
            pending_futs.push(async move {
                let executed = execute_prepared_tool_call(
                    &tool_call,
                    &tool,
                    args.clone(),
                    cancel.as_ref(),
                    &emit,
                    &config,
                )
                .await;
                let finalized = finalize_executed_tool_call(
                    &current_context,
                    &assistant_message,
                    &tool_call,
                    &args,
                    executed,
                    &config,
                    cancel.as_ref(),
                )
                .await;
                emit_tool_execution_end(&finalized, &emit).await;
                finalized
            });
        }
    }

    let results = futures::future::join_all(pending_futs).await;
    let mut finalized_by_index: Vec<Option<FinalizedToolCall>> =
        entries.iter().map(|_| None).collect();
    for (entry, i) in entries.into_iter().zip(0..) {
        match entry {
            Entry::Done(f) => finalized_by_index[i] = Some(f),
            Entry::Pending { .. } => {}
        }
    }
    for (finalized, &i) in results.into_iter().zip(index_map.iter()) {
        finalized_by_index[i] = Some(finalized);
    }

    let ordered: Vec<FinalizedToolCall> = finalized_by_index.into_iter().flatten().collect();
    let mut messages = Vec::new();
    for finalized in &ordered {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&ordered),
    }
}

enum PrepOutcome {
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
    Prepared {
        tool_call: ToolCall,
        tool: AgentTool,
        args: serde_json::Value,
    },
}

/// [`prepare_tool_call_inner`] inside a `tool.preflight` observation.
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
) -> PrepOutcome {
    let span = preflight_span(tool_call);
    let outcome = prepare_tool_call_inner(
        current_context,
        assistant_message,
        tool_call,
        config,
        cancel,
    )
    .instrument(span.clone())
    .await;
    if let PrepOutcome::Immediate { result, is_error } = &outcome {
        record_tool_result(&span, result, *is_error);
    }
    outcome
}

async fn prepare_tool_call_inner(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
) -> PrepOutcome {
    let tool = current_context
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|t| t.name == tool_call.name).cloned());

    let Some(tool) = tool else {
        return PrepOutcome::Immediate {
            result: create_error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    let prepared_args = if let Some(prepare) = &tool.prepare_arguments {
        prepare(tool_call.arguments.clone())
    } else {
        tool_call.arguments.clone()
    };

    let llm_tool = tool.to_llm_tool();
    let validated = match validate_tool_arguments(&llm_tool, &prepared_args) {
        Ok(v) => v,
        Err(e) => {
            return PrepOutcome::Immediate {
                result: create_error_tool_result(e.to_string()),
                is_error: true,
            };
        }
    };

    if let Some(before) = &config.before_tool_call {
        let before_result = before(
            BeforeToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: validated.clone(),
                context: current_context.clone(),
            },
            cancel.cloned(),
        )
        .await;
        if cancel.map(|c| c.is_cancelled()).unwrap_or(false) {
            return PrepOutcome::Immediate {
                result: create_error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(BeforeToolCallResult {
            block: true,
            reason,
        }) = before_result
        {
            return PrepOutcome::Immediate {
                result: create_error_tool_result(
                    reason.unwrap_or_else(|| "Tool execution was blocked".into()),
                ),
                is_error: true,
            };
        }
    }

    if cancel.map(|c| c.is_cancelled()).unwrap_or(false) {
        return PrepOutcome::Immediate {
            result: create_error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    PrepOutcome::Prepared {
        tool_call: tool_call.clone(),
        tool,
        args: validated,
    }
}

/// The latest progress update of a running tool, waiting to be emitted.
enum UpdateSlot {
    Empty,
    Update(Box<AgentToolResult>),
    Done,
}

struct ExecutedOutcome {
    result: AgentToolResult,
    is_error: bool,
}

async fn execute_prepared_tool_call(
    tool_call: &ToolCall,
    tool: &AgentTool,
    args: serde_json::Value,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    config: &AgentLoopConfig,
) -> ExecutedOutcome {
    // Progress updates go through one slot that holds only the latest (bash
    // sends its whole output each time), emitted in order before the tool's
    // end: a task per update could land after the end and show a finished
    // call as running again, and a queue could grow behind a slow listener.
    let accepting = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let slot = Arc::new((
        parking_lot::Mutex::new(UpdateSlot::Empty),
        tokio::sync::Notify::new(),
    ));
    let forward = {
        let emit = Arc::clone(emit);
        let slot = Arc::clone(&slot);
        let tool_call_id = tool_call.id.clone();
        let tool_name = tool_call.name.clone();
        let args = tool_call.arguments.clone();
        tokio::spawn(async move {
            loop {
                let next = {
                    let mut guard = slot.0.lock();
                    match &*guard {
                        UpdateSlot::Done => UpdateSlot::Done,
                        _ => std::mem::replace(&mut *guard, UpdateSlot::Empty),
                    }
                };
                match next {
                    UpdateSlot::Empty => slot.1.notified().await,
                    // A tool may keep its callback alive: `Done` ends the wait.
                    UpdateSlot::Done => break,
                    UpdateSlot::Update(partial) => {
                        emit(AgentEvent::ToolExecutionUpdate {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: args.clone(),
                            partial_result: *partial,
                        })
                        .await;
                    }
                }
            }
        })
    };
    let accepting_cb = Arc::clone(&accepting);
    let slot_cb = Arc::clone(&slot);
    let on_update: AgentToolUpdateCallback = Arc::new(move |partial| {
        if accepting_cb.load(std::sync::atomic::Ordering::SeqCst) {
            let mut guard = slot_cb.0.lock();
            if !matches!(*guard, UpdateSlot::Done) {
                *guard = UpdateSlot::Update(Box::new(partial));
                drop(guard);
                slot_cb.1.notify_one();
            }
        }
    });

    let span = tool_span(
        tool_call,
        &args,
        config.stream_options.base.session_id.as_deref(),
        &config.run_labels,
    );
    let result = (tool.execute)(tool_call.id.clone(), args, cancel.cloned(), Some(on_update))
        .instrument(span.clone())
        .await;

    accepting.store(false, std::sync::atomic::Ordering::SeqCst);
    // The end carries the full result, so an update still waiting is dropped.
    *slot.0.lock() = UpdateSlot::Done;
    slot.1.notify_one();
    let _ = forward.await;

    let outcome = match result {
        Ok(result) => ExecutedOutcome {
            result,
            is_error: false,
        },
        Err(err) => ExecutedOutcome {
            result: create_error_tool_result(err),
            is_error: true,
        },
    };
    record_tool_result(&span, &outcome.result, outcome.is_error);
    outcome
}

async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &serde_json::Value,
    executed: ExecutedOutcome,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
) -> FinalizedToolCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after) = &config.after_tool_call {
        if let Some(patch) = after(
            AfterToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: args.clone(),
                result: result.clone(),
                is_error,
                context: current_context.clone(),
            },
            cancel.cloned(),
        )
        .await
        {
            if let Some(content) = patch.content {
                result.content = content;
            }
            if let Some(details) = patch.details {
                result.details = details;
            }
            if let Some(usage) = patch.usage {
                result.usage = Some(usage);
            }
            if let Some(terminate) = patch.terminate {
                result.terminate = Some(terminate);
            }
            if let Some(flag) = patch.is_error {
                is_error = flag;
            }
        }
    }

    FinalizedToolCall {
        tool_call: tool_call.clone(),
        result,
        is_error,
    }
}

fn should_terminate_tool_batch(finalized: &[FinalizedToolCall]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate == Some(true))
}

fn create_error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: message.into(),
            text_signature: None,
        })],
        details: serde_json::json!({}),
        usage: None,
        added_tool_names: None,
        terminate: None,
    }
}

async fn emit_tool_execution_end(finalized: &FinalizedToolCall, emit: &AgentEventSink) {
    emit_ev(
        emit,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        },
    )
    .await;
}

fn create_tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: Some(finalized.result.details.clone()),
        usage: finalized.result.usage.clone(),
        added_tool_names: finalized.result.added_tool_names.clone(),
        is_error: finalized.is_error,
        timestamp: now_ms(),
    }
}

async fn emit_tool_result_message(msg: &ToolResultMessage, emit: &AgentEventSink) {
    let agent_msg = AgentMessage::tool_result(msg.clone());
    emit_ev(
        emit,
        AgentEvent::MessageStart {
            message: agent_msg.clone(),
        },
    )
    .await;
    emit_ev(emit, AgentEvent::MessageEnd { message: agent_msg }).await;
}

/// Collect events from an observational stream into a vec (test helper).
pub async fn collect_agent_events<S>(mut stream: S) -> Vec<AgentEvent>
where
    S: futures::Stream<Item = AgentEvent> + Unpin,
{
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        let is_end = ev.is_agent_end();
        events.push(ev);
        if is_end {
            break;
        }
    }
    events
}

/// Unused import guard for Message in convert paths.
#[allow(dead_code)]
fn _msg_role(m: &Message) -> &str {
    m.role()
}
