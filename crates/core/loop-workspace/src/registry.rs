//! Registry of Workspace drivers, and the process-wide entry point for
//! drivers that live in other crates.

use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

use crate::adapter::FactoryDriver;
use crate::error::WorkspaceError;
use crate::platform::HostOs;
use crate::sandbox::{Sandbox, SandboxConfig, SandboxError, SandboxFactory};
use crate::workspace::WorkspaceDriver;

/// Ordered set of drivers, keyed by driver id.
///
/// Selection by kind takes the first driver, in registration order, that
/// serves the kind and supports the platform. Registering an id that exists
/// replaces that driver in its place.
#[derive(Default)]
pub struct WorkspaceDriverRegistry {
    drivers: RwLock<Vec<Arc<dyn WorkspaceDriver>>>,
}

/// The old name. `SandboxRegistry::register` and `create` keep their
/// behaviour.
pub type SandboxRegistry = WorkspaceDriverRegistry;

impl WorkspaceDriverRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `driver`, replacing a driver with the same id in its place.
    pub fn register_driver(&self, driver: Arc<dyn WorkspaceDriver>) {
        let mut drivers = self.drivers.write();
        match drivers.iter().position(|d| d.id() == driver.id()) {
            Some(at) => drivers[at] = driver,
            None => drivers.push(driver),
        }
    }

    /// Register an old-style factory (replaces every driver of its kind, as
    /// the old registry replaced the factory of a kind). It supports every
    /// platform.
    pub fn register(&self, factory: Arc<dyn SandboxFactory>) {
        let driver: Arc<dyn WorkspaceDriver> = Arc::new(FactoryDriver::new(factory));
        let mut drivers = self.drivers.write();
        let at = drivers.iter().position(|d| d.kind() == driver.kind());
        drivers.retain(|d| d.kind() != driver.kind());
        let at = at.unwrap_or(drivers.len()).min(drivers.len());
        drivers.insert(at, driver);
    }

    /// Add every driver registered with [`register_workspace_driver`].
    pub fn with_registered_drivers(self) -> Self {
        for driver in registered_workspace_drivers() {
            self.register_driver(driver);
        }
        self
    }

    /// The driver with id `id`.
    pub fn driver(&self, id: &str) -> Option<Arc<dyn WorkspaceDriver>> {
        self.drivers.read().iter().find(|d| d.id() == id).cloned()
    }

    /// Driver ids in registration order.
    pub fn driver_ids(&self) -> Vec<String> {
        self.drivers
            .read()
            .iter()
            .map(|d| d.id().to_owned())
            .collect()
    }

    /// Kinds served by at least one driver, in registration order.
    pub fn kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = Vec::new();
        for driver in self.drivers.read().iter() {
            if !kinds.iter().any(|k| k == driver.kind()) {
                kinds.push(driver.kind().to_owned());
            }
        }
        kinds
    }

    /// The driver for sandbox kind `kind` on `os`: the first that serves the
    /// kind and supports the platform.
    pub fn select(
        &self,
        kind: &str,
        os: HostOs,
    ) -> Result<Arc<dyn WorkspaceDriver>, WorkspaceError> {
        let drivers = self.drivers.read();
        let mut of_kind = drivers.iter().filter(|d| d.kind() == kind).peekable();
        let first = of_kind
            .peek()
            .map(|d| d.id().to_owned())
            .ok_or_else(|| WorkspaceError::UnknownKind(kind.to_owned()))?;
        of_kind
            .find(|d| d.supports(os))
            .cloned()
            .ok_or(WorkspaceError::PlatformUnsupported {
                driver: first,
                platform: os,
            })
    }

    /// Create a sandbox of `kind` on this platform (the old registry call).
    /// Not started. The error texts are the old ones.
    pub async fn create(
        &self,
        kind: &str,
        config: SandboxConfig,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let driver = self
            .select(kind, HostOs::current())
            .map_err(|_| SandboxError::Other(format!("unknown sandbox kind: {kind}")))?;
        let workspace = driver.create(config).await?;
        Ok(workspace)
    }
}

