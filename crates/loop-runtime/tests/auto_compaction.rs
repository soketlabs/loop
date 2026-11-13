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
    let rt = Runtime::new(
        globals,
        opts(dir.path()),
        Arc::new(MemoryEventLog::new()),
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
