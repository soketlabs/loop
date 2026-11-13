//! Shared conformance suites for Loop stores.
//!
//! A store that lives outside this repository (for example a private
//! Postgres store) can run the same checks as the stores in this repository.
//! Add this crate as a dev-dependency and call a suite function with a
//! factory. The factory builds one empty, isolated store per call.
//!
//! ```ignore
//! use loop_store_testkit::event_log;
//!
//! #[tokio::test]
//! async fn my_event_log_conforms() {
//!     event_log::run_all(|| async {
//!         event_log::fixture(MyEventLog::connect_to_fresh_schema().await)
//!     })
//!     .await;
//! }
//! ```
//!
//! Features: `event-log` (the Event Log suite) and `session-store` (the
//! Session store suite). Both are on by default.

use std::any::Any;
use std::sync::Arc;

#[cfg(feature = "event-log")]
pub mod event_log;
#[cfg(feature = "session-store")]
pub mod session_store;

/// A store under test plus anything that must live as long as it.
///
/// A factory returns one of these. The guard keeps a temporary directory,
/// a database container or a schema alive until the test ends.
pub struct Fixture<T: ?Sized> {
    /// The store under test. It must start empty.
    pub store: Arc<T>,
    _guard: Option<Box<dyn Any + Send + Sync>>,
}

impl<T: ?Sized> Fixture<T> {
    /// A store with nothing else to keep alive.
    pub fn new(store: Arc<T>) -> Self {
        Self {
            store,
            _guard: None,
        }
    }

    /// Keep `guard` alive for as long as the fixture lives.
    pub fn with_guard(mut self, guard: impl Any + Send + Sync) -> Self {
        self._guard = Some(Box::new(guard));
        self
    }
}
