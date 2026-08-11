//! Keybinding IDs and defaults (pi-inspired).

use std::collections::HashMap;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Logical keybinding action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Interrupt,
    Clear,
    Exit,
    Submit,
    NewLine,
    ModelSelect,
    ModelCycleForward,
    ModelCycleBackward,
    ThinkingCycle,
    ThinkingToggle,
    ToolsExpand,
    MessageCopy,
    FollowUp,
    Dequeue,
    ExternalEditor,
    SessionNew,
    SessionTree,
    SessionFork,
    SessionResume,
    // Editor navigation / editing
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordLeft,
    MoveWordRight,
    MoveLineStart,
    MoveLineEnd,
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToLineStart,
    DeleteToLineEnd,
    DeleteLine,
}

/// Keybinding configuration.
#[derive(Debug, Clone)]
pub struct Keybindings {
    map: HashMap<String, Action>,
}

impl Default for Keybindings {
    fn default() -> Self {
        let mut map = HashMap::new();
        let defaults = [
            ("escape", Action::Interrupt),
            ("ctrl+c", Action::Clear),
            ("ctrl+d", Action::Exit),
            ("enter", Action::Submit),
            ("shift+enter", Action::NewLine),
            ("ctrl+enter", Action::NewLine),
            ("ctrl+j", Action::NewLine),
            ("ctrl+l", Action::ModelSelect),
            ("ctrl+p", Action::ModelCycleForward),
            ("ctrl+shift+p", Action::ModelCycleBackward),
            ("shift+tab", Action::ThinkingCycle),
            ("ctrl+t", Action::ThinkingToggle),
            ("ctrl+o", Action::ToolsExpand),
            ("ctrl+x", Action::MessageCopy),
            ("alt+enter", Action::FollowUp),
            ("alt+up", Action::Dequeue),
            ("ctrl+g", Action::ExternalEditor),
            // Cursor movement
            ("left", Action::MoveLeft),
            ("right", Action::MoveRight),
            ("up", Action::MoveUp),
            ("down", Action::MoveDown),
            ("alt+left", Action::MoveWordLeft),
            ("alt+right", Action::MoveWordRight),
            ("ctrl+left", Action::MoveWordLeft),
            ("ctrl+right", Action::MoveWordRight),
            ("home", Action::MoveLineStart),
            ("end", Action::MoveLineEnd),
            ("ctrl+a", Action::MoveLineStart),
            ("ctrl+e", Action::MoveLineEnd),
            // Deletion
            ("backspace", Action::DeleteBackward),
            ("delete", Action::DeleteForward),
            ("ctrl+h", Action::DeleteBackward),
            ("ctrl+w", Action::DeleteWordBackward),
            ("alt+backspace", Action::DeleteWordBackward),
            ("ctrl+backspace", Action::DeleteWordBackward),
            ("alt+d", Action::DeleteWordForward),
            ("alt+delete", Action::DeleteWordForward),
            ("ctrl+u", Action::DeleteToLineStart),
            ("ctrl+k", Action::DeleteToLineEnd),
            // Cmd (super) bindings — macOS: cmd+backspace / cmd+delete
            ("super+backspace", Action::DeleteToLineStart),
            ("super+delete", Action::DeleteLine),
            ("ctrl+shift+backspace", Action::DeleteLine),
            ("ctrl+shift+u", Action::DeleteLine),
        ];
        for (k, a) in defaults {
            map.insert(k.to_string(), a);
        }
        Self { map }
    }
}

impl Keybindings {
    /// Load overrides from JSON (maps binding string → action id).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut kb = Self::default();
        if !path.exists() {
            return Ok(kb);
        }
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(kb);
        }
        // Support both { "app.interrupt": "escape" } and reverse maps.
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                let Some(s) = v.as_str() else { continue };
                if let Some(action) = action_from_id(k) {
                    kb.map.insert(normalize_key(s), action);
                } else if let Some(action) = action_from_id(s) {
                    kb.map.insert(normalize_key(k), action);
                }
            }
        }
        Ok(kb)
    }

    /// Resolve a key event to an action.
    pub fn resolve(&self, key: KeyEvent) -> Option<Action> {
        // Prefer Shift+Enter / Ctrl+Enter as newline even when the terminal
        // reports odd modifier combinations.
        if matches!(key.code, KeyCode::Enter) {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL)
            {
                return Some(Action::NewLine);
            }
        }
        // Some terminals emit `\n` for Shift+Enter.
        if matches!(key.code, KeyCode::Char('\n')) {
            return Some(Action::NewLine);
        }
        let s = key_to_string(key)?;
        self.map.get(&s).copied()
    }
}

