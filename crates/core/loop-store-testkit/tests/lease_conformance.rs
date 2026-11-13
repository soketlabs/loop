//! Runs the shared Session lease suite on the SQLite and in-memory stores.

use std::sync::Arc;

use loop_agent::harness::session::sqlite::SqliteSessionStore;
use loop_agent::harness::session::{InMemoryRuntimeStore, RuntimeSessionStore};
use loop_store_testkit::session_leases::{self as suite, ScriptedEnvironment};
use loop_store_testkit::Fixture;

async fn sqlite(env: Arc<ScriptedEnvironment>) -> Fixture<dyn RuntimeSessionStore> {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::with_environment(dir.path().join("sessions.db"), env).unwrap();
    Fixture::new(Arc::new(store) as Arc<dyn RuntimeSessionStore>).with_guard(dir)
}

async fn memory(env: Arc<ScriptedEnvironment>) -> Fixture<dyn RuntimeSessionStore> {
    let dir = tempfile::tempdir().unwrap();
    let store = InMemoryRuntimeStore::new(env, Some(dir.path().join("sessions.db")));
    Fixture::new(Arc::new(store) as Arc<dyn RuntimeSessionStore>).with_guard(dir)
}

macro_rules! conformance {
    ($name:ident, $factory:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn acquire() {
                suite::acquire(&$factory).await;
            }
            #[tokio::test]
            async fn renew() {
                suite::renew(&$factory).await;
            }
            #[tokio::test]
            async fn takeover_after_stale() {
                suite::takeover_after_stale(&$factory).await;
            }
            #[tokio::test]
            async fn runtime_reclaim() {
                suite::runtime_reclaim(&$factory).await;
            }
            #[tokio::test]
            async fn release() {
                suite::release(&$factory).await;
            }
            #[tokio::test]
            async fn sweep() {
                suite::sweep(&$factory).await;
            }
            #[tokio::test]
            async fn two_owners() {
                suite::two_owners(&$factory).await;
            }
            #[tokio::test]
            async fn delete_removes_the_lease() {
                suite::delete_removes_the_lease(&$factory).await;
            }
            #[tokio::test]
            async fn control_markers() {
                suite::control_markers(&$factory).await;
            }
            #[tokio::test]
            async fn takeover_stale_boundary() {
                suite::takeover_stale_boundary(&$factory).await;
            }
            #[tokio::test]
            async fn sweep_stale_boundary() {
                suite::sweep_stale_boundary(&$factory).await;
            }
            #[tokio::test]
            async fn claimant_pid_too_large() {
                suite::claimant_pid_too_large(&$factory).await;
            }
            #[tokio::test]
            async fn reclaim_same_pid_new_generation() {
                suite::reclaim_same_pid_new_generation(&$factory).await;
            }
            #[tokio::test]
            async fn takeover_missing_session() {
                suite::takeover_missing_session(&$factory).await;
            }
            #[tokio::test]
            async fn destinations() {
                suite::destinations(&$factory).await;
            }
            #[tokio::test]
            async fn delete_removes_control_and_receipt() {
                suite::delete_removes_control_and_receipt(&$factory).await;
            }
            #[tokio::test]
            async fn old_writer_fenced_after_takeover() {
                suite::old_writer_fenced_after_takeover(&$factory).await;
            }
            #[tokio::test]
            async fn check_resume_refuses_stale_lease() {
                suite::check_resume_refuses_stale_lease(&$factory).await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn concurrent_acquires() {
                suite::concurrent_acquires(&$factory).await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn renew_races_takeover() {
                suite::renew_races_takeover(&$factory).await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn run_all() {
                suite::run_all($factory).await;
            }
        }
    };
}

conformance!(sqlite_store, sqlite);
conformance!(memory_store, memory);
