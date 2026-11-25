//! NativeBackend model control: Session-scoped model and effort, recorded in
//! the Session tree, announced as `config_changed`, restored on load.

use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::session::types::{PendingSessionWrite, SessionMetadata, SessionReader};
use loop_agent::harness::types::SessionError;
use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    ExecutionEnv, HostExecutionEnv, SandboxMode, SessionStore, SessionTreeEntry,
};
use loop_agent::native_backend::NativeBackend;
use loop_agent::{AgentThinkingLevel, AgentTool, AgentToolResult};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Model, Models, ToolCall};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, SessionUpdate, TurnOutcome, UpdateEnvelope,
};
use serde_json::json;
use tokio::sync::broadcast;

fn reasoning(id: &str) -> Model {
    let mut model = faux_provider(FauxScript::new())
        .model("faux-model")
        .unwrap();
    model.id = id.into();
    model.name = id.into();
    model.reasoning = true;
    model
}

async fn harness(responses: Vec<FauxResponse>, tools: Vec<AgentTool>) -> Arc<AgentHarness> {
    harness_on(create_in_memory_session_store(), responses, tools).await
}

async fn harness_on(
    store: Arc<dyn SessionStore>,
    responses: Vec<FauxResponse>,
    tools: Vec<AgentTool>,
) -> Arc<AgentHarness> {
    let script = FauxScript::new();
    script.extend(responses);
    let provider = faux_provider(script);
    provider.set_models(vec![reasoning("one"), reasoning("two/with-slash")]);
    let models = Arc::new(Models::new());
    models.set_provider(provider);
    let model = models.model("faux", "one").unwrap();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools,
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    harness.set_thinking_level(AgentThinkingLevel::Medium).await;
    harness
}

async fn config_changed(
    rx: &mut broadcast::Receiver<UpdateEnvelope>,
) -> (u64, loop_protocol::ConfigChanged) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a config_changed update")
            .unwrap();
        if let SessionUpdate::ConfigChanged(changed) = envelope.update {
            return (envelope.seq, changed);
        }
    }
}

