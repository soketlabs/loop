//! Built-in coding tools over [`ExecutionEnv`](crate::harness::types::ExecutionEnv).

mod command_policy;
mod documents;
mod file_mutation_queue;
mod plan;
mod search;
pub mod shell;
mod todo;
#[cfg(feature = "web")]
pub mod web;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use similar::{ChangeTag, TextDiff};

pub use command_policy::check_command_policy;
pub use documents::{
    run_document_reader_child, set_document_reader, DocumentReader,
    CHILD_ARG as DOCUMENT_READER_ARG,
};
pub use plan::{create_exit_plan_tool, EXIT_PLAN_TOOL};
pub use search::{
    create_glob_tool, create_grep_tool, create_ls_tool, search_tools_with_filter, SearchFilter,
    SEARCH_MAX_RESULTS,
};
pub use todo::{create_todo_tool, todos_from_details};

use crate::harness::tools::file_mutation_queue::with_file_mutation_queue;
use crate::harness::types::{ExecutionEnv, FileErrorCode, FileIdentity, ShellExecOptions};
use crate::harness::utils::{execute_shell_with_capture, truncate_head, DEFAULT_MAX_BYTES};
use crate::types::{AgentTool, AgentToolResult};
use loop_ai::{ImageContent, TextContent, ToolResultContent};

/// Added to permission-denied errors from file writes and shell commands, so
/// the agent fixes the mode instead of working on a copy elsewhere.
pub const PERMISSION_DENIED_HINT: &str = "Hint: the file is not writable. If it is clearly \
yours to change, run `chmod u+w <file>` and retry; otherwise say you could not change it. \
Do not edit a copy elsewhere.";

/// A failed file write as tool error text: a permission error on a file
/// (not a sandbox refusal, which `chmod` cannot fix) carries
/// [`PERMISSION_DENIED_HINT`], whichever environment raised it.
fn write_error(error: crate::harness::types::FileError) -> String {
    let text = error.to_string();
    if error.code == FileErrorCode::PermissionDenied
        && !text.contains(PERMISSION_DENIED_HINT)
        && !text.contains("sandbox")
    {
        format!("{text}. {PERMISSION_DENIED_HINT}")
    } else {
        text
    }
}

/// Whether failed bash output shows a file write refused by its mode, which
/// [`PERMISSION_DENIED_HINT`] can fix: a redirect or `cannot create` from
/// the shell, a refused `cp`/`mv`/`tee`/`touch`/`sed`, or a read-only file
/// system. Not in a sandbox (where `chmod` does not help), and not for other
/// refusals such as `Permission denied (publickey)`.
fn bash_permission_hint(output: &str, exit_code: i32, sandboxed: bool) -> bool {
    const WRITERS: &[&str] = &[
        "bash:",
        "sh:",
        "zsh:",
        "/bin/sh:",
        "/bin/bash:",
        "dash:",
        "cp:",
        "mv:",
        "tee:",
        "touch:",
        "sed:",
        "install:",
        "ln:",
        "mkdir:",
    ];
    if exit_code == 0 || sandboxed {
        return false;
    }
    output.lines().any(|line| {
        let line = line.trim().to_lowercase();
        line.contains("read-only file system")
            || (line.contains("permission denied")
                && !line.contains("publickey")
                && (line.contains("cannot create")
                    || WRITERS
                        .iter()
                        .any(|w| line.starts_with(w) || line.starts_with(&w.replace(':', "")))))
    })
}

pub(crate) fn schema_object(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

pub(crate) fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field `{key}`"))
}

