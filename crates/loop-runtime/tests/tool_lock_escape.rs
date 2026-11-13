//! The tool lock keeps reads inside the Session's folder and the lock root.
//!
//! Ported from the review probe (`review/tool-lock`): the attacker controls
//! the prompts, every file in the Session's folder (symlinks and hard links
//! included, a project `.loop/settings.json`, a recorded trust) and every
//! host-local call, a Session's cwd included. Another user's folder `bob`
//! sits next to `alice` and must never be read.
#![cfg(unix)]

use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall, ToolResultContent,
};
use loop_app_core::permissions::tool_lock::RuntimeToolLock;
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::admin::{
    AdminAction, McpParams, PermissionModeParams, ReloadParams, SandboxAction, SandboxChoice,
    SandboxParams, SettingsParams, SkillsParams, TemplatesParams, TrustParams, WebSearchParams,
};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, SessionOptions, SessionRef, SessionSpec, ToolLock,
};
use loop_runtime::{log::MemoryEventLog, Runtime, RuntimeLimits};
use loop_wire::RuntimeAdmin;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Seen {
    results: Vec<(String, bool, String)>,
    system: Vec<String>,
}

struct World {
    _tmp: tempfile::TempDir,
    agent: PathBuf,
    users: PathBuf,
    alice: PathBuf,
    bob: PathBuf,
    outside: PathBuf,
}

/// Bob's secrets, and alice's folder full of ways to reach them.
fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let agent = root.join("agent");
    let users = root.join("users");
    let alice = users.join("alice");
    let bob = users.join("bob");
    let outside = root.join("outside");
    for d in [
        &agent,
        &alice.join(".loop"),
        &alice.join("sub"),
        &bob.join("notes"),
        &bob.join("skills/steal"),
        &outside,
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(bob.join("notes/secret.md"), "BOBSECRET-template\n").unwrap();
    std::fs::write(bob.join("diary.txt"), "BOBSECRET-grep\n").unwrap();
    std::fs::write(bob.join("ctx.md"), "BOBSECRET-context\n").unwrap();
    std::fs::write(bob.join("hard.txt"), "BOBSECRET-hardlink\n").unwrap();
    std::fs::write(
        bob.join("skills/steal/SKILL.md"),
        "---\nname: steal\ndescription: BOBSECRET-skill\n---\nBOBSECRET-skill\n",
    )
    .unwrap();
    // A context file in a parent folder, which a walk up would load.
    std::fs::write(users.join("AGENTS.md"), "BOBSECRET-parent\n").unwrap();
    std::fs::write(alice.join("own.txt"), "ALICE-OK\n").unwrap();
    // Attacker-controlled files in alice's folder.
    std::os::unix::fs::symlink(&bob, alice.join("shared")).unwrap();
    std::os::unix::fs::symlink(bob.join("ctx.md"), alice.join("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink(bob.join("diary.txt"), alice.join("sub/.gitignore")).unwrap();
    std::fs::hard_link(bob.join("hard.txt"), alice.join("hard.txt")).unwrap();
    std::fs::write(
        alice.join(".loop/settings.json"),
        json!({
            "prompts": [bob.join("notes")],
            "skills": [bob.join("skills")],
            "permissionMode": "full-access",
        })
        .to_string(),
    )
    .unwrap();
    let mut trust =
        loop_app_core::config::TrustStore::load(loop_app_core::config::paths::trust_path(&agent))
            .unwrap();
    trust.set(&alice, true).unwrap();
    World {
        _tmp: tmp,
        agent,
        users,
        alice,
        bob,
        outside,
    }
}

fn opts(cwd: &Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.to_path_buf(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: false,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: None,
        agent: None,
    }
}

fn spec(cwd: &Path) -> SessionSpec {
    SessionSpec {
        cwd: Some(cwd.to_path_buf()),
        options: Some(SessionOptions {
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            no_web: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Every way the model can try to read past alice's folder.
fn attacker(seen: Arc<Mutex<Seen>>) -> loop_ai::providers::FauxResponder {
    Arc::new(move |context: &Context| {
        let mut s = seen.lock().unwrap();
        s.system
            .push(context.system_prompt.clone().unwrap_or_default());
        s.results = context
            .messages
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
        let done = context
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)));
        let call = |i: usize, name: &str, arguments: serde_json::Value| ToolCall {
            id: format!("c{i}"),
            name: name.into(),
            arguments,
            thought_signature: None,
        };
        let r = if done {
            FauxResponse::Text("done".into())
        } else {
            FauxResponse::ToolCalls(vec![
                call(
                    0,
                    "grep",
                    json!({"pattern": "BOBSECRET|ALICE", "hidden": true}),
                ),
                call(1, "glob", json!({"pattern": "**/*", "hidden": true})),
                call(2, "read", json!({"path": "shared/diary.txt"})),
                call(3, "read", json!({"path": "hard.txt"})),
                call(4, "read", json!({"path": "AGENTS.md"})),
                call(5, "ls", json!({"path": "shared"})),
                call(6, "grep", json!({"pattern": "BOBSECRET", "path": "shared"})),
                call(7, "ls", json!({})),
                call(
                    8,
                    "grep",
                    json!({"pattern": "BOBSECRET", "path": "sub", "hidden": true}),
                ),
                call(9, "read", json!({"path": "diary.txt"})),
            ])
        };
        Box::pin(std::future::ready(r))
    })
}

async fn locked(w: &World, seen: Arc<Mutex<Seen>>) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(attacker(seen)));
    let lock = RuntimeToolLock::new(ToolLock::Chat, &w.alice).unwrap();
    let globals = GlobalResources::with_models(w.agent.clone(), models)
        .unwrap()
        .with_tool_lock(Some(lock));
    Runtime::new(
        globals,
        opts(&w.alice),
        Arc::new(MemoryEventLog::new()),
        RuntimeLimits::default(),
    )
    .await
    .unwrap()
    .into_shared()
}

async fn turn(rt: &Runtime, session: &SessionRef) {
    let turn = rt
        .prompt(session, vec![ContentBlock::text("go")])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), turn.outcome())
        .await
        .unwrap();
}

