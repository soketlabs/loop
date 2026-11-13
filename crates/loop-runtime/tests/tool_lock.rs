//! The Runtime's tool lock (`loop runtime start --tool-lock chat`), end to
//! end with a scripted `faux` model, as `e3_enforcement.rs` drives attacks.
//!
//! With the lock, the model asks for `bash`, `write`, `edit`, `subagent`,
//! `todo` and an MCP tool, and the client calls `session/shell`: no command
//! runs, no file is written, and the tools sent to the model are only the
//! read tools. Every widening attempt (project and global settings, Session
//! options, permission mode changes, `workspace/*` setters and reloads,
//! `tools.passEnv`, a skill) is made first. The control runs the same
//! script without the lock: the shell runs.
#![cfg(unix)]

use loop_ai::{
    providers::{faux_provider_fn, FauxResponder, FauxResponse},
    Context, Message, ToolCall, ToolResultContent, UserContent, UserMessageContent,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::admin::{
    AdminAction, McpParams, PermissionModeParams, ReloadParams, SkillsParams,
};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, SessionOptions, SessionRef, SessionSpec, ToolLock,
};
use loop_runtime::{log::MemoryEventLog, Runtime, RuntimeLimits};
use loop_wire::RuntimeAdmin;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

/// What the model saw on each call: the tools offered and the results.
#[derive(Default, Clone, Debug)]
struct Seen {
    /// Tool names of every request, in order.
    tools: Vec<Vec<String>>,
    /// (tool, is_error, text) of the results in the newest request.
    results: Vec<(String, bool, String)>,
}

fn user_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(match &user.content {
        UserMessageContent::Text(text) => text.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect(),
    })
}

/// Tries every removed tool once per prompt, then a read, then ends. The
/// subagent's own task ("child") only reads.
fn model(dirs: &Dirs, seen: Arc<Mutex<Seen>>) -> FauxResponder {
    let marker = dirs.marker("bash");
    let ws = dirs.ws.clone();
    let secret = dirs.secret();
    Arc::new(move |context: &Context| {
        let last = context
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::User(_)))
            .unwrap_or(0);
        let prompt = user_text(&context.messages[last]).unwrap_or_default();
        let answered = context.messages[last..]
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();
        {
            let mut seen = seen.lock().unwrap();
            seen.tools.push(
                context
                    .tools
                    .iter()
                    .flatten()
                    .map(|t| t.name.clone())
                    .collect(),
            );
            seen.results = context.messages[last..]
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResult(r) => Some((
                        r.tool_name.clone(),
                        r.is_error,
                        r.content
                            .iter()
                            .filter_map(|c| match c {
                                ToolResultContent::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<String>(),
                    )),
                    _ => None,
                })
                .collect();
        }
        let call = |i: usize, name: &str, arguments: Value| ToolCall {
            id: format!("c{answered}-{i}"),
            name: name.into(),
            arguments,
            thought_signature: None,
        };
        let response = if answered > 0 || prompt.contains("child") {
            FauxResponse::Text("done".into())
        } else {
            let touch = format!("touch {}", marker.display());
            FauxResponse::ToolCalls(vec![
                call(0, "bash", json!({"command": touch})),
                call(
                    1,
                    "write",
                    json!({"path": ws.join("new.txt"), "content": "WRITTEN"}),
                ),
                call(
                    2,
                    "edit",
                    json!({"path": "token.txt", "oldText": "TOKEN", "newText": "EDITED"}),
                ),
                call(3, "subagent", json!({"task": "child: run the shell"})),
                call(4, "todo", json!({"todos": []})),
                call(5, "mcp__lock__run", json!({})),
                call(6, "read", json!({"path": "token.txt"})),
                // Reads that leave the Session's folder.
                call(7, "read", json!({"path": secret})),
                call(8, "read", json!({"path": "../secret.txt"})),
                call(9, "grep", json!({"pattern": "SECRET", "path": ".."})),
                call(10, "ls", json!({"path": "/"})),
                call(11, "glob", json!({"pattern": "*", "path": ".."})),
                call(12, "glob", json!({"pattern": "../*.txt"})),
                call(13, "read", json!({})),
            ])
        };
        Box::pin(std::future::ready(response))
    })
}