fn action_from_id(id: &str) -> Option<Action> {
    Some(match id {
        "app.interrupt" | "interrupt" => Action::Interrupt,
        "app.clear" | "clear" => Action::Clear,
        "app.exit" | "exit" => Action::Exit,
        "tui.input.submit" | "submit" => Action::Submit,
        "tui.input.newLine" | "newLine" | "newline" => Action::NewLine,
        "app.model.select" | "modelSelect" => Action::ModelSelect,
        "app.model.cycleForward" | "modelCycleForward" => Action::ModelCycleForward,
        "app.model.cycleBackward" | "modelCycleBackward" => Action::ModelCycleBackward,
        "app.thinking.cycle" | "thinkingCycle" => Action::ThinkingCycle,
        "app.thinking.toggle" | "thinkingToggle" => Action::ThinkingToggle,
        "app.tools.expand" | "toolsExpand" => Action::ToolsExpand,
        "app.message.copy" | "messageCopy" => Action::MessageCopy,
        "app.message.followUp" | "followUp" => Action::FollowUp,
        "app.message.dequeue" | "dequeue" => Action::Dequeue,
        "app.editor.external" | "externalEditor" => Action::ExternalEditor,
        "app.session.new" | "sessionNew" => Action::SessionNew,
        "app.session.tree" | "sessionTree" => Action::SessionTree,
        "app.session.fork" | "sessionFork" => Action::SessionFork,
        "app.session.resume" | "sessionResume" => Action::SessionResume,
        "tui.input.moveLeft" | "moveLeft" => Action::MoveLeft,
        "tui.input.moveRight" | "moveRight" => Action::MoveRight,
        "tui.input.moveUp" | "moveUp" => Action::MoveUp,
        "tui.input.moveDown" | "moveDown" => Action::MoveDown,
        "tui.input.moveWordLeft" | "moveWordLeft" => Action::MoveWordLeft,
        "tui.input.moveWordRight" | "moveWordRight" => Action::MoveWordRight,
        "tui.input.moveLineStart" | "moveLineStart" => Action::MoveLineStart,
        "tui.input.moveLineEnd" | "moveLineEnd" => Action::MoveLineEnd,
        "tui.input.deleteBackward" | "deleteBackward" => Action::DeleteBackward,
        "tui.input.deleteForward" | "deleteForward" => Action::DeleteForward,
        "tui.input.deleteWordBackward" | "deleteWordBackward" => Action::DeleteWordBackward,
        "tui.input.deleteWordForward" | "deleteWordForward" => Action::DeleteWordForward,
        "tui.input.deleteToLineStart" | "deleteToLineStart" => Action::DeleteToLineStart,
        "tui.input.deleteToLineEnd" | "deleteToLineEnd" => Action::DeleteToLineEnd,
        "tui.input.deleteLine" | "deleteLine" => Action::DeleteLine,
        _ => return None,
    })
}

fn normalize_key(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .replace(' ', "")
        .replace("cmd+", "super+")
        .replace("command+", "super+")
        .replace("option+", "alt+")
        .replace("opt+", "alt+")
}

fn key_to_string(key: KeyEvent) -> Option<String> {
    let mut parts = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("super");
    }
    let code = match key.code {
        KeyCode::Char(c) => {
            // Ctrl+char often arrives as a control code; crossterm usually gives Char.
            c.to_lowercase().to_string()
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        _ => return None,
    };
    // BackTab already includes shift semantics for our map.
    if key.code == KeyCode::BackTab {
        return Some("shift+tab".into());
    }
    parts.push(code.as_str());
    Some(parts.join("+"))
}

/// Human-readable hotkey help lines.
pub fn hotkey_help() -> Vec<(&'static str, &'static str)> {
    vec![
        ("escape", "Interrupt / abort and clear message queue"),
        ("ctrl+c", "Clear input (twice to quit)"),
        ("ctrl+d", "Exit when input empty"),
        ("enter", "Send message (queues while agent is busy)"),
        ("shift+enter / ctrl+j", "New line"),
        ("alt/ctrl+left/right", "Jump by word"),
        ("ctrl+a / ctrl+e", "Line start / end"),
        ("ctrl+u / ctrl+k", "Delete to line start / end"),
        ("ctrl+w / alt+backspace", "Delete word"),
        ("cmd/super+backspace", "Delete to line start"),
        ("cmd/super+delete", "Delete entire line"),
        ("ctrl+l", "Select model"),
        ("ctrl+p / ctrl+shift+p", "Cycle models"),
        ("shift+tab", "Cycle thinking level"),
        ("ctrl+t", "Toggle thinking visibility"),
        ("ctrl+o", "Expand/collapse tool output & reasoning"),
        ("↑↓", "Navigate / commands & model list"),
        ("ctrl+x", "Copy last assistant message"),
        ("alt+enter", "Queue message while busy"),
        ("alt+up", "Remove last queued message"),
        ("ctrl+g", "External editor"),
        ("/", "Slash commands"),
        ("!command", "Run shell locally (not sent to the model)"),
    ]
}
