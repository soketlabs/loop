//! JSONL session store integration tests.

use std::sync::Arc;

use loop_agent::harness::{
    create_jsonl_session_store, create_session_repository, HostExecutionEnv, SessionForkSelection,
};
use loop_agent::AgentMessage;

fn test_env(dir: &tempfile::TempDir) -> Arc<HostExecutionEnv> {
    Arc::new(HostExecutionEnv::new(dir.path()))
}

#[tokio::test]
async fn jsonl_create_append_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let env = test_env(&dir);
    let sessions_root = dir.path().join("sessions");
    let store = create_jsonl_session_store(Arc::clone(&env) as Arc<_>, sessions_root);
    let repo = create_session_repository(Arc::clone(&store), None);

    let session = repo
        .create(Some("/tmp/proj".into()), Some("test".into()))
        .await
        .unwrap();
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

    let id = session.metadata().id.clone();
    drop(session);

    let loaded = repo.open(&id).await.unwrap();
    let ctx2 = loaded.build_context().await.unwrap();
    assert_eq!(ctx2.messages.len(), 2);
    assert_eq!(loaded.metadata().name.as_deref(), Some("test"));
    assert_eq!(loaded.metadata().cwd.as_deref(), Some("/tmp/proj"));
    assert!(loaded.metadata().path.is_some());
}

#[tokio::test]
async fn jsonl_reopen_after_store_reload() {
    let dir = tempfile::tempdir().unwrap();
    let env = test_env(&dir);
    let sessions_root = dir.path().join("sessions");

    let id = {
        let store = create_jsonl_session_store(Arc::clone(&env) as Arc<_>, sessions_root.clone());
        let repo = create_session_repository(store, None);
        let session = repo.create(Some("/data".into()), None).await.unwrap();
        session
            .append_message(AgentMessage::user_text("persisted"))
            .await
            .unwrap();
        session.metadata().id.clone()
    };

    let store2 = create_jsonl_session_store(Arc::clone(&env) as Arc<_>, sessions_root);
    let loaded = store2.load(&id).await.unwrap();
    let entries = loaded.read_entries(None).await.unwrap();
    assert_eq!(entries.len(), 1);
    let ctx = loop_agent::harness::Session::new(store2, loaded)
        .build_context()
        .await
        .unwrap();
    assert_eq!(ctx.messages.len(), 1);
}

#[tokio::test]
async fn jsonl_fork_all() {
    let dir = tempfile::tempdir().unwrap();
    let env = test_env(&dir);
    let sessions_root = dir.path().join("sessions");
    let store = create_jsonl_session_store(Arc::clone(&env) as Arc<_>, sessions_root);
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
        .fork(&session.metadata().id, SessionForkSelection::All, None, Some("fork".into()))
        .await
        .unwrap();
    let entries = forked.read_entries(None).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(forked.metadata().name.as_deref(), Some("fork"));
}

#[tokio::test]
async fn jsonl_list_by_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let env = test_env(&dir);
    let sessions_root = dir.path().join("sessions");
    let store = create_jsonl_session_store(Arc::clone(&env) as Arc<_>, sessions_root);

    let repo_a = create_session_repository(Arc::clone(&store), None);
    repo_a.create(Some("/alpha".into()), None).await.unwrap();
    repo_a.create(Some("/beta".into()), None).await.unwrap();
    repo_a.create(Some("/alpha".into()), None).await.unwrap();

    let alpha = store.list(Some("/alpha")).await.unwrap();
    assert_eq!(alpha.len(), 2);
    assert!(alpha.iter().all(|m| m.cwd.as_deref() == Some("/alpha")));

    let all = store.list(None).await.unwrap();
    assert_eq!(all.len(), 3);
}
