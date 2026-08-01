//! Memory + SQLite session store tests.

use std::sync::Arc;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, SessionForkSelection,
};
use loop_agent::AgentMessage;

#[tokio::test]
async fn memory_session_roundtrip() {
    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(Some("/tmp".into()), Some("t".into())).await.unwrap();
    session
        .append_message(AgentMessage::user_text("hello"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("world"))
        .await
        .unwrap();
    let ctx = session.build_context().await.unwrap();
    assert_eq!(ctx.messages.len(), 2);

    let loaded = repo.open(&session.metadata().id).await.unwrap();
    let ctx2 = loaded.build_context().await.unwrap();
    assert_eq!(ctx2.messages.len(), 2);
}

#[tokio::test]
async fn memory_fork_all() {
    let store = create_in_memory_session_store();
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    session
        .append_message(AgentMessage::user_text("a"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("b"))
        .await
        .unwrap();
    let forked = store
        .fork(&session.metadata().id, SessionForkSelection::All, Some("f".into()))
        .await
        .unwrap();
    let entries = forked.read_entries(None).await.unwrap();
    assert_eq!(entries.len(), 2);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_session_roundtrip() {
    use loop_agent::harness::{create_sqlite_session_search, create_sqlite_session_store};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let search = create_sqlite_session_search(&path, Arc::clone(&store));
    let repo = create_session_repository(store, Some(search));
    let session = repo.create(Some("/proj".into()), None).await.unwrap();
    session
        .append_message(AgentMessage::user_text("sqlite hello"))
        .await
        .unwrap();
    let ctx = session.build_context().await.unwrap();
    assert_eq!(ctx.messages.len(), 1);
    let hits = repo.search("sqlite", Some("/proj"), 10).await.unwrap();
    assert!(!hits.is_empty());
}
