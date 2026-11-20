//! The toolset of a `/patch` or `/audit` run.
//!
//! File access and the shell are Loop's own `read`, `write`, `edit` and
//! `bash`, bound to the run's container ([`crate::env::SandboxEnv`]). The
//! workflow adds what Loop has no tool for: preparing the run, listing and
//! searching inside the container, the analyzers, the diff, submissions and
//! grading. `list_directory` and `search_code` stay separate from Loop's
//! `ls` and `grep` because those read the host filesystem, which must not
//! happen on a tree the sandboxed code can change.

use std::sync::Arc;
use std::time::Instant;

use loop_agent::harness::{create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, ExecutionEnv};
use loop_agent::types::{AgentTool, AgentToolResult};
use serde_json::{json, Value};

use crate::env::SandboxEnv;
use crate::session::Session;

/// Which workflow a toolset serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workflow {
    /// `/patch`.
    Patch,
    /// `/audit`.
    Audit,
}

impl Workflow {
    /// The tree the container sees, which Loop's tools are rooted at.
    pub fn root(self) -> &'static str {
        match self {
            Workflow::Patch => "/work",
            Workflow::Audit => "/opt/audit/source",
        }
    }

    /// Loop's own tools the workflow uses, bound to its container.
    pub fn builtins(self) -> &'static [&'static str] {
        match self {
            Workflow::Patch => &["read", "write", "edit", "bash"],
            Workflow::Audit => &["read"],
        }
    }
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

fn specs(workflow: Workflow) -> Vec<(&'static str, &'static str, Value)> {
    let s = json!({"type": "string"});
    let i = json!({"type": "integer"});
    let a = json!({"type": "array", "items": {"type": "string"}});
    let common = [
        ("list_directory", "List a directory of the source inside the sandbox.", schema(json!({"path": s}), &[])),
        (
            "search_code",
            "Regex search across the source inside the sandbox (use this, not grep).",
            schema(json!({"pattern": s, "path": s, "max_results": i}), &["pattern"]),
        ),
    ];
    match workflow {
        Workflow::Patch => {
            let mut v = vec![
                (
                    "prepare_task",
                    "Set up a task and its podman sandbox. `target` is a CVE id (resolve it, fetch the vulnerable \
                     source, confirm the bug reproduces), a GitHub repo URL (optionally .../tree/<ref>) or a path to \
                     a local folder (copied into the sandbox; the original is never modified). Prints a report per \
                     stage. After it, read, write, edit and bash act inside the sandbox on /work.",
                    schema(json!({"target": s, "ref": s}), &["target"]),
                ),
                ("get_task", "Public facts about the task: the vulnerability or repository, language, build and test commands.", schema(json!({}), &[])),
            ];
            v.extend(common);
            v.extend([
                (
                    "run_analyzer",
                    "Static analyzers over the workspace (gosec | go_vet | govulncheck | bandit | npm_audit | all); \
                     normalized leads to confirm or dismiss.",
                    schema(json!({"tool": s}), &[]),
                ),
                ("show_diff", "Your changes versus the original source.", schema(json!({}), &[])),
                (
                    "submit_report",
                    "Submit the root-cause report (before grade_patch).",
                    schema(
                        json!({"summary": s, "root_cause": s, "affected_files": a, "vulnerable_lines": s,
                               "fix_description": s, "verification": s,
                               "repro_command": {"type": "string", "description":
                                   "Non-Go repo targets: shell command that runs ONLY your reproduction test (non-zero exit = vulnerable)."},
                               "test_command": {"type": "string", "description":
                                   "Optional: override the project's test-suite command."}}),
                        &["summary", "root_cause", "affected_files", "vulnerable_lines", "fix_description", "verification"],
                    ),
                ),
                (
                    "grade_patch",
                    "Stage 6: the trusted grader rebuilds your patch in a fresh container and runs the security tests \
                     (hidden ones for a CVE; your own reproduction test for a repo) plus the existing tests. Prints the \
                     verdict report.",
                    schema(json!({}), &[]),
                ),
                ("finish_task", "Write the final report and stop the sandbox (use if giving up).", schema(json!({}), &[])),
            ]);
            v
        }
        Workflow::Audit => {
            let mut v = vec![
                (
                    "prepare_audit",
                    "Stages 0-3 for a GitHub repo URL: clone at a pinned commit, build the audit sandbox, run the \
                     baseline static scan. Prints a report per stage. After it, read acts inside the sandbox.",
                    schema(json!({"url": s, "ref": s}), &["url"]),
                ),
                ("get_task", "Public facts about the repository under audit.", schema(json!({}), &[])),
            ];
            v.extend(common);
            v.extend([
                (
                    "run_analyzer",
                    "Run a static analyzer (gosec | go_vet | govulncheck | bandit | all); normalized findings.",
                    schema(json!({"tool": s}), &[]),
                ),
                ("run_go", "Run `go build|test|vet|list <args>` in the sandbox (no network).", schema(json!({"args": s}), &["args"])),
                (
                    "submit_finding",
                    "Record ONE finding: title, severity, cwe, file, start_line, end_line, description, data_flow, \
                     reachability (exploitability), impact, recommendation.",
                    schema(
                        json!({"title": s, "severity": s, "cwe": s, "file": s, "start_line": i, "end_line": i,
                               "description": s, "data_flow": s, "reachability": s, "impact": s,
                               "recommendation": s, "evidence": s}),
                        &["title", "severity", "cwe", "file", "start_line", "end_line", "description", "data_flow",
                          "reachability", "impact", "recommendation"],
                    ),
                ),
                (
                    "submit_audit_report",
                    "Record the overview (summary, scope, methodology, overall_risk).",
                    schema(json!({"summary": s, "scope": s, "methodology": s, "overall_risk": s}), &["summary", "scope", "methodology", "overall_risk"]),
                ),
                ("finish_audit", "Corroborate the findings and write the final report.", schema(json!({}), &[])),
            ]);
            v
        }
    }
}

