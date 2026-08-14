//! Sandbox trait surface for N local/remote backends.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::harness::types::ExecutionEnv;

/// Lifecycle status of a sandbox instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// Created but not started.
    Created,
    /// Starting.
    Starting,
    /// Ready for tool execution.
    Ready,
    /// Stopping.
    Stopping,
    /// Stopped.
    Stopped,
    /// Failed.
    Failed,
}

impl SandboxStatus {
    /// Stable lowercase label for status displays.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for SandboxStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Printable sandbox details for `/sandbox status` (not sent to the model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInfo {
    /// Box title (e.g. `Sandbox`).
    pub title: String,
    /// Ordered label / value rows.
    pub fields: Vec<(String, String)>,
}

impl SandboxInfo {
    /// Status when tools run on the host (sandbox off).
    pub fn off() -> Self {
        Self {
            title: "Sandbox".into(),
            fields: vec![
                ("Mode".into(), "off".into()),
                ("Tools".into(), "host".into()),
            ],
        }
    }

    /// Build an info card for an enabled sandbox.
    pub fn enabled(kind: impl Into<String>, fields: Vec<(String, String)>) -> Self {
        let mut rows = vec![
            ("Mode".into(), "on".into()),
            ("Kind".into(), kind.into()),
        ];
        rows.extend(fields);
        Self {
            title: "Sandbox".into(),
            fields: rows,
        }
    }

    /// Render as a unicode box suitable for the CLI transcript.
    pub fn format_box(&self) -> String {
        const PAD: usize = 2;
        let label_w = self
            .fields
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        let row_w = self
            .fields
            .iter()
            .map(|(_, v)| PAD + label_w + 2 + v.chars().count() + PAD)
            .max()
            .unwrap_or(16);
        let title = format!(" {} ", self.title);
        let title_w = title.chars().count();
        let inner = row_w.max(title_w + 4).max(20);

        let mut out = String::new();
        // Title centered in the top border.
        let side = inner.saturating_sub(title_w);
        let left = side / 2;
        let right = side - left;
        out.push('╭');
        out.push_str(&"─".repeat(left));
        out.push_str(&title);
        out.push_str(&"─".repeat(right));
        out.push('╮');
        out.push('\n');

        for (label, value) in &self.fields {
            let content = format!(
                "{pad}{label:<label_w$}  {value}",
                pad = " ".repeat(PAD),
                label_w = label_w,
            );
            let used = content.chars().count();
            let fill = inner.saturating_sub(used);
            out.push('│');
            out.push_str(&content);
            out.push_str(&" ".repeat(fill));
            out.push('│');
            out.push('\n');
        }

        out.push('╰');
        out.push_str(&"─".repeat(inner));
        out.push('╯');
        out
    }
}

/// Sandbox error.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// Not ready.
    #[error("sandbox not ready: {0}")]
    NotReady(String),
    /// Start failed.
    #[error("start failed: {0}")]
    StartFailed(String),
    /// Path escape / policy.
    #[error("policy: {0}")]
    Policy(String),
    /// Other.
    #[error("{0}")]
    Other(String),
}

/// Configuration for creating a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// Logical workspace root (host path; bind-mounted at the same path in the guest).
    pub workdir: PathBuf,
    /// Opaque backend options.
    #[serde(default)]
    pub options: Value,
    /// Labels for observability.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// A sandbox yields an [`ExecutionEnv`] for tool calls when enabled.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Stable kind id (`local`, `remote`, …).
    fn kind(&self) -> &str;
    /// Instance id.
    fn id(&self) -> &str;
    /// Current status.
    fn status(&self) -> SandboxStatus;
    /// Printable details for `/sandbox status` (CLI only; not sent to the model).
    fn info(&self) -> SandboxInfo;
    /// Underlying env tools must use when status is Ready.
    fn env(&self) -> Arc<dyn ExecutionEnv>;
    /// Start the sandbox.
    async fn start(&self) -> Result<(), SandboxError>;
    /// Stop the sandbox.
    async fn stop(&self) -> Result<(), SandboxError>;
    /// Destroy / cleanup (idempotent).
    async fn destroy(&self) -> Result<(), SandboxError>;
}

/// Factory for a sandbox kind.
#[async_trait]
pub trait SandboxFactory: Send + Sync {
    /// Kind id.
    fn kind(&self) -> &str;
    /// Create an instance.
    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError>;
}

/// Harness sandbox mode.
#[derive(Clone, Default)]
pub enum SandboxMode {
    /// Use host ExecutionEnv for tools.
    #[default]
    Disabled,
    /// Use the given sandbox env for tools.
    Enabled {
        /// Sandbox instance.
        sandbox: Arc<dyn Sandbox>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_box_contains_mode() {
        let box_text = SandboxInfo::off().format_box();
        assert!(box_text.contains("Sandbox"));
        assert!(box_text.contains("Mode"));
        assert!(box_text.contains("off"));
        assert!(box_text.contains("host"));
        assert!(box_text.starts_with('╭'));
        assert!(box_text.contains('╰'));
    }

    #[test]
    fn enabled_box_rows_align() {
        let info = SandboxInfo::enabled(
            "local",
            vec![
                ("Status".into(), "ready".into()),
                ("Runtime".into(), "runc".into()),
            ],
        );
        let box_text = info.format_box();
        assert!(box_text.contains("Kind"));
        assert!(box_text.contains("local"));
        assert!(box_text.contains("runc"));
        for line in box_text.lines() {
            assert!(
                line.starts_with('╭')
                    || line.starts_with('╰')
                    || (line.starts_with('│') && line.ends_with('│')),
                "bad box line: {line}"
            );
        }
    }
}
