//! Loop CLI library (shared by the `loop` binary and tests).

#![allow(missing_docs)]

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod keybindings;
pub mod mcp_serve;
pub mod tui;

pub use loop_app_core::{
    self as config, bootstrap, build_models, build_tools, extensions, hooks_load, mcp_server_entries,
    resources, runtime, system_prompt, theme, tool_approval, BootstrapOpts, Runtime, ToolApprovalBridge,
};

/// CLI runtime with terminal keybindings.
pub struct CliRuntime {
    /// Shared application runtime.
    pub inner: Runtime,
    /// Terminal keybindings.
    pub keybindings: keybindings::Keybindings,
}

impl std::ops::Deref for CliRuntime {
    type Target = Runtime;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for CliRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Bootstrap CLI runtime (adds keybindings to shared core).
pub async fn bootstrap_cli(opts: BootstrapOpts) -> anyhow::Result<CliRuntime> {
    let inner = bootstrap(opts).await?;
    let keybindings =
        keybindings::Keybindings::load(&config::paths::keybindings_path(&inner.agent_dir))?;
    Ok(CliRuntime { inner, keybindings })
}
