//! Readline-style command history for the TUI input.

use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 1000;

/// Submitted prompts recalled with up/down, persisted across sessions.
#[derive(Debug, Clone)]
pub struct CommandHistory {
    entries: Vec<String>,
    /// Index into `entries` while browsing; `None` means the live draft.
    cursor: Option<usize>,
    /// Input saved when first stepping into history from a fresh draft.
    draft: String,
    path: Option<PathBuf>,
}

impl Default for CommandHistory {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
            draft: String::new(),
            path: None,
        }
    }
}

impl CommandHistory {
    /// Empty in-memory history (tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from `path`, or start empty if missing / unreadable.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut entries = match std::fs::read_to_string(&path) {
            Ok(raw) if !raw.trim().is_empty() => {
                serde_json::from_str::<Vec<String>>(&raw).unwrap_or_else(|e| {
                    tracing::warn!("command history: {e}");
                    Vec::new()
                })
            }
            _ => Vec::new(),
        };
        if entries.len() > MAX_ENTRIES {
            let extra = entries.len() - MAX_ENTRIES;
            entries.drain(..extra);
        }
        Self {
            entries,
            cursor: None,
            draft: String::new(),
            path: Some(path),
        }
    }

    /// Record a submitted line. Consecutive duplicates are ignored.
    pub fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            self.reset_browse();
            return;
        }
        if self.entries.last().map(String::as_str) != Some(line) {
            self.entries.push(line.to_string());
            if self.entries.len() > MAX_ENTRIES {
                self.entries.remove(0);
            }
            self.save();
        }
        self.reset_browse();
    }

    /// Leave history browsing (e.g. after clearing the input).
    pub fn reset_browse(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    /// Older entry. Saves `current` as the draft on the first step.
    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        match self.cursor {
            None => {
                self.draft = current.to_string();
                let i = self.entries.len() - 1;
                self.cursor = Some(i);
                Some(self.entries[i].clone())
            }
            Some(0) => None,
            Some(i) => {
                let i = i - 1;
                self.cursor = Some(i);
                Some(self.entries[i].clone())
            }
        }
    }

    /// Newer entry, or the saved draft after the newest.
    pub fn next(&mut self) -> Option<String> {
        match self.cursor {
            None => None,
            Some(i) if i + 1 >= self.entries.len() => {
                self.cursor = None;
                Some(std::mem::take(&mut self.draft))
            }
            Some(i) => {
                let i = i + 1;
                self.cursor = Some(i);
                Some(self.entries[i].clone())
            }
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(e) = save_file(path, &self.entries) {
            tracing::warn!("command history: {e}");
        }
    }
}

fn save_file(path: &Path, entries: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_next_and_draft() {
        let mut h = CommandHistory::new();
        h.push("one");
        h.push("two");
        assert_eq!(h.previous("draft").as_deref(), Some("two"));
        assert_eq!(h.previous("two").as_deref(), Some("one"));
        assert_eq!(h.previous("one"), None);
        assert_eq!(h.next().as_deref(), Some("two"));
        assert_eq!(h.next().as_deref(), Some("draft"));
        assert_eq!(h.next(), None);
    }

    #[test]
    fn skips_empty_and_consecutive_dupes() {
        let mut h = CommandHistory::new();
        h.push("  ");
        h.push("foo");
        h.push("foo");
        h.push("bar");
        assert_eq!(h.previous("").as_deref(), Some("bar"));
        assert_eq!(h.previous("bar").as_deref(), Some("foo"));
        assert_eq!(h.previous("foo"), None);
    }

    #[test]
    fn persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut h = CommandHistory::load(&path);
        h.push("alpha");
        h.push("beta");
        let mut h2 = CommandHistory::load(&path);
        assert_eq!(h2.previous("").as_deref(), Some("beta"));
        assert_eq!(h2.previous("beta").as_deref(), Some("alpha"));
    }

    #[test]
    fn push_resets_browse() {
        let mut h = CommandHistory::new();
        h.push("old");
        let _ = h.previous("");
        h.push("new");
        assert_eq!(h.previous("").as_deref(), Some("new"));
    }
}