fn global() -> &'static RwLock<Vec<Arc<dyn WorkspaceDriver>>> {
    static GLOBAL: OnceLock<RwLock<Vec<Arc<dyn WorkspaceDriver>>>> = OnceLock::new();
    GLOBAL.get_or_init(Default::default)
}

/// Register a driver for this process. A driver crate outside this
/// repository calls this at startup, before the Runtime builds its first
/// Workspace. The platform registry of the app adds these drivers after the
/// built-in ones; an id that exists replaces the built-in driver.
pub fn register_workspace_driver(driver: Arc<dyn WorkspaceDriver>) {
    let mut drivers = global().write();
    match drivers.iter().position(|d| d.id() == driver.id()) {
        Some(at) => drivers[at] = driver,
        None => drivers.push(driver),
    }
}

/// Drivers registered with [`register_workspace_driver`], in order.
pub fn registered_workspace_drivers() -> Vec<Arc<dyn WorkspaceDriver>> {
    global().read().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::WorkspaceCapabilities;
    use crate::workspace::{Workspace, WorkspaceSpec};
    use async_trait::async_trait;

    struct Named {
        id: &'static str,
        kind: &'static str,
        os: Option<HostOs>,
    }

    #[async_trait]
    impl WorkspaceDriver for Named {
        fn id(&self) -> &str {
            self.id
        }
        fn kind(&self) -> &str {
            self.kind
        }
        fn capabilities(&self) -> WorkspaceCapabilities {
            WorkspaceCapabilities::default()
        }
        fn supports(&self, os: HostOs) -> bool {
            self.os.is_none_or(|o| o == os)
        }
        async fn create(&self, _: WorkspaceSpec) -> Result<Arc<dyn Workspace>, WorkspaceError> {
            Err(WorkspaceError::DriverUnavailable {
                driver: self.id.into(),
                reason: "test".into(),
            })
        }
    }

    fn named(id: &'static str, kind: &'static str, os: Option<HostOs>) -> Arc<dyn WorkspaceDriver> {
        Arc::new(Named { id, kind, os })
    }

    #[test]
    fn select_checks_kind_then_platform() {
        let reg = WorkspaceDriverRegistry::new();
        reg.register_driver(named("podman", "local", Some(HostOs::Linux)));
        reg.register_driver(named("seatbelt", "local", Some(HostOs::MacOs)));
        assert_eq!(reg.select("local", HostOs::Linux).unwrap().id(), "podman");
        assert_eq!(reg.select("local", HostOs::MacOs).unwrap().id(), "seatbelt");
        assert!(matches!(
            reg.select("local", HostOs::Other),
            Err(WorkspaceError::PlatformUnsupported { .. })
        ));
        assert!(matches!(
            reg.select("remote", HostOs::Linux),
            Err(WorkspaceError::UnknownKind(_))
        ));
        assert_eq!(reg.kinds(), vec!["local".to_owned()]);
        assert_eq!(reg.driver_ids(), vec!["podman", "seatbelt"]);
    }

    #[test]
    fn same_id_replaces_in_place() {
        let reg = WorkspaceDriverRegistry::new();
        reg.register_driver(named("a", "local", None));
        reg.register_driver(named("b", "local", None));
        reg.register_driver(named("a", "remote", None));
        assert_eq!(reg.driver_ids(), vec!["a", "b"]);
        assert_eq!(reg.driver("a").unwrap().kind(), "remote");
    }

    #[tokio::test]
    async fn unknown_kind_keeps_the_old_text() {
        let reg = SandboxRegistry::new();
        let err = match reg.create("nope", SandboxConfig::default()).await {
            Err(e) => e,
            Ok(_) => panic!("created"),
        };
        assert_eq!(err.to_string(), "unknown sandbox kind: nope");
    }

    #[test]
    fn global_registration_is_seen_by_new_registries() {
        register_workspace_driver(named("registry-test-external", "remote", None));
        let reg = WorkspaceDriverRegistry::new().with_registered_drivers();
        assert!(reg.driver("registry-test-external").is_some());
        assert_eq!(
            reg.select("remote", HostOs::current()).unwrap().id(),
            "registry-test-external"
        );
    }
}
