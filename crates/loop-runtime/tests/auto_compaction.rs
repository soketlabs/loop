//! Automatic compaction in a Runtime Session: a context-length error makes
//! the turn compact once and retry; clients see notices and the new
//! revision; transcript, reload, fork and `/compact` work afterwards.

use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::session::sqlite::SqliteSessionStore;
use loop_agent::harness::session::types::SessionTreeEntry;
use loop_agent::harness::SessionStore;
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::{ForkParams, RenameParams};
use loop_protocol::{AgentBackend, ContentBlock, SessionSpec, SessionUpdate, TurnOutcome};
use loop_runtime::log::MemoryEventLog;
use loop_runtime::{Runtime, RuntimeLimits};

fn opts(cwd: &std::path::Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
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
    }
}

fn is_compaction(entry: &SessionTreeEntry) -> bool {
    matches!(entry, SessionTreeEntry::Compaction { .. })
}

#[tokio::test]
async fn an_overflow_compacts_and_the_session_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let script = FauxScript::new();
    script.extend([
        FauxResponse::Text("first answer".into()),
        // The second prompt: refused, then the summary, then the retry.
        FauxResponse::Error("400 context_length_exceeded: prompt is too long".into()),
        FauxResponse::Text("SUMMARY: a long first request was answered.".into()),
        FauxResponse::Text("second answer".into()),
        // `session/compact` at the end.
        FauxResponse::Text("SUMMARY: manual.".into()),
    ]);
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider(script));
    let globals = GlobalResources::with_models(dir.path().join("agent"), models).unwrap();
    let session_store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(
        globals,
        opts(dir.path()),
        Arc::new(MemoryEventLog::new()),
        session_store,
        RuntimeLimits::default(),
    )
    .await
    .unwrap();
    let session = rt.new_session(SessionSpec::default()).await.unwrap();
    let first = format!("first {}", "w".repeat(8_000));
    let turn = rt
        .prompt(&session, vec![ContentBlock::text(first)])
        .await
        .unwrap();
    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));

    let mut updates = rt.subscribe();
    let turn = rt
        .prompt(&session, vec![ContentBlock::text("next")])
        .await
        .unwrap();
    assert!(
        matches!(turn.outcome().await, TurnOutcome::Completed(_)),
        "the retry succeeded"
    );
    let mut notices = Vec::new();
    let mut revision = None;
    while let Ok(Ok(envelope)) =
        tokio::time::timeout(Duration::from_millis(300), updates.recv()).await
    {
        match envelope.update {
            SessionUpdate::Notice { text, .. } => notices.push(text),
            SessionUpdate::ConfigChanged(changed) => revision = Some(changed.revision),
            _ => {}
        }
    }
    assert!(
        notices
            .iter()
            .any(|t| t.contains("too long for the model: compacting")),
        "{notices:?}"
    );
    assert!(
        notices.iter().any(|t| t.starts_with("context compacted")),
        "{notices:?}"
    );
    let revision = revision.expect("config_changed after the compaction");

    // The tree holds one compaction, recorded like `session/compact`'s.
    let store = SqliteSessionStore::open(dir.path().join("agent/sessions.db")).unwrap();
    let entries = store
        .load(&session.native_id)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap();
    assert_eq!(entries.iter().filter(|e| is_compaction(e)).count(), 1);

    // A setter with the announced revision is accepted.
    let renamed = rt
        .rename(RenameParams {
            session: session.clone(),
            name: "compacted".into(),
            expected_revision: Some(revision),
        })
        .await
        .unwrap();
    assert_eq!(renamed.revision, revision + 1);

    // The transcript shows the summary, and a reload gives the same
    // transcript and revision.
    let transcript = rt.transcript(&session).await.unwrap();
    assert!(transcript.iter().any(
        |u| matches!(u, SessionUpdate::Notice { text, .. } if text.starts_with("compaction summary"))
    ));
    rt.close(&session.key, false).await.unwrap();
    rt.load_session(&session, SessionSpec::default())
        .await
        .unwrap();
    let reloaded = rt.transcript(&session).await.unwrap();
    assert_eq!(
        format!("{reloaded:?}").len(),
        format!("{transcript:?}").len()
    );
    let again = rt
        .rename(RenameParams {
            session: session.clone(),
            name: "reloaded".into(),
            expected_revision: Some(renamed.revision),
        })
        .await
        .unwrap();
    assert_eq!(again.revision, renamed.revision + 1);

    // A fork before the kept user message.
    let entries = store
        .load(&session.native_id)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap();
    let next = entries
        .iter()
        .find(|e| {
            matches!(e, SessionTreeEntry::Message { message, .. }
                if message.role() == "user" && format!("{message:?}").contains("\"next\""))
        })
        .expect("the kept user message")
        .id()
        .to_string();
    let fork = rt
        .fork(ForkParams {
            session: session.clone(),
            before_entry_id: next,
            name: None,
            request_id: "auto-compaction-fork".into(),
        })
        .await
        .unwrap();
    assert_ne!(fork.session.native_id, session.native_id);

    // `session/compact` works on the compacted Session.
    let report = rt.compact(&session, None).await.unwrap();
    assert!(report.summary.starts_with("SUMMARY"));
    rt.shutdown().await;
}

