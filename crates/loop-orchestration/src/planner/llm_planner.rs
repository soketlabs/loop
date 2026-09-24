//! LLM-based planner: uses an AI model to decompose goals into task graphs.

use async_trait::async_trait;
use loop_ai::{Context, Message, Model, SimpleStreamOptions};
use serde_json::Value;

use super::task_graph::{TaskConfig, TaskGraph, TaskId, TaskKind, TaskNode};
use super::{Planner, PlannerContext, PlannerError};
use crate::workflow::types::TaskResult;
use crate::StreamFn;

/// LLM-based planner that decomposes goals via structured output.
pub struct LlmPlanner {
    stream_fn: StreamFn,
    model: Model,
    options: SimpleStreamOptions,
}

impl LlmPlanner {
    /// Create a new LLM planner with the given model and stream function.
    pub fn new(stream_fn: StreamFn, model: Model) -> Self {
        Self {
            stream_fn,
            model,
            options: SimpleStreamOptions::default(),
        }
    }

    /// Set custom stream options (e.g., API key).
    pub fn with_options(mut self, options: SimpleStreamOptions) -> Self {
        self.options = options;
        self
    }

    async fn call_llm(&self, system_prompt: &str, user_prompt: &str) -> Result<String, PlannerError> {
        let context = Context {
            system_prompt: Some(system_prompt.to_string()),
            messages: vec![Message::user_text(user_prompt)],
            tools: None,
        };

        let stream = (self.stream_fn)(self.model.clone(), context, self.options.clone()).await;
        let result = stream.result().await;

        let text: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                loop_ai::AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            return Err(PlannerError::DecompositionFailed(
                "LLM returned empty response".to_string(),
            ));
        }

        Ok(text)
    }

    fn parse_task_graph(&self, json_str: &str) -> Result<TaskGraph, PlannerError> {
        let clean = if let Some(start) = json_str.find("```json") {
            let after_fence = &json_str[start + 7..];
            if let Some(end) = after_fence.find("```") {
                after_fence[..end].trim()
            } else {
                after_fence.trim()
            }
        } else if let Some(start) = json_str.find("```") {
            let after_fence = &json_str[start + 3..];
            if let Some(end) = after_fence.find("```") {
                after_fence[..end].trim()
            } else {
                after_fence.trim()
            }
        } else {
            json_str.trim()
        };

        let parsed: Value = serde_json::from_str(clean).map_err(|e| {
            PlannerError::DecompositionFailed(format!("failed to parse LLM output as JSON: {e}"))
        })?;

        self.value_to_task_graph(&parsed)
    }

    fn value_to_task_graph(&self, value: &Value) -> Result<TaskGraph, PlannerError> {
        let tasks = value
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| PlannerError::DecompositionFailed("missing 'tasks' array".to_string()))?;

        let mut graph = TaskGraph::new();

        for task_val in tasks {
            let id = task_val
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PlannerError::DecompositionFailed("task missing 'id'".to_string()))?
                .to_string();

            let description = task_val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let kind_str = task_val
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("agent_turn");

            let kind = match kind_str {
                "agent_turn" => {
                    let prompt = task_val
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&description)
                        .to_string();
                    let tools: Option<Vec<String>> = task_val
                        .get("tools")
                        .and_then(|v| serde_json::from_value(v.clone()).ok());
                    let model = task_val.get("model").and_then(|v| v.as_str()).map(String::from);
                    TaskKind::AgentTurn { prompt, tools, model }
                }
                "shell_command" => {
                    let command = task_val
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    TaskKind::ShellCommand { command }
                }
                "barrier" => TaskKind::Barrier,
                _ => TaskKind::Custom {
                    worker_type: kind_str.to_string(),
                    params: task_val.get("params").cloned().unwrap_or(Value::Null),
                },
            };

            let config = TaskConfig {
                max_retries: task_val
                    .get("max_retries")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2) as u32,
                timeout_ms: task_val
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                priority: task_val
                    .get("priority")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32,
            };

            let node = TaskNode::new(id.clone(), kind, description).with_config(config);
            graph.add_task(node);

            if let Some(deps) = task_val.get("depends_on").and_then(|v| v.as_array()) {
                for dep in deps {
                    if let Some(dep_id) = dep.as_str() {
                        graph.add_dependency(&id, dep_id);
                    }
                }
            }
        }

        graph
            .validate()
            .map_err(PlannerError::InvalidPlan)?;

        Ok(graph)
    }
}

