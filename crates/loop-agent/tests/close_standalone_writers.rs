//! `close_standalone_writers` is process-wide, so it has its own test binary.
#![cfg(feature = "sqlite")]

use loop_agent::harness::session::sqlite::{close_standalone_writers, SqliteSessionStore};
use loop_agent::harness::session::PendingSessionWrite;
use loop_agent::harness::SessionStore;

#[tokio::test]
async fn closing_every_writer_releases_them_and_refuses_later_claims() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let base = SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap();
        let first = base.standalone_writer();
        let second = base.standalone_writer();
        let a = first
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let b = second
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let spare = base.create(None, None).await.unwrap().metadata().id.clone();
        close_standalone_writers().await.unwrap();
        for id in [&a, &b] {
            assert!(base.lease(id).await.unwrap().is_none());
        }
        let label = PendingSessionWrite::Label {
            label: "late".into(),
        };
        assert!(first.append_entry(&a, label).await.is_err());
        // Even a writer made afterwards (startup still running) refuses.
        let late = base.standalone_writer();
        assert!(late.open_writer(&spare).await.is_err());
        assert!(late.create(None, None).await.is_err());
        assert!(base.lease(&spare).await.unwrap().is_none());
    })
    .await
    .unwrap();
}
