//! Multiline input buffer with readline / macOS-style editing.

/// Editable text with a character-index caret.
#[derive(Debug, Clone, Default)]
pub struct InputBuffer {
    text: String,
    /// Caret position in characters (not bytes).
    cursor: usize,
}

impl InputBuffer {
    /// Empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the full text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Character length.
    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Absolute caret (char index).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// (line_index, column_in_line) for the caret.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0usize;
        let mut col = 0usize;
        for (i, ch) in self.text.chars().enumerate() {
            if i == self.cursor {
                return (line, col);
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// Replace contents and move caret to end.
    pub fn set(&mut self, s: impl Into<String>) {
        self.text = s.into();
        self.cursor = self.len();
    }

    /// Clear buffer.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Take text and reset.
    pub fn take(&mut self) -> String {
        let out = std::mem::take(&mut self.text);
        self.cursor = 0;
        out
    }

    /// Insert a character at the caret.
    pub fn insert_char(&mut self, c: char) {
        let i = self.byte_index(self.cursor);
        self.text.insert(i, c);
        self.cursor += 1;
    }

    /// Insert a newline at the caret.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Insert a string at the caret (e.g. paste / tab complete).
    pub fn insert_str(&mut self, s: &str) {
        let i = self.byte_index(self.cursor);
        self.text.insert_str(i, s);
        self.cursor += s.chars().count();
    }

    /// Replace the character range `[start, end)` and place the caret after the insert.
    pub fn replace_char_range(&mut self, start: usize, end: usize, replacement: &str) {
        let len = self.len();
        let start = start.min(end).min(len);
        let end = end.min(len).max(start);
        let b0 = self.byte_index(start);
        let b1 = self.byte_index(end);
        self.text.replace_range(b0..b1, replacement);
        self.cursor = start + replacement.chars().count();
    }

    /// Backspace at caret.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Delete forward at caret.
    pub fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    /// Move left one character.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move right one character.
    pub fn move_right(&mut self) {
        if self.cursor < self.len() {
            self.cursor += 1;
        }
    }

    /// Move to start of current line.
    pub fn move_line_start(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    /// Move to end of current line.
    pub fn move_line_end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    /// Move up one visual line (same column when possible).
    pub fn move_up(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return false;
        }
        let prev_start = self.nth_line_start(line - 1);
        let prev_end = self.line_end(prev_start);
        let prev_len = prev_end - prev_start;
        self.cursor = prev_start + col.min(prev_len);
        true
    }

    /// Move down one visual line (same column when possible).
    pub fn move_down(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        let line_count = self.text.lines().count().max(1);
        // lines() drops a trailing empty line after final \n — count manually.
        let total_lines = self.total_lines();
        if line + 1 >= total_lines {
            let _ = line_count;
            return false;
        }
        let next_start = self.nth_line_start(line + 1);
        let next_end = self.line_end(next_start);
        let next_len = next_end - next_start;
        self.cursor = next_start + col.min(next_len);
        true
    }

    /// Jump left by word (whitespace / punctuation boundaries).
    pub fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        // Skip whitespace left of caret.
        while i > 0 && is_boundary(chars[i - 1]) {
            i -= 1;
        }
        // Skip the word.
        while i > 0 && !is_boundary(chars[i - 1]) {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Jump right by word.
    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && !is_boundary(chars[i]) {
            i += 1;
        }
        while i < n && is_boundary(chars[i]) {
            i += 1;
        }
        self.cursor = i;
    }

    /// Delete from caret to line start (Ctrl+U / Cmd+Backspace).
    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        if start == self.cursor {
            // At line start: also eat the preceding newline (join with previous).
            if self.cursor > 0 {
                self.backspace();
            }
            return;
        }
        let b0 = self.byte_index(start);
        let b1 = self.byte_index(self.cursor);
        self.text.replace_range(b0..b1, "");
        self.cursor = start;
    }

    /// Delete from caret to line end (Ctrl+K).
    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        if end == self.cursor {
            // At EOL: eat the newline.
            self.delete();
            return;
        }
        let b0 = self.byte_index(self.cursor);
        let b1 = self.byte_index(end);
        self.text.replace_range(b0..b1, "");
    }

    /// Delete the entire current line including its trailing newline.
    pub fn delete_line(&mut self) {
        let start = self.line_start(self.cursor);
        let mut end = self.line_end(self.cursor);
        // Include trailing newline if present.
        if end < self.len() {
            let ch = self.text.chars().nth(end);
            if ch == Some('\n') {
                end += 1;
            }
        } else if start > 0 {
            // Last line with no trailing newline: also remove preceding newline
            // so we don't leave a blank gap — keep start as-is for simplicity.
        }
        let b0 = self.byte_index(start);
        let b1 = self.byte_index(end);
        self.text.replace_range(b0..b1, "");
        self.cursor = start.min(self.len());
    }

    /// Delete word behind caret (Alt+Backspace / Ctrl+W).
    pub fn delete_word_backward(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        let start = self.cursor;
        let b0 = self.byte_index(start);
        let b1 = self.byte_index(end);
        self.text.replace_range(b0..b1, "");
        self.cursor = start;
    }

    /// Delete word ahead of caret (Alt+D).
    pub fn delete_word_forward(&mut self) {
        let start = self.cursor;
        self.move_word_right();
        let end = self.cursor;
        let b0 = self.byte_index(start);
        let b1 = self.byte_index(end);
        self.text.replace_range(b0..b1, "");
        self.cursor = start;
    }

    fn total_lines(&self) -> usize {
        if self.text.is_empty() {
            return 1;
        }
        self.text.chars().filter(|c| *c == '\n').count() + 1
    }

    fn line_start(&self, pos: usize) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = pos.min(chars.len());
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end(&self, pos: usize) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = pos.min(chars.len());
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }

    fn nth_line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut seen = 0usize;
        for (i, ch) in self.text.chars().enumerate() {
            if ch == '\n' {
                seen += 1;
                if seen == line {
                    return i + 1;
                }
            }
        }
        self.len()
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || c.is_ascii_punctuation()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_nav_and_delete_line() {
        let mut b = InputBuffer::new();
        b.insert_str("hello world");
        b.move_word_left();
        assert_eq!(b.cursor(), 6);
        b.delete_word_forward();
        assert_eq!(b.as_str(), "hello ");
        b.set("one\ntwo\nthree");
        // caret at end after set — on "three"
        b.move_line_start();
        b.delete_line();
        assert_eq!(b.as_str(), "one\ntwo\n");
    }

    #[test]
    fn replace_char_range_at_mention() {
        let mut b = InputBuffer::new();
        b.insert_str("see @src/app please");
        b.replace_char_range(4, 12, "/abs/src/app.rs");
        assert_eq!(b.as_str(), "see /abs/src/app.rs please");
        assert_eq!(b.cursor(), 4 + "/abs/src/app.rs".chars().count());
    }
}
