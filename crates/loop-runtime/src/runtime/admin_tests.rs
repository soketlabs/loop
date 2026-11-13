//! Workspace admin on a shared Runtime: reload reaches idle Sessions at
//! once and busy ones when their turn ends; toggles write the global
//! settings; permission mode is recorded in the Session tree.
use super::*;
use crate::log::{EventLog, MemoryEventLog};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall,
};
use loop_protocol::admin::*;
use loop_wire::RuntimeAdmin;

const LIMIT: Duration = Duration::from_secs(60);

/// A prompt starting `park` asks for an approval nobody answers.
async fn runtime(dir: &std::path::Path) -> (Arc<Runtime>, Arc<MemoryEventLog>) {
    runtime_with(dir, "ask", RuntimeLimits::default()).await
}

/// The same Runtime in another permission mode and with other limits. Its
/// provider offers a second model, `faux-two`. Every prompt asks for one
/// `echo parked` command and then finishes.
async fn runtime_with(
    dir: &std::path::Path,
    permission_mode: &str,
    limits: RuntimeLimits,
) -> (Arc<Runtime>, Arc<MemoryEventLog>) {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    let provider = faux_provider_fn(Arc::new(|context: &Context| {
        let asked = matches!(context.messages.last(), Some(Message::User(_)));
        let response = if asked {
            FauxResponse::ToolCalls(vec![ToolCall {
                id: format!("park{}", context.messages.len()),
                name: "bash".into(),
                arguments: json!({"command": "echo parked"}),
                thought_signature: None,
            }])
        } else {
            FauxResponse::Text("finished".into())
        };
        Box::pin(std::future::ready(response))
    }));
    let first = provider.get_model("faux-model").unwrap();
    let mut second = first.clone();
    second.id = "faux-two".into();
    second.name = "Faux Two".into();
    provider.set_models(vec![first, second]);
    models.set_provider(provider);
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let opts = BootstrapOpts {
        cwd: dir.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some(permission_mode.into()),
        agent: None,
    };
    let log = Arc::new(MemoryEventLog::new());
    let rt = {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(globals, opts, log.clone(), store, limits)
    }
    .await
    .unwrap()
    .with_close_grace(Duration::from_millis(500))
    .into_shared();
    (rt, log)
}

fn write_skill(dir: &std::path::Path, name: &str) {
    let skill = dir.join("agent").join("skills").join(name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Keeps {name}\n---\nBody\n"),
    )
    .unwrap();
}

fn workspace(dir: &std::path::Path) -> String {
    tree::workspace_id(&dir.canonicalize().unwrap())
}

/// Whether `session` is offered the test's `notes` skill.
async fn has_notes(rt: &Runtime, session: &SessionRef) -> bool {
    let live = rt.route(session).unwrap();
    let resources = live.opened.harness.get_resources().await;
    resources.skills.iter().any(|s| s.name == "notes")
}