/// Create a read tool bound to `env`.
pub fn create_read_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "read",
        "Read",
        format!(
            "Read a file's contents (text or image).{}",
            documents::READ_GUIDE
        ),
        schema_object(
            json!({"path": {"type": "string", "description": "File path"}}),
            &["path"],
        ),
        move |_id, args, cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = arg_str(&args, "path")?;
                let abs = env
                    .absolute_path(Path::new(&path))
                    .map_err(|e| e.to_string())?;
                let info = env.file_info(&abs).await.map_err(|e| e.to_string())?;
                if info.is_dir {
                    return Err(format!("path is a directory: {}", abs.display()));
                }
                let mime = mime_guess::from_path(&abs).first_or_octet_stream();
                if mime.type_() == mime_guess::mime::IMAGE {
                    let bytes = env
                        .read_binary_file(&abs)
                        .await
                        .map_err(|e| e.to_string())?;
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    return Ok(AgentToolResult {
                        content: vec![ToolResultContent::Image(ImageContent {
                            data: b64,
                            mime_type: mime.essence_str().to_string(),
                        })],
                        details: json!({"path": abs, "bytes": bytes.len()}),
                        usage: None,
                        added_tool_names: None,
                        terminate: None,
                    });
                }
                let document = documents::read(env.as_ref(), &abs, info.size, cancel.as_ref());
                if let Some(result) = until_cancelled(cancel.as_ref(), document).await? {
                    return result;
                }
                let text = env.read_text_file(&abs).await.map_err(|e| e.to_string())?;
                let (text, truncated) = truncate_head(&text, DEFAULT_MAX_BYTES);
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text,
                        text_signature: None,
                    })],
                    details: json!({"path": abs, "truncated": truncated}),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    )
}

/// Create a write tool bound to `env`.
pub fn create_write_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "write",
        "Write",
        format!(
            "Write contents to a file (create or overwrite).{}",
            documents::WRITE_GUIDE
        ),
        schema_object(
            json!({
                "path": {"type": "string"},
                "content": {"type": "string"}
            }),
            &["path", "content"],
        ),
        move |_id, args, cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = arg_str(&args, "path")?;
                let content = arg_str(&args, "content")?;
                let abs = env
                    .absolute_path(Path::new(&path))
                    .map_err(|e| e.to_string())?;
                with_file_mutation_queue(abs.clone(), || {
                    let env = Arc::clone(&env);
                    let content = content.clone();
                    let cancel = cancel.clone();
                    async move {
                        let cancel = cancel.as_ref();
                        let bytes =
                            until_cancelled(cancel, documents::encode(&abs, &content)).await??;
                        // Only a missing file counts as created. Any other
                        // error (a directory, no permission) fails the write
                        // before it happens, and an existing file that is
                        // not text gets a byte snapshot, so a rejected
                        // review restores it instead of deleting it.
                        let previous = match env.file_info(&abs).await {
                            Ok(_) => Some(env.read_binary_file(&abs).await.map_err(|e| {
                                format!("cannot snapshot {} for review: {e}", abs.display())
                            })?),
                            Err(e) if e.code == FileErrorCode::NotFound => None,
                            Err(e) => return Err(e.to_string()),
                        };
                        let created = previous.is_none();
                        let previous = previous.unwrap_or_default();
                        // A document diffs as its text, not as its bytes,
                        // read back by the document reader.
                        let previous_text = if created {
                            documents::Previous::Text(String::new())
                        } else {
                            until_cancelled(
                                cancel,
                                documents::text_of(&abs, &previous, cancel, env.cwd()),
                            )
                            .await??
                        };
                        let rebuilt = !created && documents::is_document(&abs);
                        let previous_text = match previous_text {
                            documents::Previous::Text(text) => text,
                            documents::Previous::Unreadable(why) => {
                                return Err(format!(
                                    "{} could not be read ({why}), so writing it from text \
                                     would replace content you have not seen. Write the new \
                                     document to another path instead.",
                                    abs.display()
                                ))
                            }
                            documents::Previous::TooLong => {
                                return Err(format!(
                                    "{} has more text than read can show, so writing it from \
                                     text would drop the rest. Write the new document to \
                                     another path instead.",
                                    abs.display()
                                ))
                            }
                        };
                        let mut diff = unified_diff(&previous_text, &content, &abs);
                        if rebuilt {
                            diff = format!("# {}\n{diff}", documents::REBUILT_NOTE);
                        }
                        // Past this point the write runs to the end: a
                        // cancel mid-write could leave half a file.
                        if cancel.is_some_and(|c| c.is_cancelled()) {
                            return Err("cancelled".into());
                        }
                        let previous_path = write_review_snapshot(env.as_ref(), &abs, &previous)
                            .await
                            .map_err(|e| e.to_string())?;
                        env.write_file(&abs, &bytes).await.map_err(write_error)?;
                        let (target_path, created_identity) =
                            written_target(env.as_ref(), &abs, created).await;
                        let via_link = through_link(env.as_ref(), &abs, &target_path).await;
                        let mut text = format!("Wrote {} bytes to {}", bytes.len(), abs.display());
                        if rebuilt {
                            text = format!("{text}. {}", documents::REBUILT_NOTE);
                        }
                        Ok(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text,
                                text_signature: None,
                            })],
                            details: json!({
                                "path": abs,
                                "bytes": bytes.len(),
                                "created": created,
                                "previousPath": previous_path,
                                "targetPath": target_path,
                                "viaLink": via_link,
                                "createdPath": created_identity.map(|_| &target_path),
                                "createdIdentity": created_identity,
                                "diff": diff,
                            }),
                            usage: None,
                            added_tool_names: None,
                            terminate: None,
                        })
                    }
                })
                .await
            }
        },
    )
}