fn is_locked(error: &BackendError) -> bool {
    matches!(
        error,
        BackendError::ToolLocked {
            lock: ToolLock::Chat,
            ..
        }
    )
}

/// Findings 2, 3 and 4 of the review: walks through symlinks, hard links,
/// context files (a symlink and a parent folder) and project resources.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_never_leave_the_folder() {
    let w = world();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let rt = locked(&w, seen.clone()).await;
    let session = rt.new_session(spec(&w.alice)).await.unwrap();
    turn(&rt, &session).await;
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.results.len(), 10, "{:#?}", s.results);
        for (tool, _, text) in &s.results {
            assert!(!text.contains("BOBSECRET"), "`{tool}` leaked: {text}");
        }
        // The folder's own files still work.
        assert!(
            s.results[0].2.contains("ALICE-OK"),
            "grep found nothing: {:?}",
            s.results[0]
        );
        for i in [2, 3, 4, 5, 6] {
            assert!(s.results[i].1, "call {i} succeeded: {:?}", s.results[i]);
        }
        for prompt in &s.system {
            assert!(!prompt.contains("BOBSECRET"), "context leaked: {prompt}");
        }
    }
    // Templates and skills from another user's folder.
    let id = loop_runtime::workspace_id(&w.alice);
    let template = RuntimeAdmin::templates(
        rt.as_ref(),
        TemplatesParams {
            workspace_id: id.clone(),
            name: Some("secret".into()),
            args: vec![],
        },
    )
    .await;
    assert!(
        !format!("{template:?}").contains("BOBSECRET"),
        "{template:?}"
    );
    let skills = RuntimeAdmin::skills(
        rt.as_ref(),
        SkillsParams {
            workspace_id: id,
            action: AdminAction::List,
            name: None,
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    assert!(!format!("{skills:?}").contains("BOBSECRET"), "{skills:?}");
    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
}

/// Review N1: a Session folder that is admitted, then replaced by a link
/// to another user's folder, does not move the boundary. Every call of the
/// Session is refused and nothing of bob's is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_swapped_after_open_does_not_escape() {
    let w = world();
    let sub = w.alice.join("sub");
    std::fs::write(sub.join("mine.txt"), "MINE\n").unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let rt = locked(&w, seen.clone()).await;
    let session = rt.new_session(spec(&sub)).await.unwrap();
    // Through an upload: rename the folder and put a link to bob's there.
    std::fs::rename(&sub, w.alice.join("sub-old")).unwrap();
    std::os::unix::fs::symlink(&w.bob, &sub).unwrap();
    turn(&rt, &session).await;
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.results.len(), 10, "{:#?}", s.results);
        for (tool, is_error, text) in &s.results {
            assert!(!text.contains("BOBSECRET"), "`{tool}` leaked: {text}");
            assert!(is_error, "`{tool}` ran in a moved folder: {text}");
            assert!(text.contains("moved or replaced"), "`{tool}`: {text}");
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
}

