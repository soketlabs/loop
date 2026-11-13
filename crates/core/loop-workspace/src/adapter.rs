//! Adapters from the old sandbox traits to the Workspace traits.
//!
//! An existing [`Sandbox`] becomes a [`Workspace`] with [`SandboxWorkspace`]:
//! every sandbox method is forwarded unchanged, and the new methods use
//! their "unsupported" defaults. An existing [`SandboxFactory`] becomes a
//! [`WorkspaceDriver`] with [`FactoryDriver`]. The backends do not change.

use std::sync::Arc;

use async_trait::async_trait;

use crate::capabilities::WorkspaceCapabilities;
use crate::env::ExecutionEnv;
use crate::error::WorkspaceError;
use crate::platform::HostOs;
use crate::sandbox::{Sandbox, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus};
use crate::workspace::{Workspace, WorkspaceDriver, WorkspaceSpec};

/// A [`Sandbox`] seen as a [`Workspace`].
pub struct SandboxWorkspace {
    inner: Arc<dyn Sandbox>,
    driver: String,
    capabilities: WorkspaceCapabilities,
}

impl SandboxWorkspace {
    /// Wrap `inner`, made by driver `driver`, with `capabilities`.
    pub fn new(
        inner: Arc<dyn Sandbox>,
        driver: impl Into<String>,
        capabilities: WorkspaceCapabilities,
    ) -> Self {
        Self {
            inner,
            driver: driver.into(),
            capabilities,
        }
    }

    /// The wrapped sandbox.
    pub fn inner(&self) -> &Arc<dyn Sandbox> {
        &self.inner
    }
}

#[async_trait]
impl Sandbox for SandboxWorkspace {
    fn kind(&self) -> &str {
        self.inner.kind()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn status(&self) -> SandboxStatus {
        self.inner.status()
    }
    fn info(&self) -> SandboxInfo {
        self.inner.info()
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.inner.env()
    }
    async fn start(&self) -> Result<(), SandboxError> {
        self.inner.start().await
    }
    async fn stop(&self) -> Result<(), SandboxError> {
        self.inner.stop().await
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.inner.destroy().await
    }
    fn outlives_process(&self) -> bool {
        self.inner.outlives_process()
    }
}

#[async_trait]
impl Workspace for SandboxWorkspace {
    fn driver_id(&self) -> &str {
        &self.driver
    }
    fn capabilities(&self) -> WorkspaceCapabilities {
        let mut caps = self.capabilities.clone();
        caps.outlives_process = self.inner.outlives_process();
        caps
    }
}

/// Computes the capabilities of one Workspace from its spec.
pub type CapabilitiesFn = fn(&WorkspaceSpec) -> WorkspaceCapabilities;

/// A [`SandboxFactory`] seen as a [`WorkspaceDriver`].
pub struct FactoryDriver {
    factory: Arc<dyn SandboxFactory>,
    id: String,
    platforms: Option<Vec<HostOs>>,
    capabilities: CapabilitiesFn,
}

impl FactoryDriver {
    /// Driver for `factory`. Its id and kind are the factory kind, it
    /// supports every platform, and it reports default capabilities.
    pub fn new(factory: Arc<dyn SandboxFactory>) -> Self {
        let id = factory.kind().to_owned();
        Self {
            factory,
            id,
            platforms: None,
            capabilities: |_| WorkspaceCapabilities::default(),
        }
    }

    /// Use `id` as the driver id (the kind stays the factory kind).
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Support only these platforms.
    pub fn with_platforms(mut self, platforms: &[HostOs]) -> Self {
        self.platforms = Some(platforms.to_vec());
        self
    }

    /// Report capabilities from the spec with `capabilities`.
    pub fn with_capabilities(mut self, capabilities: CapabilitiesFn) -> Self {
        self.capabilities = capabilities;
        self
    }
}

#[async_trait]
impl WorkspaceDriver for FactoryDriver {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &str {
        self.factory.kind()
    }
    fn capabilities(&self) -> WorkspaceCapabilities {
        (self.capabilities)(&WorkspaceSpec::default())
    }
    fn supports(&self, os: HostOs) -> bool {
        self.platforms
            .as_ref()
            .is_none_or(|platforms| platforms.contains(&os))
    }
    async fn create(&self, spec: WorkspaceSpec) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let capabilities = (self.capabilities)(&spec);
        let sandbox = self.factory.create(spec).await?;
        Ok(Arc::new(SandboxWorkspace::new(
            sandbox,
            self.id.clone(),
            capabilities,
        )))
    }
}
