//! Low-level agent loop: turns, tool execution, and event emission.

use std::sync::Arc;

use futures::StreamExt;
use loop_ai::{
    now_ms, validate_tool_arguments, AssistantContent, AssistantMessage, AssistantMessageEvent,
    Context, Message, StopReason, TextContent, Tool, ToolCall, ToolResultContent, ToolResultMessage,
};
use tokio_util::sync::CancellationToken;

use crate::stream_fn::{resolve_stream_fn, StreamFn};
use crate::types::{
    AgentContext, AgentEvent, AgentEventSink, AgentLoopConfig, AgentLoopTurnUpdate, AgentMessage,
    AgentTool, AgentToolResult, AgentToolUpdateCallback, BeforeToolCallContext, BeforeToolCallResult,
    AfterToolCallContext, ToolExecutionMode,
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
    tokio::spawn(async move {
        let emit: AgentEventSink = Arc::new(move |event| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(event);
            })
        });
        let _ = run_agent_loop(prompts, context, config, emit, cancel, Some(stream_fn)).await;
    });
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
    tokio::spawn(async move {
        let emit: AgentEventSink = Arc::new(move |event| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(event);
            })
        });
        let _ = run_agent_loop_continue(context, config, emit, cancel, Some(stream_fn)).await;
    });
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

    run_loop(
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
        return Err(AgentLoopError::CannotContinue("no messages in context".into()));
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

    run_loop(
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

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> Result<(), AgentLoopError> {
    let mut first_turn = true;
    let mut pending_messages = if let Some(getter) = &config.get_steering_messages {
        getter().await
    } else {
        Vec::new()
    };

    loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
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

            let message =
                stream_assistant_response(current_context, &config, cancel, emit, stream_fn).await;
            new_messages.push(AgentMessage::assistant(message.clone()));

            if matches!(
                message.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
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
                let batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated(&tool_calls, emit).await
                } else {
                    execute_tool_calls(current_context, &message, &config, cancel, emit).await
                };
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
        }

        let follow_up = if let Some(getter) = &config.get_follow_up_messages {
            getter().await
        } else {
            Vec::new()
        };
        if !follow_up.is_empty() {
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

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> AssistantMessage {
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

    let response = stream_fn(config.model.clone(), llm_context, options).await;
    let mut stream = response;
    let mut partial_message: Option<AssistantMessage> = None;
    let mut added_partial = false;

    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context
                    .messages
                    .push(AgentMessage::assistant(partial.clone()));
                added_partial = true;
                emit_ev(
                    emit,
                    AgentEvent::MessageStart {
                        message: AgentMessage::assistant(partial.clone()),
                    },
                )
                .await;
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
                if let Some(_) = &partial_message {
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
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                let final_message = stream.result().await;
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::assistant(final_message.clone());
                    }
                } else {
                    context
                        .messages
                        .push(AgentMessage::assistant(final_message.clone()));
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
                return final_message;
            }
        }
    }

    let final_message = stream.result().await;
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = AgentMessage::assistant(final_message.clone());
        }
    } else {
        context
            .messages
            .push(AgentMessage::assistant(final_message.clone()));
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

        let preparation =
            prepare_tool_call(current_context, assistant_message, tool_call, config, cancel).await;
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
                let executed =
                    execute_prepared_tool_call(&tool_call, &tool, args.clone(), cancel, emit).await;
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

        let preparation =
            prepare_tool_call(current_context, assistant_message, tool_call, config, cancel).await;
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

async fn prepare_tool_call(
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
) -> ExecutedOutcome {
    let accepting = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let accepting_cb = Arc::clone(&accepting);
    let emit_cb = Arc::clone(emit);
    let tool_call_id = tool_call.id.clone();
    let tool_name = tool_call.name.clone();
    let args_for_cb = tool_call.arguments.clone();

    let on_update: AgentToolUpdateCallback = Arc::new(move |partial| {
        if !accepting_cb.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let emit = Arc::clone(&emit_cb);
        let tool_call_id = tool_call_id.clone();
        let tool_name = tool_name.clone();
        let args = args_for_cb.clone();
        tokio::spawn(async move {
            emit(AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result: partial,
            })
            .await;
        });
    });

    let result = (tool.execute)(
        tool_call.id.clone(),
        args,
        cancel.cloned(),
        Some(on_update),
    )
    .await;

    accepting.store(false, std::sync::atomic::Ordering::SeqCst);

    match result {
        Ok(result) => ExecutedOutcome {
            result,
            is_error: false,
        },
        Err(err) => ExecutedOutcome {
            result: create_error_tool_result(err),
            is_error: true,
        },
    }
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
        match after(
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
            Some(patch) => {
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
            None => {}
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