/// A faux provider whose summary requests wait for `release`.
struct SlowSummary {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

async fn runtime_with_slow_summary(dir: &std::path::Path) -> (Arc<Runtime>, SlowSummary) {
    use loop_agent::harness::compaction::SUMMARY_SYSTEM_PROMPT;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (s, r) = (Arc::clone(&started), Arc::clone(&release));
    let provider = loop_ai::providers::faux_provider_fn(Arc::new(move |ctx: &loop_ai::Context| {
        let summary = ctx.system_prompt.as_deref() == Some(SUMMARY_SYSTEM_PROMPT);
        let (s, r) = (Arc::clone(&s), Arc::clone(&r));
        Box::pin(async move {
            if summary {
                s.notify_one();
                r.notified().await;
                FauxResponse::Text("SUMMARY: slow.".into())
            } else {
                FauxResponse::Text("answer".into())
            }
        })
    }));
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(provider);
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let session_store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(
        globals,
        opts(dir),
        Arc::new(MemoryEventLog::new()),
        session_store,
        RuntimeLimits::default(),
    )
    .await
    .unwrap();
    (Arc::new(rt), SlowSummary { started, release })
}

async fn compactable_session(rt: &Runtime) -> loop_protocol::SessionRef {
    let session = rt.new_session(SessionSpec::default()).await.unwrap();
    for text in [format!("first {}", "w".repeat(8_000)), "second".into()] {
        let turn = rt
            .prompt(&session, vec![ContentBlock::text(text)])
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
    }
    session
}

/// M1: while `/compact` waits for its summary, attach and prompt answer at
/// once (the prompt is refused as busy), and the compaction then commits.
#[tokio::test]
async fn attach_does_not_wait_for_a_slow_compaction() {
    use loop_wire::RuntimeControl;
    let dir = tempfile::tempdir().unwrap();
    let (rt, slow) = runtime_with_slow_summary(dir.path()).await;
    let session = compactable_session(&rt).await;
    let (rt2, s2) = (Arc::clone(&rt), session.clone());
    let compact = tokio::spawn(async move { rt2.compact(&s2, None).await });
    tokio::time::timeout(Duration::from_secs(5), slow.started.notified())
        .await
        .expect("the summary request started");
    tokio::time::timeout(
        Duration::from_secs(2),
        rt.attach_snapshot_watching(1, &session, 0),
    )
    .await
    .expect("attach did not wait for the summary")
    .unwrap();
    let prompt = tokio::time::timeout(
        Duration::from_secs(2),
        rt.prompt(&session, vec![ContentBlock::text("during")]),
    )
    .await
    .expect("prompt did not wait for the summary");
    match prompt {
        Err(error) => assert!(
            matches!(error, loop_protocol::BackendError::Busy)
                || error.to_string().contains("busy"),
            "{error}"
        ),
        Ok(turn) => assert!(!matches!(turn.outcome().await, TurnOutcome::Completed(_))),
    }
    slow.release.notify_one();
    let report = compact.await.unwrap().unwrap();
    assert!(report.summary.starts_with("SUMMARY"));
    rt.shutdown().await;
}

/// M1: closing the Session during a slow `/compact` does not wait for the
/// summary, and the compaction writes nothing after the close.
#[tokio::test]
async fn close_during_a_slow_compaction_cancels_it() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, slow) = runtime_with_slow_summary(dir.path()).await;
    let session = compactable_session(&rt).await;
    let (rt2, s2) = (Arc::clone(&rt), session.clone());
    let compact = tokio::spawn(async move { rt2.compact(&s2, None).await });
    tokio::time::timeout(Duration::from_secs(5), slow.started.notified())
        .await
        .expect("the summary request started");
    tokio::time::timeout(Duration::from_secs(5), rt.close(&session.key, false))
        .await
        .expect("close did not wait for the summary")
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), compact)
        .await
        .expect("the compaction ended")
        .unwrap();
    assert!(result.is_err());
    let store = SqliteSessionStore::open(dir.path().join("agent/sessions.db")).unwrap();
    let entries = store
        .load(&session.native_id)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap();
    assert!(!entries.iter().any(is_compaction));
    assert!(store.lease(&session.native_id).await.unwrap().is_none());
    rt.shutdown().await;
}