#[tokio::test]
async fn effort_and_model_are_recorded_announced_and_listed() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let control = backend
            .model_control()
            .expect("native offers model control");
        assert!(backend.capabilities().set_model && backend.capabilities().efforts);
        assert!(!backend.capabilities().modes);

        let efforts = control.efforts(&session).await.unwrap();
        assert_eq!(efforts.current.as_deref(), Some("medium"));
        assert_eq!(efforts.revision, 0);
        assert!(efforts.models.iter().any(|c| c.id == "max"));

        let mut rx = backend.subscribe();
        let result = control.set_effort(&session, "off", None).await.unwrap();
        assert_eq!(result.revision, 1);
        assert!(!result.pending);
        let (seq, changed) = config_changed(&mut rx).await;
        assert_eq!(seq, result.seq);
        assert_eq!(changed.revision, 1);
        assert_eq!(changed.config.effort.as_deref(), Some("off"));
        assert_eq!(changed.effective, changed.config);
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Off);
        assert_eq!(harness.thinking_level().await, AgentThinkingLevel::Off);

        // Model ids may contain `/`; the provider is the part before the first.
        let models = control.models(&session).await.unwrap();
        assert_eq!(models.current.as_deref(), Some("faux/one"));
        assert!(models.models.iter().any(|m| m.id == "faux/two/with-slash"));
        let result = control
            .set_model(&session, "faux/two/with-slash", Some(1))
            .await
            .unwrap();
        assert_eq!(result.revision, 2);
        let (_, changed) = config_changed(&mut rx).await;
        assert_eq!(changed.config.model.as_deref(), Some("faux/two/with-slash"));
        assert_eq!(harness.model().await.unwrap().id, "two/with-slash");

        let entries = harness.read_session_entries().await.unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: AgentThinkingLevel::Off,
                ..
            }
        )));
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ModelChange { provider, model_id, .. }
                if provider == "faux" && model_id == "two/with-slash"
        )));

        let error = control
            .set_effort(&session, "ultra", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("choose one of"), "{error}");
        assert!(control
            .set_model(&session, "faux/none", None)
            .await
            .is_err());
        assert!(control.set_model(&session, "nonsense", None).await.is_err());
        assert_eq!(control.efforts(&session).await.unwrap().revision, 2);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_stale_revision_conflicts_without_changing_anything() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let control = backend.model_control().unwrap();
        control.set_effort(&session, "low", Some(0)).await.unwrap();
        assert_eq!(
            control
                .set_effort(&session, "high", Some(0))
                .await
                .unwrap_err(),
            BackendError::Conflict { revision: 1 }
        );
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Low);
        assert_eq!(
            control
                .set_model(&session, "faux/one", Some(5))
                .await
                .unwrap_err(),
            BackendError::Conflict { revision: 1 }
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn concurrent_setters_get_distinct_revisions_and_the_last_one_wins() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let levels: [&'static str; 6] = ["off", "low", "medium", "high", "max", "minimal"];
        let tasks: Vec<_> = levels
            .into_iter()
            .map(|level| {
                let backend = Arc::clone(&backend);
                let session = session.clone();
                tokio::spawn(async move {
                    let control = backend.model_control().unwrap();
                    let result = control.set_effort(&session, level, None).await.unwrap();
                    (result.revision, level)
                })
            })
            .collect();
        let mut done = Vec::new();
        for task in tasks {
            done.push(task.await.unwrap());
        }
        done.sort();
        let revisions: Vec<u64> = done.iter().map(|(r, _)| *r).collect();
        assert_eq!(revisions, [1, 2, 3, 4, 5, 6]);
        assert_eq!(
            backend.thinking_level(),
            loop_agent::effort::parse_thinking(done.last().unwrap().1)
        );

        // With the same expected revision, exactly one setter wins.
        let tasks: Vec<_> = ["low", "high"]
            .into_iter()
            .map(|level| {
                let backend = Arc::clone(&backend);
                let session = session.clone();
                tokio::spawn(async move {
                    backend
                        .model_control()
                        .unwrap()
                        .set_effort(&session, level, Some(6))
                        .await
                })
            })
            .collect();
        let mut won = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(result) => {
                    won += 1;
                    assert_eq!(result.revision, 7);
                }
                Err(error) => assert_eq!(error, BackendError::Conflict { revision: 7 }),
            }
        }
        assert_eq!(won, 1);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn explicit_off_is_restored_as_off_and_absent_inherits_the_default() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        backend.set_default_effort(AgentThinkingLevel::Medium);
        let first = backend.current_session().await;
        let control = backend.model_control().unwrap();
        control.set_effort(&first, "off", None).await.unwrap();

        // A new Session starts from the default.
        let second = backend
            .new_session(loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Medium);
        assert_eq!(control.efforts(&second).await.unwrap().revision, 0);

        // Loading the first again restores its explicit off.
        backend
            .load_session(&first, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Off);
        assert_eq!(harness.thinking_level().await, AgentThinkingLevel::Off);
        assert_eq!(control.efforts(&first).await.unwrap().revision, 1);

        // And the second, which never chose, inherits the default again.
        backend
            .load_session(&second, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Medium);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn an_effort_change_during_a_turn_applies_after_it() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let wait = AgentTool::simple(
            "wait",
            "Wait",
            "Waits until cancelled.",
            json!({"type": "object", "properties": {}}),
            |_id, _args, cancel, _update| async move {
                if let Some(cancel) = cancel {
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    }
                }
                Ok(AgentToolResult::text("waited"))
            },
        );
        let harness = harness(
            vec![
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "w1".into(),
                    name: "wait".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                FauxResponse::Text("after".into()),
            ],
            vec![wait],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        let turn = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(envelope.update, SessionUpdate::ToolCall(_)) {
                break envelope.turn.unwrap();
            }
        };
        let control = backend.model_control().unwrap();
        let result = control.set_effort(&session, "high", None).await.unwrap();
        assert!(result.pending);
        assert_eq!(result.applies_after_turn.as_ref(), Some(&turn));
        let (_, changed) = config_changed(&mut rx).await;
        assert!(changed.pending);
        assert_eq!(changed.config.effort.as_deref(), Some("high"));
        assert_eq!(changed.effective.effort.as_deref(), Some("medium"));

        backend.cancel(&session);
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            TurnOutcome::Cancelled | TurnOutcome::Completed(_)
        ));
        let mut ended = false;
        let changed = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            match envelope.update {
                SessionUpdate::TurnEnded { .. } => ended = true,
                SessionUpdate::ConfigChanged(changed) => break changed,
                _ => {}
            }
        };
        assert!(ended, "the change is announced after the turn ends");
        assert!(!changed.pending);
        assert_eq!(changed.revision, result.revision);
        assert_eq!(changed.effective.effort.as_deref(), Some("high"));
        // The deferred write reached the tree once the turn ended.
        let entries = harness.read_session_entries().await.unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: AgentThinkingLevel::High,
                ..
            }
        )));
    })
    .await
    .expect("test timed out");
}

