//! End to end with real podman: `/patch` on a local folder, driven by a
//! scripted agent through the run's toolset (Loop's own `read`, `write`,
//! `edit`, `bash` bound to the container, plus the workflow's tools). Needs
//! rootless podman and network for the image build, so it is ignored:
//!
//! `cargo test -p loop-defensive --test e2e_podman -- --ignored --nocapture`

use std::fs;
use std::path::Path;
use std::sync::Arc;

use loop_agent::types::AgentTool;
use loop_defensive::{tools, Session, Workflow};
use serde_json::{json, Value};

struct Agent {
    tools: Vec<AgentTool>,
}

impl Agent {
    /// Call a tool; returns (text, is_error).
    async fn call(&self, name: &str, args: Value) -> (String, bool) {
        let tool = self.tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no tool {name}"));
        match (tool.execute)("id".into(), args, None, None).await {
            Ok(r) => {
                let text = r
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        loop_ai::ToolResultContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (text, false)
            }
            Err(e) => (e, true),
        }
    }

    async fn json(&self, name: &str, args: Value) -> (Value, bool) {
        let (text, err) = self.call(name, args).await;
        (serde_json::from_str(&text).unwrap_or(Value::String(text)), err)
    }
}

fn project(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("package.json"), r#"{"name": "e2e", "version": "1.0.0"}"#).unwrap();
    // Path traversal: "../" is not rejected.
    fs::write(
        dir.join("src/paths.js"),
        "function isSafe(p) {\n  return !p.startsWith('/');\n}\nmodule.exports = { isSafe };\n",
    )
    .unwrap();
}

const TEST: &str = "const test = require('node:test');\nconst assert = require('node:assert');\n\
                    const { isSafe } = require('../src/paths');\n\
                    test('rejects traversal', () => { assert.strictEqual(isSafe('../etc/passwd'), false); });\n";