/// `work`, or an error as soon as `cancel` fires (dropping `work`, which
/// kills a document reader child).
async fn until_cancelled<T>(
    cancel: Option<&tokio_util::sync::CancellationToken>,
    work: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    match cancel {
        Some(token) => tokio::select! {
            r = work => Ok(r),
            _ = token.cancelled() => Err("cancelled".into()),
        },
        None => Ok(work.await),
    }
}

/// Create an edit tool (exact/fuzzy replace) bound to `env`.
pub fn create_edit_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "edit",
        "Edit",
        "Replace text in a file. Prefers exact match; falls back to fuzzy match.",
        schema_object(
            json!({
                "path": {"type": "string"},
                "oldText": {"type": "string"},
                "newText": {"type": "string"}
            }),
            &["path", "oldText", "newText"],
        ),
        move |_id, args, _cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = arg_str(&args, "path")?;
                let old_text = args
                    .get("oldText")
                    .and_then(|v| v.as_str())
                    .ok_or("missing oldText")?
                    .to_string();
                let new_text = args
                    .get("newText")
                    .and_then(|v| v.as_str())
                    .ok_or("missing newText")?
                    .to_string();
                let abs = env
                    .absolute_path(Path::new(&path))
                    .map_err(|e| e.to_string())?;
                with_file_mutation_queue(abs.clone(), || {
                    let env = Arc::clone(&env);
                    async move {
                        documents::check_editable(&abs)?;
                        let original = env.read_text_file(&abs).await.map_err(|e| e.to_string())?;
                        let updated = if original.contains(&old_text) {
                            original.replacen(&old_text, &new_text, 1)
                        } else {
                            fuzzy_replace(&original, &old_text, &new_text)?
                        };
                        let previous_path =
                            write_review_snapshot(env.as_ref(), &abs, original.as_bytes())
                                .await
                                .map_err(|e| e.to_string())?;
                        let diff = unified_diff(&original, &updated, &abs);
                        env.write_file(&abs, updated.as_bytes())
                            .await
                            .map_err(write_error)?;
                        let (target_path, _) = written_target(env.as_ref(), &abs, false).await;
                        let via_link = through_link(env.as_ref(), &abs, &target_path).await;
                        Ok(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text: format!("Edited {}", abs.display()),
                                text_signature: None,
                            })],
                            details: json!({
                                "path": abs,
                                "diff": diff,
                                "created": false,
                                "previousPath": previous_path,
                                "targetPath": target_path,
                                "viaLink": via_link,
                            }),
                            usage: None,
                            added_tool_names: None,
                            terminate: None,
                        })
                    }
                })
                .await
            }
        },
    )
}

/// Persist pre-edit contents so the CLI can open a diff and revert on reject.
async fn write_review_snapshot(
    env: &dyn ExecutionEnv,
    path: &Path,
    contents: &[u8],
) -> Result<PathBuf, crate::harness::types::FileError> {
    let dir = env.create_temp_dir(".loop-file-review").await?;
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let out = dir.join(format!("{stem}.before"));
    env.write_file(&out, contents).await?;
    Ok(out)
}