/// A store whose message writes wait while `held`, to pause a turn's final
/// persistence.
struct GatedStore {
    inner: Arc<dyn SessionStore>,
    held: std::sync::atomic::AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SessionStore for GatedStore {
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.create(cwd, name).await
    }
    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.load(id).await
    }
    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        self.inner.list(cwd).await
    }
    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        if matches!(pending, PendingSessionWrite::Message { .. })
            && self.held.swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.append_entry(session_id, pending).await
    }
    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.delete(id).await
    }
    async fn fork(
        &self,
        source_id: &str,
        selection: loop_agent::harness::session::SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner
            .fork(source_id, selection, through_entry_id, name)
            .await
    }
}

/// A change acknowledged while a turn's final writes are paused is in the
/// tree as soon as they finish; none is lost to the drain.
#[tokio::test]
async fn a_setter_racing_the_final_persistence_is_recorded() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let store = Arc::new(GatedStore {
            inner: create_in_memory_session_store(),
            held: std::sync::atomic::AtomicBool::new(true),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let harness = harness_on(
            store.clone(),
            vec![FauxResponse::Text("done".into())],
            vec![],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let turn = backend
            .prompt(&session, vec![ContentBlock::text("go")])
            .await
            .unwrap();
        // The turn's messages are being written; the setter arrives now.
        store.started.notified().await;
        let setter = tokio::spawn({
            let backend = Arc::clone(&backend);
            let session = session.clone();
            async move {
                backend
                    .model_control()
                    .unwrap()
                    .set_effort(&session, "high", None)
                    .await
            }
        });
        tokio::task::yield_now().await;
        store.release.notify_one();
        let result = setter.await.unwrap().unwrap();
        turn.outcome().await;
        let entries = harness.read_session_entries().await.unwrap();
        let recorded = loop_agent::harness::session::types::config_from_entries(
            &entries,
            entries.last().map(|e| e.id()),
        );
        assert_eq!(recorded.thinking_level, Some(AgentThinkingLevel::High));
        assert_eq!(recorded.revision, result.revision);
        assert!(entries
            .iter()
            .any(|e| matches!(e, SessionTreeEntry::Message { .. })));
    })
    .await
    .expect("test timed out");
}

/// Changes during a turn are durable when acknowledged, before it ends.
#[tokio::test]
async fn an_acknowledged_change_during_a_turn_is_already_recorded() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(
            vec![
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "w1".into(),
                    name: "wait".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                FauxResponse::Text("after".into()),
            ],
            vec![wait_tool()],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        let turn = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        let control = backend.model_control().unwrap();
        let result = control.set_effort(&session, "high", None).await.unwrap();
        assert!(result.pending);
        let entries = harness.read_session_entries().await.unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: AgentThinkingLevel::High,
                ..
            }
        )));
        // The model never changes mid-turn: it is accepted and waits.
        let model = control
            .set_model(&session, "faux/two/with-slash", None)
            .await
            .unwrap();
        assert!(model.pending);
        assert_eq!(harness.model().await.unwrap().id, "one");
        backend.cancel(&session);
        turn.outcome().await;
        control
            .set_model(&session, "faux/two/with-slash", None)
            .await
            .unwrap();
    })
    .await
    .expect("test timed out");
}

