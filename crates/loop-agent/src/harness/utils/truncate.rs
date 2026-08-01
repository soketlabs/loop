//! UTF-8 safe truncation helpers.

/// Default max lines for tool output.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default max bytes for tool output.
pub const DEFAULT_MAX_BYTES: usize = 50_000;
/// Max line length for grep-like display.
pub const GREP_MAX_LINE_LENGTH: usize = 2000;

/// Format a byte size for humans.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Truncate keeping the head of the string within max_bytes (UTF-8 safe).
pub fn truncate_head(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Truncate keeping the tail of the string within max_bytes (UTF-8 safe).
pub fn truncate_tail(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_string(), true)
}

/// Truncate a single line.
pub fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    line.chars().take(max_chars).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_utf8_safely() {
        let s = "hello 🦀 world";
        let (head, truncated) = truncate_head(s, 8);
        assert!(truncated);
        assert!(s.starts_with(&head) || head.len() <= 8);
    }
}