/// The file a write to `abs` landed in, resolved once right after the
/// write (a symlink is followed here, never at review time), and, when the
/// write created it, its identity. A review that rejects the write removes
/// only that path, and only while its identity still matches.
async fn written_target(
    env: &dyn ExecutionEnv,
    abs: &Path,
    created: bool,
) -> (PathBuf, Option<FileIdentity>) {
    let target = env
        .canonical_path(abs)
        .await
        .unwrap_or_else(|_| abs.to_path_buf());
    let identity = if created {
        env.file_identity(&target).await.ok().flatten()
    } else {
        None
    };
    (target, identity)
}

/// Whether a write to `abs` landed somewhere its path does not show: a
/// symlink on the way took it to `target`. Only the working directory is
/// resolved for the comparison, so a workspace below a link (such as `/var`
/// on macOS) does not count, and a plain `..` does not either.
async fn through_link(env: &dyn ExecutionEnv, abs: &Path, target: &Path) -> bool {
    let cwd = env.cwd();
    let expected = match (abs.strip_prefix(cwd), env.canonical_path(cwd).await) {
        (Ok(rest), Ok(root)) => root.join(rest),
        _ => abs.to_path_buf(),
    };
    let mut normal = PathBuf::new();
    for part in expected.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normal.pop();
            }
            part => normal.push(part),
        }
    }
    normal != target
}

fn fuzzy_replace(original: &str, old_text: &str, new_text: &str) -> Result<String, String> {
    // Strategy 1: whitespace-normalized match.
    // Build a map from normalized-string byte offset back to original byte offset.
    let needle: String = old_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !needle.is_empty() {
        let mut norm_buf = String::new();
        let mut norm_to_orig: Vec<usize> = Vec::new();
        let mut in_ws = true;
        for (byte_idx, ch) in original.char_indices() {
            if ch.is_whitespace() {
                if !in_ws && !norm_buf.is_empty() {
                    norm_buf.push(' ');
                    norm_to_orig.push(byte_idx);
                    in_ws = true;
                }
            } else {
                norm_buf.push(ch);
                norm_to_orig.push(byte_idx);
                in_ws = false;
            }
        }
        norm_to_orig.push(original.len());

        if let Some(norm_start) = norm_buf.find(&needle) {
            let norm_end = norm_start + needle.len();
            let orig_start = norm_to_orig.get(norm_start).copied().unwrap_or(0);
            // Find the original end: skip any trailing whitespace in the original that was
            // collapsed during normalization.
            let raw_orig_end = norm_to_orig
                .get(norm_end)
                .copied()
                .unwrap_or(original.len());
            let _orig_end = original[..raw_orig_end]
                .trim_end()
                .len()
                .max(raw_orig_end.min(original.len()));

            let mut out = String::with_capacity(original.len() + new_text.len());
            out.push_str(&original[..orig_start]);
            out.push_str(new_text);
            out.push_str(&original[raw_orig_end..]);
            return Ok(out);
        }
    }

    // Strategy 2: trimmed match (ignore leading/trailing whitespace).
    let old_norm = old_text.trim();
    if !old_norm.is_empty() {
        if let Some(pos) = original.find(old_norm) {
            let mut out = String::with_capacity(original.len() + new_text.len());
            out.push_str(&original[..pos]);
            out.push_str(new_text);
            out.push_str(&original[pos + old_norm.len()..]);
            return Ok(out);
        }
    }

    Err("oldText not found (exact or fuzzy)".into())
}

fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- a/{}\n+++ b/{}\n", path.display(), path.display());
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Create a bash tool bound to `env`.
pub fn create_bash_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    create_bash_tool_with_prepare(env, None)
}

/// Optional prepare hook for bash.
pub type BashPrepare = Arc<
    dyn Fn(
            String,
            serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(String, Option<PathBuf>), String>> + Send>,
        > + Send
        + Sync,
>;

/// How long a bash command may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BashLimits {
    /// Used when the call names no `timeoutMs`.
    pub default_timeout_ms: u64,
    /// The most a call may ask for.
    pub max_timeout_ms: u64,
}