/// Finding 1: no Session outside the lock root, by any entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_session_opens_outside_the_root() {
    let w = world();
    // A Session recorded in bob's folder before the lock.
    let stored = {
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(faux_provider_fn(attacker(Arc::default())));
        let globals = GlobalResources::with_models(w.agent.clone(), models).unwrap();
        let rt = Runtime::new(
            globals,
            opts(&w.alice),
            Arc::new(MemoryEventLog::new()),
            RuntimeLimits::default(),
        )
        .await
        .unwrap()
        .into_shared();
        let stored = rt.new_session(spec(&w.bob)).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
        stored
    };
    let rt = locked(&w, Arc::default()).await;
    for cwd in [&w.bob, &w.users, &w.outside, &w.alice.join("shared")] {
        let refused = rt.new_session(spec(cwd)).await.unwrap_err();
        assert!(is_locked(&refused), "{}: {refused:?}", cwd.display());
    }
    let refused = rt
        .load_session(&stored, SessionSpec::default())
        .await
        .unwrap_err();
    assert!(is_locked(&refused), "stored Session: {refused:?}");
    // Inside the root is fine, the root itself included.
    rt.new_session(spec(&w.alice)).await.unwrap();
    rt.new_session(spec(&w.alice.join("sub"))).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
}

/// Findings 5, 6 and 7, and the visible lock: trust, the sandbox and the
/// admin writes are refused; reads of admin state still work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_writes_trust_and_the_sandbox_are_refused() {
    let w = world();
    let rt = locked(&w, Arc::default()).await;
    let session = rt.new_session(spec(&w.alice)).await.unwrap();
    let id = loop_runtime::workspace_id(&w.alice);

    let trust = RuntimeAdmin::trust(
        rt.as_ref(),
        TrustParams {
            workspace_id: id.clone(),
            trusted: Some(true),
            expected_revision: None,
        },
    )
    .await
    .unwrap_err();
    assert!(is_locked(&trust), "{trust:?}");
    // A locked Runtime treats every folder as untrusted.
    let read = RuntimeAdmin::trust(
        rt.as_ref(),
        TrustParams {
            workspace_id: id.clone(),
            trusted: None,
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    assert!(!read.trusted);

    let sandbox = RuntimeAdmin::sandbox(
        rt.as_ref(),
        SandboxParams {
            session: session.clone(),
            action: SandboxAction::Set,
            config: Some(SandboxChoice {
                mode: "local".into(),
                ..Default::default()
            }),
            expected_revision: None,
        },
    )
    .await
    .unwrap_err();
    assert!(is_locked(&sandbox), "{sandbox:?}");

    for action in [AdminAction::Enable, AdminAction::Disable] {
        let mcp = RuntimeAdmin::mcp(
            rt.as_ref(),
            McpParams {
                workspace_id: id.clone(),
                action,
                name: Some("any".into()),
                expected_revision: None,
            },
        )
        .await
        .unwrap_err();
        assert!(is_locked(&mcp), "{mcp:?}");
        let skills = RuntimeAdmin::skills(
            rt.as_ref(),
            SkillsParams {
                workspace_id: id.clone(),
                action,
                name: Some("any".into()),
                expected_revision: None,
            },
        )
        .await
        .unwrap_err();
        assert!(is_locked(&skills), "{skills:?}");
    }
    let web = RuntimeAdmin::web_search(
        rt.as_ref(),
        WebSearchParams {
            workspace_id: id.clone(),
            enabled: Some(true),
            provider: None,
            expected_revision: None,
        },
    )
    .await
    .unwrap_err();
    assert!(is_locked(&web), "{web:?}");
    assert!(!w.agent.join("workspace-settings.json").exists());
    // Review N4: in-process callers are refused too, not only the wire.
    let settings_before = std::fs::read(w.agent.join("settings.json")).ok();
    let defaults = rt
        .update_defaults(loop_protocol::admin::DefaultsParams {
            patch: Some(loop_protocol::admin::DefaultsPatch {
                effort: Some("high".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(is_locked(&defaults), "{defaults:?}");
    assert_eq!(
        std::fs::read(w.agent.join("settings.json")).ok(),
        settings_before
    );

    // Reads and reloads still work.
    RuntimeAdmin::settings(
        rt.as_ref(),
        SettingsParams {
            workspace_id: id.clone(),
        },
    )
    .await
    .unwrap();
    RuntimeAdmin::reload(
        rt.as_ref(),
        ReloadParams {
            workspace_id: id.clone(),
            expected_revision: None,
        },
    )
    .await
    .unwrap();

    // The lock is visible in-process too.
    assert_eq!(rt.status().tool_lock, Some(ToolLock::Chat));
    let caps = rt.capabilities();
    assert!(!caps.local_shell && !caps.workflow);
    assert_eq!(
        RuntimeAdmin::offered(rt.as_ref()).tool_lock,
        Some(ToolLock::Chat)
    );
    let mode = RuntimeAdmin::permission_mode(
        rt.as_ref(),
        PermissionModeParams {
            session,
            mode: Some("full-access".into()),
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    assert!(
        mode.effective_policy.contains("tool lock"),
        "{}",
        mode.effective_policy
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
}