/// Envelopes from setters and from a running turn reach subscribers in
/// sequence order.
#[tokio::test]
async fn setters_and_turns_never_publish_out_of_order() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let rounds = 20;
        let harness = harness(
            (0..rounds)
                .map(|i| FauxResponse::Text(format!("answer {i}")))
                .collect(),
            vec![],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        for _ in 0..rounds {
            let turn = backend
                .prompt(&session, vec![ContentBlock::text("go")])
                .await
                .unwrap();
            let setters: Vec<_> = ["low", "high", "off", "medium"]
                .into_iter()
                .map(|level| {
                    let backend = Arc::clone(&backend);
                    let session = session.clone();
                    tokio::spawn(async move {
                        backend
                            .model_control()
                            .unwrap()
                            .set_effort(&session, level, None)
                            .await
                            .unwrap()
                    })
                })
                .collect();
            for setter in setters {
                setter.await.unwrap();
            }
            turn.outcome().await;
        }
        let mut last = 0;
        let mut seen = 0;
        while let Ok(envelope) = rx.try_recv() {
            assert!(envelope.seq > last, "seq {} after {last}", envelope.seq);
            last = envelope.seq;
            seen += 1;
        }
        assert!(seen > rounds * 4);
    })
    .await
    .expect("test timed out");
}

/// A model chosen in one Session is not carried into a new Session or into
/// one that never chose a model.
#[tokio::test]
async fn a_sessions_model_stays_in_that_session() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let control = backend.model_control().unwrap();
        let first = backend.current_session().await;
        control
            .set_model(&first, "faux/two/with-slash", None)
            .await
            .unwrap();
        let second = backend
            .new_session(loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        let current = |choices: loop_protocol::ModelChoices| choices.current.unwrap();
        assert_eq!(current(control.models(&second).await.unwrap()), "faux/one");
        backend
            .load_session(&first, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(
            current(control.models(&first).await.unwrap()),
            "faux/two/with-slash"
        );
        backend
            .load_session(&second, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(current(control.models(&second).await.unwrap()), "faux/one");
        // A changed default reaches Sessions without a choice of their own.
        backend.set_default_model(harness.models().model("faux", "two/with-slash"));
        backend
            .load_session(&second, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(
            current(control.models(&second).await.unwrap()),
            "faux/two/with-slash"
        );
    })
    .await
    .expect("test timed out");
}

fn wait_tool() -> AgentTool {
    AgentTool::simple(
        "wait",
        "Wait",
        "Waits until cancelled.",
        json!({"type": "object", "properties": {}}),
        |_id, _args, cancel, _update| async move {
            if let Some(cancel) = cancel {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
            }
            Ok(AgentToolResult::text("waited"))
        },
    )
}

async fn tool_running(rx: &mut broadcast::Receiver<UpdateEnvelope>) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(envelope.update, SessionUpdate::ToolCall(_)) {
            return;
        }
    }
}

/// A model chosen during a turn is recorded and reported as pending, the
/// running turn keeps its model, and the next turn starts on the new one.
#[tokio::test]
async fn a_model_change_during_a_turn_applies_from_the_next_turn() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(
            vec![
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "w1".into(),
                    name: "wait".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                FauxResponse::Text("after".into()),
                FauxResponse::Text("next".into()),
            ],
            vec![wait_tool()],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        let control = backend.model_control().unwrap();
        let result = control
            .set_model(&session, "faux/two/with-slash", None)
            .await
            .unwrap();
        assert!(result.pending);
        assert!(result.applies_after_turn.is_some());
        let (_, changed) = config_changed(&mut rx).await;
        assert!(changed.pending);
        assert_eq!(changed.config.model.as_deref(), Some("faux/two/with-slash"));
        assert_eq!(changed.effective.model.as_deref(), Some("faux/one"));
        // The running turn is unaffected.
        assert_eq!(harness.model().await.unwrap().id, "one");
        let entries = harness.read_session_entries().await.unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ModelChange { model_id, .. } if model_id == "two/with-slash"
        )));

        backend.cancel(&session);
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        let changed = loop {
            let (_, changed) = config_changed(&mut rx).await;
            if !changed.pending {
                break changed;
            }
        };
        assert_eq!(
            changed.effective.model.as_deref(),
            Some("faux/two/with-slash")
        );

        let next = backend
            .prompt(&session, vec![ContentBlock::text("again")])
            .await
            .unwrap();
        assert_eq!(harness.model().await.unwrap().id, "two/with-slash");
        tokio::time::timeout(Duration::from_secs(5), next.outcome())
            .await
            .unwrap();
    })
    .await
    .expect("test timed out");
}