impl Default for BashLimits {
    fn default() -> Self {
        Self {
            default_timeout_ms: 120_000,
            max_timeout_ms: 600_000,
        }
    }
}

impl BashLimits {
    /// The timeout for a call that asked for `requested` ms: the default
    /// when it asked for none, never more than the maximum.
    pub fn timeout_for(self, requested: Option<u64>) -> u64 {
        let max = self.max_timeout_ms.max(1);
        requested
            .filter(|ms| *ms > 0)
            .unwrap_or(self.default_timeout_ms)
            .clamp(1, max)
    }
}

/// Create bash tool with optional prepare hook returning (command, cwd).
pub fn create_bash_tool_with_prepare(
    env: Arc<dyn ExecutionEnv>,
    prepare: Option<BashPrepare>,
) -> AgentTool {
    create_bash_tool_with(env, prepare, BashLimits::default())
}

/// Create bash tool with a prepare hook and timeout `limits`.
pub fn create_bash_tool_with(
    env: Arc<dyn ExecutionEnv>,
    prepare: Option<BashPrepare>,
    limits: BashLimits,
) -> AgentTool {
    AgentTool::simple(
        "bash",
        "Bash",
        "Run a shell command.",
        schema_object(
            json!({
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": format!(
                        "Milliseconds before the command is stopped (default {}, at most {}). \
                         Raise it for long builds and test runs.",
                        limits.default_timeout_ms, limits.max_timeout_ms
                    )
                }
            }),
            &["command"],
        ),
        move |_id, args, cancel, on_update| {
            let env = Arc::clone(&env);
            let prepare = prepare.clone();
            async move {
                let mut command = arg_str(&args, "command")?;
                let mut cwd = args.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);

                check_command_policy(&command, &[])?;

                if let Some(prepare) = &prepare {
                    let (c, d) = prepare(command.clone(), args.clone()).await?;
                    command = c;
                    if d.is_some() {
                        cwd = d;
                    }
                }
                let timeout_ms = limits.timeout_for(args.get("timeoutMs").and_then(|v| v.as_u64()));
                let mut options = ShellExecOptions {
                    cwd,
                    cancel,
                    timeout_ms: Some(timeout_ms),
                    ..Default::default()
                };
                // Ensure relative cwd resolves against env.cwd
                if let Some(ref c) = options.cwd {
                    if c.is_relative() {
                        options.cwd = Some(env.cwd().join(c));
                    }
                }
                if let Some(on_update) = on_update {
                    options.on_output = Some(Arc::new(move |text: &str| {
                        on_update(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text: text.to_string(),
                                text_signature: None,
                            })],
                            details: json!({ "partial": true }),
                            usage: None,
                            added_tool_names: None,
                            terminate: None,
                        });
                    }));
                }
                let sandboxed = env.sandboxed();
                let note_env = Arc::clone(&env);
                let captured = execute_shell_with_capture(env, &command, options, None)
                    .await
                    .map_err(|e| {
                        if e.contains("timed out") {
                            format!(
                                "command timed out after {}s; pass timeoutMs (at most {}) to \
                                 give it longer",
                                timeout_ms / 1000,
                                limits.max_timeout_ms
                            )
                        } else {
                            e
                        }
                    })?;
                let mut text = captured.text.clone();
                if bash_permission_hint(&text, captured.exit_code, sandboxed) {
                    text.push_str("\n\n");
                    text.push_str(PERMISSION_DENIED_HINT);
                }
                if let Some(note) = note_env.removed_env_note(&captured.text, captured.exit_code) {
                    text.push_str("\n\n");
                    text.push_str(&note);
                }
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text,
                        text_signature: None,
                    })],
                    details: json!({
                        "exitCode": captured.exit_code,
                        "truncated": captured.truncated,
                        "spillPath": captured.spill_path,
                    }),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    )
}

#[cfg(test)]
mod bash_limit_tests {
    use super::*;