fn report(repro: &str) -> Value {
    json!({"summary": "path traversal", "root_cause": "isSafe only rejected absolute paths",
           "affected_files": ["src/paths.js"], "vulnerable_lines": "src/paths.js:1-3",
           "fix_description": "reject .. segments", "verification": "repro fails before, passes after",
           "repro_command": repro})
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs rootless podman and network"]
async fn patch_a_local_folder_end_to_end() {
    let runs = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    project(src.path());
    let session = Arc::new(Session::new(runs.path().into(), src.path().into()));
    let agent = Agent { tools: tools::tools(&session, Workflow::Patch) };

    let (ready, err) = agent.call("prepare_task", json!({"target": src.path().to_string_lossy()})).await;
    println!("{ready}");
    assert!(!err && ready.contains("STAGE 2 SANDBOX — PASS") && ready.contains("Sandbox ready"), "{ready}");
    assert!(session.in_progress());

    let (task, _) = agent.json("get_task", json!({})).await;
    assert_eq!(task["mode"], "repo", "{task}");
    let (ls, _) = agent.json("list_directory", json!({})).await;
    assert!(ls["entries"].as_array().unwrap().iter().any(|e| e == "src/"), "{ls}");
    let (hits, _) = agent.json("search_code", json!({"pattern": "startsWith"})).await;
    assert_eq!(hits["matches"][0], "src/paths.js:2: return !p.startsWith('/');");

    // Loop's own tools act inside the container.
    let (read, err) = agent.call("read", json!({"path": "src/paths.js"})).await;
    assert!(!err && read.contains("return !p.startsWith('/');"), "{read}");
    let (pwd, _) = agent.call("bash", json!({"command": "pwd && cat /etc/os-release | head -1"})).await;
    assert!(pwd.contains("/work") && pwd.contains("Debian"), "{pwd}");
    let (net, _) = agent.call("bash", json!({"command": "getent hosts example.com || echo offline"})).await;
    assert!(net.contains("offline"), "{net}");
    // A planted symlink resolves in the container, never on the host.
    let marker = src.path().join("host-only-secret");
    fs::write(&marker, "HOST SECRET").unwrap();
    agent.call("bash", json!({"command": format!("ln -s {} leak", marker.display())})).await;
    let (leak, _) = agent.call("read", json!({"path": "leak"})).await;
    assert!(!leak.contains("HOST SECRET"), "{leak}");
    agent.call("bash", json!({"command": "rm leak"})).await;

    // Reproduce, then fix.
    let (w, err) = agent.call("write", json!({"path": "test/repro.test.js", "content": TEST})).await;
    assert!(!err, "{w}");
    let (before, _) = agent.call("bash", json!({"command": "node --test test/repro.test.js; echo exit=$?"})).await;
    assert!(before.contains("exit=1"), "{before}");
    let (e, err) = agent
        .call(
            "edit",
            json!({"path": "src/paths.js", "oldText": "return !p.startsWith('/');",
                   "newText": "return !p.startsWith('/') && !p.split('/').includes('..');"}),
        )
        .await;
    assert!(!err, "{e}");
    let (after, _) = agent.call("bash", json!({"command": "node --test test/repro.test.js; echo exit=$?"})).await;
    assert!(after.contains("exit=0"), "{after}");
    let (diff, _) = agent.json("show_diff", json!({})).await;
    assert_eq!(diff["code_files"], json!(["src/paths.js"]), "{diff}");
    assert_eq!(diff["test_files"], json!(["test/repro.test.js"]));

    let (rep, err) = agent.json("submit_report", report("node --test test/repro.test.js")).await;
    assert!(!err && rep["ok"] == true, "{rep}");
    let (graded, _) = agent.call("grade_patch", json!({})).await;
    println!("{graded}");
    assert!(graded.contains("verdict PATCH_VERIFIED") && graded.contains("STAGE 7 FINAL — PASS"), "{graded}");
    // Loop's own tools are in the stage 5 tool count.
    assert!(graded.contains("bash×") && graded.contains("edit×1"), "{graded}");
    assert!(!session.in_progress());
    let (after_end, err) = agent.call("read", json!({"path": "src/paths.js"})).await;
    assert!(err || after_end.contains("prepare_task"), "{after_end}");

    let latest = fs::read_dir(runs.path().join("repo")).unwrap().next().unwrap().unwrap().path().join("latest");
    let patch = fs::read_to_string(latest.join("attempt-1/candidate.patch")).unwrap();
    assert!(patch.contains("+++ b/src/paths.js"), "{patch}");
    let report: Value = serde_json::from_str(&fs::read_to_string(latest.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["lifecycle"], "PATCH_VERIFIED");
    // The original folder was not modified.
    assert!(fs::read_to_string(src.path().join("src/paths.js")).unwrap().contains("return !p.startsWith('/');\n"));
    assert!(!src.path().join("test").exists());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs rootless podman and network"]
async fn a_wrong_fix_is_not_verified() {
    let runs = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    project(src.path());
    let session = Arc::new(Session::new(runs.path().into(), src.path().into()));
    let agent = Agent { tools: tools::tools(&session, Workflow::Patch) };
    let (ready, _) = agent.call("prepare_task", json!({"target": src.path().to_string_lossy()})).await;
    assert!(ready.contains("Sandbox ready"), "{ready}");
    agent.call("write", json!({"path": "test/repro.test.js", "content": TEST})).await;
    agent
        .call("edit", json!({"path": "src/paths.js", "oldText": "return", "newText": "// noop\n  return"}))
        .await;
    agent.call("submit_report", report("node --test test/repro.test.js")).await;
    let (graded, _) = agent.call("grade_patch", json!({})).await;
    println!("{graded}");
    assert!(graded.contains("verdict REPRODUCIBLE"), "{graded}");
    assert!(graded.contains("Not verified: your reproduction test still fails on the patched code"), "{graded}");
    assert!(session.in_progress(), "attempts remain");
    let (finished, _) = agent.call("finish_task", json!({})).await;
    assert!(finished.contains("STAGE 7 FINAL"), "{finished}");
    assert!(!session.in_progress());
}
