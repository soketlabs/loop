//! Session files: one Session's active branch written out by `/export` and
//! read back by `/import` as a new Session.
//!
//! A session file is JSON Lines. The first line is a header,
//! `{"type":"loop_session","version":1,"id":…,"name":…,"cwd":…,"createdAt":…}`;
//! each later line is one [`SessionTreeEntry`] of the branch, root first, in
//! the form the session stores keep. `/export` also writes a Markdown
//! transcript of the same entries ([`SessionFile::markdown`]).

use std::collections::HashMap;

use loop_ai::{AssistantContent, Message, ToolResultContent, UserContent, UserMessageContent};
use serde::{Deserialize, Serialize};

use super::fork::{fork_points_from_branch, read_session_entries_for_fork, SessionForkSelection};
use super::types::{PendingSessionWrite, SessionReader, SessionStore, SessionTreeEntry};
use crate::harness::types::SessionError;
use crate::types::{AgentMessage, CustomAgentMessage};

/// The session file format this build writes and reads.
pub const SESSION_FILE_VERSION: u32 = 1;

/// The `type` of a session file's header line.
const HEADER_TYPE: &str = "loop_session";

/// Lines of a tool result kept in the Markdown transcript.
const MARKDOWN_RESULT_LINES: usize = 20;

/// One Session's active branch, as a session file holds it.
#[derive(Debug, Clone)]
pub struct SessionFile {
    /// The exported Session's id.
    pub id: String,
    /// Its name, if it has one.
    pub name: Option<String>,
    /// The directory it was opened in.
    pub cwd: Option<String>,
    /// Created at (unix ms).
    pub created_at: i64,
    /// The active branch, root first, without leaf moves.
    pub entries: Vec<SessionTreeEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    created_at: i64,
}

impl SessionFile {
    /// The active branch of the Session `reader` reads.
    pub async fn from_reader(reader: &dyn SessionReader) -> Result<Self, SessionError> {
        let meta = reader.metadata();
        let head = reader.read_head().await?;
        let all = reader.read_entries(None).await?;
        let entries: Vec<SessionTreeEntry> =
            read_session_entries_for_fork(&all, head.as_deref(), SessionForkSelection::All, None)?
                .into_iter()
                .filter(|entry| !matches!(entry, SessionTreeEntry::Leaf { .. }))
                .collect();
        let name = entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                SessionTreeEntry::SessionInfo { name, .. } => Some(name.clone()),
                _ => None,
            })
            .or_else(|| meta.name.clone());
        Ok(Self {
            id: meta.id.clone(),
            name,
            cwd: meta.cwd.clone(),
            created_at: meta.created_at,
            entries,
        })
    }

    /// The file's text: the header line, then one line per entry.
    pub fn to_jsonl(&self) -> String {
        let header = Header {
            kind: HEADER_TYPE.into(),
            version: SESSION_FILE_VERSION,
            id: self.id.clone(),
            name: self.name.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
        };
        let mut out = serde_json::to_string(&header).unwrap_or_default();
        out.push('\n');
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry).unwrap_or_default());
            out.push('\n');
        }
        out
    }

    /// Parse a session file's text.
    pub fn from_jsonl(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let first = lines.next().ok_or("the file is empty")?;
        let header: Header = serde_json::from_str(first)
            .ok()
            .filter(|h: &Header| h.kind == HEADER_TYPE)
            .ok_or("not a Loop session file (export one with /export)")?;
        if header.version > SESSION_FILE_VERSION {
            return Err(format!(
                "session file version {} is newer than this Loop reads ({SESSION_FILE_VERSION}); \
                 update Loop",
                header.version
            ));
        }
        let entries = lines
            .enumerate()
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|e| format!("line {}: {e}", index + 2))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            id: header.id,
            name: header.name,
            cwd: header.cwd,
            created_at: header.created_at,
            entries,
        })
    }

    /// The title: the name, else the first prompt.
    pub fn title(&self) -> String {
        self.name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                fork_points_from_branch(&self.entries)
                    .into_iter()
                    .next()
                    .map(|point| point.preview)
            })
            .unwrap_or_else(|| "Loop session".into())
    }

    /// A readable transcript: prompts, answers, tool calls with the start of
    /// their results, and compaction summaries. Thinking and images are left
    /// out.
    pub fn markdown(&self) -> String {
        let mut out = format!("# {}\n\nSession `{}`\n", self.title(), self.id);
        for entry in &self.entries {
            match entry {
                SessionTreeEntry::Message { message, .. } => markdown_message(&mut out, message),
                SessionTreeEntry::Compaction { summary, .. } => {
                    out.push_str(&format!(
                        "\n## Earlier messages (compacted)\n\n{}\n",
                        summary.trim()
                    ));
                }
                SessionTreeEntry::BranchSummary { summary, .. } => {
                    out.push_str(&format!("\n## Branch summary\n\n{}\n", summary.trim()));
                }
                SessionTreeEntry::ModelChange {
                    provider, model_id, ..
                } => out.push_str(&format!("\n*Model: {provider}/{model_id}*\n")),
                _ => {}
            }
        }
        out
    }
}