/// Start a turn on `session` that waits on an unanswered Escalation.
async fn park(rt: &Runtime, log: &MemoryEventLog, session: &SessionRef) -> TurnHandle {
    let turn = rt
        .prompt(session, vec![ContentBlock::text("park")])
        .await
        .unwrap();
    loop {
        let raised = log.read_runtime_after(0).await.unwrap().iter().any(|e| {
            matches!(e.event.kind, EventKind::EscalationRaised(_))
                && e.event.session_key.as_ref() == Some(&session.key)
        });
        if raised {
            break turn;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn reload_reaches_idle_sessions_now_and_busy_ones_when_their_turn_ends() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path()).await;
        let idle = rt.new_session(SessionSpec::default()).await.unwrap();
        let busy = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = park(&rt, &log, &busy).await;

        // The Runtime panel sees one idle and one parked Session.
        let panel = admin(&rt).sessions().await.unwrap();
        assert_eq!(panel.totals.sessions, 2);
        assert_eq!(panel.totals.busy, 1);
        let state = |key: &SessionKey| {
            panel
                .sessions
                .iter()
                .find(|row| &row.session.key == key)
                .map(|row| row.state.clone())
        };
        assert_eq!(state(&idle.key).as_deref(), Some("idle"));
        assert_eq!(state(&busy.key).as_deref(), Some("parked"));
        assert_eq!(panel.sessions[0].workspace_id, workspace(dir.path()));

        write_skill(dir.path(), "notes");
        let reloaded = admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(reloaded.revision, 1);
        assert_eq!(reloaded.pending_sessions, 1);
        assert!(has_notes(&rt, &idle).await);
        assert!(!has_notes(&rt, &busy).await, "applied mid-turn");

        rt.cancel(&busy);
        turn.outcome().await;
        assert!(has_notes(&rt, &busy).await);
        // New Sessions open with the reloaded workspace.
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(has_notes(&rt, &fresh).await);
        // A stale revision is refused with the current one.
        let stale = admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: Some(0),
            })
            .await
            .unwrap_err();
        assert_eq!(stale, BackendError::Conflict { revision: 1 });
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn toggling_a_skill_saves_global_settings_and_reaches_sessions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "notes");
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(has_notes(&rt, &session).await);
        let id = workspace(dir.path());
        let result = rt
            .skills(SkillsParams {
                workspace_id: id.clone(),
                action: AdminAction::Disable,
                name: Some("notes".into()),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(result.revision, 1);
        assert_eq!(result.pending_sessions, 0);
        // The host's own global skills may be listed too.
        let notes: Vec<_> = result
            .skills
            .into_iter()
            .filter(|s| s.name == "notes")
            .collect();
        assert_eq!(
            notes,
            vec![SkillStatus {
                name: "notes".into(),
                description: "Keeps notes".into(),
                enabled: false,
            }]
        );
        assert!(!has_notes(&rt, &session).await);
        // Saved for this workspace on the host, not in the global defaults.
        let saved =
            std::fs::read_to_string(dir.path().join("agent/workspace-settings.json")).unwrap();
        assert!(saved.contains("notes"), "{saved}");
        let global =
            std::fs::read_to_string(dir.path().join("agent/settings.json")).unwrap_or_default();
        assert!(!global.contains("notes"), "{global}");
        assert!(rt
            .skills(SkillsParams {
                workspace_id: id.clone(),
                action: AdminAction::Enable,
                name: Some("missing".into()),
                expected_revision: None,
            })
            .await
            .is_err());
        // An id the host did not issue is refused, never read as a path.
        let path = dir.path().to_string_lossy().into_owned();
        assert!(admin(&rt)
            .reload(ReloadParams {
                workspace_id: path,
                expected_revision: None,
            })
            .await
            .is_err());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn templates_list_and_render_and_trust_reads_and_writes() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("agent/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("review.md"), "Review $1 carefully\n").unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let id = workspace(dir.path());
        let listed = rt
            .templates(TemplatesParams {
                workspace_id: id.clone(),
                name: None,
                args: vec![],
            })
            .await
            .unwrap();
        let names: Vec<_> = listed
            .templates
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["review".to_string()]);
        let rendered = rt
            .templates(TemplatesParams {
                workspace_id: id.clone(),
                name: Some("review".into()),
                args: vec!["main.rs".into()],
            })
            .await
            .unwrap();
        assert_eq!(
            rendered.text.as_deref().map(str::trim),
            Some("Review main.rs carefully")
        );

        let read = |trusted| TrustParams {
            workspace_id: id.clone(),
            trusted,
            expected_revision: None,
        };
        assert!(!rt.trust(read(None)).await.unwrap().trusted);
        let written = rt.trust(read(Some(true))).await.unwrap();
        assert!(written.trusted);
        assert_eq!(written.revision, 1);
        assert!(rt.trust(read(None)).await.unwrap().trusted);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn permission_mode_is_recorded_and_restored_on_reopen() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let params = |mode: Option<&str>, expected| PermissionModeParams {
            session: session.clone(),
            mode: mode.map(str::to_owned),
            expected_revision: expected,
        };
        let before = rt.permission_mode(params(None, None)).await.unwrap();
        assert_eq!(before.mode, "ask");
        let set = rt
            .permission_mode(params(Some("plan"), Some(before.revision)))
            .await
            .unwrap();
        assert_eq!(set.mode, "plan");
        assert_eq!(set.revision, before.revision + 1);
        assert!(set.seq.is_some());
        assert_eq!(
            rt.permission_mode(params(Some("ask"), Some(before.revision)))
                .await
                .unwrap_err(),
            BackendError::Conflict {
                revision: set.revision
            }
        );
        assert!(rt
            .permission_mode(params(Some("everything"), None))
            .await
            .is_err());

        rt.close(&session.key, false).await.unwrap();
        let reopened = rt
            .load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let after = rt
            .permission_mode(PermissionModeParams {
                session: reopened,
                mode: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(after.mode, "plan");
        assert_eq!(after.revision, set.revision);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sandbox_status_reports_off_and_a_bad_mode_is_refused() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let status = rt
            .sandbox(SandboxParams {
                session: session.clone(),
                action: SandboxAction::Status,
                config: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(status.config.mode, "off");
        assert!(!status.info.is_empty());
        let refused = rt
            .sandbox(SandboxParams {
                session: session.clone(),
                action: SandboxAction::Set,
                config: Some(SandboxChoice {
                    mode: "remote".into(),
                    ..SandboxChoice::default()
                }),
                expected_revision: None,
            })
            .await;
        assert!(refused.is_err());
        // Turning an already-off sandbox off rebuilds its tools and stays off.
        let off = rt
            .sandbox(SandboxParams {
                session,
                action: SandboxAction::Set,
                config: Some(SandboxChoice {
                    mode: "off".into(),
                    ..SandboxChoice::default()
                }),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(off.config.mode, "off");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// The admin service, past `Runtime`'s own `reload`.
fn admin(rt: &Runtime) -> &dyn RuntimeAdmin {
    rt
}

async fn tools_and_prompt(rt: &Runtime, session: &SessionRef) -> (Vec<String>, String) {
    let live = rt.route(session).unwrap();
    let mut names: Vec<String> = live
        .opened
        .harness
        .get_tools()
        .await
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    (names, live.opened.harness.system_prompt().await)
}

#[tokio::test]
async fn a_reloaded_session_matches_a_fresh_one_and_keeps_runtime_tools() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let before = tools_and_prompt(&rt, &session).await;
        assert!(
            before.0.iter().any(|n| n == subagent::TOOL),
            "{:?}",
            before.0
        );
        let snapshot = rt.snapshot(&session).await.unwrap();
        assert_eq!(snapshot.workspace_revision, 0);
        admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(tools_and_prompt(&rt, &session).await, before);
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert_eq!(tools_and_prompt(&rt, &fresh).await, before);
        // Snapshots report the workspace revision admin results use.
        assert_eq!(rt.snapshot(&session).await.unwrap().workspace_revision, 1);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

fn generation(rt: &Runtime, session: &SessionRef) -> Arc<admin::Generation> {
    let live = rt.route(session).unwrap();
    let held = live.generation.lock().unwrap().clone();
    held.expect("a published Session holds its generation")
}

#[tokio::test]
async fn a_second_reload_keeps_the_generation_a_busy_session_still_uses() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path()).await;
        let busy = rt.new_session(SessionSpec::default()).await.unwrap();
        let in_use = generation(&rt, &busy);
        let in_use_closed = Arc::clone(&in_use.released);
        drop(in_use);
        let turn = park(&rt, &log, &busy).await;
        let reload = || {
            admin(&rt).reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: None,
            })
        };
        reload().await.unwrap();
        let skipped_closed = {
            let live = rt.route(&busy).unwrap();
            let pending = live.pending_workspace.lock().unwrap();
            Arc::clone(&pending.as_ref().unwrap().released)
        };
        reload().await.unwrap();
        // The generation it was only going to switch to is closed; the one
        // its running turn uses keeps its connections.
        assert!(skipped_closed.load(Ordering::Acquire));
        assert!(!in_use_closed.load(Ordering::Acquire));

        rt.cancel(&busy);
        turn.outcome().await;
        assert!(in_use_closed.load(Ordering::Acquire));
        assert!(!generation(&rt, &busy).released.load(Ordering::Acquire));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_session_opening_across_a_reload_adopts_the_new_workspace() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join(".agents/skills/proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("SKILL.md"),
            "---\nname: proj\ndescription: Project skill\n---\nBody\n",
        )
        .unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let trust = |trusted| TrustParams {
            workspace_id: workspace(dir.path()),
            trusted: Some(trusted),
            expected_revision: None,
        };
        // Loads the workspace, trusted, with the project skill.
        let first = rt.new_session(SessionSpec::default()).await.unwrap();
        admin(&rt).trust(trust(true)).await.unwrap();
        let offered = |names: Vec<String>| names.iter().any(|n| n == "proj");
        let skills_of = |session: SessionRef| {
            let rt = rt.clone();
            async move {
                let live = rt.route(&session).unwrap();
                let names: Vec<String> = live
                    .opened
                    .harness
                    .get_resources()
                    .await
                    .skills
                    .into_iter()
                    .map(|s| s.name)
                    .collect();
                names
            }
        };
        assert!(offered(skills_of(first.clone()).await));

        // A Session opens with the trusted workspace; trust is revoked
        // before it is published.
        let (reached, release) = rt.pauses.arm("open_publishing");
        let opening = tokio::spawn({
            let rt = rt.clone();
            async move { rt.new_session(SessionSpec::default()).await }
        });
        reached.notified().await;
        let revoked = admin(&rt).trust(trust(false)).await.unwrap();
        assert_eq!(revoked.pending_sessions, 0);
        release.send(false).unwrap();
        let late = opening.await.unwrap().unwrap();
        assert!(!offered(skills_of(late.clone()).await));
        assert!(!offered(skills_of(first).await));
        let current = rt
            .workspaces
            .lock()
            .unwrap()
            .get(&dir.path().canonicalize().unwrap())
            .map(|state| Arc::clone(&state.generation))
            .unwrap();
        assert!(Arc::ptr_eq(&generation(&rt, &late), &current));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn workspace_toggles_stay_in_their_own_workspace() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "notes");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let open = |cwd: &std::path::Path| SessionSpec {
            cwd: Some(cwd.to_path_buf()),
            ..SessionSpec::default()
        };
        let in_a = rt.new_session(open(&a)).await.unwrap();
        let in_b = rt.new_session(open(&b)).await.unwrap();
        let disabled = admin(&rt)
            .skills(SkillsParams {
                workspace_id: workspace(&a),
                action: AdminAction::Disable,
                name: Some("notes".into()),
                expected_revision: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(disabled.revision, 1);
        assert!(!has_notes(&rt, &in_a).await);
        assert!(has_notes(&rt, &in_b).await);
        let b_canonical = b.canonicalize().unwrap();
        assert_eq!(rt.workspace_revision(&b_canonical), 0);
        // B reloaded, or a new Session there, still has it.
        let reloaded = admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(&b),
                expected_revision: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(reloaded.revision, 1);
        assert!(has_notes(&rt, &in_b).await);
        let again = rt.new_session(open(&a)).await.unwrap();
        assert!(!has_notes(&rt, &again).await);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reload_rereads_model_definitions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let models = |ids: &[&str]| {
            json!({"providers": [{"id": "local", "baseUrl": "http://127.0.0.1:9/v1",
                "models": ids}]})
            .to_string()
        };
        let path = dir.path().join("agent/models.json");
        std::fs::write(&path, models(&["first"])).unwrap();
        let catalog = &rt.factory.globals.models;
        assert!(catalog.get_model("local", "first").is_none());
        let reload = || {
            admin(&rt).reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: None,
            })
        };
        reload().await.unwrap();
        assert!(catalog.get_model("local", "first").is_some());
        std::fs::write(&path, models(&["second"])).unwrap();
        reload().await.unwrap();
        assert!(catalog.get_model("local", "second").is_some());
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        let offered = ModelControl::models(&*rt, &fresh).await.unwrap();
        assert!(
            offered.models.iter().any(|m| m.id == "local/second"),
            "{:?}",
            offered.models
        );
        drop(session);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn defaults_writes_are_serialized_with_each_other_and_workspace_changes() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "notes");
        let (rt, _log) = runtime(dir.path()).await;
        let _session = rt.new_session(SessionSpec::default()).await.unwrap();
        let save = |ms| {
            rt.update_defaults(DefaultsParams {
                patch: Some(DefaultsPatch {
                    response_header_timeout_ms: Some(ms),
                    ..DefaultsPatch::default()
                }),
                expected_revision: Some(0),
                ..DefaultsParams::default()
            })
        };
        let (one, two) = tokio::join!(save(11_000), save(22_000));
        let conflicts = [&one, &two]
            .iter()
            .filter(|r| matches!(r, Err(BackendError::Conflict { revision: 1 })))
            .count();
        assert_eq!(conflicts, 1, "{one:?} {two:?}");
        let saved = one.or(two).unwrap().response_header_timeout_ms.unwrap();

        // A defaults write racing a workspace toggle loses neither.
        let toggle = admin(&rt).skills(SkillsParams {
            workspace_id: workspace(dir.path()),
            action: AdminAction::Disable,
            name: Some("notes".into()),
            expected_revision: None,
        });
        let defaults = rt.update_defaults(DefaultsParams {
            patch: Some(DefaultsPatch {
                response_header_timeout_ms: Some(33_000),
                ..DefaultsPatch::default()
            }),
            ..DefaultsParams::default()
        });
        let (toggled, written) = tokio::join!(toggle, defaults);
        toggled.unwrap();
        assert_eq!(written.unwrap().revision, 2);
        let global = std::fs::read_to_string(dir.path().join("agent/settings.json")).unwrap();
        assert!(
            global.contains("33000") && !global.contains(&saved.to_string()),
            "{global}"
        );
        let overrides =
            std::fs::read_to_string(dir.path().join("agent/workspace-settings.json")).unwrap();
        assert!(overrides.contains("notes"), "{overrides}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

async fn refuses_idle_shutdown_after_time_passes(rt: &Runtime) {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!rt.is_idle().await);
    assert!(!rt.shutdown_if_idle().await);
    tokio::time::resume();
}

#[tokio::test]
async fn attached_surface_prevents_idle_shutdown() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        loop_wire::RuntimeControl::attach_snapshot_watching(rt.as_ref(), 41, &session, 0)
            .await
            .unwrap();
        refuses_idle_shutdown_after_time_passes(&rt).await;
        rt.register_watch(&session, 41, false, None).await.unwrap();
        assert!(rt.shutdown_if_idle().await);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn busy_turn_prevents_idle_shutdown() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("park")])
            .await
            .unwrap();
        refuses_idle_shutdown_after_time_passes(&rt).await;
        rt.cancel(&session);
        let _ = turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn parked_approval_prevents_idle_shutdown() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = park(&rt, &log, &session).await;
        assert!(rt.route(&session).unwrap().escalations.has_pending().await);
        refuses_idle_shutdown_after_time_passes(&rt).await;
        rt.cancel(&session);
        let _ = turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_environment_reaches_shell_without_changing_other_sessions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        rt.factory.globals.models.set_provider(faux_provider_fn(Arc::new(|context: &Context| {
            let answered = context.messages.iter().any(|m| matches!(m, Message::Assistant(_)));
            let response = if answered { FauxResponse::Text("finished".into()) } else {
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "env-tool".into(), name: "bash".into(),
                    arguments: json!({"command": "printf '%s' \"$LOOP_SESSION_TEST\" > env-result"}),
                    thought_signature: None,
                }])
            };
            Box::pin(std::future::ready(response))
        })));
        let session = rt
            .new_session(SessionSpec {
                options: Some(loop_protocol::SessionOptions { provider: Some("faux".into()), model: Some("faux-model".into()), permission_mode: Some("full-access".into()), no_context_files: true, no_web: true, ..Default::default() }),
                env: Some(std::collections::BTreeMap::from([(
                    "LOOP_SESSION_TEST".into(),
                    "caller value".into(),
                )])),
                ..Default::default()
            })
            .await
            .unwrap();
        let result = rt
            .route(&session)
            .unwrap()
            .opened
            .harness
            .tool_env()
            .await
            .unwrap()
            .exec(
                "printf '%s:%s' \"$LOOP_SESSION_TEST\" \"$PWD\"",
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.stdout,
            format!(
                "caller value:{}",
                dir.path().canonicalize().unwrap().display()
            )
        );
        let turn = rt.prompt(&session, vec![ContentBlock::text("check environment")]).await.unwrap();
        let outcome = turn.outcome().await;
        assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
        assert_eq!(std::fs::read_to_string(dir.path().join("env-result")).unwrap(), "caller value");
        let second = rt.new_session(SessionSpec::default()).await.unwrap();
        let result = rt
            .route(&second)
            .unwrap()
            .opened
            .harness
            .tool_env()
            .await
            .unwrap()
            .exec("printf '%s' \"$LOOP_SESSION_TEST\"", Default::default())
            .await
            .unwrap();
        assert_eq!(result.stdout, "");
        let shell = LocalShell::run(
            rt.as_ref(),
            &session,
            "printf '%s' \"$LOOP_SESSION_TEST\"",
            None,
        )
        .await
        .unwrap();
        assert_eq!(shell.stdout, "caller value");
        rt.close(&session.key, false).await.unwrap();
        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let shell = LocalShell::run(
            rt.as_ref(),
            &session,
            "printf '%s' \"$LOOP_SESSION_TEST\"",
            None,
        )
        .await
        .unwrap();
        assert_eq!(shell.stdout, "caller value");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn automatic_shutdown_refuses_watchers_and_fences_later_admissions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        loop_wire::RuntimeControl::attach_snapshot_watching(rt.as_ref(), 41, &session, 0)
            .await
            .unwrap();
        assert!(!rt.shutdown_if_idle().await);
        rt.register_watch(&session, 41, false, None).await.unwrap();
        assert!(rt.shutdown_if_idle().await);
        assert!(rt.new_session(SessionSpec::default()).await.is_err());
        assert!(
            loop_wire::RuntimeControl::attach_snapshot_watching(rt.as_ref(), 42, &session, 0)
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn version_replacement_refuses_even_an_idle_loaded_session() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(!rt.shutdown_if_empty().await);
        rt.close(&session.key, false).await.unwrap();
        assert!(rt.shutdown_if_empty().await);
        assert!(rt.new_session(SessionSpec::default()).await.is_err());
    })
    .await
    .unwrap();
}