/// Work filed during a turn waits for the next one; a newer job of the same
/// key replaces it (and is told); an idle backend hands the job back.
#[tokio::test]
async fn work_filed_during_a_turn_runs_when_the_next_turn_starts() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(
            vec![
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "w1".into(),
                    name: "wait".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                FauxResponse::Text("after".into()),
                FauxResponse::Text("next".into()),
            ],
            vec![wait_tool()],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let log = Arc::new(StdMutex::new(Vec::<(u32, bool)>::new()));
        let ran = Arc::new(AtomicU32::new(0));
        let job = |id: u32| -> loop_agent::native_backend::NextTurnJob {
            let log = Arc::clone(&log);
            let ran = Arc::clone(&ran);
            Box::new(move |apply| {
                Box::pin(async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    log.lock().unwrap().push((id, apply));
                })
            })
        };
        // Idle: handed back, nothing runs.
        assert!(backend.after_turn("k", job(0)).await.is_some());
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        assert!(backend.after_turn("k", job(1)).await.is_none());
        assert!(backend.after_turn("k", job(2)).await.is_none());
        // The replaced job was released; the kept one has not run.
        assert_eq!(*log.lock().unwrap(), vec![(1, false)]);

        backend.cancel(&session);
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        assert_eq!(log.lock().unwrap().len(), 1, "waits for the next turn");
        let next = backend
            .prompt(&session, vec![ContentBlock::text("again")])
            .await
            .unwrap();
        assert_eq!(*log.lock().unwrap(), vec![(1, false), (2, true)]);
        tokio::time::timeout(Duration::from_secs(5), next.outcome())
            .await
            .unwrap();
    })
    .await
    .expect("test timed out");
}

fn running_prompt_responses() -> Vec<FauxResponse> {
    vec![
        FauxResponse::ToolCalls(vec![ToolCall {
            id: "w1".into(),
            name: "wait".into(),
            arguments: json!({}),
            thought_signature: None,
        }]),
        FauxResponse::Text("after".into()),
        FauxResponse::Text("next".into()),
    ]
}

/// An older queued job never outlives a newer choice of its key, even when
/// the newer one arrives with the backend idle.
#[tokio::test]
async fn an_idle_job_releases_an_older_queued_one_of_its_key() {
    use std::sync::Mutex as StdMutex;
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(running_prompt_responses(), vec![wait_tool()]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let log = Arc::new(StdMutex::new(Vec::<(u32, bool)>::new()));
        let job = |id: u32| -> loop_agent::native_backend::NextTurnJob {
            let log = Arc::clone(&log);
            Box::new(move |apply| Box::pin(async move { log.lock().unwrap().push((id, apply)) }))
        };
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        assert!(backend.after_turn("k", job(1)).await.is_none());
        backend.cancel(&session);
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        // Idle again: the newer job is handed back and the older one released.
        let mut idle = false;
        for _ in 0..100 {
            if !backend.is_busy(&session) {
                idle = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(idle);
        assert!(backend.after_turn("k", job(2)).await.is_some());
        assert_eq!(*log.lock().unwrap(), vec![(1, false)]);
        let next = backend
            .prompt(&session, vec![ContentBlock::text("again")])
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), next.outcome())
            .await
            .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec![(1, false)],
            "the stale job does not run at the next prompt"
        );
    })
    .await
    .expect("test timed out");
}