/// `session/fork` and `session/clone` of a Session holding a summary
/// compaction and a shorten-only compaction: the copy builds the same
/// context as the source at that point.
#[tokio::test]
async fn fork_and_clone_after_compactions_build_the_source_context() {
    use loop_agent::harness::session::types::{
        default_context_from_entries, PendingSessionWrite, Session,
    };
    use loop_agent::AgentMessage;
    use loop_protocol::rpc::CloneParams;

    let dir = tempfile::tempdir().unwrap();
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider(FauxScript::new()));
    let globals = GlobalResources::with_models(dir.path().join("agent"), models).unwrap();
    let session_store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(
        globals,
        opts(dir.path()),
        Arc::new(MemoryEventLog::new()),
        session_store,
        RuntimeLimits::default(),
    )
    .await
    .unwrap();
    let session = rt.new_session(SessionSpec::default()).await.unwrap();
    rt.close(&session.key, false).await.unwrap();

    // Write the source tree while no one holds it.
    let store: Arc<dyn SessionStore> =
        Arc::new(SqliteSessionStore::open(dir.path().join("agent/sessions.db")).unwrap());
    let sid = session.native_id.clone();
    let append = |w: PendingSessionWrite| {
        let store = Arc::clone(&store);
        let sid = sid.clone();
        async move { store.append_entry(&sid, w).await.unwrap() }
    };
    let message = |m: AgentMessage| PendingSessionWrite::Message { message: m };
    let result = |text: &str| {
        AgentMessage::tool_result(loop_ai::ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "big".into(),
            content: vec![loop_ai::ToolResultContent::Text(loop_ai::TextContent {
                text: text.into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: 0,
            usage: None,
            added_tool_names: Default::default(),
        })
    };
    append(message(AgentMessage::user_text("first request"))).await;
    append(message(AgentMessage::user_text("first follow-up"))).await;
    let second = append(message(AgentMessage::user_text("second request"))).await;
    let output = append(message(result(&"r".repeat(3_000)))).await;
    append(PendingSessionWrite::Compaction {
        summary: "summary of the first turn".into(),
        first_kept_entry_id: Some(second.id().into()),
        details: None,
    })
    .await;
    let shortened = append(PendingSessionWrite::Compaction {
        summary: "outputs shortened".into(),
        first_kept_entry_id: Some(second.id().into()),
        details: Some(serde_json::json!({"shortened": [output.id()]})),
    })
    .await;
    let third = append(message(AgentMessage::user_text("third request"))).await;
    append(message(AgentMessage::user_text("third follow-up"))).await;

    let context_of = |sid: String, leaf: Option<String>| {
        let store = Arc::clone(&store);
        async move {
            let reader = store.load(&sid).await.unwrap();
            let messages = match leaf {
                Some(leaf) => {
                    let path = reader
                        .read_path_to_root_or_compaction(Some(&leaf))
                        .await
                        .unwrap();
                    default_context_from_entries(&path).messages
                }
                None => {
                    Session::new(Arc::clone(&store), reader)
                        .build_context()
                        .await
                        .unwrap()
                        .messages
                }
            };
            format!("{messages:?}")
                .split("timestamp: ")
                .map(|part| part.trim_start_matches(char::is_numeric))
                .collect::<Vec<_>>()
                .join("timestamp: ")
        }
    };

    rt.load_session(&session, SessionSpec::default())
        .await
        .unwrap();
    let source_now = context_of(sid.clone(), None).await;
    assert!(source_now.contains("output shortened by automatic compaction"));
    assert!(!source_now.contains("first follow-up"));

    let clone = rt
        .clone_session(CloneParams {
            session: session.clone(),
            name: None,
            request_id: "clone-after-compaction".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        context_of(clone.session.native_id.clone(), None).await,
        source_now,
        "clone"
    );

    for (before, label) in [
        (third.id(), "fork after the compactions"),
        (second.id(), "fork at the kept point"),
    ] {
        let leaf = entries_before(&store, &sid, before).await;
        let fork = rt
            .fork(ForkParams {
                session: session.clone(),
                before_entry_id: before.into(),
                name: None,
                request_id: format!("fork-{label}"),
            })
            .await
            .unwrap();
        assert_eq!(
            context_of(fork.session.native_id.clone(), None).await,
            context_of(sid.clone(), Some(leaf)).await,
            "{label}"
        );
    }
    let _ = shortened;
    rt.shutdown().await;
}

/// The id of the entry just before `entry` in Session `sid`.
async fn entries_before(store: &Arc<dyn SessionStore>, sid: &str, entry: &str) -> String {
    let entries = store
        .load(sid)
        .await
        .unwrap()
        .read_entries(None)
        .await
        .unwrap();
    let at = entries.iter().position(|e| e.id() == entry).unwrap();
    entries[at - 1].id().to_string()
}
