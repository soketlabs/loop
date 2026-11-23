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
/// serves the kind and supports the platform. Built-in drivers are added
/// with [`register_builtin`](Self::register_builtin); another driver cannot
/// take a built-in id except through
/// [`replace_builtin`](Self::replace_builtin), which logs the replacement.
#[derive(Default)]
pub struct WorkspaceDriverRegistry {
    drivers: RwLock<Vec<Entry>>,
}

struct Entry {
    driver: Arc<dyn WorkspaceDriver>,
    builtin: bool,
}

/// The old name. `SandboxRegistry::register` and `create` keep their
/// behaviour.
pub type SandboxRegistry = WorkspaceDriverRegistry;

impl WorkspaceDriverRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn put(&self, driver: Arc<dyn WorkspaceDriver>, builtin: bool) {
        let mut drivers = self.drivers.write();
        let entry = Entry { driver, builtin };
        match drivers
            .iter()
            .position(|e| e.driver.id() == entry.driver.id())
        {
            Some(at) => drivers[at] = entry,
            None => drivers.push(entry),
        }
    }

    /// Register a built-in driver (replaces a driver with the same id).
    pub fn register_builtin(&self, driver: Arc<dyn WorkspaceDriver>) {
        self.put(driver, true);
    }

    /// Register `driver`. A driver with the same id that is not built in is
    /// replaced in its place. Taking a built-in id is refused.
    pub fn register_driver(&self, driver: Arc<dyn WorkspaceDriver>) -> Result<(), WorkspaceError> {
        if self.is_builtin(driver.id()) {
            return Err(WorkspaceError::BuiltinId(driver.id().to_owned()));
        }
        self.put(driver, false);
        Ok(())
    }

    /// Replace the built-in driver with the same id, on purpose. Logged.
    pub fn replace_builtin(&self, driver: Arc<dyn WorkspaceDriver>) {
        tracing::warn!(
            driver = driver.id(),
            "workspace driver replaces a built-in driver"
        );
        self.put(driver, false);
    }

    fn is_builtin(&self, id: &str) -> bool {
        self.drivers
            .read()
            .iter()
            .any(|e| e.builtin && e.driver.id() == id)
    }

    /// Register an old-style factory (replaces every driver of its kind, as
    /// the old registry replaced the factory of a kind). It supports every
    /// platform.
    pub fn register(&self, factory: Arc<dyn SandboxFactory>) {
        let driver: Arc<dyn WorkspaceDriver> = Arc::new(FactoryDriver::new(factory));
        let mut drivers = self.drivers.write();
        let at = drivers
            .iter()
            .position(|e| e.driver.kind() == driver.kind());
        drivers.retain(|e| e.driver.kind() != driver.kind());
        let at = at.unwrap_or(drivers.len()).min(drivers.len());
        drivers.insert(
            at,
            Entry {
                driver,
                builtin: false,
            },
        );
    }

    /// Add every driver registered with [`register_workspace_driver`] and
    /// [`replace_builtin_workspace_driver`]. A plain registration that takes
    /// a built-in id is refused and logged.
    pub fn with_registered_drivers(self) -> Self {
        for (driver, replace) in global().read().iter() {
            if *replace {
                self.replace_builtin(driver.clone());
            } else if let Err(error) = self.register_driver(driver.clone()) {
                tracing::error!(%error, "workspace driver not registered");
            }
        }
        self
    }

    /// The driver with id `id`.
    pub fn driver(&self, id: &str) -> Option<Arc<dyn WorkspaceDriver>> {
        self.drivers
            .read()
            .iter()
            .find(|e| e.driver.id() == id)
            .map(|e| e.driver.clone())
    }

    /// Driver ids in registration order.
    pub fn driver_ids(&self) -> Vec<String> {
        self.drivers
            .read()
            .iter()
            .map(|e| e.driver.id().to_owned())
            .collect()
    }

    /// Kinds that at least one driver serves on this platform, in
    /// registration order (the old meaning: kinds that can be created here).
    pub fn kinds(&self) -> Vec<String> {
        self.kinds_where(|d| d.supports(HostOs::current()))
    }

    /// Kinds served by any registered driver, on any platform.
    pub fn all_kinds(&self) -> Vec<String> {
        self.kinds_where(|_| true)
    }

    fn kinds_where(&self, keep: impl Fn(&dyn WorkspaceDriver) -> bool) -> Vec<String> {
        let mut kinds: Vec<String> = Vec::new();
        for entry in self.drivers.read().iter() {
            let driver = entry.driver.as_ref();
            if keep(driver) && !kinds.iter().any(|k| k == driver.kind()) {
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
        let mut of_kind = drivers
            .iter()
            .map(|e| &e.driver)
            .filter(|d| d.kind() == kind)
            .peekable();
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
        let workspace = driver.create(config.into()).await?;
        Ok(workspace)
    }
}

type Global = RwLock<Vec<(Arc<dyn WorkspaceDriver>, bool)>>;

fn global() -> &'static Global {
    static GLOBAL: OnceLock<Global> = OnceLock::new();
    GLOBAL.get_or_init(Default::default)
}