fn markdown_message(out: &mut String, message: &AgentMessage) {
    match message {
        AgentMessage::Llm(Message::User(user)) => {
            let text = match &user.content {
                UserMessageContent::Text(text) => text.clone(),
                UserMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        UserContent::Text(t) => t.text.clone(),
                        UserContent::Image(_) => "[image]".into(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            out.push_str(&format!("\n## User\n\n{}\n", text.trim()));
        }
        AgentMessage::Llm(Message::Assistant(assistant)) => {
            let mut body = String::new();
            for block in &assistant.content {
                match block {
                    AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                        body.push_str(&format!("\n{}\n", t.text.trim()));
                    }
                    AssistantContent::ToolCall(call) => {
                        let args =
                            serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                        body.push_str(&format!(
                            "\n**Tool call** `{}`\n\n```json\n{args}\n```\n",
                            call.name
                        ));
                    }
                    _ => {}
                }
            }
            if !body.is_empty() {
                out.push_str(&format!("\n## Assistant\n{body}"));
            }
        }
        AgentMessage::Llm(Message::ToolResult(result)) => {
            let text: Vec<&str> = result
                .content
                .iter()
                .map(|block| match block {
                    ToolResultContent::Text(t) => t.text.as_str(),
                    ToolResultContent::Image(_) => "[image]",
                })
                .collect();
            let text = text.join("\n");
            let mut lines: Vec<&str> = text.lines().take(MARKDOWN_RESULT_LINES).collect();
            let more = text.lines().count().saturating_sub(lines.len());
            let note = if more > 0 {
                format!("… {more} more lines\n")
            } else {
                String::new()
            };
            if lines.is_empty() {
                lines.push("(no output)");
            }
            let label = if result.is_error { "error" } else { "result" };
            out.push_str(&format!(
                "\n**Tool {label}** `{}`\n\n```text\n{}\n{note}```\n",
                result.tool_name,
                lines.join("\n")
            ));
        }
        AgentMessage::Custom(CustomAgentMessage::BashExecution {
            command, output, ..
        }) => {
            out.push_str(&format!(
                "\n**Shell** `{command}`\n\n```text\n{}\n```\n",
                output.trim_end()
            ));
        }
        AgentMessage::Custom(_) => {}
    }
}

/// Write `file`'s entries into a new Session in `store`, opened in `cwd`,
/// and return its id.
///
/// The entries keep their order and content but get new ids. Labels (which
/// hold "accept all" approvals) and opaque custom entries are left out: a
/// file must not grant approvals.
pub async fn import_session_file(
    store: &dyn SessionStore,
    file: &SessionFile,
    cwd: Option<String>,
) -> Result<String, SessionError> {
    let reader = store.create(cwd, file.name.clone()).await?;
    let id = reader.metadata().id.clone();
    let mut new_ids: HashMap<String, String> = HashMap::new();
    for entry in &file.entries {
        let pending = match entry.clone() {
            SessionTreeEntry::Message { message, .. } => PendingSessionWrite::Message { message },
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                PendingSessionWrite::ThinkingLevelChange { thinking_level }
            }
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => PendingSessionWrite::ModelChange { provider, model_id },
            SessionTreeEntry::ActiveToolsChange { tool_names, .. } => {
                PendingSessionWrite::ActiveToolsChange { tool_names }
            }
            SessionTreeEntry::Compaction {
                summary,
                first_kept_entry_id,
                details,
                ..
            } => PendingSessionWrite::Compaction {
                summary,
                first_kept_entry_id: first_kept_entry_id.and_then(|old| new_ids.get(&old).cloned()),
                details,
            },
            SessionTreeEntry::BranchSummary { summary, .. } => {
                PendingSessionWrite::BranchSummary { summary }
            }
            SessionTreeEntry::SessionInfo { name, .. } => PendingSessionWrite::SessionInfo { name },
            SessionTreeEntry::Label { .. }
            | SessionTreeEntry::Custom { .. }
            | SessionTreeEntry::Leaf { .. } => continue,
        };
        let written = store.append_entry(&id, pending).await?;
        new_ids.insert(entry.id().to_owned(), written.id().to_owned());
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::session::{create_in_memory_session_store, Session};
    use loop_ai::{AssistantMessage, StopReason, TextContent, ToolCall, ToolResultMessage, Usage};
    use serde_json::json;

    fn assistant(content: Vec<AssistantContent>) -> AgentMessage {
        AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content,
            api: "faux".into(),
            provider: "faux".into(),
            model: "faux-model".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 2,
        }))
    }

    /// A Session with a prompt, a tool call and its result, an answer, a
    /// model change, a name and an "accept all" label.
    async fn sample(store: &std::sync::Arc<dyn SessionStore>) -> Session {
        let reader = store.create(Some("/work".into()), None).await.unwrap();
        let session = Session::new(std::sync::Arc::clone(store), reader);
        let write = |pending| {
            let store = std::sync::Arc::clone(store);
            let id = session.metadata().id.clone();
            async move { store.append_entry(&id, pending).await.unwrap() }
        };
        write(PendingSessionWrite::ModelChange {
            provider: "soket".into(),
            model_id: "glm".into(),
        })
        .await;
        session
            .append_message(AgentMessage::user_text("find the TODOs"))
            .await
            .unwrap();
        session
            .append_message(assistant(vec![AssistantContent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "grep".into(),
                arguments: json!({"pattern": "TODO"}),
                thought_signature: None,
            })]))
            .await
            .unwrap();
        session
            .append_message(AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "grep".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "src/main.rs:2:// TODO add args".into(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 3,
            })))
            .await
            .unwrap();
        write(PendingSessionWrite::Label {
            label: "loop.toolApproval:auto:bash".into(),
        })
        .await;
        session
            .append_message(assistant(vec![AssistantContent::Text(TextContent {
                text: "One TODO, in src/main.rs.".into(),
                text_signature: None,
            })]))
            .await
            .unwrap();
        write(PendingSessionWrite::SessionInfo {
            name: "todo hunt".into(),
        })
        .await;
        session
    }

    fn messages(entries: &[SessionTreeEntry]) -> Vec<serde_json::Value> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                SessionTreeEntry::Message { message, .. } => serde_json::to_value(message).ok(),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_session_round_trips_through_a_file_as_a_new_session() {
        let store = create_in_memory_session_store();
        let session = sample(&store).await;
        let file = SessionFile::from_reader(session.reader().as_ref())
            .await
            .unwrap();
        assert_eq!(file.name.as_deref(), Some("todo hunt"));

        let text = file.to_jsonl();
        assert!(
            text.starts_with(r#"{"type":"loop_session","version":1,"#),
            "{text}"
        );
        let read = SessionFile::from_jsonl(&text).unwrap();
        assert_eq!(read.entries.len(), file.entries.len());

        let other = create_in_memory_session_store();
        let id = import_session_file(other.as_ref(), &read, Some("/here".into()))
            .await
            .unwrap();
        assert_ne!(id, file.id, "an import is a new Session");
        let imported = Session::new(
            std::sync::Arc::clone(&other),
            other.load(&id).await.unwrap(),
        );
        assert_eq!(imported.metadata().cwd.as_deref(), Some("/here"));
        let copy = SessionFile::from_reader(imported.reader().as_ref())
            .await
            .unwrap();
        assert_eq!(messages(&copy.entries), messages(&file.entries));
        assert_eq!(copy.name.as_deref(), Some("todo hunt"));
        assert!(
            !copy
                .entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::Label { .. })),
            "a file grants no approvals"
        );
        let context = imported.build_context().await.unwrap();
        assert_eq!(context.messages.len(), 4);
        assert_eq!(context.model, Some(("soket".into(), "glm".into())));
    }

    #[tokio::test]
    async fn the_markdown_reads_as_a_transcript() {
        let store = create_in_memory_session_store();
        let session = sample(&store).await;
        let file = SessionFile::from_reader(session.reader().as_ref())
            .await
            .unwrap();
        let md = file.markdown();
        assert!(md.starts_with("# todo hunt\n"), "{md}");
        let order = [
            "*Model: soket/glm*",
            "## User\n\nfind the TODOs",
            "**Tool call** `grep`",
            "\"pattern\": \"TODO\"",
            "**Tool result** `grep`",
            "src/main.rs:2:// TODO add args",
            "## Assistant\n\nOne TODO, in src/main.rs.",
        ];
        let mut at = 0;
        for part in order {
            let found = md[at..]
                .find(part)
                .unwrap_or_else(|| panic!("{part} in\n{md}"));
            at += found + part.len();
        }
        assert!(!md.contains("toolApproval"));
    }

    #[test]
    fn other_files_are_refused() {
        assert!(SessionFile::from_jsonl("").is_err());
        let err = SessionFile::from_jsonl(r#"{"type":"session","version":3}"#).unwrap_err();
        assert!(err.contains("not a Loop session file"), "{err}");
        let err = SessionFile::from_jsonl(
            r#"{"type":"loop_session","version":99,"id":"x","createdAt":0}"#,
        )
        .unwrap_err();
        assert!(err.contains("newer"), "{err}");
        let err = SessionFile::from_jsonl(
            "{\"type\":\"loop_session\",\"version\":1,\"id\":\"x\",\"createdAt\":0}\nnot json",
        )
        .unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }
}
