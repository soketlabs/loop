//! Truncation and shell-output helpers.

mod shell_output;
mod truncate;

pub use shell_output::{execute_shell_with_capture, sanitize_binary_output, ShellCaptureResult};
pub use truncate::{
    format_size, truncate_head, truncate_line, truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
    GREP_MAX_LINE_LENGTH,
};
