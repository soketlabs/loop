//! Forking a Session after a compaction, on every store: the fork builds
//! the same context as the source at the fork point. A compaction names
//! entries by id (`first_kept_entry_id`, `details.shortened`), so a store
//! that gives copied entries new ids must map those references too.

use std::sync::Arc;

use loop_agent::harness::session::types::{
    default_context_from_entries, PendingSessionWrite, Session, SessionTreeEntry,
};
use loop_agent::harness::session::SessionForkSelection;
use loop_agent::harness::{
    create_in_memory_session_store, create_jsonl_session_store, create_sqlite_session_store,
    ExecutionEnv, HostExecutionEnv, SessionStore,
};
use loop_agent::{AgentMessage, AgentToolResult, CustomAgentMessage};
use loop_ai::{AssistantContent, AssistantMessage, Message, TextContent, ToolCall};
use serde_json::json;

/// Which compactions the source Session holds.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// A summary that keeps the second turn (`first_kept_entry_id`).
    Summary,
    /// A shorten-only compaction (`details.shortened`), keeping everything.
    Shorten,
    /// A summary, then a shorten-only compaction.
    Both,
}

fn assistant_call(id: &str) -> AgentMessage {
    let model = loop_ai::providers::faux_provider(loop_ai::providers::FauxScript::new())
        .get_model("faux-model")
        .unwrap();
    let mut a = AssistantMessage::pending(&model);
    a.content.push(AssistantContent::ToolCall(ToolCall {
        id: id.into(),
        name: "big".into(),
        arguments: json!({}),
        thought_signature: None,
    }));
    a.stop_reason = loop_ai::StopReason::ToolUse;
    AgentMessage::assistant(a)
}

fn assistant_text(text: &str) -> AgentMessage {
    let model = loop_ai::providers::faux_provider(loop_ai::providers::FauxScript::new())
        .get_model("faux-model")
        .unwrap();
    let mut a = AssistantMessage::pending(&model);
    a.content.push(AssistantContent::Text(TextContent {
        text: text.into(),
        text_signature: None,
    }));
    a.stop_reason = loop_ai::StopReason::Stop;
    AgentMessage::assistant(a)
}

fn tool_result(id: &str) -> AgentMessage {
    let result = AgentToolResult::text("r".repeat(3_000));
    AgentMessage::tool_result(loop_ai::ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "big".into(),
        content: result.content,
        details: None,
        is_error: false,
        timestamp: 0,
        usage: None,
        added_tool_names: Default::default(),
    })
}

/// Debug text with summary timestamps cleared (they are set when a context
/// is built).
fn same(messages: &[AgentMessage]) -> String {
    let messages: Vec<AgentMessage> = messages
        .iter()
        .cloned()
        .map(|mut m| {
            if let AgentMessage::Custom(CustomAgentMessage::CompactionSummary {
                timestamp, ..
            }) = &mut m
            {
                *timestamp = 0;
            }
            m
        })
        .collect();
    format!("{messages:?}")
}

/// Build the source Session. Returns its id, the id of an entry between
/// the kept point and the (first) compaction, and the last entry's id.
async fn source(store: &Arc<dyn SessionStore>, kind: Kind) -> (String, String, String) {
    let reader = store.create(None, None).await.unwrap();
    let sid = reader.metadata().id.clone();
    let append = |w: PendingSessionWrite| {
        let store = Arc::clone(store);
        let sid = sid.clone();
        async move { store.append_entry(&sid, w).await.unwrap() }
    };
    let message = |m: AgentMessage| PendingSessionWrite::Message { message: m };
    append(message(AgentMessage::user_text("first request"))).await;
    append(message(assistant_text("first answer"))).await;
    let kept = append(message(AgentMessage::user_text("second request"))).await;
    append(message(assistant_call("call-1"))).await;
    let result = append(message(tool_result("call-1"))).await;
    let between = append(message(assistant_text("second answer"))).await;
    if matches!(kind, Kind::Summary | Kind::Both) {
        append(PendingSessionWrite::Compaction {
            summary: "summary of the first turn".into(),
            first_kept_entry_id: Some(kept.id().to_string()),
            details: None,
        })
        .await;
    }
    if matches!(kind, Kind::Shorten | Kind::Both) {
        // A shorten-only compaction keeps everything from the first message
        // (here: from the kept request of the earlier summary).
        let first = if matches!(kind, Kind::Both) {
            kept.id().to_string()
        } else {
            let entries = store
                .load(&sid)
                .await
                .unwrap()
                .read_entries(None)
                .await
                .unwrap();
            entries[0].id().to_string()
        };
        append(PendingSessionWrite::Compaction {
            summary: "outputs shortened".into(),
            first_kept_entry_id: Some(first),
            details: Some(json!({"shortened": [result.id()]})),
        })
        .await;
    }
    append(message(AgentMessage::user_text("third request"))).await;
    let last = append(message(assistant_text("third answer"))).await;
    (sid, between.id().to_string(), last.id().to_string())
}

