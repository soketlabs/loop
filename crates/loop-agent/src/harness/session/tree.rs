//! One Session's branches as rows for `/tree`.
//!
//! Every entry has a parent, so a Session is a tree: going back to an
//! earlier point and sending a new prompt starts a branch beside the old
//! one. The rows show prompts, answers and summaries; tool calls and other
//! entries are folded into the nearest row above them. A row is indented one
//! step more than its parent only where the tree branches.

use std::collections::{HashMap, HashSet};

use loop_ai::{AssistantContent, Message};

use super::fork::fork_points_from_branch;
use super::types::SessionTreeEntry;
use crate::types::AgentMessage;

/// Longest preview, in characters.
const PREVIEW_CHARS: usize = 72;

/// What a [`SessionTreeRow`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTreeRowKind {
    /// A prompt.
    User,
    /// An answer.
    Assistant,
    /// A compaction or branch summary.
    Summary,
}

/// One row of `/tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTreeRow {
    /// The entry the row shows.
    pub entry_id: String,
    /// The entry's parent, where going back to a prompt moves to.
    pub parent_id: Option<String>,
    /// Indent steps.
    pub depth: usize,
    /// What the row shows.
    pub kind: SessionTreeRowKind,
    /// The text on one line, shortened.
    pub preview: String,
    /// A prompt's full text, to edit and send again.
    pub text: Option<String>,
    /// Whether the row is on the branch the Session is on.
    pub on_branch: bool,
    /// Whether the Session is at this row (its last row on the branch).
    pub current: bool,
}

/// The rows for `entries`, oldest first in depth-first order, with the
/// branch that ends at `leaf_id` marked.
pub fn session_tree_rows(
    entries: &[SessionTreeEntry],
    leaf_id: Option<&str>,
) -> Vec<SessionTreeRow> {
    let by_id: HashMap<&str, &SessionTreeEntry> = entries.iter().map(|e| (e.id(), e)).collect();
    let shown: Shown = entries
        .iter()
        .filter_map(|entry| row_content(entry).map(|content| (entry.id(), content)))
        .collect();
    let shown_ancestor = |id| nearest_shown(&by_id, &shown, id);
    let mut children: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    for entry in entries {
        if shown.contains_key(entry.id()) {
            let parent = shown_ancestor(entry.parent_id());
            children.entry(parent).or_default().push(entry.id());
        }
    }
    let mut branch: HashSet<&str> = HashSet::new();
    let mut id = leaf_id;
    while let Some(current) = id {
        branch.insert(current);
        id = by_id.get(current).and_then(|e| e.parent_id());
    }
    let current = shown_ancestor(leaf_id);

    let mut rows = Vec::new();
    // Depth-first, with an explicit stack: (entry, depth).
    let mut stack: Vec<(&str, usize)> = Vec::new();
    push_children(&children, &mut stack, None, 0);
    // Roots never indent, even when there are several.
    for item in stack.iter_mut() {
        item.1 = 0;
    }
    while let Some((id, depth)) = stack.pop() {
        let (kind, preview, text) = shown[id].clone();
        let entry = by_id[id];
        rows.push(SessionTreeRow {
            entry_id: id.to_owned(),
            parent_id: entry.parent_id().map(str::to_owned),
            depth,
            kind,
            preview,
            text,
            on_branch: branch.contains(id),
            current: current == Some(id),
        });
        push_children(&children, &mut stack, Some(id), depth);
    }
    rows
}

/// Push `parent`'s children for depth-first order, one step deeper when
/// there are several.
fn push_children<'a>(
    children: &HashMap<Option<&'a str>, Vec<&'a str>>,
    stack: &mut Vec<(&'a str, usize)>,
    parent: Option<&'a str>,
    depth: usize,
) {
    let kids = children.get(&parent).map(Vec::as_slice).unwrap_or(&[]);
    let depth = if kids.len() > 1 { depth + 1 } else { depth };
    for kid in kids.iter().rev() {
        stack.push((kid, depth));
    }
}

type Shown<'a> = HashMap<&'a str, (SessionTreeRowKind, String, Option<String>)>;

/// The nearest entry from `id` up that gets a row (`id` itself when it does).
fn nearest_shown<'a>(
    by_id: &HashMap<&'a str, &'a SessionTreeEntry>,
    shown: &Shown<'a>,
    mut id: Option<&'a str>,
) -> Option<&'a str> {
    while let Some(current) = id {
        if shown.contains_key(current) {
            return Some(current);
        }
        id = by_id.get(current).and_then(|e| e.parent_id());
    }
    None
}

