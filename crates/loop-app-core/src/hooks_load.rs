//! Declarative JSON hooks → harness HookRegistry.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use loop_agent::harness::{HarnessHookEvent, HookOutcome, HookRegistry};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookFile {
    #[allow(dead_code)]
    name: Option<String>,
    on: String,
    #[serde(default)]
    #[allow(dead_code)]
    match_rules: Option<MatchRules>,
    #[serde(rename = "match", default)]
    #[allow(dead_code)]
    match_alt: Option<MatchRules>,
    action: HookAction,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct MatchRules {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    args_contains: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookAction {
    #[serde(default)]
    block: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

/// Load JSON hook files and register them on the harness.
pub fn register_json_hooks(harness: &loop_agent::harness::AgentHarness, paths: &[std::path::PathBuf]) {
    let mut hooks: Vec<HookFile> = Vec::new();
    for path in paths {
        if let Ok(raw) = std::fs::read_to_string(path) {
            match serde_json::from_str::<HookFile>(&raw) {
                Ok(h) => hooks.push(h),
                Err(e) => tracing::warn!("invalid hook {}: {e}", path.display()),
            }
        }
    }
    if hooks.is_empty() {
        return;
    }
    let hooks = Arc::new(hooks);
    harness.on(move |event| {
        let hooks = Arc::clone(&hooks);
        async move {
            let mut outcome = HookOutcome::default();
            for hook in hooks.iter() {
                if event_matches(&hook.on, &event) {
                    if hook.action.block {
                        outcome.cancel = true;
                        if let Some(r) = &hook.action.reason {
                            tracing::warn!("hook blocked: {r}");
                        }
                    }
                    if outcome.summary.is_none() {
                        outcome.summary = hook.action.summary.clone();
                    }
                }
            }
            outcome
        }
    });
}

fn event_matches(on: &str, event: &HarnessHookEvent) -> bool {
    match (on, event) {
        ("before_agent_start" | "BeforeAgentStart", HarnessHookEvent::BeforeAgentStart { .. }) => {
            true
        }
        (
            "session_before_compact" | "SessionBeforeCompact",
            HarnessHookEvent::SessionBeforeCompact { .. },
        ) => true,
        (
            "session_before_tree" | "SessionBeforeTree",
            HarnessHookEvent::SessionBeforeTree { .. },
        ) => true,
        ("settled" | "Settled", HarnessHookEvent::Settled) => true,
        ("queue_update" | "QueueUpdate", HarnessHookEvent::QueueUpdate) => true,
        ("shutdown_requested" | "ShutdownRequested", HarnessHookEvent::ShutdownRequested) => true,
        // tool_call not yet a harness event — ignore gracefully
        ("tool_call" | "tool_result", _) => false,
        _ => false,
    }
}

/// Validate a single hook file (tests / /reload).
pub fn parse_hook_file(path: &Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let _: HookFile = serde_json::from_str(&raw)?;
    Ok(())
}

/// Attach hooks to a registry directly (for tests).
pub fn load_hooks_into_registry(registry: &HookRegistry, paths: &[std::path::PathBuf]) {
    let mut hooks: Vec<HookFile> = Vec::new();
    for path in paths {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(h) = serde_json::from_str::<HookFile>(&raw) {
                hooks.push(h);
            }
        }
    }
    let hooks = Arc::new(hooks);
    registry.on(move |event| {
        let hooks = Arc::clone(&hooks);
        async move {
            let mut outcome = HookOutcome::default();
            for hook in hooks.iter() {
                if event_matches(&hook.on, &event) && hook.action.block {
                    outcome.cancel = true;
                }
            }
            outcome
        }
    });
}
