//! Conformance suite for Loop Workspace drivers.
//!
//! A driver that lives outside this repository (for example a private
//! remote sandbox driver) runs the same checks as the drivers in this
//! repository. Add this crate as a dev-dependency and call [`run_all`] with
//! a factory. The factory returns one [`DriverFixture`] per call: the driver
//! and a spec for a new, empty Workspace.
//!
//! ```ignore
//! use loop_workspace_testkit::{run_all, DriverFixture};
//!
//! #[tokio::test]
//! async fn my_driver_conforms() {
//!     run_all(|| async {
//!         let (spec, outside, guard) = fresh_project().await;
//!         DriverFixture::new(Arc::new(MyDriver::connect().await), spec)
//!             .with_outside(outside)
//!             .with_guard(guard)
//!     })
//!     .await;
//! }
//! ```
//!
//! The checks read the driver's capabilities and check that the driver
//! does what they say: a confined driver must refuse escapes, a driver
//! without snapshots must return `WorkspaceError::Unsupported`, and so on.
//! The checks only use the contract (`Workspace`, `env()`), so they also
//! work for a remote driver whose paths are not host paths. Some extra
//! checks look at the host disk; they run only when `remote` is false.

#![warn(missing_docs)]

use std::any::Any;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use loop_workspace::{Workspace, WorkspaceDriver, WorkspaceSpec};

mod checks;
mod process_checks;
mod transfer_checks;

pub use checks::*;
pub use process_checks::*;
pub use transfer_checks::*;

/// Name of the file every fixture with an outside folder must hold there.
pub const OUTSIDE_SECRET: &str = "secret.txt";
/// Content of [`OUTSIDE_SECRET`].
pub const OUTSIDE_SECRET_TEXT: &str = "outside-sentinel";

/// Name of the sentinel secret that [`run_all`] puts in the Runtime
/// (test) process environment. No command in a Workspace may see it.
pub const SECRET_ENV: &str = "LOOP_WS_KIT_SECRET";
/// Value of [`SECRET_ENV`].
pub const SECRET_VALUE: &str = "kit-secret-sentinel";

/// Put the sentinel secret into this process's environment. Called once by
/// [`run_all`]; call it yourself before running single checks.
pub fn set_sentinel_secret() {
    static SET: std::sync::Once = std::sync::Once::new();
    // Set once, before the checks spawn commands. Edition 2021: `set_var`
    // is safe; the kit sets the same value every time.
    SET.call_once(|| std::env::set_var(SECRET_ENV, SECRET_VALUE));
}

/// A driver under test, the spec of a new Workspace, and what must live as
/// long as the test.
pub struct DriverFixture {
    /// The driver under test.
    pub driver: Arc<dyn WorkspaceDriver>,
    /// Spec for a new, empty Workspace.
    pub spec: WorkspaceSpec,
    /// A folder outside the Workspace, in the driver's namespace, that
    /// holds [`OUTSIDE_SECRET`] with [`OUTSIDE_SECRET_TEXT`]. The escape
    /// checks try to read and write it. `None` skips them.
    pub outside: Option<PathBuf>,
    /// The spec makes an idle upload expire after this time. `None` skips
    /// the expiry check.
    pub upload_idle: Option<std::time::Duration>,
    _guard: Option<Box<dyn Any + Send + Sync>>,
}

impl DriverFixture {
    /// Fixture for `driver` and `spec`, with no outside folder.
    pub fn new(driver: Arc<dyn WorkspaceDriver>, spec: WorkspaceSpec) -> Self {
        Self {
            driver,
            spec,
            outside: None,
            upload_idle: None,
            _guard: None,
        }
    }

    /// Say that the spec makes an idle upload expire after `idle`.
    pub fn with_upload_idle(mut self, idle: std::time::Duration) -> Self {
        self.upload_idle = Some(idle);
        self
    }

    /// Set the outside folder for the escape checks.
    pub fn with_outside(mut self, outside: PathBuf) -> Self {
        self.outside = Some(outside);
        self
    }

    /// Keep `guard` (a temporary folder, a remote lease) alive with the fixture.
    pub fn with_guard(mut self, guard: impl Any + Send + Sync) -> Self {
        self._guard = Some(Box::new(guard));
        self
    }

    /// Create and start a Workspace from the spec. Panics on failure.
    pub async fn start(&self) -> Arc<dyn Workspace> {
        let workspace = self
            .driver
            .create(self.spec.clone())
            .await
            .unwrap_or_else(|e| panic!("driver `{}` create: {e}", self.driver.id()));
        workspace
            .start()
            .await
            .unwrap_or_else(|e| panic!("driver `{}` start: {e}", self.driver.id()));
        workspace
    }
}

/// Run every check. Each check gets a fresh fixture from `factory`.
pub async fn run_all<F, Fut>(factory: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    set_sentinel_secret();
    capabilities_match_behaviour(&factory).await;
    lifecycle_is_idempotent(&factory).await;
    file_round_trips(&factory).await;
    path_escapes_are_refused(&factory).await;
    exec_runs_commands(&factory).await;
    exec_env_is_explicit(&factory).await;
    exec_timeout_and_cancel(&factory).await;
    exec_output_is_capped(&factory).await;
    exec_runs_concurrently(&factory).await;
    processes_conform(&factory).await;
    transfers_conform(&factory).await;
}
