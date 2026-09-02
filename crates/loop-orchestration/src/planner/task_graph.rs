//! Task graph: DAG of tasks with typed edges.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::workflow::types::TaskResult;

/// Unique task identifier.
pub type TaskId = String;

/// A directed acyclic graph of tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraph {
    /// All task nodes keyed by ID.
    pub tasks: HashMap<TaskId, TaskNode>,
    /// Edges between tasks (dependencies and data flow).
    pub edges: Vec<TaskEdge>,
}

impl TaskGraph {
    /// Create an empty task graph.
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            edges: Vec::new(),
        }
    }

    /// Add a task node to the graph.
    pub fn add_task(&mut self, node: TaskNode) {
        self.tasks.insert(node.id.clone(), node);
    }

    /// Add a dependency edge: `from` depends on `to` (to must complete before from starts).
    pub fn add_dependency(&mut self, from: impl Into<TaskId>, to: impl Into<TaskId>) {
        self.edges.push(TaskEdge::DependsOn {
            from: from.into(),
            to: to.into(),
        });
    }

    /// Add a data flow edge: output of `from` flows into `to` under `key`.
    pub fn add_data_flow(
        &mut self,
        from: impl Into<TaskId>,
        to: impl Into<TaskId>,
        key: impl Into<String>,
    ) {
        self.edges.push(TaskEdge::DataFlow {
            from: from.into(),
            to: to.into(),
            key: key.into(),
        });
    }

    /// Get all task IDs that a given task depends on.
    pub fn dependencies_of(&self, task_id: &str) -> Vec<TaskId> {
        self.edges
            .iter()
            .filter_map(|edge| match edge {
                TaskEdge::DependsOn { from, to } if from == task_id => Some(to.clone()),
                TaskEdge::DataFlow { to, from, .. } if to == task_id => Some(from.clone()),
                _ => None,
            })
            .collect()
    }

    /// Get all task IDs that depend on a given task.
    pub fn dependents_of(&self, task_id: &str) -> Vec<TaskId> {
        self.edges
            .iter()
            .filter_map(|edge| match edge {
                TaskEdge::DependsOn { from, to } if to == task_id => Some(from.clone()),
                TaskEdge::DataFlow { from, to, .. } if from == task_id => Some(to.clone()),
                _ => None,
            })
            .collect()
    }

    /// Validate the graph: no cycles, all edge references exist.
    pub fn validate(&self) -> Result<(), String> {
        for edge in &self.edges {
            let (from, to) = match edge {
                TaskEdge::DependsOn { from, to } => (from, to),
                TaskEdge::DataFlow { from, to, .. } => (from, to),
            };
            if !self.tasks.contains_key(from) {
                return Err(format!("edge references unknown task: {from}"));
            }
            if !self.tasks.contains_key(to) {
                return Err(format!("edge references unknown task: {to}"));
            }
        }
        // Cycle detection via topological sort (Kahn's algorithm)
        let mut in_degree: HashMap<&str, usize> =
            self.tasks.keys().map(|k| (k.as_str(), 0)).collect();
        for edge in &self.edges {
            let from = match edge {
                TaskEdge::DependsOn { from, .. } => from.as_str(),
                TaskEdge::DataFlow { to, .. } => to.as_str(),
            };
            *in_degree.entry(from).or_insert(0) += 1;
        }

        let mut queue: Vec<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&k, _)| k)
            .collect();
        let mut visited = 0;

        while let Some(node) = queue.pop() {
            visited += 1;
            for edge in &self.edges {
                let (dep, dependent) = match edge {
                    TaskEdge::DependsOn { from, to } => (to.as_str(), from.as_str()),
                    TaskEdge::DataFlow { from, to, .. } => (from.as_str(), to.as_str()),
                };
                if dep == node {
                    let deg = in_degree.get_mut(dependent).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(dependent);
                    }
                }
            }
        }

        if visited != self.tasks.len() {
            return Err("task graph contains a cycle".to_string());
        }
        Ok(())
    }
}

impl Default for TaskGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// A single task in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    /// Unique task identifier.
    pub id: TaskId,
    /// What kind of work this task performs.
    pub kind: TaskKind,
    /// Human-readable description.
    pub description: String,
    /// Execution configuration.
    #[serde(default)]
    pub config: TaskConfig,
}

impl TaskNode {
    /// Create a new task node.
    pub fn new(id: impl Into<TaskId>, kind: TaskKind, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            description: description.into(),
            config: TaskConfig::default(),
        }
    }

    /// Set the task configuration.
    pub fn with_config(mut self, config: TaskConfig) -> Self {
        self.config = config;
        self
    }
}

/// What kind of work a task performs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskKind {
    /// Run an LLM agent turn with a prompt.
    AgentTurn {
        /// Prompt text for the agent.
        prompt: String,
        /// Optional tool name filter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        /// Optional model override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Execute a shell command.
    ShellCommand {
        /// The command to execute.
        command: String,
    },
    /// A nested workflow.
    SubWorkflow {
        /// Nested task graph.
        plan: Box<TaskGraph>,
    },
    /// Synchronization barrier: waits for all parent tasks.
    Barrier,
    /// Application-defined task type.
    Custom {
        /// Worker type identifier.
        worker_type: String,
        /// Parameters for the worker.
        params: Value,
    },
}