/// A job that is still preparing when the user cancels stops the prompt
/// before its first model call.
#[tokio::test]
async fn cancelling_while_the_next_turn_prepares_stops_the_prompt() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(running_prompt_responses(), vec![wait_tool()]).await;
        let backend = Arc::new(NativeBackend::new(Arc::clone(&harness)).await);
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = std::sync::Mutex::new(Some(started_tx));
        let job: loop_agent::native_backend::NextTurnJob = Box::new(move |apply| {
            Box::pin(async move {
                if apply {
                    if let Some(tx) = started_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    let _ = release_rx.await;
                }
            })
        });
        assert!(backend.after_turn("k", job).await.is_none());
        backend.cancel(&session);
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        let prompting = {
            let backend = Arc::clone(&backend);
            let session = session.clone();
            tokio::spawn(async move {
                backend
                    .prompt(&session, vec![ContentBlock::text("again")])
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        // The job is paused mid-preparation: there is no turn to abort yet.
        backend.cancel(&session);
        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), prompting)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err(), "the cancelled prompt must not start");
        // Only the first turn's model call (and its tool follow-up) happened.
        let entries = harness.read_session_entries().await.unwrap();
        assert!(!entries
            .iter()
            .any(|e| format!("{e:?}").contains("\"again\"")));
        assert!(!backend.is_busy(&session));
    })
    .await
    .expect("test timed out");
}

/// Shutdown waits for a job that is preparing and releases the ones still
/// queued, once each.
#[tokio::test]
async fn shutdown_releases_queued_jobs_and_waits_for_a_preparing_one() {
    use std::sync::Mutex as StdMutex;
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(running_prompt_responses(), vec![wait_tool()]).await;
        let backend = Arc::new(NativeBackend::new(Arc::clone(&harness)).await);
        let session = backend.current_session().await;
        let log = Arc::new(StdMutex::new(Vec::<(&'static str, bool)>::new()));
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        let queued = {
            let log = Arc::clone(&log);
            Box::new(move |apply| {
                Box::pin(async move { log.lock().unwrap().push(("queued", apply)) })
                    as futures::future::BoxFuture<'static, ()>
            }) as loop_agent::native_backend::NextTurnJob
        };
        assert!(backend.after_turn("k", queued).await.is_none());
        backend.shutdown().await;
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        assert_eq!(*log.lock().unwrap(), vec![("queued", false)]);
        backend.shutdown().await;
        assert_eq!(log.lock().unwrap().len(), 1, "released once");
    })
    .await
    .expect("test timed out");
}

/// A recorded sandbox choice is announced as pending then effective, is
/// saved in the Session, and is read back when the Session is restored.
#[tokio::test]
async fn a_sandbox_choice_is_recorded_announced_and_restored() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(running_prompt_responses(), vec![wait_tool()]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        tool_running(&mut rx).await;
        let choice = json!({"mode": "local", "isolation": "full"});
        let result = backend
            .record_sandbox(&session, choice.clone(), None, true)
            .await
            .unwrap();
        assert!(result.pending);
        let (_, changed) = config_changed(&mut rx).await;
        assert!(changed.pending);
        assert_eq!(changed.config.sandbox.as_deref(), Some("local/full"));
        assert_eq!(changed.effective.sandbox, None);

        backend.cancel(&session);
        tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        // The turn ended, but the sandbox is not installed until the next one.
        loop {
            let (_, changed) = config_changed(&mut rx).await;
            if !changed.pending {
                assert_eq!(changed.effective.sandbox, None);
                break;
            }
        }
        backend.sandbox_applied();
        let (_, changed) = config_changed(&mut rx).await;
        assert_eq!(changed.effective.sandbox.as_deref(), Some("local/full"));
        assert_eq!(changed.config.sandbox.as_deref(), Some("local/full"));

        backend.restore_config().await.unwrap();
        assert_eq!(backend.recorded_sandbox(), Some(choice));
        assert_eq!(
            backend.config().config.sandbox.as_deref(),
            Some("local/full")
        );
    })
    .await
    .expect("test timed out");
}
