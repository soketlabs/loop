//! Unloading idle Sessions at the `max_sessions` cap, and its races with
//! attach, prompt and Escalation registration.
use super::*;
use crate::log::{EventLog, LoggedEvent, MemoryEventLog};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall, UserContent, UserMessageContent,
};
use std::pin::Pin;

const LIMIT: Duration = Duration::from_secs(60);

type Script =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> + Send + Sync>;

fn first_prompt(context: &Context) -> String {
    context
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User(user) => Some(match &user.content {
                UserMessageContent::Text(text) => text.clone(),
                UserMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect(),
            }),
            _ => None,
        })
        .unwrap_or_default()
}
fn ready(r: FauxResponse) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> {
    Box::pin(std::future::ready(r))
}
fn tool(name: &str, arguments: serde_json::Value) -> FauxResponse {
    static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    FauxResponse::ToolCalls(vec![ToolCall {
        id: format!("u-{}", IDS.fetch_add(1, Ordering::Relaxed)),
        name: name.into(),
        arguments,
        thought_signature: None,
    }])
}
/// Answers by the Session's first prompt: `park` asks for an approval that
/// no Surface answers, `spawn` opens a Subagent that parks, anything else
/// is answered at once.
fn default_script() -> Script {
    Arc::new(|task: String| {
        if task.starts_with("park") {
            ready(tool("bash", json!({"command": "echo parked"})))
        } else if task.starts_with("spawn") {
            ready(tool("subagent", json!({"task": "park child"})))
        } else {
            ready(FauxResponse::Text(format!("done {task}")))
        }
    })
}

async fn runtime(
    dir: &std::path::Path,
    max_sessions: usize,
) -> (Arc<Runtime>, Arc<MemoryEventLog>) {
    let script = default_script();
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(move |context: &Context| {
        // Only the first model call of a turn follows the script.
        if context
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(_)))
        {
            return ready(FauxResponse::Text("finished".into()));
        }
        script(first_prompt(context))
    })));
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
        permission_mode: Some("ask".into()),
        agent: None,
    };
    let log = Arc::new(MemoryEventLog::new());
    let rt = {
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(
            globals,
            opts,
            log.clone(),
            store,
            RuntimeLimits {
                max_sessions,
                max_active_turns: 8,
            },
        )
    }
    .await
    .unwrap()
    .with_close_grace(Duration::from_millis(500))
    .into_shared();
    (rt, log)
}

async fn run(rt: &Runtime, session: &SessionRef, prompt: &str) -> TurnOutcome {
    rt.prompt(session, vec![ContentBlock::text(prompt)])
        .await
        .unwrap()
        .outcome()
        .await
}

fn loaded(rt: &Runtime, session: &SessionRef) -> bool {
    rt.sessions.lock().unwrap().contains_key(&session.key)
}

fn closed_reasons(events: &[LoggedEvent], key: &SessionKey) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event.session_key.as_ref() == Some(key))
        .filter_map(|e| match &e.event.kind {
            EventKind::SessionClosed(p) => Some(p["reason"].as_str().unwrap_or("").to_owned()),
            _ => None,
        })
        .collect()
}

/// Wait until `session` has a parked Escalation.
async fn parked(rt: &Runtime, session: &SessionRef) {
    let live = rt.route(session).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !live.escalations.has_pending().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("no Escalation was parked");
}

struct Watching;
#[async_trait]
impl EscalationRecipient for Watching {
    fn caller(&self) -> Option<loop_protocol::Caller> {
        None
    }

    fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
    async fn interact(
        &self,
        _: loop_protocol::rpc::InteractParams,
        withdrawn: oneshot::Receiver<()>,
    ) -> Result<loop_protocol::InteractionOutcome, ()> {
        let _ = withdrawn.await;
        Err(())
    }
}
async fn watch(rt: &Runtime, session: &SessionRef, connection: u64) {
    loop_wire::RuntimeControl::recipient(rt, session, connection, Some(Arc::new(Watching)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_least_recently_active_idle_session_is_unloaded_and_logged() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path(), 2).await;
        let a = rt.new_session(SessionSpec::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = rt.new_session(SessionSpec::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        // `a` was opened first but is now the more recently active one.
        assert!(matches!(
            run(&rt, &a, "hello").await,
            TurnOutcome::Completed(_)
        ));
        let c = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(loaded(&rt, &a) && loaded(&rt, &c));
        assert!(
            !loaded(&rt, &b),
            "the least recently active Session is unloaded"
        );
        assert_eq!(rt.status().loaded, 2);
        assert!(rt.store.lease(&b.native_id).await.unwrap().is_none());
        let events = log
            .read_runtime_after(
                loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
                0,
            )
            .await
            .unwrap();
        assert_eq!(closed_reasons(&events, &b.key), ["unloaded"]);
        // Its data is kept: loading it again unloads the next idle one.
        let before = log
            .latest_seq(&loop_store::Tenant::local(), &b.key)
            .await
            .unwrap();
        rt.load_session(&b, SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &b, "again").await,
            TurnOutcome::Completed(_)
        ));
        assert!(
            log.latest_seq(&loop_store::Tenant::local(), &b.key)
                .await
                .unwrap()
                > before
        );
        assert_eq!(rt.status().loaded, 2);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_busy_and_parked_sessions_are_never_unloaded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path(), 2).await;
        let attached = rt.new_session(SessionSpec::default()).await.unwrap();
        watch(&rt, &attached, 1).await;
        let busy = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&busy, vec![ContentBlock::text("park")])
            .await
            .unwrap();
        parked(&rt, &busy).await;
        assert_eq!(
            rt.new_session(SessionSpec::default()).await,
            Err(BackendError::Busy),
            "every loaded Session is protected"
        );
        assert!(loaded(&rt, &attached) && loaded(&rt, &busy));
        assert_eq!(rt.status().loaded, 2);
        // Detaching makes it eligible.
        loop_wire::RuntimeControl::recipient(rt.as_ref(), &attached, 1, None)
            .await
            .unwrap();
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(!loaded(&rt, &attached) && loaded(&rt, &busy) && loaded(&rt, &fresh));
        rt.cancel(&busy);
        tokio::time::timeout(Duration::from_secs(10), turn.outcome())
            .await
            .expect("cancelled turn never ended");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiting_parent_and_its_subagent_are_never_unloaded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path(), 3).await;
        let idle = rt.new_session(SessionSpec::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let parent = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&parent, vec![ContentBlock::text("spawn")])
            .await
            .unwrap();
        let child = loop {
            let child = rt
                .sessions
                .lock()
                .unwrap()
                .values()
                .find(|s| s.parent.as_ref() == Some(&parent.key))
                .map(|s| s.reference.clone());
            if let Some(child) = child {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        parked(&rt, &child).await;
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(!loaded(&rt, &idle), "only the idle root is unloaded");
        assert!(loaded(&rt, &parent) && loaded(&rt, &child) && loaded(&rt, &fresh));
        // `fresh` is attached now; nothing else can go.
        watch(&rt, &fresh, 7).await;
        assert_eq!(
            rt.new_session(SessionSpec::default()).await,
            Err(BackendError::Busy)
        );
        rt.cancel(&parent);
        let _ = turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_racing_eviction_is_never_lost() {
    tokio::time::timeout(LIMIT, async {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path(), 1).await;
            let a = rt.new_session(SessionSpec::default()).await.unwrap();
            let (opened, prompted) = tokio::join!(
                rt.new_session(SessionSpec::default()),
                rt.prompt(&a, vec![ContentBlock::text(format!("race {round}"))]),
            );
            let prompted = prompted.unwrap_or_else(|e| panic!("round {round}: prompt lost: {e}"));
            assert!(
                matches!(prompted.outcome().await, TurnOutcome::Completed(_)),
                "round {round}"
            );
            match opened {
                Ok(_) | Err(BackendError::Busy) => {}
                Err(error) => panic!("round {round}: {error}"),
            }
            rt.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_racing_eviction_ends_loaded_and_watched() {
    tokio::time::timeout(LIMIT, async {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path(), 1).await;
            let a = rt.new_session(SessionSpec::default()).await.unwrap();
            // The wire's order: attach (snapshot), then register the Surface.
            let attach = async {
                loop_wire::RuntimeControl::attach_watching(rt.as_ref(), 3, &a, 0).await?;
                watch(&rt, &a, 3).await;
                Ok::<_, BackendError>(())
            };
            let (opened, attached) = tokio::join!(rt.new_session(SessionSpec::default()), attach);
            // Either the attach won (no room), or the Session was unloaded
            // and the attach loaded it back before succeeding: a successful
            // attach is never left with its Session unloaded.
            attached.unwrap_or_else(|e| panic!("round {round}: {e} ({opened:?})"));
            assert!(
                loaded(&rt, &a),
                "round {round}: attached Session left unloaded ({opened:?})"
            );
            let live = rt.route(&a).unwrap();
            assert_eq!(live.escalations.connections().await, [3], "round {round}");
            rt.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_escalation_raised_while_evicting_keeps_its_session() {
    tokio::time::timeout(LIMIT, async {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path(), 1).await;
            let a = rt.new_session(SessionSpec::default()).await.unwrap();
            let (opened, prompted) = tokio::join!(
                rt.new_session(SessionSpec::default()),
                rt.prompt(&a, vec![ContentBlock::text("park")]),
            );
            let turn = prompted.unwrap();
            let a = rt
                .sessions
                .lock()
                .unwrap()
                .get(&a.key)
                .map(|s| s.reference.clone())
                .unwrap_or_else(|| panic!("round {round}: prompted Session not loaded"));
            parked(&rt, &a).await;
            // The parked Session can no longer be unloaded.
            if opened.is_ok() {
                assert_eq!(
                    rt.new_session(SessionSpec::default()).await,
                    Err(BackendError::Busy)
                );
            }
            assert!(loaded(&rt, &a), "round {round}");
            rt.cancel(&a);
            let _ = turn.outcome().await;
            rt.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_tree_transfer_keeps_source_watchable() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 8).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        run(&rt, &source, "original").await;
        rt.register_watch(&source, 77, true, None).await.unwrap();
        let exported = rt.export(&source).await.unwrap();
        let copied = rt
            .clone_session(loop_protocol::rpc::CloneParams {
                session: source.clone(),
                name: None,
                request_id: "clone-once".into(),
            })
            .await
            .unwrap();
        assert_ne!(source, copied.session);
        assert_eq!(rt.watcher_count(&source.key), 1);
        assert_eq!(
            copied,
            rt.clone_session(loop_protocol::rpc::CloneParams {
                session: source.clone(),
                name: None,
                request_id: "clone-once".into(),
            })
            .await
            .unwrap()
        );
        run(&rt, &source, "still usable").await;
        let imported = rt
            .import(loop_protocol::rpc::ImportParams {
                workspace_id: copied.workspace_id,
                jsonl: exported.jsonl.clone(),
                request_id: "import-once".into(),
            })
            .await
            .unwrap();
        let restored = rt.export(&imported.session).await.unwrap();
        let original =
            loop_agent::harness::session::SessionFile::from_jsonl(&exported.jsonl).unwrap();
        let restored =
            loop_agent::harness::session::SessionFile::from_jsonl(&restored.jsonl).unwrap();
        assert_eq!(original.entries.len(), restored.entries.len());
        let oversized = loop_protocol::rpc::ImportParams {
            workspace_id: imported.workspace_id,
            jsonl: "x".repeat(loop_protocol::rpc::SESSION_TRANSFER_LIMIT + 1),
            request_id: "too-large".into(),
        };
        assert!(rt.import(oversized).await.is_err());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tree_capacity_retry_and_import_validation_are_side_effect_free() {
    tokio::time::timeout(LIMIT, async {
        use loop_protocol::rpc::*;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.register_watch(&source, 1, true, None).await.unwrap();
        run(&rt, &source, "one").await;
        let params = CloneParams {
            session: source.clone(),
            name: None,
            request_id: "capacity-retry".into(),
        };
        let destination = rt.clone_session(params.clone()).await.unwrap();
        rt.register_watch(&destination.session, 2, true, None)
            .await
            .unwrap();
        assert_eq!(rt.clone_session(params.clone()).await.unwrap(), destination);
        let mut conflict = params.clone();
        conflict.name = Some("different".into());
        assert!(rt.clone_session(conflict).await.is_err());
        let mut full = params;
        full.request_id = "no-room".into();
        assert_eq!(
            rt.clone_session(full).await.unwrap_err(),
            BackendError::Busy
        );
        assert_eq!(rt.list().await.unwrap().len(), 2);
        run(&rt, &source, "still running").await;
        let export = rt.export(&source).await.unwrap();
        let mut lines: Vec<serde_json::Value> = export
            .jsonl
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[0]["cwd"] = json!("/untrusted/imported/path");
        lines[1]["parent_id"] = json!("missing-parent");
        let invalid = lines
            .into_iter()
            .map(|line| line.to_string() + "\n")
            .collect();
        assert!(rt
            .import(ImportParams {
                workspace_id: destination.workspace_id.clone(),
                jsonl: invalid,
                request_id: "bad-parent".into()
            })
            .await
            .is_err());
        assert!(rt
            .import(ImportParams {
                workspace_id: "/tmp/not-an-id".into(),
                jsonl: export.jsonl,
                request_id: "bad-workspace".into()
            })
            .await
            .is_err());
        assert_eq!(rt.list().await.unwrap().len(), 2);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn export_rejects_oversized_committed_context_without_truncation() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        let store = rt.factory.globals.repository.store();
        store
            .append_entry(
                &source.native_id,
                loop_agent::harness::session::PendingSessionWrite::Message {
                    message: loop_agent::types::AgentMessage::user_text(
                        "x".repeat(loop_protocol::rpc::SESSION_TRANSFER_LIMIT),
                    ),
                },
            )
            .await
            .unwrap();
        assert!(rt
            .export(&source)
            .await
            .unwrap_err()
            .to_string()
            .contains("size limit"));
        let entries = store
            .load(&source.native_id)
            .await
            .unwrap()
            .read_entries(None)
            .await
            .unwrap();
        assert!(entries.iter().any(|entry| matches!(
            entry,
            loop_agent::harness::session::SessionTreeEntry::Message { .. }
        )));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn abandoned_destination_releases_its_own_lease_and_receipt() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        let file = rt
            .route(&source)
            .unwrap()
            .opened
            .harness
            .export_session()
            .await
            .unwrap();
        let created = rt
            .store
            .create_destination(
                file,
                dir.path().to_string_lossy().into_owned(),
                Some(source.native_id.clone()),
                "cancelled-copy".into(),
                "fingerprint".into(),
                "{}".into(),
                rt.identity.clone(),
            )
            .await
            .unwrap();
        let destination = created.id.clone();
        assert!(rt.store.lease(&destination).await.unwrap().is_some());
        drop(created);
        while rt.store.list(None).await.unwrap().len() != 1 {
            tokio::task::yield_now().await;
        }
        assert!(rt.store.lease(&destination).await.unwrap().is_none());
        assert!(rt
            .store
            .destination_receipt("cancelled-copy", "fingerprint")
            .await
            .unwrap()
            .is_none());
        assert!(rt.store.lease(&source.native_id).await.unwrap().is_some());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn busy_attach_view_contains_already_committed_current_turn_updates() {
    tokio::time::timeout(LIMIT,async {
        use loop_protocol::rpc::ViewParams;
        use loop_wire::RuntimeControl;
        let dir = tempfile::tempdir().unwrap();
        let (rt,_) = runtime(dir.path(),2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt.prompt(&source,vec![ContentBlock::text("park snapshot")]).await.unwrap();
        parked(&rt,&source).await;
        let attachment = rt.attach_snapshot_watching(90,&source,0).await.unwrap();
        let snapshot = attachment.snapshot.unwrap();
        let view = rt.view(ViewParams { session:source.clone(),view_id:snapshot.view_id.unwrap(),cursor:None,limit:None }).await.unwrap();
        assert_eq!(view.at_seq,attachment.latest_seq);
        assert!(view.updates.iter().any(|update| matches!(update,SessionUpdate::MessageChunk { content,.. } if content.as_text() == Some("park snapshot"))));
        rt.cancel(&source);
        turn.outcome().await;
        rt.shutdown().await;
    }).await.unwrap();
}

#[tokio::test]
async fn export_limits_the_whole_encoded_response() {
    tokio::time::timeout(LIMIT, async {
        use loop_agent::harness::session::PendingSessionWrite;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.factory
            .globals
            .repository
            .store()
            .append_entry(
                &source.native_id,
                PendingSessionWrite::Message {
                    message: loop_agent::types::AgentMessage::user_text("\0".repeat(1_300_000)),
                },
            )
            .await
            .unwrap();
        let file = rt
            .route(&source)
            .unwrap()
            .opened
            .harness
            .export_session()
            .await
            .unwrap();
        assert!(file
            .to_jsonl_bounded(loop_protocol::rpc::SESSION_TRANSFER_LIMIT)
            .is_ok());
        assert!(rt
            .export(&source)
            .await
            .unwrap_err()
            .to_string()
            .contains("16 MiB"));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// Open `session` the way a standalone (non-Runtime) process resumes it.
async fn open_standalone(rt: &Runtime, session: &str) -> OpenedSession {
    let d = &rt.defaults;
    let opts = BootstrapOpts {
        cwd: d.cwd.clone(),
        provider: d.provider.clone(),
        model: d.model.clone(),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: Some(session.into()),
        no_web: true,
        permission_mode: d.permission_mode.clone(),
        agent: None,
    };
    let workspace = rt.factory.workspace(&opts).await.unwrap();
    rt.factory.open(workspace, &opts).await.unwrap()
}

#[tokio::test]
async fn copied_and_imported_sessions_never_restore_approval_grants() {
    tokio::time::timeout(LIMIT, async {
        use loop_protocol::rpc::*;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 8).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        run(&rt, &source, "granted").await;
        let store = rt.factory.globals.repository.store();
        for group in ["bash", "file"] {
            store
                .append_entry(
                    &source.native_id,
                    loop_agent::harness::session::PendingSessionWrite::Label {
                        label: format!(
                            "{}{group}",
                            loop_app_core::tool_approval::AUTO_APPROVE_LABEL_PREFIX
                        ),
                    },
                )
                .await
                .unwrap();
        }
        let exported = rt.export(&source).await.unwrap();
        assert!(exported.jsonl.contains("loop.toolApproval:auto:bash"));
        let cloned = rt
            .clone_session(CloneParams {
                session: source.clone(),
                name: None,
                request_id: "grant-clone".into(),
            })
            .await
            .unwrap();
        let tree = rt
            .tree(TreeParams {
                session: source.clone(),
                cursor: None,
                limit: None,
            })
            .await
            .unwrap();
        let forked = rt
            .fork(ForkParams {
                session: source.clone(),
                before_entry_id: tree.fork_points[0].entry_id.clone(),
                name: None,
                request_id: "grant-fork".into(),
            })
            .await
            .unwrap();
        let imported = rt
            .import(ImportParams {
                workspace_id: cloned.workspace_id.clone(),
                jsonl: exported.jsonl,
                request_id: "grant-import".into(),
            })
            .await
            .unwrap();
        rt.shutdown().await;
        let standalone = open_standalone(&rt, &source.native_id).await;
        assert!(
            !standalone.approval_bridge.should_ask("bash"),
            "the source keeps its own grant"
        );
        drop(standalone);
        for copy in [cloned, forked, imported] {
            let opened = open_standalone(&rt, &copy.session.native_id).await;
            assert!(opened.approval_bridge.auto_approve_groups().is_empty());
            assert!(opened.approval_bridge.should_ask("bash"));
            let entries = opened.harness.read_session_entries().await.unwrap();
            assert!(!entries.iter().any(|entry| matches!(
                entry,
                loop_agent::harness::session::SessionTreeEntry::Label { label, .. }
                    if label.starts_with("loop.toolApproval:")
            )));
        }
    })
    .await
    .unwrap();
}

/// Page through `session`'s tree one row at a time from `cursor`.
async fn tree_page(
    rt: &Runtime,
    session: &SessionRef,
    cursor: Option<String>,
) -> BackendResult<loop_protocol::rpc::TreeResult> {
    rt.tree(loop_protocol::rpc::TreeParams {
        session: session.clone(),
        cursor,
        limit: Some(1),
    })
    .await
}

#[tokio::test]
async fn tree_cursors_reject_a_row_set_changed_by_a_turn() {
    tokio::time::timeout(LIMIT, async {
        use loop_agent::harness::session::PendingSessionWrite;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 4).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        let store = rt.factory.globals.repository.store();
        let user = |text: &str| PendingSessionWrite::Message {
            message: loop_agent::types::AgentMessage::user_text(text),
        };
        // Two root branches; the active one parks a turn when prompted.
        store
            .append_entry(&source.native_id, user("park a"))
            .await
            .unwrap();
        rt.navigate(loop_protocol::rpc::NavigateParams {
            session: source.clone(),
            target_entry_id: None,
            summarize: false,
            expected_revision: None,
        })
        .await
        .unwrap();
        store
            .append_entry(&source.native_id, user("park b"))
            .await
            .unwrap();
        store
            .append_entry(&source.native_id, user("park b2"))
            .await
            .unwrap();

        // Unchanged state: every page follows from the first.
        let mut ids = Vec::new();
        let mut cursor = None;
        loop {
            let page = tree_page(&rt, &source, cursor).await.unwrap();
            ids.extend(page.entries.into_iter().map(|e| e.entry_id));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert!(ids.len() >= 3, "both branches are listed: {ids:?}");

        // An idle cursor does not survive a turn starting.
        let idle = tree_page(&rt, &source, None)
            .await
            .unwrap()
            .next_cursor
            .unwrap();
        let turn = rt
            .prompt(&source, vec![ContentBlock::text("park c")])
            .await
            .unwrap();
        parked(&rt, &source).await;
        let error = tree_page(&rt, &source, Some(idle)).await.unwrap_err();
        assert!(error.to_string().contains("tree changed"), "{error}");

        // A busy cursor does not survive that turn completing.
        let busy = tree_page(&rt, &source, None)
            .await
            .unwrap()
            .next_cursor
            .unwrap();
        rt.cancel(&source);
        turn.outcome().await;
        let error = tree_page(&rt, &source, Some(busy)).await.unwrap_err();
        assert!(error.to_string().contains("tree changed"), "{error}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_retry_never_answers_with_a_destination_admission_will_roll_back() {
    tokio::time::timeout(LIMIT, async {
        use loop_protocol::rpc::CloneParams;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 8).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        run(&rt, &source, "copy me").await;
        let params = CloneParams {
            session: source.clone(),
            name: None,
            request_id: "lost-response".into(),
        };
        let (reached, release) = rt.pauses.arm("destination_admit");
        let first = tokio::spawn({
            let (rt, params) = (rt.clone(), params.clone());
            async move { rt.clone_session(params).await }
        });
        reached.notified().await;
        // The receipt is committed but its Session is not admitted yet.
        let waiting = rt.pauses.watch("destination_retry_wait");
        let mut retry = tokio::spawn({
            let (rt, params) = (rt.clone(), params.clone());
            async move { rt.clone_session(params).await }
        });
        tokio::select! {
            result = &mut retry => panic!("a retry answered before admission settled: {result:?}"),
            () = waiting.notified() => {}
        }
        release.send(true).unwrap();
        assert!(first.await.unwrap().is_err());
        let retried = retry.await.unwrap().unwrap();
        assert!(rt.route(&retried.session).is_ok());
        assert!(rt
            .store
            .list(None)
            .await
            .unwrap()
            .iter()
            .any(|meta| meta.id == retried.session.native_id));
        assert_eq!(rt.clone_session(params).await.unwrap(), retried);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_attach_snapshot_holds_turn_settlement_until_it_is_captured() {
    tokio::time::timeout(LIMIT, async {
        use loop_protocol::rpc::ViewParams;
        use loop_wire::RuntimeControl;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&source, vec![ContentBlock::text("park race")])
            .await
            .unwrap();
        parked(&rt, &source).await;
        let (reached, release) = rt.pauses.arm("view_capture");
        let settling = rt.pauses.watch("turn_settling");
        let attach = tokio::spawn({
            let (rt, source) = (rt.clone(), source.clone());
            async move { rt.attach_snapshot_watching(91, &source, 0).await }
        });
        reached.notified().await;
        rt.cancel(&source);
        settling.notified().await;
        // Single-threaded: a settlement that does not wait for the snapshot
        // has already cleared busy by the time this task runs again.
        assert!(rt.is_busy(&source), "turn settled during a snapshot capture");
        release.send(false).unwrap();
        let attachment = attach.await.unwrap().unwrap();
        let snapshot = attachment.snapshot.unwrap();
        assert_eq!(snapshot.at_seq, attachment.latest_seq);
        let view = rt
            .view(ViewParams {
                session: source.clone(),
                view_id: snapshot.view_id.unwrap(),
                cursor: None,
                limit: None,
            })
            .await
            .unwrap();
        let prompts = view
            .updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::MessageChunk { content, .. } if content.as_text() == Some("park race")))
            .count();
        assert_eq!(prompts, 1, "{:?}", view.updates);
        turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

async fn resets(log: &MemoryEventLog, session: &SessionRef) -> usize {
    log.read_after(&loop_store::Tenant::local(), &session.key, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| {
            matches!(&record.event.kind, EventKind::Update(envelope)
                if matches!(envelope.update, SessionUpdate::BranchReset { .. }))
        })
        .count()
}

#[tokio::test]
async fn navigation_at_the_view_limit_changes_nothing() {
    tokio::time::timeout(LIMIT, async {
        use loop_protocol::rpc::NavigateParams;
        use loop_wire::RuntimeControl;
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path(), 2).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        run(&rt, &source, "keep this branch").await;
        for connection in 0..tree::VIEW_LIMIT as u64 {
            rt.attach_snapshot_watching(connection, &source, 0)
                .await
                .unwrap();
        }
        let before = rt.store.control_state(&source.native_id).await.unwrap();
        let navigate = || NavigateParams {
            session: source.clone(),
            target_entry_id: None,
            summarize: false,
            expected_revision: None,
        };
        assert_eq!(
            rt.navigate(navigate()).await.unwrap_err(),
            BackendError::Busy
        );
        let after = rt.store.control_state(&source.native_id).await.unwrap();
        assert_eq!(after.branch_revision, before.branch_revision);
        assert_eq!(after.pending, None);
        assert_eq!(resets(&log, &source).await, 0);
        assert!(matches!(
            run(&rt, &source, "still on the old branch").await,
            TurnOutcome::Completed { .. }
        ));
        rt.views.lock().unwrap().clear();
        rt.navigate(navigate()).await.unwrap();
        assert_eq!(resets(&log, &source).await, 1);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_dropped_destination_request_settles_before_any_retry_answers() {
    tokio::time::timeout(LIMIT, async {
        use loop_protocol::rpc::CloneParams;
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = runtime(dir.path(), 8).await;
        let source = rt.new_session(SessionSpec::default()).await.unwrap();
        run(&rt, &source, "copy me").await;
        for fail in [true, false] {
            let params = CloneParams {
                session: source.clone(),
                name: None,
                request_id: format!("dropped-{fail}"),
            };
            let (reached, release) = rt.pauses.arm("destination_admit");
            let request = tokio::spawn({
                let (rt, params) = (rt.clone(), params.clone());
                async move { rt.clone_session(params).await }
            });
            reached.notified().await;
            // The connection that asked goes away mid-admission.
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            let waiting = rt.pauses.watch("destination_retry_wait");
            let mut retry = tokio::spawn({
                let (rt, params) = (rt.clone(), params.clone());
                async move { rt.clone_session(params).await }
            });
            tokio::select! {
                result = &mut retry => panic!("a retry answered before admission settled: {result:?}"),
                () = waiting.notified() => {}
            }
            assert!(
                release.send(fail).is_ok(),
                "the Runtime, not the dropped request, owns admission"
            );
            let retried = retry.await.unwrap().unwrap();
            assert!(rt.route(&retried.session).is_ok());
            assert!(rt
                .store
                .list(None)
                .await
                .unwrap()
                .iter()
                .any(|meta| meta.id == retried.session.native_id));
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
