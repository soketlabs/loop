//! Imperative builder for constructing task graphs programmatically.

use async_trait::async_trait;

use super::task_graph::{TaskConfig, TaskGraph, TaskId, TaskKind, TaskNode};
use super::{Planner, PlannerContext, PlannerError};
use crate::workflow::types::TaskResult;

/// Builder for constructing task graphs without an LLM.
/// Useful for tests, scripts, and predefined multi-agent workflows.
pub struct ManualPlanner {
    graph: TaskGraph,
    next_id: u32,
}

impl ManualPlanner {
    /// Create a new empty manual planner.
    pub fn new() -> Self {
        Self {
            graph: TaskGraph::new(),
            next_id: 1,
        }
    }

    fn gen_id(&mut self) -> TaskId {
        let id = format!("task_{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Add an agent turn task with a prompt.
    pub fn add_agent_turn(
        &mut self,
        description: impl Into<String>,
        prompt: impl Into<String>,
    ) -> TaskId {
        let id = self.gen_id();
        let node = TaskNode::new(
            id.clone(),
            TaskKind::AgentTurn {
                prompt: prompt.into(),
                tools: None,
                model: None,
            },
            description,
        );
        self.graph.add_task(node);
        id
    }

    /// Add an agent turn with specific tools, model, and configuration.
    pub fn add_agent_turn_with_config(
        &mut self,
        description: impl Into<String>,
        prompt: impl Into<String>,
        tools: Option<Vec<String>>,
        model: Option<String>,
        config: TaskConfig,
    ) -> TaskId {
        let id = self.gen_id();
        let node = TaskNode::new(
            id.clone(),
            TaskKind::AgentTurn {
                prompt: prompt.into(),
                tools,
                model,
            },
            description,
        )
        .with_config(config);
        self.graph.add_task(node);
        id
    }

    /// Add a shell command task.
    pub fn add_shell_command(
        &mut self,
        description: impl Into<String>,
        command: impl Into<String>,
    ) -> TaskId {
        let id = self.gen_id();
        let node = TaskNode::new(
            id.clone(),
            TaskKind::ShellCommand {
                command: command.into(),
            },
            description,
        );
        self.graph.add_task(node);
        id
    }

    /// Add a barrier task (waits for all dependencies).
    pub fn add_barrier(&mut self, description: impl Into<String>) -> TaskId {
        let id = self.gen_id();
        let node = TaskNode::new(id.clone(), TaskKind::Barrier, description);
        self.graph.add_task(node);
        id
    }

    /// Add a custom task.
    pub fn add_custom(
        &mut self,
        description: impl Into<String>,
        worker_type: impl Into<String>,
        params: serde_json::Value,
    ) -> TaskId {
        let id = self.gen_id();
        let node = TaskNode::new(
            id.clone(),
            TaskKind::Custom {
                worker_type: worker_type.into(),
                params,
            },
            description,
        );
        self.graph.add_task(node);
        id
    }

    /// Add a task node with a specific ID.
    pub fn add_task_with_id(
        &mut self,
        id: impl Into<TaskId>,
        kind: TaskKind,
        description: impl Into<String>,
    ) -> TaskId {
        let id = id.into();
        let node = TaskNode::new(id.clone(), kind, description);
        self.graph.add_task(node);
        id
    }

    /// Declare that `from` depends on `to` (to must complete before from starts).
    pub fn depends_on(&mut self, from: &str, to: &str) -> &mut Self {
        self.graph.add_dependency(from, to);
        self
    }

    /// Declare a data flow from one task to another.
    pub fn data_flows(
        &mut self,
        from: &str,
        to: &str,
        key: impl Into<String>,
    ) -> &mut Self {
        self.graph.add_data_flow(from, to, key);
        self
    }

    /// Consume the builder and return the validated task graph.
    pub fn build(self) -> Result<TaskGraph, PlannerError> {
        self.graph.validate().map_err(PlannerError::InvalidPlan)?;
        Ok(self.graph)
    }

    /// Get a reference to the graph being built.
    pub fn graph(&self) -> &TaskGraph {
        &self.graph
    }
}

impl Default for ManualPlanner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Planner for ManualPlanner {
    async fn decompose(
        &self,
        _goal: &str,
        _context: &PlannerContext,
    ) -> Result<TaskGraph, PlannerError> {
        self.graph.validate().map_err(PlannerError::InvalidPlan)?;
        Ok(self.graph.clone())
    }

    async fn replan(
        &self,
        current_graph: &TaskGraph,
        _failed_task: &TaskId,
        _feedback: &TaskResult,
        _context: &PlannerContext,
    ) -> Result<TaskGraph, PlannerError> {
        Ok(current_graph.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_linear_workflow() {
        let mut p = ManualPlanner::new();
        let t1 = p.add_agent_turn("first task", "do step 1");
        let t2 = p.add_agent_turn("second task", "do step 2");
        p.depends_on(&t2, &t1);
        let graph = p.build().unwrap();
        assert_eq!(graph.tasks.len(), 2);
        assert_eq!(graph.dependencies_of(&t2), vec![t1]);
    }

    #[test]
    fn build_parallel_workflow() {
        let mut p = ManualPlanner::new();
        let _a = p.add_agent_turn("task a", "do a");
        let _b = p.add_shell_command("run tests", "cargo test");
        let _c = p.add_barrier("sync point");
        let graph = p.build().unwrap();
        assert_eq!(graph.tasks.len(), 3);
    }

    #[test]
    fn build_diamond_with_data_flow() {
        let mut p = ManualPlanner::new();
        let root = p.add_agent_turn("design", "design the API");
        let left = p.add_agent_turn("implement", "write code");
        let right = p.add_shell_command("scaffold", "mkdir -p src");
        let join = p.add_barrier("merge");
        p.depends_on(&left, &root);
        p.depends_on(&right, &root);
        p.depends_on(&join, &left);
        p.depends_on(&join, &right);
        p.data_flows(&root, &left, "api_spec");
        let graph = p.build().unwrap();
        assert_eq!(graph.tasks.len(), 4);
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn build_with_config() {
        let mut p = ManualPlanner::new();
        let t = p.add_agent_turn_with_config(
            "important task",
            "do it carefully",
            Some(vec!["read".into(), "write".into()]),
            Some("gpt-4".into()),
            TaskConfig { max_retries: 5, timeout_ms: 60000, priority: 10 },
        );
        let graph = p.build().unwrap();
        let node = graph.tasks.get(&t).unwrap();
        assert_eq!(node.config.max_retries, 5);
        assert_eq!(node.config.timeout_ms, 60000);
    }

    #[test]
    fn build_custom_task() {
        let mut p = ManualPlanner::new();
        let _t = p.add_custom("deploy", "kubernetes", serde_json::json!({"replicas": 3}));
        let graph = p.build().unwrap();
        assert_eq!(graph.tasks.len(), 1);
    }

    #[test]
    fn cycle_in_manual_planner_fails() {
        let mut p = ManualPlanner::new();
        let a = p.add_agent_turn("a", "a");
        let b = p.add_agent_turn("b", "b");
        p.depends_on(&a, &b);
        p.depends_on(&b, &a);
        assert!(p.build().is_err());
    }

    #[tokio::test]
    async fn decompose_returns_built_graph() {
        let mut p = ManualPlanner::new();
        p.add_agent_turn("task", "hello");
        let graph = p.decompose("any goal", &PlannerContext::default()).await.unwrap();
        assert_eq!(graph.tasks.len(), 1);
    }

    #[test]
    fn auto_id_generation() {
        let mut p = ManualPlanner::new();
        let t1 = p.add_agent_turn("a", "a");
        let t2 = p.add_agent_turn("b", "b");
        assert_eq!(t1, "task_1");
        assert_eq!(t2, "task_2");
    }

    #[test]
    fn add_task_with_explicit_id() {
        let mut p = ManualPlanner::new();
        let id = p.add_task_with_id("my_custom_id", TaskKind::Barrier, "sync");
        assert_eq!(id, "my_custom_id");
        let graph = p.build().unwrap();
        assert!(graph.tasks.contains_key("my_custom_id"));
    }
}
