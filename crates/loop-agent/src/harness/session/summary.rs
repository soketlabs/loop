//! One line per session for pickers: title, name, model, size and last use.

use super::fork::fork_points_from_branch;
use super::types::{SessionMetadata, SessionStore, SessionTreeEntry};
use crate::harness::types::SessionError;

/// A session as a picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session id.
    pub id: String,
    /// Working directory the session was opened in.
    pub cwd: Option<String>,
    /// The name set with `/name`, if any (the latest one wins).
    pub name: Option<String>,
    /// The first user message, on one line and shortened.
    pub title: Option<String>,
    /// The last model used, as `provider/model`.
    pub model: Option<String>,
    /// User and assistant messages.
    pub message_count: usize,
    /// Created at (unix ms).
    pub created_at: i64,
    /// Last entry written (unix ms); `created_at` when there is none.
    pub updated_at: i64,
}

/// Summarize one session from its metadata and entries.
pub fn summarize_session(meta: &SessionMetadata, entries: &[SessionTreeEntry]) -> SessionSummary {
    let mut name = meta.name.clone();
    let mut model = None;
    let mut message_count = 0;
    for entry in entries {
        match entry {
            SessionTreeEntry::SessionInfo { name: n, .. } => name = Some(n.clone()),
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => model = Some(format!("{provider}/{model_id}")),
            SessionTreeEntry::Message { message, .. }
                if matches!(message.role(), "user" | "assistant") =>
            {
                message_count += 1;
            }
            _ => {}
        }
    }
    let title = fork_points_from_branch(entries)
        .into_iter()
        .next()
        .map(|point| point.preview);
    SessionSummary {
        id: meta.id.clone(),
        cwd: meta.cwd.clone(),
        name: name.filter(|n| !n.trim().is_empty()),
        title,
        model,
        message_count,
        created_at: meta.created_at,
        updated_at: entries
            .iter()
            .map(SessionTreeEntry::timestamp)
            .max()
            .unwrap_or(meta.created_at)
            .max(meta.created_at),
    }
}

/// Summaries of the sessions in `store` (only those opened in `cwd`, when
/// given) that have at least one message, most recently used first.
pub async fn list_session_summaries(
    store: &dyn SessionStore,
    cwd: Option<&str>,
) -> Result<Vec<SessionSummary>, SessionError> {
    let mut out = Vec::new();
    for meta in store.list(cwd).await? {
        let reader = match store.load(&meta.id).await {
            Ok(reader) => reader,
            Err(e) => {
                tracing::warn!(session = %meta.id, "skipping unreadable session: {e}");
                continue;
            }
        };
        let entries = reader.read_entries(None).await?;
        let summary = summarize_session(&meta, &entries);
        if summary.message_count > 0 {
            out.push(summary);
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::session::create_in_memory_session_store;
    use crate::harness::session::types::PendingSessionWrite;
    use crate::types::AgentMessage;

    fn user(text: &str) -> PendingSessionWrite {
        PendingSessionWrite::Message {
            message: AgentMessage::Llm(loop_ai::Message::User(loop_ai::UserMessage {
                content: loop_ai::UserMessageContent::Text(text.into()),
                timestamp: 0,
            })),
        }
    }

    #[tokio::test]
    async fn summaries_have_a_title_and_skip_empty_sessions() {
        let store = create_in_memory_session_store();
        let empty = store.create(Some("/w".into()), None).await.unwrap();
        let first = store.create(Some("/w".into()), None).await.unwrap();
        let first_id = first.metadata().id.clone();
        store
            .append_entry(&first_id, user("Fix   the\nlogin bug"))
            .await
            .unwrap();
        store
            .append_entry(&first_id, user("and add tests"))
            .await
            .unwrap();
        store
            .append_entry(
                &first_id,
                PendingSessionWrite::ModelChange {
                    provider: "soket".into(),
                    model_id: "qwen".into(),
                },
            )
            .await
            .unwrap();
        store
            .append_entry(
                &first_id,
                PendingSessionWrite::SessionInfo {
                    name: "login".into(),
                },
            )
            .await
            .unwrap();
        let elsewhere = store.create(Some("/other".into()), None).await.unwrap();
        store
            .append_entry(&elsewhere.metadata().id, user("hi"))
            .await
            .unwrap();

        let listed = list_session_summaries(store.as_ref(), Some("/w"))
            .await
            .unwrap();
        assert_eq!(
            listed.len(),
            1,
            "the empty and the other folder's sessions are left out"
        );
        let summary = &listed[0];
        assert_eq!(summary.id, first_id);
        assert_ne!(summary.id, empty.metadata().id);
        assert_eq!(summary.title.as_deref(), Some("Fix the login bug"));
        assert_eq!(summary.name.as_deref(), Some("login"));
        assert_eq!(summary.model.as_deref(), Some("soket/qwen"));
        assert_eq!(summary.message_count, 2);
        assert!(summary.updated_at >= summary.created_at);
    }
}