    #[test]
    fn timeouts_default_and_cap() {
        let limits = BashLimits {
            default_timeout_ms: 120_000,
            max_timeout_ms: 600_000,
        };
        assert_eq!(limits.timeout_for(None), 120_000);
        assert_eq!(limits.timeout_for(Some(0)), 120_000);
        assert_eq!(limits.timeout_for(Some(300_000)), 300_000);
        assert_eq!(limits.timeout_for(Some(9_000_000)), 600_000);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_call_can_ask_for_less_time_and_learns_how_to_ask_for_more() {
        let env: Arc<dyn ExecutionEnv> =
            Arc::new(crate::harness::HostExecutionEnv::new(std::env::temp_dir()));
        let tool = create_bash_tool_with(
            env,
            None,
            BashLimits {
                default_timeout_ms: 60_000,
                max_timeout_ms: 120_000,
            },
        );
        let started = std::time::Instant::now();
        let err = (tool.execute)(
            "c1".into(),
            json!({"command": "sleep 5", "timeoutMs": 200}),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert!(err.contains("pass timeoutMs (at most 120000)"), "{err}");
        assert!(tool.parameters["properties"]["timeoutMs"].is_object());
    }
}

#[cfg(test)]
mod write_error_tests {
    use super::*;
    use crate::harness::types::FileError;

    #[test]
    fn permission_errors_get_the_hint_once_and_sandbox_refusals_none() {
        let denied = |m: &str| FileError::new(FileErrorCode::PermissionDenied, m);
        assert!(write_error(denied("Permission denied (os error 13)")).contains("chmod u+w"));
        let hinted = write_error(denied(&format!("denied. {PERMISSION_DENIED_HINT}")));
        assert_eq!(hinted.matches("chmod u+w").count(), 1, "{hinted}");
        assert!(!write_error(denied("sandbox denies access to /etc/x")).contains("chmod"));
        assert!(!write_error(FileError::new(FileErrorCode::Io, "disk full")).contains("chmod"));
    }
}

#[cfg(test)]
mod bash_hint_tests {
    use super::*;

    #[test]
    fn only_write_failures_outside_a_sandbox_get_the_hint() {
        for text in [
            "bash: line 1: ro.txt: Permission denied",
            "zsh:1: permission denied: ro.txt",
            "sh: 1: cannot create ro.txt: Permission denied",
            "touch: x: Read-only file system",
            "sed: couldn't open temporary file ./sedX: Permission denied",
            "cp: ro.txt: Permission denied",
        ] {
            assert!(bash_permission_hint(text, 1, false), "{text}");
            assert!(!bash_permission_hint(text, 1, true), "sandboxed: {text}");
            assert!(!bash_permission_hint(text, 0, false), "exit 0: {text}");
        }
        for text in [
            "git@github.com: Permission denied (publickey).",
            "ls: /root: Permission denied",
            "error: could not compile",
        ] {
            assert!(!bash_permission_hint(text, 1, false), "{text}");
        }
    }
}

#[cfg(test)]
mod via_link_tests {
    use super::*;

    /// The working directory is reached through a link (as `/var` is on
    /// macOS): a plain write is not marked, a write through a link is.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_a_link_below_the_cwd_marks_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(real.join("ws")).unwrap();
        std::fs::create_dir_all(real.join("out")).unwrap();
        std::fs::write(real.join("out/target.txt"), "x").unwrap();
        std::os::unix::fs::symlink(real.join("ws"), real.join("ws-link")).unwrap();
        std::os::unix::fs::symlink(real.join("out/target.txt"), real.join("ws/notes")).unwrap();
        let env: Arc<dyn ExecutionEnv> =
            Arc::new(crate::harness::HostExecutionEnv::new(real.join("ws-link")));
        let tool = create_write_tool(env);
        let write = |path: &str| {
            (tool.execute)(
                "w".into(),
                json!({"path": path, "content": "y"}),
                None,
                None,
            )
        };
        let plain = write("plain.txt").await.unwrap();
        assert_eq!(plain.details["viaLink"], json!(false));
        let dotdot = write("../ws/other.txt").await.unwrap();
        assert_eq!(dotdot.details["viaLink"], json!(false), "a plain `..`");
        let linked = write("notes").await.unwrap();
        assert_eq!(linked.details["viaLink"], json!(true));
        assert_eq!(
            linked.details["targetPath"],
            json!(real.join("out/target.txt"))
        );
    }
}
