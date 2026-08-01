//! Shell capture with truncation and optional spill of full output.

use std::path::PathBuf;
use std::sync::Arc;

use crate::harness::types::{ExecutionEnv, ShellExecOptions};
use crate::harness::utils::truncate::{format_size, truncate_tail, DEFAULT_MAX_BYTES};

/// Result of captured shell output.
#[derive(Debug, Clone)]
pub struct ShellCaptureResult {
    /// Possibly truncated stdout+stderr text for the model.
    pub text: String,
    /// Exit code.
    pub exit_code: i32,
    /// Path where full output was spilled, if truncated.
    pub spill_path: Option<PathBuf>,
    /// Whether output was truncated.
    pub truncated: bool,
}

/// Replace non-utf8 / control-ish binary blobs for display.
pub fn sanitize_binary_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| {
            if c == '\n' || c == '\t' || c == '\r' || !c.is_control() {
                c
            } else {
                '�'
            }
        })
        .collect()
}

/// Execute a command, truncate for the model, and spill full output when needed.
pub async fn execute_shell_with_capture(
    env: Arc<dyn ExecutionEnv>,
    command: &str,
    options: ShellExecOptions,
    max_bytes: Option<usize>,
) -> Result<ShellCaptureResult, String> {
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let output = env
        .exec(command, options)
        .await
        .map_err(|e| e.to_string())?;

    let combined = if output.stderr.is_empty() {
        output.stdout
    } else if output.stdout.is_empty() {
        output.stderr
    } else {
        format!("{}\n{}", output.stdout, output.stderr)
    };

    let (text, truncated) = truncate_tail(&combined, max_bytes);
    let mut spill_path = None;
    if truncated {
        let path = env
            .create_temp_dir("loop-shell")
            .await
            .map_err(|e| e.to_string())?
            .join("full-output.txt");
        env.write_file(&path, combined.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let note = format!(
            "\n\n[output truncated to {}; full output: {}]",
            format_size(max_bytes),
            path.display()
        );
        spill_path = Some(path);
        return Ok(ShellCaptureResult {
            text: text + &note,
            exit_code: output.exit_code,
            spill_path,
            truncated: true,
        });
    }

    Ok(ShellCaptureResult {
        text,
        exit_code: output.exit_code,
        spill_path,
        truncated: false,
    })
}
