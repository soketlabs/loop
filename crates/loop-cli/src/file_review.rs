//! Interactive review of agent file edits (accept / reject + external diff).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use loop_agent::harness::ExecutionEnv;
use loop_agent::{AfterToolCallContext, AfterToolCallResult};
use loop_ai::{TextContent, ToolResultContent};
use serde_json::json;
use tokio::sync::{mpsc, Mutex, oneshot};
use tokio_util::sync::CancellationToken;

/// Policy for when to prompt on write/edit tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEditReviewPolicy {
    /// Review only for brand-new sessions (not `--resume`).
    NewSession,
    /// Always review.
    Always,
    /// Never review.
    Never,
}

impl FileEditReviewPolicy {
    /// Parse settings value (`newSession` / `always` / `never`).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "on" | "true" => Self::Always,
            "never" | "off" | "false" => Self::Never,
            _ => Self::NewSession,
        }
    }

    /// Label for status / settings display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewSession => "newSession",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// Whether review is active for this session.
    pub fn enabled_for_session(self, is_new_session: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::NewSession => is_new_session,
        }
    }
}

/// User decision for a pending file edit.
#[derive(Debug, Clone)]
pub enum FileReviewDecision {
    /// Keep the change.
    Accept,
    /// Revert; optional reason is returned to the model.
    Reject { reason: Option<String> },
}

/// Prompt shown in the TUI while the agent waits.
#[derive(Debug)]
pub struct FileReviewPrompt {
    /// Absolute path that was edited.
    pub path: PathBuf,
    /// Tool name (`write` / `edit`).
    pub tool_name: String,
    /// Short relative/display path.
    pub display_path: String,
    /// Oneshoot reply channel.
    pub response_tx: oneshot::Sender<FileReviewDecision>,
}

/// Shared bridge between the agent hook and the TUI.
#[derive(Clone)]
pub struct FileReviewBridge {
    enabled: Arc<std::sync::atomic::AtomicBool>,
    prompt_tx: mpsc::UnboundedSender<FileReviewPrompt>,
    env: Arc<dyn ExecutionEnv>,
    diff_editor: Arc<Mutex<Option<String>>>,
    /// Serialize reviews so parallel tool calls don't fight over the TUI.
    gate: Arc<Mutex<()>>,
}

impl FileReviewBridge {
    /// Create a bridge; `prompt_rx` is drained by the TUI.
    pub fn new(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        enabled: bool,
    ) -> (Self, mpsc::UnboundedReceiver<FileReviewPrompt>) {
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        (
            Self {
                enabled: Arc::new(std::sync::atomic::AtomicBool::new(enabled)),
                prompt_tx,
                env,
                diff_editor: Arc::new(Mutex::new(diff_editor)),
                gate: Arc::new(Mutex::new(())),
            },
            prompt_rx,
        )
    }

    /// Enable or disable interactive review for the current session.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether review is currently enabled.
    pub fn enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Update preferred editor binary (`cursor`, `code`, or absolute path).
    pub async fn set_diff_editor(&self, editor: Option<String>) {
        *self.diff_editor.lock().await = editor;
    }

