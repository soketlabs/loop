//! LLM-generated short titles for new sessions.

use loop_agent::harness::session::{
    create_sqlite_session_store, PendingSessionWrite, SessionTreeEntry,
};
use loop_ai::{AssistantContent, Context, Message, SimpleStreamOptions};
use loop_app_core::Runtime;

use crate::state::first_user_prompt;

const TITLE_SYSTEM: &str = "Generate a short 3-6 word title for a chat session based on the user's first message. Reply with ONLY the title text: no quotes, punctuation, or explanation.";

const PLACEHOLDERS: &[&str] = &["new chat", "new session", "untitled", "untitled chat"];

/// True when the stored name is missing or a generic placeholder.
pub fn is_untitled(name: Option<&str>) -> bool {
    match name {
        None => true,
        Some(n) => {
            let t = n.trim();
            t.is_empty()
                || PLACEHOLDERS
                    .iter()
                    .any(|p| t.eq_ignore_ascii_case(p))
        }
    }
}

/// Label shown in the session list.
pub fn display_title(name: Option<&str>) -> String {
    name.map(str::trim)
        .filter(|n| !n.is_empty() && !is_untitled(Some(n)))
        .unwrap_or("New chat")
        .to_string()
}

/// Ask the configured model for a concise session title.
pub async fn generate_session_title(
    runtime: &Runtime,
    first_message: &str,
    provider: Option<&str>,
    model_id: Option<&str>,
) -> Option<String> {
    let model = match (provider, model_id) {
        (Some(p), Some(m)) => runtime.models.get_model(p, m),
        _ => None,
    }
    .or_else(|| {
        runtime.models.get_model(
            &runtime.settings.default_provider,
            &runtime.settings.default_model,
        )
    })?;
    let context = Context {
        system_prompt: Some(TITLE_SYSTEM.into()),
        messages: vec![Message::user_text(first_message)],
        ..Default::default()
    };
    let response = runtime
        .models
        .complete_simple(&model, &context, SimpleStreamOptions::default())
        .await;
    if response.stop_reason.is_error() {
        tracing::warn!(
            "session title generation failed: {}",
            response.error_message.as_deref().unwrap_or("unknown")
        );
        return None;
    }
    let mut text = String::new();
    for block in &response.content {
        if let AssistantContent::Text(t) = block {
            text.push_str(&t.text);
        }
    }
    normalize_title(&text)
}

fn normalize_title(raw: &str) -> Option<String> {
    let title = raw
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
        .trim_start_matches('#')
        .trim_end_matches('.')
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    if title.is_empty() || is_untitled(Some(title)) {
        return None;
    }
    Some(truncate_title(title))
}

fn truncate_title(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    if chars.len() > 42 {
        format!("{}…", chars[..39].iter().collect::<String>())
    } else {
        title.to_string()
    }
}

/// Derive a readable title from the first user message when LLM naming fails.
pub fn fallback_title(message: &str) -> String {
    let line = message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .trim_start_matches(|c: char| c == '#' || c == '>' || c == '-' || c == '*')
        .trim();
    let words: Vec<_> = line.split_whitespace().take(8).collect();
    if words.is_empty() {
        return "New chat".into();
    }
    truncate_title(&words.join(" "))
}

/// Persist a session display name even while a turn is in flight.
pub async fn persist_session_name(
    runtime: &Runtime,
    session_id: &str,
    name: &str,
) -> anyhow::Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let store = create_sqlite_session_store(&runtime.sessions_db)
        .map_err(|e| anyhow::anyhow!(e))?;
    store
        .append_entry(
            session_id,
            PendingSessionWrite::SessionInfo {
                name: trimmed.to_string(),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

/// First user prompt stored on a session, if any.
pub async fn first_user_message_text(
    runtime: &Runtime,
    session_id: &str,
) -> Option<String> {
    let store = create_sqlite_session_store(&runtime.sessions_db).ok()?;
    let reader = store.load(session_id).await.ok()?;
    let entries = reader.read_entries(None).await.ok()?;
    for entry in entries {
        if let SessionTreeEntry::Message { message, .. } = entry {
            if let Some(text) = first_user_prompt(&message) {
                return Some(text);
            }
        }
    }
    None
}

/// Name every untitled session from its first user message.
pub async fn backfill_untitled_sessions(runtime: &Runtime) -> anyhow::Result<usize> {
    let store = create_sqlite_session_store(&runtime.sessions_db)
        .map_err(|e| anyhow::anyhow!(e))?;
    let repo = loop_agent::harness::create_session_repository(store, None);
    let list = repo.list(None).await.map_err(|e| anyhow::anyhow!(e))?;
    let mut named = 0usize;
    for meta in list {
        if !is_untitled(meta.name.as_deref()) {
            continue;
        }
        let Some(text) = first_user_message_text(runtime, &meta.id).await else {
            continue;
        };
        let title = fallback_title(&text);
        if is_untitled(Some(&title)) {
            continue;
        }
        persist_session_name(runtime, &meta.id, &title).await?;
        named += 1;
    }
    Ok(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untitled_detects_placeholders() {
        assert!(is_untitled(None));
        assert!(is_untitled(Some("")));
        assert!(is_untitled(Some("New Chat")));
        assert!(is_untitled(Some("untitled")));
        assert!(!is_untitled(Some("Fix the sidebar")));
    }

    #[test]
    fn fallback_uses_first_words() {
        assert_eq!(
            fallback_title("Refactor the desktop session list\nmore"),
            "Refactor the desktop session list"
        );
        assert_eq!(fallback_title("   "), "New chat");
    }

    #[test]
    fn normalize_strips_quotes() {
        assert_eq!(
            normalize_title("\"Fix auth flow.\"").as_deref(),
            Some("Fix auth flow")
        );
    }
}