fn install_openai_fixture(rt: &Runtime) -> String {
    let mut provider = faux_provider_fn(Arc::new(|_| {
        Box::pin(std::future::ready(FauxResponse::Text("offline".into())))
    }));
    provider.id = "openai".into();
    let mut models = provider.get_models();
    models[0].provider = "openai".into();
    let id = models[0].id.clone();
    provider.set_models(models);
    rt.factory.globals.models.set_provider(provider);
    id
}

#[tokio::test]
async fn differing_provider_environment_warns_without_exposing_either_key() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        rt.factory
            .globals
            .credentials
            .connect("openai", Some(loop_ai::Credential::api_key("owner-secret")))
            .unwrap();
        let model = install_openai_fixture(&rt);
        let session = rt
            .new_session(SessionSpec {
                options: Some(loop_protocol::SessionOptions {
                    provider: Some("openai".into()),
                    model: Some(model),
                    ..Default::default()
                }),
                env: Some(std::collections::BTreeMap::from([
                    ("OPENAI_API_KEY".into(), "surface-secret".into()),
                    ("LOOP_RUNTIME_TOKEN".into(), "runtime-secret".into()),
                    ("TASK_VALUE".into(), "visible".into()),
                ])),
                ..Default::default()
            })
            .await
            .unwrap();
        let snapshot = loop_wire::RuntimeControl::config(
            rt.as_ref(),
            loop_protocol::rpc::SessionParams {
                session: session.clone(),
            },
        )
        .await
        .unwrap();
        assert!(snapshot
            .notices
            .iter()
            .any(|line| line.contains("differs from the Runtime")));
        let public = serde_json::to_string(&snapshot).unwrap();
        assert!(
            !public.contains("owner-secret")
                && !public.contains("surface-secret")
                && !public.contains("runtime-secret")
        );
        let result = LocalShell::run(
            rt.as_ref(),
            &session,
            "printf '%s:%s:%s' \"$TASK_VALUE\" \"$OPENAI_API_KEY\" \"$LOOP_RUNTIME_TOKEN\"",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.stdout, "visible::");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A Session opened without a caller environment (`loop session new`) uses the