    /// Hook suitable for [`AgentHarness::set_after_tool_call`].
    pub fn after_tool_hook(
        self: &Arc<Self>,
    ) -> Arc<
        dyn Fn(
                AfterToolCallContext,
                Option<CancellationToken>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Option<AfterToolCallResult>> + Send>,
            > + Send
            + Sync,
    > {
        let bridge = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let bridge = Arc::clone(&bridge);
            Box::pin(async move { bridge.review_after_tool(ctx, cancel).await })
        })
    }

    async fn review_after_tool(
        &self,
        ctx: AfterToolCallContext,
        cancel: Option<CancellationToken>,
    ) -> Option<AfterToolCallResult> {
        if !self.enabled() || ctx.is_error {
            return None;
        }
        let name = ctx.tool_call.name.as_str();
        if name != "write" && name != "edit" {
            return None;
        }

        let path = ctx
            .result
            .details
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)?;
        let previous_path = ctx
            .result
            .details
            .get("previousPath")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let created = ctx
            .result
            .details
            .get("created")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let _lock = self.gate.lock().await;
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Some(reject_patch(
                "File edit review cancelled",
                &path,
                previous_path.as_deref(),
            ));
        }

        if let Some(prev) = &previous_path {
            let editor = self.resolve_editor().await;
            if let Some(editor) = editor {
                if let Err(e) = open_diff(&editor, prev, &path) {
                    tracing::warn!("failed to open diff editor ({editor}): {e}");
                }
            } else {
                tracing::warn!(
                    "no diff editor found (set settings.diffEditor or install cursor/code)"
                );
            }
        }

        let (response_tx, response_rx) = oneshot::channel();
        let display_path = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.display().to_string());
        if self
            .prompt_tx
            .send(FileReviewPrompt {
                path: path.clone(),
                tool_name: name.to_string(),
                display_path,
                response_tx,
            })
            .is_err()
        {
            return None;
        }

        let decision = tokio::select! {
            biased;
            _ = async {
                if let Some(c) = &cancel {
                    c.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => FileReviewDecision::Reject { reason: Some("review cancelled".into()) },
            res = response_rx => res.unwrap_or(FileReviewDecision::Reject {
                reason: Some("review UI closed".into()),
            }),
        };

        match decision {
            FileReviewDecision::Accept => {
                cleanup_snapshot(previous_path.as_deref());
                None
            }
            FileReviewDecision::Reject { reason } => {
                if let Err(e) = self.revert_change(&path, previous_path.as_deref(), created).await {
                    tracing::warn!("revert after reject failed: {e}");
                }
                cleanup_snapshot(previous_path.as_deref());
                let msg = match reason.filter(|r| !r.trim().is_empty()) {
                    Some(r) => format!(
                        "User rejected the edit to {}. Reason: {r}",
                        path.display()
                    ),
                    None => format!("User rejected the edit to {}.", path.display()),
                };
                Some(reject_patch(&msg, &path, None))
            }
        }
    }

    async fn revert_change(
        &self,
        path: &Path,
        previous_path: Option<&Path>,
        created: bool,
    ) -> Result<(), String> {
        if created {
            let _ = self.env.remove(path).await;
            return Ok(());
        }
        let Some(prev) = previous_path else {
            return Err("missing previous snapshot".into());
        };
        let bytes = tokio::fs::read(prev)
            .await
            .map_err(|e| e.to_string())?;
        self.env
            .write_file(path, &bytes)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resolve_editor(&self) -> Option<String> {
        if let Some(custom) = self.diff_editor.lock().await.clone() {
            if !custom.trim().is_empty() {
                return Some(custom);
            }
        }
        detect_diff_editor()
    }
}

fn reject_patch(
    message: &str,
    path: &Path,
    previous_path: Option<&Path>,
) -> AfterToolCallResult {
    cleanup_snapshot(previous_path);
    AfterToolCallResult {
        content: Some(vec![ToolResultContent::Text(TextContent {
            text: message.to_string(),
            text_signature: None,
        })]),
        details: Some(json!({
            "path": path,
            "rejected": true,
        })),
        is_error: Some(true),
        usage: None,
        terminate: None,
    }
}

fn cleanup_snapshot(path: Option<&Path>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

/// Prefer Cursor, then VS Code / code-server style binaries.
pub fn detect_diff_editor() -> Option<String> {
    for candidate in ["cursor", "code", "code-insiders", "codium"] {
        if which_ok(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn which_ok(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Open a side-by-side diff in Cursor / VS Code (`--diff left right`).
pub fn open_diff(editor: &str, left: &Path, right: &Path) -> Result<(), String> {
    Command::new(editor)
        .arg("--diff")
        .arg(left)
        .arg(right)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {editor}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_policy_defaults_to_new_session() {
        assert_eq!(
            FileEditReviewPolicy::parse("newSession"),
            FileEditReviewPolicy::NewSession
        );
        assert_eq!(
            FileEditReviewPolicy::parse("always"),
            FileEditReviewPolicy::Always
        );
        assert_eq!(
            FileEditReviewPolicy::parse("never"),
            FileEditReviewPolicy::Never
        );
        assert!(FileEditReviewPolicy::NewSession.enabled_for_session(true));
        assert!(!FileEditReviewPolicy::NewSession.enabled_for_session(false));
        assert!(FileEditReviewPolicy::Always.enabled_for_session(false));
        assert!(!FileEditReviewPolicy::Never.enabled_for_session(true));
    }
}
