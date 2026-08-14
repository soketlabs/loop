//! Agent worker: wraps `run_agent_loop` as a scheduler worker.

use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::Model;
use loop_orchestration::planner::task_graph::{TaskKind, TaskNode};
use loop_orchestration::scheduler::worker::{Worker, WorkerContext, WorkerError};
use loop_orchestration::workflow::types::{Signal, TaskResult};

use super::tools::{
    create_memory_list_tool, create_memory_read_tool, create_memory_write_tool,
};
use crate::agent_loop::run_agent_loop;
use crate::harness::types::ExecutionEnv;
use crate::messages::convert_to_llm;
use crate::stream_fn::StreamFn;
use crate::types::{
    AgentContext, AgentEvent, AgentEventSink, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult,
};

/// Worker that executes `AgentTurn` tasks by running the agent loop.
pub struct AgentWorker {
    stream_fn: StreamFn,
    #[allow(dead_code)]
    host_env: Arc<dyn ExecutionEnv>,
    base_tools: Vec<AgentTool>,
    default_model: Model,
    system_prompt: String,
}

impl AgentWorker {
    /// Create a new agent worker.
    pub fn new(
        stream_fn: StreamFn,
        host_env: Arc<dyn ExecutionEnv>,
        base_tools: Vec<AgentTool>,
        default_model: Model,
        system_prompt: String,
    ) -> Self {
        Self {
            stream_fn,
            host_env,
            base_tools,
            default_model,
            system_prompt,
        }
    }

    fn build_system_prompt(&self, task: &TaskNode) -> String {
        format!(
            "{}\n\n## Current Task\n\nTask ID: {}\nDescription: {}\n\nYou are one agent in a multi-agent workflow. \
             Use the memory tools to coordinate with other agents. \
             Write important findings to shared memory so other agents can access them.",
            self.system_prompt, task.id, task.description
        )
    }
}

#[async_trait]
impl Worker for AgentWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["agent_turn"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let TaskKind::AgentTurn {
            prompt,
            tools: tool_filter,
            model: _model_override,
        } = &task.kind
        else {
            return Err(WorkerError::UnsupportedKind(format!("{:?}", task.kind)));
        };

        let mut agent_tools: Vec<AgentTool> = if let Some(filter) = tool_filter {
            self.base_tools
                .iter()
                .filter(|t| filter.contains(&t.name))
                .cloned()
                .collect()
        } else {
            self.base_tools.clone()
        };

        agent_tools.push(create_memory_read_tool(
            Arc::clone(&ctx.shared_memory),
            Arc::clone(&ctx.task_memory),
        ));
        agent_tools.push(create_memory_write_tool(
            Arc::clone(&ctx.shared_memory),
            Arc::clone(&ctx.task_memory),
            task.id.clone(),
        ));
        agent_tools.push(create_memory_list_tool(
            Arc::clone(&ctx.shared_memory),
            Arc::clone(&ctx.task_memory),
        ));

        let mut initial_messages: Vec<AgentMessage> = Vec::new();
        if !ctx.dependency_results.is_empty() {
            let dep_summary: Vec<String> = ctx
                .dependency_results
                .iter()
                .map(|(id, result)| {
                    format!(
                        "- Task '{}': {}",
                        id,
                        serde_json::to_string(&result.output).unwrap_or_default()
                    )
                })
                .collect();
            initial_messages.push(AgentMessage::user_text(format!(
                "Context from completed dependency tasks:\n{}",
                dep_summary.join("\n")
            )));
        }

        let context = AgentContext {
            system_prompt: self.build_system_prompt(task),
            messages: initial_messages,
            tools: Some(agent_tools),
        };

        let model = self.default_model.clone();
        let mut config = AgentLoopConfig::new(model);
        config.convert_to_llm = Arc::new(|msgs| Box::pin(async move { convert_to_llm(&msgs) }));

        let signal_rx = Arc::new(tokio::sync::Mutex::new(ctx.signal_rx));
        let steer_rx = Arc::clone(&signal_rx);
        config.get_steering_messages = Some(Arc::new(move || {
            let rx = Arc::clone(&steer_rx);
            Box::pin(async move {
                let mut messages = Vec::new();
                let mut guard = rx.lock().await;
                while let Ok(signal) = guard.try_recv() {
                    if let Signal::UserSteer { message } = signal {
                        if let Ok(m) = serde_json::from_value::<AgentMessage>(message) {
                            messages.push(m);
                        }
                    }
                }
                messages
            })
        }));

        let collected_messages = Arc::new(tokio::sync::Mutex::new(Vec::<AgentMessage>::new()));
        let msgs_for_emit = Arc::clone(&collected_messages);
        let emit: AgentEventSink = Arc::new(move |event| {
            let msgs = Arc::clone(&msgs_for_emit);
            Box::pin(async move {
                if let AgentEvent::MessageEnd { message } = &event {
                    msgs.lock().await.push(message.clone());
                }
            })
        });

        let prompts = vec![AgentMessage::user_text(prompt.clone())];
        let cancel_token = ctx.cancel.clone();

        let result = run_agent_loop(
            prompts,
            context,
            config,
            emit,
            Some(cancel_token.clone()),
            Some(Arc::clone(&self.stream_fn)),
        )
        .await;

        if cancel_token.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }

        match result {
            Ok(messages) => {
                let output = extract_output_from_messages(&messages);
                let serialized_messages: Vec<serde_json::Value> = messages
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                Ok(TaskResult {
                    output,
                    artifacts: Vec::new(),
                    messages: serialized_messages,
                })
            }
            Err(e) => Err(WorkerError::ExecutionFailed(e.to_string())),
        }
    }
}