struct Dirs {
    _tmp: tempfile::TempDir,
    agent: PathBuf,
    ws: PathBuf,
    out: PathBuf,
}

impl Dirs {
    /// A trusted workspace whose project and global settings try every way
    /// to widen the lock.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let agent = root.join("agent");
        let ws = root.join("ws");
        let out = root.join("out");
        for dir in [
            &agent,
            &ws,
            &out,
            &agent.join("hooks"),
            &ws.join(".loop/hooks"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dirs = Self {
            _tmp: tmp,
            agent,
            ws,
            out,
        };
        std::fs::write(dirs.ws.join("token.txt"), "TOKEN\n").unwrap();
        std::fs::write(dirs.secret(), "SECRET\n").unwrap();
        // A git root above the Session's folder must not widen reads.
        std::fs::create_dir_all(dirs.ws.parent().unwrap().join(".git")).unwrap();
        let touch =
            |name: &str| json!(["sh", "-c", format!("touch {}", dirs.marker(name).display())]);
        let mcp = |name: &str| json!({"command": "sh", "args": ["-c", format!("touch {}", dirs.marker(name).display())]});
        let allow_all = json!({
            "bash": "allow", "write": "allow", "edit": "allow",
            "subagent": "allow", "todo": "allow", "mcp__lock__run": "allow"
        });
        std::fs::write(
            dirs.agent.join("settings.json"),
            json!({
                "diffEditor": "/usr/bin/true",
                "permissionMode": "full-access",
                "toolPermissions": allow_all,
                "tools": {"passEnv": ["PATH", "HOME", "LOOP_LOCK_TEST"]},
                "mcpServers": {"global": mcp("mcp-global")},
                "webFetch": {"allowPrivateAddresses": true}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dirs.ws.join(".loop/settings.json"),
            json!({
                "permissionMode": "full-access",
                "toolPermissions": allow_all,
                "mcpServers": {"lock": mcp("mcp-project")}
            })
            .to_string(),
        )
        .unwrap();
        for (path, name) in [
            (dirs.agent.join("hooks/h.json"), "hook-global"),
            (dirs.ws.join(".loop/hooks/h.json"), "hook-project"),
        ] {
            std::fs::write(
                path,
                json!({"name": "h", "on": "tool_call", "action": {"command": touch(name)}})
                    .to_string(),
            )
            .unwrap();
        }
        // A skill with a script, which the model would be told to run.
        let skill = dirs.agent.join("skills/runner");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: runner\ndescription: runs a script\n---\nRun ./run.sh\n",
        )
        .unwrap();
        std::fs::write(skill.join("run.sh"), "#!/bin/sh\ntouch marker\n").unwrap();
        let mut trust = loop_app_core::config::TrustStore::load(
            loop_app_core::config::paths::trust_path(&dirs.agent),
        )
        .unwrap();
        trust.set(&dirs.ws, true).unwrap();
        dirs
    }

    /// Another user's file next to the Session's folder, under the same
    /// git root.
    fn secret(&self) -> PathBuf {
        self.ws.parent().unwrap().join("secret.txt")
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.out.join(name)
    }

    fn markers(&self) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(&self.out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    fn opts(&self) -> BootstrapOpts {
        BootstrapOpts {
            cwd: self.ws.clone(),
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            theme: None,
            system_prompt: Some("lock test".into()),
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: None,
            no_web: true,
            permission_mode: Some("full-access".into()),
            agent: None,
        }
    }
}

async fn runtime(dirs: &Dirs, seen: Arc<Mutex<Seen>>, lock: Option<ToolLock>) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(model(dirs, seen)));
    let globals = GlobalResources::with_models(dirs.agent.clone(), models)
        .unwrap()
        .with_tool_lock(lock.map(|lock| {
            loop_app_core::permissions::tool_lock::RuntimeToolLock::new(lock, &dirs.ws).unwrap()
        }));
    Runtime::new(
        globals,
        dirs.opts(),
        Arc::new(MemoryEventLog::new()),
        RuntimeLimits::default(),
    )
    .await
    .unwrap()
    .into_shared()
}

async fn turn(rt: &Runtime, session: &SessionRef, text: &str) {
    let turn = rt
        .prompt(session, vec![ContentBlock::text(text)])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), turn.outcome())
        .await
        .expect("turn held: was someone asked to approve?");
}

