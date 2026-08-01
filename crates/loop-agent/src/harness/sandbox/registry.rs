//! Registry of sandbox factories by kind.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::harness::sandbox::traits::{Sandbox, SandboxConfig, SandboxError, SandboxFactory};

/// Register and create sandboxes by kind id.
#[derive(Default)]
pub struct SandboxRegistry {
    factories: RwLock<HashMap<String, Arc<dyn SandboxFactory>>>,
}

impl SandboxRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory (replaces existing kind).
    pub fn register(&self, factory: Arc<dyn SandboxFactory>) {
        self.factories
            .write()
            .insert(factory.kind().to_string(), factory);
    }

    /// List registered kinds.
    pub fn kinds(&self) -> Vec<String> {
        self.factories.read().keys().cloned().collect()
    }

    /// Create a sandbox for `kind`.
    pub async fn create(
        &self,
        kind: &str,
        config: SandboxConfig,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let factory = self
            .factories
            .read()
            .get(kind)
            .cloned()
            .ok_or_else(|| SandboxError::Other(format!("unknown sandbox kind: {kind}")))?;
        factory.create(config).await
    }
}