/// Wrap one of Loop's tools so its calls land in the run's tool log.
fn logged(tool: AgentTool, session: &Arc<Session>) -> AgentTool {
    let inner = Arc::clone(&tool.execute);
    let session = Arc::clone(session);
    let name = tool.name.clone();
    AgentTool {
        execute: Arc::new(move |id, args, cancel, on_update| {
            let (inner, session, name) = (Arc::clone(&inner), Arc::clone(&session), name.clone());
            Box::pin(async move {
                let t0 = Instant::now();
                let logged_args = args.clone();
                let out = inner(id, args, cancel, on_update).await;
                let summary = match &out {
                    Ok(r) => Ok(serde_json::to_value(&r.content).unwrap_or_default()),
                    Err(e) => Err(e.clone()),
                };
                let secs = t0.elapsed().as_secs_f64();
                let _ = tokio::task::spawn_blocking(move || session.log_builtin(&name, &logged_args, &summary, secs)).await;
                out
            })
        }),
        ..tool
    }
}

/// The toolset for `workflow`, all backed by `session`.
pub fn tools(session: &Arc<Session>, workflow: Workflow) -> Vec<AgentTool> {
    let env: Arc<dyn ExecutionEnv> = Arc::new(SandboxEnv::new(Arc::clone(session), workflow.root()));
    let mut out: Vec<AgentTool> = workflow
        .builtins()
        .iter()
        .map(|name| match *name {
            "read" => create_read_tool(Arc::clone(&env)),
            "write" => create_write_tool(Arc::clone(&env)),
            "edit" => create_edit_tool(Arc::clone(&env)),
            _ => create_bash_tool(Arc::clone(&env)),
        })
        .map(|t| logged(t, session))
        .collect();
    out.extend(specs(workflow).into_iter().map(|(name, description, parameters)| {
        let session = Arc::clone(session);
        AgentTool::simple(name, name, description, parameters, move |_id, args, _cancel, _update| {
            let session = Arc::clone(&session);
            async move {
                let (text, is_error) = tokio::task::spawn_blocking(move || session.call(name, &args))
                    .await
                    .map_err(|e| format!("tool failed: {e}"))?;
                if is_error {
                    Err(text)
                } else {
                    Ok(AgentToolResult::text(text))
                }
            }
        })
    }));
    out
}

/// Every tool name of `workflow`: Loop's own first, then the workflow's.
pub fn names(workflow: Workflow) -> Vec<String> {
    workflow
        .builtins()
        .iter()
        .map(|n| (*n).to_owned())
        .chain(specs(workflow).into_iter().map(|(n, _, _)| n.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolsets_reuse_loop_tools_and_add_their_own() {
        let patch = names(Workflow::Patch);
        for n in ["read", "write", "edit", "bash", "prepare_task", "grade_patch", "search_code"] {
            assert!(patch.contains(&n.to_owned()), "{n}");
        }
        for gone in ["read_file", "write_file", "edit_file", "run_command", "run_go", "submit_finding"] {
            assert!(!patch.contains(&gone.to_owned()), "{gone}");
        }
        let audit = names(Workflow::Audit);
        assert!(audit.contains(&"finish_audit".to_owned()) && audit.contains(&"read".to_owned()));
        for writes in ["write", "edit", "bash"] {
            assert!(!audit.contains(&writes.to_owned()), "audit is read-only: {writes}");
        }
        let unique: std::collections::BTreeSet<_> = patch.iter().collect();
        assert_eq!(unique.len(), patch.len());
    }

    #[tokio::test]
    async fn tools_before_prepare_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(Session::new(dir.path().into(), dir.path().into()));
        let tools = tools(&session, Workflow::Patch);
        let find = |n: &str| tools.iter().find(|t| t.name == n).unwrap().clone();
        let err = (find("list_directory").execute)("1".into(), json!({}), None, None).await.unwrap_err();
        assert!(err.contains("prepare_task"), "{err}");
        let read = (find("read").execute)("2".into(), json!({"path": "a.txt"}), None, None).await;
        let text = match read {
            Ok(r) => serde_json::to_string(&r.content).unwrap(),
            Err(e) => e,
        };
        assert!(text.contains("prepare_task"), "{text}");
    }
}