/// The context of `sid` at `leaf`.
async fn context_at(store: &Arc<dyn SessionStore>, sid: &str, leaf: &str) -> Vec<AgentMessage> {
    let reader = store.load(sid).await.unwrap();
    let path = reader
        .read_path_to_root_or_compaction(Some(leaf))
        .await
        .unwrap();
    default_context_from_entries(&path).messages
}

async fn forks_build_the_source_context(store: Arc<dyn SessionStore>, label: &str) {
    for kind in [Kind::Summary, Kind::Shorten, Kind::Both] {
        let (sid, between, last) = source(&store, kind).await;
        for at in [&last, &between] {
            let expected = context_at(&store, &sid, at).await;
            let fork = store
                .fork(&sid, SessionForkSelection::ThroughEntry, Some(at), None)
                .await
                .unwrap();
            let forked = Session::new(Arc::clone(&store), fork)
                .build_context()
                .await
                .unwrap()
                .messages;
            assert_eq!(
                same(&forked),
                same(&expected),
                "{label}: {kind:?} fork through {}",
                if at == &last {
                    "the end"
                } else {
                    "an entry before the compaction"
                }
            );
        }
        // The whole branch, as `/fork` with no point copies it.
        let expected = context_at(&store, &sid, &last).await;
        let fork = store
            .fork(&sid, SessionForkSelection::All, None, None)
            .await
            .unwrap();
        let forked = Session::new(Arc::clone(&store), fork)
            .build_context()
            .await
            .unwrap()
            .messages;
        assert_eq!(
            same(&forked),
            same(&expected),
            "{label}: {kind:?} fork of all"
        );
        if matches!(kind, Kind::Shorten | Kind::Both) {
            assert!(
                format!("{forked:?}").contains("output shortened by automatic compaction"),
                "{label}: {kind:?}: the shortened output is full again"
            );
        }
    }
}

#[tokio::test]
async fn fork_after_compaction_on_the_memory_store() {
    forks_build_the_source_context(create_in_memory_session_store(), "memory").await;
}

#[tokio::test]
async fn fork_after_compaction_on_the_jsonl_store() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(dir.path().to_path_buf()));
    forks_build_the_source_context(
        create_jsonl_session_store(fs, dir.path().join("sessions")),
        "jsonl",
    )
    .await;
}

#[tokio::test]
async fn fork_after_compaction_on_the_sqlite_store() {
    let dir = tempfile::tempdir().unwrap();
    forks_build_the_source_context(
        create_sqlite_session_store(dir.path().join("s.db")).unwrap(),
        "sqlite",
    )
    .await;
}

/// The harness `fork_session` (TUI `/fork`) on SQLite after a compaction.
#[tokio::test]
async fn harness_fork_after_compaction_on_the_sqlite_store() {
    use loop_agent::harness::{
        create_session_repository, AgentHarness, AgentHarnessOptions, SandboxMode,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = create_sqlite_session_store(dir.path().join("s.db")).unwrap();
    let (sid, between, last) = source(&store, Kind::Both).await;
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.open(&sid).await.unwrap();
    let models = Arc::new(loop_ai::Models::new());
    models.set_provider(loop_ai::providers::faux_provider(
        loop_ai::providers::FauxScript::new(),
    ));
    let harness = AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(&models),
        model: models.get_model("faux", "faux-model"),
        session,
        host_env: Arc::new(HostExecutionEnv::new(std::env::temp_dir())),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });
    for at in [&last, &between] {
        harness.switch_session(&sid).await.unwrap();
        let expected = context_at(&store, &sid, at).await;
        harness
            .fork_session(SessionForkSelection::ThroughEntry, Some(at), None)
            .await
            .unwrap();
        let forked = harness.session_context().await.unwrap().messages;
        assert_eq!(same(&forked), same(&expected));
    }
    // Keep the entry types in use for readers of this test.
    let _ = std::mem::size_of::<SessionTreeEntry>();
    let _ = Message::user_text("");
}