#[async_trait]
impl Planner for LlmPlanner {
    async fn decompose(
        &self,
        goal: &str,
        context: &PlannerContext,
    ) -> Result<TaskGraph, PlannerError> {
        let system_prompt = build_decompose_system_prompt(context);
        let user_prompt = format!(
            "Decompose the following goal into a task graph:\n\n{goal}\n\n\
             Return a JSON object with a 'tasks' array. Each task should have:\n\
             - id: unique string identifier\n\
             - description: what the task does\n\
             - kind: one of 'agent_turn', 'shell_command', 'barrier'\n\
             - prompt: (for agent_turn) the instruction for the agent\n\
             - command: (for shell_command) the shell command\n\
             - depends_on: array of task IDs this depends on (optional)\n\
             - tools: array of tool names to use (optional, for agent_turn)\n\n\
             Tasks with no dependencies will run in parallel. Use depends_on to express ordering.\n\
             Return ONLY valid JSON."
        );

        let response = self.call_llm(&system_prompt, &user_prompt).await?;
        self.parse_task_graph(&response)
    }

    async fn replan(
        &self,
        current_graph: &TaskGraph,
        failed_task: &TaskId,
        feedback: &TaskResult,
        context: &PlannerContext,
    ) -> Result<TaskGraph, PlannerError> {
        let system_prompt = build_decompose_system_prompt(context);

        let current_tasks: Vec<String> = current_graph
            .tasks
            .values()
            .map(|t| format!("  - {}: {}", t.id, t.description))
            .collect();

        let user_prompt = format!(
            "A task in the workflow has failed. Please replan.\n\n\
             Failed task ID: {failed_task}\n\
             Feedback: {}\n\n\
             Current task graph:\n{}\n\n\
             Generate a revised task graph that accounts for the failure. \
             You can retry the failed task with modifications, skip it, or add alternative tasks.\n\
             Return ONLY valid JSON with the same format as before.",
            serde_json::to_string(&feedback.output).unwrap_or_default(),
            current_tasks.join("\n")
        );

        let response = self.call_llm(&system_prompt, &user_prompt).await?;
        self.parse_task_graph(&response)
    }
}

fn build_decompose_system_prompt(context: &PlannerContext) -> String {
    let mut prompt = String::from(
        "You are a task planner that decomposes high-level goals into concrete, \
         executable task graphs. Each task should be atomic and clearly scoped.\n\n\
         Guidelines:\n\
         - Independent tasks should NOT have dependencies (they run in parallel)\n\
         - Use 'barrier' tasks as sync points when multiple tasks must complete before proceeding\n\
         - Keep tasks focused: each agent_turn should have a single clear objective\n\
         - Use shell_command for deterministic operations (build, test, lint)\n\
         - Use agent_turn for tasks requiring reasoning or code generation\n\
         - Every agent_turn must produce a concrete result: a text summary in the final message, \
           and any requested files via the write tool\n\
         - When the goal asks to save a document or task graph, put that in an agent_turn prompt \
           that explicitly instructs the agent to write the file\n",
    );

    if !context.available_tools.is_empty() {
        prompt.push_str(&format!(
            "\nAvailable tools: {}\n",
            context.available_tools.join(", ")
        ));
    }

    if !context.available_models.is_empty() {
        prompt.push_str(&format!(
            "\nAvailable models: {}\n",
            context.available_models.join(", ")
        ));
    }

    if let Some(cwd) = &context.cwd {
        prompt.push_str(&format!("\nWorking directory: {cwd}\n"));
    }

    prompt
}