/// Worker that executes `ShellCommand` tasks.
pub struct ShellWorker {
    host_env: Arc<dyn ExecutionEnv>,
}

impl ShellWorker {
    /// Create a new shell worker.
    pub fn new(host_env: Arc<dyn ExecutionEnv>) -> Self {
        Self { host_env }
    }
}

#[async_trait]
impl Worker for ShellWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["shell_command"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let TaskKind::ShellCommand { command } = &task.kind else {
            return Err(WorkerError::UnsupportedKind(format!("{:?}", task.kind)));
        };

        use crate::harness::types::ShellExecOptions;

        let options = ShellExecOptions {
            cancel: Some(ctx.cancel.clone()),
            ..Default::default()
        };

        let output = self
            .host_env
            .exec(command, options)
            .await
            .map_err(|e| WorkerError::ExecutionFailed(e.to_string()))?;

        if ctx.cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }

        let success = output.exit_code == 0;
        let result_output = serde_json::json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "exit_code": output.exit_code,
        });

        if success {
            Ok(TaskResult::with_output(result_output))
        } else {
            Err(WorkerError::ExecutionFailed(format!(
                "command exited with code {}: {}",
                output.exit_code,
                output.stderr.chars().take(500).collect::<String>()
            )))
        }
    }
}

fn extract_output_from_messages(messages: &[AgentMessage]) -> serde_json::Value {
    let last_assistant = messages.iter().rev().find_map(|m| m.as_assistant());
    match last_assistant {
        Some(msg) => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    loop_ai::AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::Value::String(text)
        }
        None => serde_json::Value::Null,
    }
}

/// Create a `spawn_task` tool that allows an agent to dynamically add tasks.
pub fn create_spawn_task_tool(
    workflow_engine: Arc<loop_orchestration::WorkflowEngine>,
    workflow_id: String,
) -> AgentTool {
    AgentTool::simple(
        "spawn_task",
        "Spawn Task",
        "Dynamically spawn a new sub-task in the workflow. The task will be scheduled and executed by an available worker.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Description of the task to spawn"
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt for the agent (if agent_turn type)"
                },
                "kind": {
                    "type": "string",
                    "enum": ["agent_turn", "shell_command"],
                    "description": "Type of task to spawn"
                },
                "command": {
                    "type": "string",
                    "description": "Shell command (if shell_command type)"
                },
                "depends_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs this task depends on"
                }
            },
            "required": ["description", "kind"]
        }),
        move |_id, args, _cancel, _on_update| {
            let engine = Arc::clone(&workflow_engine);
            let wf_id = workflow_id.clone();
            async move {
                let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("spawned task");
                let kind_str = args.get("kind").and_then(|v| v.as_str()).unwrap_or("agent_turn");
                let depends_on: Vec<String> = args
                    .get("depends_on")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                let task_id = format!("dynamic_{}", uuid::Uuid::now_v7());

                let kind = match kind_str {
                    "agent_turn" => {
                        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or(description);
                        TaskKind::AgentTurn {
                            prompt: prompt.to_string(),
                            tools: None,
                            model: None,
                        }
                    }
                    "shell_command" => {
                        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        TaskKind::ShellCommand { command: command.to_string() }
                    }
                    _ => return Err(format!("unsupported task kind: {kind_str}")),
                };

                let task_node = loop_orchestration::planner::task_graph::TaskNode::new(
                    task_id.clone(),
                    kind,
                    description,
                );

                engine
                    .add_task(&wf_id, task_node, depends_on)
                    .await
                    .map_err(|e| e.to_string())?;

                Ok(AgentToolResult::text(format!("Spawned task: {task_id}")))
            }
        },
    )
}
