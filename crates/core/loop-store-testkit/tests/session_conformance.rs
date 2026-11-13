//! Runs the shared Session store suite on the memory, SQLite and JSONL stores.

use std::sync::Arc;

use loop_agent::harness::{
    create_in_memory_session_store, create_jsonl_session_store, create_sqlite_session_store,
    HostExecutionEnv, SessionStore,
};
use loop_store_testkit::session_store::{self as suite, Capabilities};
use loop_store_testkit::Fixture;

async fn memory() -> Fixture<dyn SessionStore> {
    Fixture::new(create_in_memory_session_store())
}

async fn sqlite() -> Fixture<dyn SessionStore> {
    let dir = tempfile::tempdir().unwrap();
    let store = create_sqlite_session_store(dir.path().join("sessions.sqlite")).unwrap();
    Fixture::new(store).with_guard(dir)
}

async fn jsonl() -> Fixture<dyn SessionStore> {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(HostExecutionEnv::new(dir.path()));
    let store = create_jsonl_session_store(env, dir.path().join("sessions"));
    Fixture::new(store).with_guard(dir)
}

macro_rules! conformance {
    ($name:ident, $factory:ident, $caps:expr) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn create_and_load() {
                suite::create_and_load(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn list() {
                suite::list(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn append_entry_order() {
                suite::append_entry_order(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn concurrent_appends() {
                suite::concurrent_appends(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn read_entries_after_seq() {
                suite::read_entries_after_seq(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn read_path() {
                suite::read_path(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn leaf_moves() {
                suite::leaf_moves(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn fork() {
                suite::fork(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn delete() {
                suite::delete(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn open_writer() {
                suite::open_writer(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn trait_defaults() {
                suite::trait_defaults(&$factory, &$caps).await;
            }
            #[tokio::test]
            async fn run_all() {
                suite::run_all(&$factory, &$caps).await;
            }
        }
    };
}

conformance!(memory_store, memory, Capabilities::memory());
conformance!(sqlite_store, sqlite, Capabilities::sqlite());
conformance!(jsonl_store, jsonl, Capabilities::jsonl());