/// A row's kind, preview and (for prompts) full text; `None` for entries
/// that get no row.
fn row_content(entry: &SessionTreeEntry) -> Option<(SessionTreeRowKind, String, Option<String>)> {
    match entry {
        SessionTreeEntry::Message { message, .. } => match message {
            AgentMessage::Llm(Message::User(_)) => {
                let point = fork_points_from_branch(std::slice::from_ref(entry))
                    .into_iter()
                    .next()?;
                Some((SessionTreeRowKind::User, point.preview, Some(point.text)))
            }
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                let text: String = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let preview = one_line(&text)?;
                Some((SessionTreeRowKind::Assistant, preview, None))
            }
            _ => None,
        },
        SessionTreeEntry::Compaction { summary, .. } => Some((
            SessionTreeRowKind::Summary,
            format!("compacted: {}", one_line(summary).unwrap_or_default()),
            None,
        )),
        SessionTreeEntry::BranchSummary { summary, .. } => Some((
            SessionTreeRowKind::Summary,
            format!("branch summary: {}", one_line(summary).unwrap_or_default()),
            None,
        )),
        _ => None,
    }
}

/// `text` on one line, shortened; `None` when blank.
fn one_line(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= PREVIEW_CHARS {
        return Some(line);
    }
    let mut short: String = line.chars().take(PREVIEW_CHARS - 1).collect();
    short.push('…');
    Some(short)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantMessage, StopReason, TextContent, ToolCall, Usage};

    fn message(id: &str, parent: Option<&str>, message: AgentMessage) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: 0,
            message,
        }
    }

    fn user(id: &str, parent: Option<&str>, text: &str) -> SessionTreeEntry {
        message(id, parent, AgentMessage::user_text(text))
    }

    fn answer(id: &str, parent: Option<&str>, content: Vec<AssistantContent>) -> SessionTreeEntry {
        message(
            id,
            parent,
            AgentMessage::Llm(Message::Assistant(AssistantMessage {
                content,
                api: "faux".into(),
                provider: "faux".into(),
                model: "faux".into(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                raw_stop_reason: None,
                timestamp: 0,
            })),
        )
    }

    fn text(t: &str) -> Vec<AssistantContent> {
        vec![AssistantContent::Text(TextContent {
            text: t.into(),
            text_signature: None,
        })]
    }

    /// `u1 → a1 → u2 → a2`, and a second branch `a1 → u3 → (tool call) → a3`
    /// that the Session is on.
    fn branched() -> Vec<SessionTreeEntry> {
        vec![
            SessionTreeEntry::ModelChange {
                id: "m".into(),
                parent_id: None,
                timestamp: 0,
                provider: "faux".into(),
                model_id: "faux".into(),
            },
            user("u1", Some("m"), "first prompt"),
            answer("a1", Some("u1"), text("first answer")),
            user("u2", Some("a1"), "old follow-up"),
            answer("a2", Some("u2"), text("old answer")),
            user("u3", Some("a1"), "new follow-up"),
            answer(
                "t3",
                Some("u3"),
                vec![AssistantContent::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "ls".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                })],
            ),
            answer("a3", Some("t3"), text("new answer")),
        ]
    }

    fn shape(rows: &[SessionTreeRow]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                format!(
                    "{}{}{}{}",
                    "  ".repeat(row.depth),
                    row.preview,
                    if row.on_branch { " *" } else { "" },
                    if row.current { " <" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn branches_indent_where_they_split_and_the_current_one_is_marked() {
        let rows = session_tree_rows(&branched(), Some("a3"));
        assert_eq!(
            shape(&rows),
            [
                "first prompt *",
                "first answer *",
                "  old follow-up",
                "  old answer",
                "  new follow-up *",
                "  new answer * <",
            ]
        );
        assert_eq!(rows[2].parent_id.as_deref(), Some("a1"));
        assert_eq!(rows[2].text.as_deref(), Some("old follow-up"));
        assert_eq!(rows[2].kind, SessionTreeRowKind::User);
        assert_eq!(rows[5].parent_id.as_deref(), Some("t3"));
    }

    #[test]
    fn the_current_row_follows_the_leaf_through_hidden_entries() {
        let rows = session_tree_rows(&branched(), Some("t3"));
        let current: Vec<_> = rows.iter().filter(|r| r.current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].entry_id, "u3");
        assert!(rows.iter().all(|r| !r.current || r.on_branch));
    }

    #[test]
    fn an_empty_session_has_no_rows() {
        assert!(session_tree_rows(&[], None).is_empty());
    }
}
