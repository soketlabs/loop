//! Clipboard helpers with OSC 52 fallback for SSH / headless sessions.
//!
//! Mirrors pi's strategy: prefer native / platform tools, then emit OSC 52 so
//! terminal multiplexers and remote sessions can still receive the text.

use std::io::{self, Write};
use std::process::{Command, Stdio};

use base64::Engine;

const MAX_OSC52_ENCODED_LEN: usize = 100_000;

/// Copy `text` to the system clipboard, falling back to OSC 52 when needed.
pub fn copy_text(text: &str) -> Result<(), String> {
    let remote = is_remote_session();
    let mut copied = try_arboard(text);

    if copied && !remote {
        return Ok(());
    }

    if !copied {
        copied = try_platform_clipboard(text);
    }

    if remote || !copied {
        if emit_osc52(text) {
            copied = true;
        }
    }

    if copied {
        Ok(())
    } else {
        Err("failed to copy to clipboard".into())
    }
}

fn is_remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
        || std::env::var_os("MOSH_CONNECTION").is_some()
}

fn try_arboard(text: &str) -> bool {
    // On Linux, arboard/clipboard-rs is unreliable (X11-only, may not retain
    // selection ownership). Prefer platform tools + OSC 52 instead.
    if cfg!(target_os = "linux") {
        return false;
    }
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text.to_string()).is_ok(),
        Err(_) => false,
    }
}

fn try_platform_clipboard(text: &str) -> bool {
    if cfg!(target_os = "macos") {
        return pipe_to_command("pbcopy", &[], text);
    }
    if cfg!(target_os = "windows") {
        return pipe_to_command("clip", &[], text);
    }

    // Linux / other Unix
    if std::env::var_os("TERMUX_VERSION").is_some()
        && pipe_to_command("termux-clipboard-set", &[], text)
    {
        return true;
    }

    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = std::env::var_os("DISPLAY").is_some();
    let is_wayland_session = std::env::var_os("XDG_SESSION_TYPE")
        .map(|v| v == "wayland")
        .unwrap_or(has_wayland);

    if is_wayland_session && has_wayland {
        if pipe_to_command("wl-copy", &[], text) {
            return true;
        }
        if has_x11 {
            return pipe_to_x11(text);
        }
        return false;
    }

    if has_x11 {
        return pipe_to_x11(text);
    }

    false
}

fn pipe_to_x11(text: &str) -> bool {
    pipe_to_command("xclip", &["-selection", "clipboard"], text)
        || pipe_to_command("xsel", &["--clipboard", "--input"], text)
}

fn pipe_to_command(program: &str, args: &[&str], text: &str) -> bool {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return false;
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        let _ = child.kill();
        return false;
    }
    drop(stdin);
    matches!(child.wait(), Ok(status) if status.success())
}

fn osc52_sequence(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LEN {
        return None;
    }
    // OSC 52: ESC ] 52 ; c ; <base64> BEL
    Some(format!("\x1b]52;c;{encoded}\x07"))
}

fn emit_osc52(text: &str) -> bool {
    let Some(seq) = osc52_sequence(text) else {
        return false;
    };
    let mut out = io::stdout();
    out.write_all(seq.as_bytes()).is_ok() && out.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_rejects_huge_payload() {
        let huge = "x".repeat(MAX_OSC52_ENCODED_LEN);
        assert!(osc52_sequence(&huge).is_none());
    }

    #[test]
    fn osc52_builds_for_small_text() {
        let seq = osc52_sequence("hello").expect("sequence");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }
}
