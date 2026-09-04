//! Rhai extension host (pi ExtensionAPI subset).

use std::path::Path;
use std::sync::{Arc, Mutex};

use rhai::{Engine, Scope};

/// Commands registered by extensions.
#[derive(Debug, Clone)]
pub struct ExtensionCommand {
    /// Slash command name (without `/`).
    pub name: String,
    /// Description.
    pub description: String,
}

/// State contributed by extensions.
#[derive(Debug, Default)]
pub struct ExtensionState {
    /// Registered slash commands.
    pub commands: Vec<ExtensionCommand>,
    /// Notification messages to show at startup.
    pub notices: Vec<String>,
}

/// Load and run Rhai extension scripts.
pub fn load_extensions(paths: &[std::path::PathBuf]) -> ExtensionState {
    let state = Arc::new(Mutex::new(ExtensionState::default()));
    let mut engine = Engine::new();

    {
        let state_cmd = Arc::clone(&state);
        engine.register_fn(
            "register_command",
            move |name: &str, description: &str| {
                if let Ok(mut s) = state_cmd.lock() {
                    s.commands.push(ExtensionCommand {
                        name: name.to_string(),
                        description: description.to_string(),
                    });
                }
            },
        );
    }
    {
        let state_n = Arc::clone(&state);
        engine.register_fn("notify", move |msg: &str| {
            if let Ok(mut s) = state_n.lock() {
                s.notices.push(msg.to_string());
            }
        });
    }

    // Stubs for API surface parity (no-ops until deeper integration).
    engine.register_fn("register_tool", |_name: &str, _desc: &str| {});
    engine.register_fn("on", |_event: &str| {});
    engine.register_fn("set_model", |_provider: &str, _model: &str| {});
    engine.register_fn("set_active_tools", |_tools: &str| {});
    engine.register_fn("send_user_message", |_msg: &str| {});

    for path in paths {
        if let Err(e) = run_extension(&mut engine, path) {
            tracing::warn!("extension {}: {e}", path.display());
        }
    }

    Arc::try_unwrap(state)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone_state())
}

trait CloneState {
    fn clone_state(&self) -> ExtensionState;
}

impl CloneState for ExtensionState {
    fn clone_state(&self) -> ExtensionState {
        ExtensionState {
            commands: self.commands.clone(),
            notices: self.notices.clone(),
        }
    }
}

fn run_extension(engine: &mut Engine, path: &Path) -> anyhow::Result<()> {
    let mut scope = Scope::new();
    engine.run_file_with_scope(&mut scope, path.to_path_buf())?;
    Ok(())
}