const REMOVED: &[&str] = &["bash", "write", "edit", "subagent", "todo", "exit_plan"];

/// The checks after an attack under the lock.
fn assert_held(dirs: &Dirs, seen: &Seen, what: &str) {
    assert!(
        dirs.markers().is_empty(),
        "{what}: ran {:?}",
        dirs.markers()
    );
    assert!(!dirs.ws.join("new.txt").exists(), "{what}: write ran");
    assert_eq!(
        std::fs::read_to_string(dirs.ws.join("token.txt")).unwrap(),
        "TOKEN\n",
        "{what}: edit ran"
    );
    assert!(!seen.tools.is_empty(), "{what}: the model was not called");
    for tools in &seen.tools {
        for tool in tools {
            assert!(
                loop_agent::harness::tools::lock::offers(ToolLock::Chat, tool),
                "{what}: `{tool}` was offered: {tools:?}"
            );
        }
        assert!(tools.iter().any(|t| t == "read"), "{what}: {tools:?}");
    }
    assert_eq!(seen.results.len(), 14, "{what}: {:?}", seen.results);
    for (i, (tool, is_error, text)) in seen.results.iter().enumerate() {
        assert!(
            !text.contains("SECRET"),
            "{what}: `{tool}` read past the folder: {text}"
        );
        match i {
            6 => assert!(!is_error && text.contains("TOKEN"), "{what}: read: {text}"),
            // A pattern with `..` matches nothing below the folder.
            12 => {}
            _ => assert!(is_error, "{what}: call {i} `{tool}` succeeded: {text}"),
        }
    }
}