fn put_global(driver: Arc<dyn WorkspaceDriver>, replace: bool) {
    let mut drivers = global().write();
    match drivers.iter().position(|(d, _)| d.id() == driver.id()) {
        Some(at) => drivers[at] = (driver, replace),
        None => drivers.push((driver, replace)),
    }
}

/// Register a driver for this process. A driver crate outside this
/// repository calls this at startup, before the Runtime builds its first
/// Workspace. The platform registry of the app adds these drivers after the
/// built-in ones. A driver that takes a built-in id is refused there (and
/// logged); use [`replace_builtin_workspace_driver`] for that.
pub fn register_workspace_driver(driver: Arc<dyn WorkspaceDriver>) {
    put_global(driver, false);
}

/// Register a driver that replaces the built-in driver with the same id.
pub fn replace_builtin_workspace_driver(driver: Arc<dyn WorkspaceDriver>) {
    put_global(driver, true);
}

/// Drivers registered with [`register_workspace_driver`] or
/// [`replace_builtin_workspace_driver`], in order.
pub fn registered_workspace_drivers() -> Vec<Arc<dyn WorkspaceDriver>> {
    global().read().iter().map(|(d, _)| d.clone()).collect()
}

/// Remove every process-wide registration. For tests.
pub fn reset_registered_workspace_drivers() {
    global().write().clear();
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
        reg.register_builtin(named("podman", "local", Some(HostOs::Linux)));
        reg.register_builtin(named("seatbelt", "local", Some(HostOs::MacOs)));
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
        assert_eq!(reg.all_kinds(), vec!["local".to_owned()]);
        assert_eq!(reg.driver_ids(), vec!["podman", "seatbelt"]);
    }

    #[test]
    fn same_id_replaces_in_place() {
        let reg = WorkspaceDriverRegistry::new();
        reg.register_driver(named("a", "local", None)).unwrap();
        reg.register_driver(named("b", "local", None)).unwrap();
        reg.register_driver(named("a", "remote", None)).unwrap();
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
    fn builtin_ids_need_an_explicit_replace() {
        let reg = WorkspaceDriverRegistry::new();
        reg.register_builtin(named("podman", "local", None));
        assert!(matches!(
            reg.register_driver(named("podman", "remote", None)),
            Err(WorkspaceError::BuiltinId(_))
        ));
        assert_eq!(reg.driver("podman").unwrap().kind(), "local");
        reg.replace_builtin(named("podman", "remote", None));
        assert_eq!(reg.driver("podman").unwrap().kind(), "remote");
    }

    #[test]
    fn kinds_are_those_usable_here() {
        let reg = WorkspaceDriverRegistry::new();
        let elsewhere = if HostOs::current() == HostOs::Linux {
            HostOs::MacOs
        } else {
            HostOs::Linux
        };
        reg.register_builtin(named("x", "far", Some(elsewhere)));
        reg.register_builtin(named("y", "near", None));
        assert_eq!(reg.kinds(), vec!["near".to_owned()]);
        assert_eq!(reg.all_kinds(), vec!["far".to_owned(), "near".to_owned()]);
    }

    // One test owns the process-wide list, so tests do not race on it.
    #[test]
    fn global_registration_is_seen_by_new_registries() {
        reset_registered_workspace_drivers();
        register_workspace_driver(named("registry-test-external", "remote", None));
        register_workspace_driver(named("podman", "remote", None));
        let reg = WorkspaceDriverRegistry::new();
        reg.register_builtin(named("podman", "local", None));
        let reg = reg.with_registered_drivers();
        assert!(reg.driver("registry-test-external").is_some());
        assert_eq!(
            reg.driver("podman").unwrap().kind(),
            "local",
            "built-in kept"
        );
        assert_eq!(
            reg.select("remote", HostOs::current()).unwrap().id(),
            "registry-test-external"
        );
        reset_registered_workspace_drivers();
        replace_builtin_workspace_driver(named("podman", "remote", None));
        let reg = WorkspaceDriverRegistry::new();
        reg.register_builtin(named("podman", "local", None));
        let reg = reg.with_registered_drivers();
        assert_eq!(
            reg.driver("podman").unwrap().kind(),
            "remote",
            "explicit replace"
        );
        reset_registered_workspace_drivers();
        assert!(registered_workspace_drivers().is_empty());
    }
}