/// Runtime's own environment. It must not hand provider keys to tools either.
/// The child copy of this binary has the keys in its environment.
#[cfg(unix)]
#[test]
fn a_session_without_a_caller_environment_gets_no_provider_keys() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::admin_tests::session_without_caller_environment_child",
            "--nocapture",
        ])
        .env("LOOP_TOOLENV_RUNTIME_CHILD", "1")
        .env("OPENAI_API_KEY", "canary")
        .env("LOOP_RUNTIME_TOKEN", "canary")
        .env("GITHUB_TOKEN", "canary")
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn session_without_caller_environment_child() {
    if std::env::var_os("LOOP_TOOLENV_RUNTIME_CHILD").is_none() {
        return;
    }
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let result = LocalShell::run(
            rt.as_ref(),
            &session,
            "printf '%s:%s:%s' \"$OPENAI_API_KEY\" \"$LOOP_RUNTIME_TOKEN\" \"$GITHUB_TOKEN\"",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.stdout, "::canary");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn credential_warnings_follow_only_the_selected_provider_and_refresh_on_resume() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path()).await;
        rt.factory
            .globals
            .credentials
            .connect("openai", Some(loop_ai::Credential::api_key("owner-key")))
            .unwrap();
        let caller = || {
            Some(std::collections::BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "caller-key".into(),
            )]))
        };
        let unrelated = rt
            .new_session(SessionSpec {
                env: caller(),
                ..Default::default()
            })
            .await
            .unwrap();
        let config = loop_wire::RuntimeControl::config(
            rt.as_ref(),
            loop_protocol::rpc::SessionParams { session: unrelated },
        )
        .await
        .unwrap();
        assert!(
            !config.notices.iter().any(|line| line.contains("API key")),
            "an unused provider must not warn"
        );
        let model = install_openai_fixture(&rt);
        let session = rt
            .new_session(SessionSpec {
                env: caller(),
                options: Some(loop_protocol::SessionOptions {
                    provider: Some("openai".into()),
                    model: Some(model),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let config = loop_wire::RuntimeControl::config(
            rt.as_ref(),
            loop_protocol::rpc::SessionParams {
                session: session.clone(),
            },
        )
        .await
        .unwrap();
        assert!(config
            .notices
            .iter()
            .any(|line| line.contains("openai") && line.contains("differs")));
        rt.load_session(
            &session,
            SessionSpec {
                env: Some(std::collections::BTreeMap::from([(
                    "OPENAI_API_KEY".into(),
                    "owner-key".into(),
                )])),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let config = loop_wire::RuntimeControl::config(
            rt.as_ref(),
            loop_protocol::rpc::SessionParams { session },
        )
        .await
        .unwrap();
        assert!(
            !config.notices.iter().any(|line| line.contains("API key")),
            "resuming with the actual key clears a stale notice"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

mod next_turn {
    use super::*;
    use loop_agent::harness::{SandboxMode, TaggedSandbox};
    use loop_protocol::{ConfigChanged, SessionUpdate};
    use tokio::sync::broadcast::Receiver;

    fn local(tag: &str) -> SandboxChoice {
        SandboxChoice {
            mode: "local".into(),
            isolation: None,
            runtime: Some(tag.into()),
        }
    }

    /// Start the fake sandboxes a choice names by its runtime tag.
    fn use_sandboxes(rt: &Runtime, boxes: &[&TaggedSandbox]) {
        let boxes: std::collections::HashMap<String, TaggedSandbox> = boxes
            .iter()
            .map(|b| (b.tag().to_owned(), (*b).clone()))
            .collect();
        *rt.sandbox_starter.lock().unwrap() = Some(Arc::new(move |choice| {
            if choice.mode == "off" {
                return Ok(SandboxMode::Disabled);
            }
            let tag = choice.runtime.clone().unwrap_or_default();
            boxes
                .get(&tag)
                .map(|b| SandboxMode::Enabled {
                    sandbox: Arc::new(b.clone()),
                })
                .ok_or_else(|| failed(format!("no sandbox `{tag}`")))
        }));
    }

    async fn set_sandbox(
        rt: &Runtime,
        session: &SessionRef,
        choice: SandboxChoice,
    ) -> BackendResult<SandboxResult> {
        rt.sandbox(SandboxParams {
            session: session.clone(),
            action: SandboxAction::Set,
            config: Some(choice),
            expected_revision: None,
        })
        .await
    }

    async fn sandbox_status(rt: &Runtime, session: &SessionRef) -> SandboxResult {
        rt.sandbox(SandboxParams {
            session: session.clone(),
            action: SandboxAction::Status,
            config: None,
            expected_revision: None,
        })
        .await
        .unwrap()
    }

    /// Wait for an outcome the test cannot await directly.
    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = what;
    }

    async fn next_config(rx: &mut Receiver<UpdateEnvelope>, key: &SessionKey) -> ConfigChanged {
        loop {
            let envelope = rx.recv().await.unwrap();
            if let SessionUpdate::ConfigChanged(changed) = envelope.update {
                if &envelope.session == key {
                    return changed;
                }
            }
        }
    }

    /// Config events as `(pending, chosen, effective)` sandbox labels, up to
    /// the first one whose effective sandbox is `until`.
    async fn sandbox_events(
        rx: &mut Receiver<UpdateEnvelope>,
        key: &SessionKey,
        until: &str,
    ) -> Vec<(bool, Option<String>, Option<String>)> {
        let mut seen = vec![];
        loop {
            let changed = next_config(rx, key).await;
            let step = (
                changed.pending,
                changed.config.sandbox.clone(),
                changed.effective.sandbox.clone(),
            );
            let done = step.2.as_deref() == Some(until);
            seen.push(step);
            if done {
                return seen;
            }
        }
    }

    fn ran(sandbox: &TaggedSandbox) -> usize {
        sandbox
            .commands()
            .iter()
            .filter(|c| c.contains("echo parked"))
            .count()
    }

    async fn go(rt: &Runtime, session: &SessionRef) -> TurnHandle {
        rt.prompt(session, vec![ContentBlock::text("go")])
            .await
            .unwrap()
    }

    /// A choice made while a tool call runs in the old sandbox leaves that
    /// call there, is announced as pending to every watcher, and takes over
    /// for the next turn once the old sandbox has been released.
    #[tokio::test]
    async fn a_sandbox_chosen_in_a_tool_call_is_used_by_the_next_turn() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime_with(dir.path(), "full-access", Default::default()).await;
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (old, new) = (
                TaggedSandbox::gated("old", gate.clone()),
                TaggedSandbox::new("new"),
            );
            use_sandboxes(&rt, &[&old, &new]);
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (mut first, mut second) = (rt.subscribe(), rt.subscribe());
            set_sandbox(&rt, &session, local("old")).await.unwrap();

            let turn = go(&rt, &session).await;
            until("a command in the old sandbox", || ran(&old) == 1).await;
            let result = set_sandbox(&rt, &session, local("new")).await.unwrap();
            assert!(result.info.contains("next turn"), "{}", result.info);
            assert!(result.seq.is_some());
            assert_eq!(sandbox_status(&rt, &session).await.config, local("new"));
            // Nothing has changed under the running call.
            assert_eq!(old.destroy_count(), 0);
            assert_eq!(new.commands().len(), 0);

            gate.add_permits(1);
            turn.outcome().await;
            let waiting = (
                true,
                Some("local/new".to_owned()),
                Some("local/old".to_owned()),
            );
            let applied = (
                false,
                Some("local/new".to_owned()),
                Some("local/new".to_owned()),
            );
            for rx in [&mut first, &mut second] {
                let seen = sandbox_events(rx, &session.key, "local/new").await;
                // Both watchers see the pending change, then the effective
                // one, in that order.
                let pending = seen.iter().position(|e| e.0).expect("a pending event");
                let effective = seen.len() - 1;
                assert!(pending < effective, "{seen:?}");
                assert_eq!(seen[pending], waiting);
                assert_eq!(seen[effective], applied);
            }
            until("the old sandbox released", || old.destroy_count() == 1).await;

            let turn = go(&rt, &session).await;
            turn.outcome().await;
            assert_eq!(ran(&new), 1, "the next turn runs in the new sandbox");
            assert_eq!(ran(&old), 1);
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }

    /// Of two choices made during one turn only the later one is installed;
    /// the earlier one is released unused.
    #[tokio::test]
    async fn the_latest_sandbox_choice_wins_and_the_stale_one_is_released() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime_with(dir.path(), "full-access", Default::default()).await;
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let old = TaggedSandbox::gated("old", gate.clone());
            let (stale, latest) = (TaggedSandbox::new("stale"), TaggedSandbox::new("latest"));
            use_sandboxes(&rt, &[&old, &stale, &latest]);
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            set_sandbox(&rt, &session, local("old")).await.unwrap();
            let turn = go(&rt, &session).await;
            until("a command in the old sandbox", || ran(&old) == 1).await;

            set_sandbox(&rt, &session, local("stale")).await.unwrap();
            set_sandbox(&rt, &session, local("latest")).await.unwrap();
            assert_eq!(stale.destroy_count(), 1, "released as soon as replaced");
            gate.add_permits(1);
            turn.outcome().await;
            until("the old sandbox released", || old.destroy_count() == 1).await;
            assert_eq!(sandbox_status(&rt, &session).await.config, local("latest"));

            go(&rt, &session).await.outcome().await;
            assert_eq!(ran(&latest), 1);
            assert_eq!(ran(&stale), 0);
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }

    /// Closing a Session that still has a sandbox waiting releases it once
    /// and never applies it to another Session.
    #[tokio::test]
    async fn closing_with_a_sandbox_waiting_releases_it_and_it_stays_with_its_session() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime_with(dir.path(), "full-access", Default::default()).await;
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let old = TaggedSandbox::gated("old", gate.clone());
            let waiting = TaggedSandbox::new("waiting");
            use_sandboxes(&rt, &[&old, &waiting]);
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let other = rt.new_session(SessionSpec::default()).await.unwrap();
            set_sandbox(&rt, &session, local("old")).await.unwrap();
            let turn = go(&rt, &session).await;
            until("a command in the old sandbox", || ran(&old) == 1).await;
            set_sandbox(&rt, &session, local("waiting")).await.unwrap();

            gate.add_permits(1);
            rt.close(&session.key, true).await.unwrap();
            let _ = turn.outcome().await;
            assert_eq!(waiting.destroy_count(), 1);
            // The other Session never ran in it.
            go(&rt, &other).await.outcome().await;
            assert_eq!(ran(&waiting), 0);
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }

    /// A sandbox choice, applied or still waiting, is in the Session's
    /// record: a reopened Session runs its tools in it.
    #[tokio::test]
    async fn a_sandbox_choice_survives_reopening_the_session() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime_with(dir.path(), "full-access", Default::default()).await;
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let old = TaggedSandbox::gated("old", gate.clone());
            let (applied, waiting) = (TaggedSandbox::new("applied"), TaggedSandbox::new("waiting"));
            use_sandboxes(&rt, &[&old, &applied, &waiting]);
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            set_sandbox(&rt, &session, local("applied")).await.unwrap();
            rt.close(&session.key, false).await.unwrap();
            let reopened = rt
                .load_session(&session, SessionSpec::default())
                .await
                .unwrap();
            assert_eq!(
                sandbox_status(&rt, &reopened).await.config,
                local("applied")
            );
            go(&rt, &reopened).await.outcome().await;
            assert_eq!(ran(&applied), 1, "restored, not the workspace default");

            // A choice still waiting when the Session closes is the one
            // that comes back.
            set_sandbox(&rt, &reopened, local("old")).await.unwrap();
            let turn = go(&rt, &reopened).await;
            until("a command in the old sandbox", || ran(&old) == 1).await;
            set_sandbox(&rt, &reopened, local("waiting")).await.unwrap();
            gate.add_permits(1);
            rt.close(&reopened.key, true).await.unwrap();
            let _ = turn.outcome().await;
            let again = rt
                .load_session(&session, SessionSpec::default())
                .await
                .unwrap();
            assert_eq!(sandbox_status(&rt, &again).await.config, local("waiting"));
            go(&rt, &again).await.outcome().await;
            assert_eq!(ran(&waiting), 1);
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }

    /// A model chosen while a tool call runs, or while the turn waits for a
    /// turn permit, is accepted: the running turn keeps its model and the
    /// next one starts on the new one.
    #[tokio::test]
    async fn a_model_chosen_mid_turn_is_accepted_and_used_next_turn() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let limits = RuntimeLimits {
                max_active_turns: 1,
                ..Default::default()
            };
            let (rt, _log) = runtime_with(dir.path(), "full-access", limits).await;
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let held = TaggedSandbox::gated("held", gate.clone());
            use_sandboxes(&rt, &[&held]);
            let running = rt.new_session(SessionSpec::default()).await.unwrap();
            let waiting = rt.new_session(SessionSpec::default()).await.unwrap();
            set_sandbox(&rt, &running, local("held")).await.unwrap();
            let mut rx = rt.subscribe();

            let first = go(&rt, &running).await;
            until("a command in the held sandbox", || ran(&held) == 1).await;
            // The second Session's turn waits for the only permit.
            let second = go(&rt, &waiting).await;
            let live = rt.route(&waiting).unwrap();
            until("the second turn reserved", || {
                live.busy.load(Ordering::Acquire)
            })
            .await;

            for session in [&running, &waiting] {
                let result = rt.set_model(session, "faux/faux-two", None).await.unwrap();
                assert!(result.seq > 0);
            }
            let changed = loop {
                let changed = next_config(&mut rx, &running.key).await;
                if changed.config.model.as_deref() == Some("faux/faux-two") {
                    break changed;
                }
            };
            assert!(changed.pending);
            assert_eq!(changed.effective.model.as_deref(), Some("faux/faux-model"));
            let live = rt.route(&running).unwrap();
            assert_eq!(live.opened.harness.model().await.unwrap().id, "faux-model");

            gate.add_permits(8);
            first.outcome().await;
            second.outcome().await;
            go(&rt, &running).await.outcome().await;
            assert_eq!(live.opened.harness.model().await.unwrap().id, "faux-two");
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }
}

mod gate_mode {
    use super::*;
    use loop_agent::{BeforeToolCallContext, ToolCall as AgentToolCall};
    use loop_protocol::{ConfigChanged, SessionUpdate};
    use loop_wire::control::EscalationRecipient;
    use loop_wire::RuntimeControl;
    use tokio::sync::{broadcast::Receiver, mpsc, oneshot};

    type Asked = (
        loop_protocol::rpc::InteractParams,
        oneshot::Sender<loop_protocol::InteractionOutcome>,
    );
    /// A Surface that hands every Escalation to the test.
    struct Watcher(mpsc::UnboundedSender<Asked>);
    #[async_trait::async_trait]
    impl EscalationRecipient for Watcher {
        fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
        async fn interact(
            &self,
            params: loop_protocol::rpc::InteractParams,
            _withdrawn: oneshot::Receiver<()>,
        ) -> Result<loop_protocol::InteractionOutcome, ()> {
            let (tx, rx) = oneshot::channel();
            self.0.send((params, tx)).map_err(|_| ())?;
            rx.await.map_err(|_| ())
        }
    }

    async fn next_config(rx: &mut Receiver<UpdateEnvelope>, key: &SessionKey) -> ConfigChanged {
        loop {
            let envelope = rx.recv().await.unwrap();
            if let SessionUpdate::ConfigChanged(changed) = envelope.update {
                if &envelope.session == key {
                    return changed;
                }
            }
        }
    }

    async fn set(rt: &Runtime, session: &SessionRef, mode: &str) {
        rt.permission_mode(PermissionModeParams {
            session: session.clone(),
            mode: Some(mode.into()),
            expected_revision: None,
        })
        .await
        .unwrap();
    }

    fn exit_plan() -> BeforeToolCallContext {
        let arguments = json!({"plan": "1. Edit main.rs"});
        let tool_call = AgentToolCall {
            id: "plan1".into(),
            name: "exit_plan".into(),
            arguments: arguments.clone(),
            thought_signature: None,
        };
        BeforeToolCallContext {
            assistant_message: loop_ai::AssistantMessage {
                content: Vec::new(),
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                response_model: None,
                response_id: None,
                usage: Default::default(),
                stop_reason: loop_ai::StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: None,
                timestamp: 0,
            },
            args: arguments,
            tool_call,
            context: Default::default(),
        }
    }

    #[tokio::test]
    async fn a_mode_change_reaches_every_attached_client() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (mut first, mut second) = (rt.subscribe(), rt.subscribe());
            set(&rt, &session, "plan").await;
            for rx in [&mut first, &mut second] {
                let changed = next_config(rx, &session.key).await;
                assert_eq!(changed.config.mode.as_deref(), Some("plan"));
                assert_eq!(changed.effective.mode.as_deref(), Some("plan"));
            }
            // Another setting keeps the mode in the announced config.
            rt.set_model(&session, "faux/faux-two", None).await.unwrap();
            let changed = next_config(&mut first, &session.key).await;
            assert_eq!(changed.config.mode.as_deref(), Some("plan"));
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_mode_survives_reopening_the_session() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            set(&rt, &session, "plan").await;
            rt.close(&session.key, false).await.unwrap();
            // Listing and attaching read the recorded config.
            let attached = rt.attach_snapshot_watching(90, &session, 0).await.unwrap();
            let snapshot = attached.snapshot.unwrap();
            assert_eq!(snapshot.config.mode.as_deref(), Some("plan"));
            let reopened = rt
                .load_session(&session, SessionSpec::default())
                .await
                .unwrap();
            let mut rx = rt.subscribe();
            set(&rt, &reopened, "plan").await;
            let changed = next_config(&mut rx, &reopened.key).await;
            assert_eq!(changed.config.mode.as_deref(), Some("plan"));
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn approving_the_plan_announces_the_mode_it_returns_to() {
        tokio::time::timeout(LIMIT, async {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime_with(dir.path(), "full-access", Default::default()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            let (tx, mut asked) = mpsc::unbounded_channel();
            RuntimeControl::recipient(rt.as_ref(), &session, 1, Some(Arc::new(Watcher(tx))))
                .await
                .unwrap();
            set(&rt, &session, "plan").await;
            let (mut first, mut second) = (rt.subscribe(), rt.subscribe());

            let live = rt.route(&session).unwrap();
            let before = live.opened.tool_gate.before_hook();
            let review = tokio::spawn(async move { before(exit_plan(), None).await });
            let (params, answer) = asked.recv().await.unwrap();
            let loop_protocol::InteractionRequest::Permission { options, .. } = &params.request
            else {
                panic!("{params:?}")
            };
            let accept = options
                .iter()
                .find(|o| o.native_id == loop_app_core::interaction::native_options::ACCEPT)
                .unwrap_or(&options[0])
                .native_id
                .clone();
            answer
                .send(loop_protocol::InteractionOutcome::Selected {
                    native_id: accept,
                    note: None,
                })
                .unwrap();
            assert!(review.await.unwrap().is_none(), "the plan is approved");

            for rx in [&mut first, &mut second] {
                let changed = next_config(rx, &session.key).await;
                assert_eq!(changed.config.mode.as_deref(), Some("full-access"));
            }
            rt.shutdown().await;
        })
        .await
        .unwrap();
    }
}
