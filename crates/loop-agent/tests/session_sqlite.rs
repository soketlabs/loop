//! SQLite session store integration tests.

#![cfg(feature = "sqlite")]

use std::sync::Arc;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_search, create_sqlite_session_store,
    SessionForkSelection,
};
use loop_agent::AgentMessage;
use rusqlite::{params, Connection};

fn open_db(path: &std::path::Path) -> Connection {
    Connection::open(path).unwrap()
}

#[tokio::test]
async fn sqlite_migrations_include_branch_tips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    create_sqlite_session_store(&path).unwrap();
    let conn = open_db(&path);
    let applied: Vec<String> = conn
        .prepare("SELECT id FROM migrations ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(applied.contains(&"001_initial.sql".to_string()));
    assert!(applied.contains(&"002_branch_tips.sql".to_string()));
    assert!(applied.contains(&"003_session_leases.sql".to_string()));
    // Reopening an upgraded database must preserve existing rows and migration state.
    create_sqlite_session_store(&path).unwrap();
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='branch_tips'",
        [],
        |_| Ok(()),
    )
    .expect("branch_tips exists");
}

#[tokio::test]
async fn sqlite_branch_cache_after_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    let sid = session.metadata().id.clone();
    session
        .append_message(AgentMessage::user_text("one"))
        .await
        .unwrap();
    let second = session
        .append_message(AgentMessage::user_text("two"))
        .await
        .unwrap();
    let conn = open_db(&path);
    let tip_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM branch_tips WHERE session_id = ?1 AND tip_id = ?2",
            params![sid, second.id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tip_count, 1);
    let branch_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM branch_entries WHERE session_id = ?1",
            params![sid],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(branch_rows, 2);
    let path_entries = session.build_context().await.unwrap();
    assert_eq!(path_entries.messages.len(), 2);
}

#[tokio::test]
async fn sqlite_fts_search_with_cwd_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let search = create_sqlite_session_search(&path, Arc::clone(&store));
    let repo = create_session_repository(store, Some(search));
    let session = repo.create(Some("/proj-a".into()), None).await.unwrap();
    session
        .append_message(AgentMessage::user_text("unique fts needle alpha"))
        .await
        .unwrap();
    let other = repo.create(Some("/proj-b".into()), None).await.unwrap();
    other
        .append_message(AgentMessage::user_text("unique fts needle beta"))
        .await
        .unwrap();

    let hits = repo
        .search("unique fts needle alpha", Some("/proj-a"), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session.cwd.as_deref(), Some("/proj-a"));

    let filtered = repo
        .search("unique fts needle", Some("/proj-b"), 10)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].session.cwd.as_deref(), Some("/proj-b"));
}

#[tokio::test]
async fn sqlite_fork_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, Some("source".into())).await.unwrap();
    session
        .append_message(AgentMessage::user_text("a"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("b"))
        .await
        .unwrap();
    let forked = store
        .fork(
            &session.metadata().id,
            SessionForkSelection::All,
            None,
            Some("fork".into()),
        )
        .await
        .unwrap();
    let entries = forked.read_entries(None).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        forked.metadata().parent_session_id.as_deref(),
        Some(session.metadata().id.as_str())
    );
}

#[tokio::test]
async fn sqlite_materialized_updates_on_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    let sid = session.metadata().id.clone();
    session
        .append_message(AgentMessage::user_text("hello"))
        .await
        .unwrap();
    let conn = open_db(&path);
    let payload: String = conn
        .query_row(
            "SELECT payload FROM session_materialized WHERE session_id = ?1",
            params![sid],
            |row| row.get(0),
        )
        .unwrap();
    let summary: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(summary["messageCount"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_appends_have_contiguous_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let session = store.create(None, None).await.unwrap();
    let id = session.metadata().id.clone();
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let store = store.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..8 {
                store
                    .append_entry(
                        &id,
                        loop_agent::harness::session::PendingSessionWrite::Message {
                            message: AgentMessage::user_text("concurrent"),
                        },
                    )
                    .await
                    .unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let entries = session.read_entries(None).await.unwrap();
    assert_eq!(entries.len(), 256);
    for pair in entries.windows(2) {
        assert_eq!(pair[1].parent_id(), Some(pair[0].id()));
    }
    let conn = open_db(&path);
    let seqs: Vec<i64> = conn
        .prepare("SELECT entry_seq FROM session_entries WHERE session_id=?1 ORDER BY entry_seq")
        .unwrap()
        .query_map([id], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(seqs, (1..=256).collect::<Vec<_>>());
}

#[tokio::test]
async fn fork_copy_failure_rolls_back_destination() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fork.sqlite");
    let store = create_sqlite_session_store(&path).unwrap();
    let source = store.create(None, None).await.unwrap();
    let id = source.metadata().id.clone();
    for text in ["one", "two", "three"] {
        store
            .append_entry(
                &id,
                loop_agent::harness::session::PendingSessionWrite::Message {
                    message: AgentMessage::user_text(text),
                },
            )
            .await
            .unwrap();
    }
    // Fail the second copied entry, after the destination and first entry exist.
    open_db(&path)
        .execute_batch(
            "CREATE TRIGGER fail_fork BEFORE INSERT ON session_entries
        WHEN NEW.entry_seq = 2 BEGIN SELECT RAISE(ABORT, 'injected copy failure'); END;",
        )
        .unwrap();
    assert!(store
        .fork(&id, SessionForkSelection::All, None, None)
        .await
        .is_err());
    let sessions = store.list(None).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, id);
    assert_eq!(source.read_entries(None).await.unwrap().len(), 3);
}