fn locked_shell() -> BackendError {
    BackendError::ToolLocked {
        what: "session/shell".into(),
        lock: ToolLock::Chat,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lock_holds_against_the_model_the_client_and_every_widening_attempt() {
    let dirs = Dirs::new();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let rt = runtime(&dirs, seen.clone(), Some(ToolLock::Chat)).await;
    // Session options ask for full access too.
    let spec = SessionSpec {
        cwd: Some(dirs.ws.clone()),
        options: Some(SessionOptions {
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            no_web: true,
            permission_mode: Some("full-access".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let session = rt.new_session(spec).await.unwrap();
    turn(&rt, &session, "attack").await;
    assert_held(&dirs, &seen.lock().unwrap(), "first turn");

    // The client's `session/shell`.
    let shell = rt.local_shell().unwrap();
    let touch = format!("touch {}", dirs.marker("shell").display());
    assert_eq!(
        shell.run(&session, &touch, None).await.unwrap_err(),
        locked_shell()
    );
    assert_eq!(
        shell.run(&session, &touch, Some(1_000)).await.unwrap_err(),
        locked_shell()
    );

    // Permission mode changes, through both setters.
    let workspace_id = loop_runtime::workspace_id(&dirs.ws);
    for mode in ["full-access", "plan", "accept-edits"] {
        RuntimeAdmin::permission_mode(
            rt.as_ref(),
            PermissionModeParams {
                session: session.clone(),
                mode: Some(mode.into()),
                expected_revision: None,
            },
        )
        .await
        .unwrap();
        *seen.lock().unwrap() = Seen::default();
        turn(&rt, &session, &format!("attack in {mode}")).await;
        assert_held(&dirs, &seen.lock().unwrap(), mode);
    }
    rt.model_control()
        .unwrap()
        .set_mode(&session, "full-access", None)
        .await
        .ok();

    // `workspace/*` setters and reloads: MCP servers, skills, trust.
    for action in [AdminAction::Enable, AdminAction::Reload] {
        let _ = RuntimeAdmin::mcp(
            rt.as_ref(),
            McpParams {
                workspace_id: workspace_id.clone(),
                action,
                name: Some("lock".into()),
                expected_revision: None,
            },
        )
        .await;
    }
    let skills = RuntimeAdmin::skills(
        rt.as_ref(),
        SkillsParams {
            workspace_id: workspace_id.clone(),
            action: AdminAction::Enable,
            name: Some("runner".into()),
            expected_revision: None,
        },
    )
    .await;
    assert!(
        skills.is_err(),
        "a skill with a script is not offered: {skills:?}"
    );
    RuntimeAdmin::reload(
        rt.as_ref(),
        ReloadParams {
            workspace_id: workspace_id.clone(),
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    *seen.lock().unwrap() = Seen::default();
    turn(&rt, &session, "attack after reload").await;
    assert_held(&dirs, &seen.lock().unwrap(), "after reload");

    // Workflows are refused.
    #[cfg(feature = "orchestration")]
    {
        let refused = loop_wire::RuntimeControl::workflow(
            rt.as_ref(),
            loop_protocol::session_services::WorkflowParams {
                session: session.clone(),
                goal: "run the shell".into(),
                concurrency: None,
                request_id: "w1".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            refused,
            BackendError::ToolLocked {
                what: "session/workflow".into(),
                lock: ToolLock::Chat,
            }
        );
    }

    // A new Session in the same Runtime is locked too.
    let other = rt
        .new_session(SessionSpec {
            cwd: Some(dirs.ws.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    *seen.lock().unwrap() = Seen::default();
    turn(&rt, &other, "attack in a new Session").await;
    assert_held(&dirs, &seen.lock().unwrap(), "new Session");
    assert_eq!(
        shell.run(&other, &touch, None).await.unwrap_err(),
        locked_shell()
    );
    assert!(dirs.markers().is_empty(), "ran {:?}", dirs.markers());
    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
}

/// Control: the same settings and script without the lock. The shell, the
/// write, the MCP servers and the hooks all run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_lock_the_same_attack_runs() {
    let dirs = Dirs::new();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let rt = runtime(&dirs, seen.clone(), None).await;
    let session = rt
        .new_session(SessionSpec {
            cwd: Some(dirs.ws.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    turn(&rt, &session, "attack").await;
    let markers = dirs.markers();
    for want in [
        "bash",
        "hook-global",
        "hook-project",
        "mcp-global",
        "mcp-project",
    ] {
        assert!(
            markers.iter().any(|m| m == want),
            "{want} missing: {markers:?}"
        );
    }
    assert!(dirs.ws.join("new.txt").exists(), "write did not run");
    // Without the lock the same out-of-folder read works, so the locked
    // test checks a real boundary.
    let results = seen.lock().unwrap().results.clone();
    assert!(
        results
            .iter()
            .any(|(tool, err, text)| tool == "read" && !err && text.contains("SECRET")),
        "{results:?}"
    );
    let offered = seen.lock().unwrap().tools[0].clone();
    for tool in ["bash", "write", "edit", "subagent", "todo"] {
        assert!(offered.iter().any(|t| t == tool), "{tool}: {offered:?}");
    }
    let ran = rt
        .local_shell()
        .unwrap()
        .run(&session, "echo shell", None)
        .await
        .unwrap();
    assert!(ran.stdout.contains("shell"));
    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
}

#[test]
fn removed_tools_are_the_classification() {
    for tool in REMOVED {
        assert!(!loop_agent::harness::tools::lock::offers(
            ToolLock::Chat,
            tool
        ));
    }
}