/// Edge types in the task graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "edge_type", rename_all = "snake_case")]
pub enum TaskEdge {
    /// `from` cannot start until `to` completes.
    DependsOn {
        /// The dependent task.
        from: TaskId,
        /// The dependency.
        to: TaskId,
    },
    /// Output of `from` flows to `to` under the given key.
    DataFlow {
        /// Source task.
        from: TaskId,
        /// Destination task.
        to: TaskId,
        /// Data flow key.
        key: String,
    },
}

/// Configuration for task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    /// Maximum retry count on failure.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Timeout in milliseconds (0 = no timeout).
    #[serde(default)]
    pub timeout_ms: u64,
    /// Priority (higher = scheduled sooner among ready tasks).
    #[serde(default)]
    pub priority: i32,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            timeout_ms: 0,
            priority: 0,
        }
    }
}

fn default_max_retries() -> u32 {
    2
}

/// Runtime status of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not yet ready to execute.
    Pending,
    /// All dependencies met; awaiting dispatch.
    Ready,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Completed(TaskResult),
    /// Execution failed.
    Failed(String),
    /// Cancelled before or during execution.
    Cancelled(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_task(id: &str, prompt: &str) -> TaskNode {
        TaskNode::new(
            id,
            TaskKind::AgentTurn {
                prompt: prompt.to_string(),
                tools: None,
                model: None,
            },
            format!("Task {id}"),
        )
    }

    #[test]
    fn empty_graph_is_valid() {
        let g = TaskGraph::new();
        assert!(g.validate().is_ok());
        assert!(g.tasks.is_empty());
    }

    #[test]
    fn single_task_graph() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "do something"));
        assert!(g.validate().is_ok());
        assert_eq!(g.tasks.len(), 1);
    }

    #[test]
    fn linear_dependency_chain() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "first"));
        g.add_task(agent_task("b", "second"));
        g.add_task(agent_task("c", "third"));
        g.add_dependency("b", "a");
        g.add_dependency("c", "b");
        assert!(g.validate().is_ok());
        assert_eq!(g.dependencies_of("b"), vec!["a"]);
        assert_eq!(g.dependencies_of("c"), vec!["b"]);
        assert!(g.dependencies_of("a").is_empty());
    }

    #[test]
    fn diamond_dependency_graph() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("root", "start"));
        g.add_task(agent_task("left", "left branch"));
        g.add_task(agent_task("right", "right branch"));
        g.add_task(agent_task("join", "merge"));
        g.add_dependency("left", "root");
        g.add_dependency("right", "root");
        g.add_dependency("join", "left");
        g.add_dependency("join", "right");
        assert!(g.validate().is_ok());

        let join_deps = g.dependencies_of("join");
        assert!(join_deps.contains(&"left".to_string()));
        assert!(join_deps.contains(&"right".to_string()));
    }

    #[test]
    fn cycle_detection() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "first"));
        g.add_task(agent_task("b", "second"));
        g.add_dependency("a", "b");
        g.add_dependency("b", "a");
        let err = g.validate().unwrap_err();
        assert!(err.contains("cycle"), "expected cycle error, got: {err}");
    }

    #[test]
    fn self_cycle_detection() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "self"));
        g.add_dependency("a", "a");
        assert!(g.validate().is_err());
    }

    #[test]
    fn unknown_edge_reference_detected() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "exists"));
        g.add_dependency("a", "ghost");
        let err = g.validate().unwrap_err();
        assert!(err.contains("unknown task"));
    }

    #[test]
    fn dependents_of() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "root"));
        g.add_task(agent_task("b", "child1"));
        g.add_task(agent_task("c", "child2"));
        g.add_dependency("b", "a");
        g.add_dependency("c", "a");
        let dependents = g.dependents_of("a");
        assert!(dependents.contains(&"b".to_string()));
        assert!(dependents.contains(&"c".to_string()));
    }

    #[test]
    fn data_flow_edges() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("producer", "produce"));
        g.add_task(agent_task("consumer", "consume"));
        g.add_data_flow("producer", "consumer", "result");
        assert!(g.validate().is_ok());
        assert_eq!(g.dependencies_of("consumer"), vec!["producer"]);
        assert_eq!(g.dependents_of("producer"), vec!["consumer"]);
    }

    #[test]
    fn task_config_defaults() {
        let config = TaskConfig::default();
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.timeout_ms, 0);
        assert_eq!(config.priority, 0);
    }

    #[test]
    fn task_node_with_config() {
        let node = TaskNode::new("t1", TaskKind::Barrier, "sync")
            .with_config(TaskConfig { max_retries: 5, timeout_ms: 30000, priority: 10 });
        assert_eq!(node.config.max_retries, 5);
        assert_eq!(node.config.timeout_ms, 30000);
        assert_eq!(node.config.priority, 10);
    }

    #[test]
    fn task_graph_serde_roundtrip() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", "first"));
        g.add_task(TaskNode::new(
            "b",
            TaskKind::ShellCommand { command: "echo hi".to_string() },
            "shell task",
        ));
        g.add_dependency("b", "a");

        let json = serde_json::to_string(&g).unwrap();
        let g2: TaskGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(g2.tasks.len(), 2);
        assert_eq!(g2.edges.len(), 1);
        assert!(g2.validate().is_ok());
    }

    #[test]
    fn three_node_cycle() {
        let mut g = TaskGraph::new();
        g.add_task(agent_task("a", ""));
        g.add_task(agent_task("b", ""));
        g.add_task(agent_task("c", ""));
        g.add_dependency("b", "a");
        g.add_dependency("c", "b");
        g.add_dependency("a", "c");
        assert!(g.validate().is_err());
    }
}
