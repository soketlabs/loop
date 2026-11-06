use std::sync::Arc;

use async_trait::async_trait;
use loop_agent::harness::session::types::{
    PendingSessionWrite, SessionMetadata, SessionReader, SessionStore, SessionTreeEntry,
};
use loop_agent::harness::session::SessionForkSelection;
use loop_agent::harness::types::SessionError;
use loop_ai::{Message, TextContent, UserContent, UserMessageContent};

const SCREEN_IMAGE_MARKER: &str = "[Screen image omitted from ephemeral session history.]";

/// Create a memory-only session store that excludes screenshot bytes from history.
pub fn screenless_store() -> Arc<dyn SessionStore> {
    Arc::new(ScreenlessSessionStore {
        inner: loop_agent::harness::session::create_in_memory_session_store(),
    })
}

struct ScreenlessSessionStore {
    inner: Arc<dyn SessionStore>,
}

#[async_trait]
impl SessionStore for ScreenlessSessionStore {
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.create(cwd, name).await
    }

    async fn open_writer(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.open_writer(id).await
    }

    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.load(id).await
    }

    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        self.inner.list(cwd).await
    }

    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        self.inner
            .append_entry(session_id, strip_user_images(pending))
            .await
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.delete(id).await
    }

    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner
            .fork(source_id, selection, through_entry_id, name)
            .await
    }
}

fn strip_user_images(pending: PendingSessionWrite) -> PendingSessionWrite {
    let PendingSessionWrite::Message { message } = pending else {
        return pending;
    };
    let loop_agent::types::AgentMessage::Llm(Message::User(mut user)) = message else {
        return PendingSessionWrite::Message { message };
    };

    if let UserMessageContent::Blocks(blocks) = user.content {
        let mut sanitized = Vec::with_capacity(blocks.len());
        for block in blocks {
            match block {
                UserContent::Image(_) => sanitized.push(UserContent::Text(TextContent {
                    text: SCREEN_IMAGE_MARKER.to_owned(),
                    text_signature: None,
                })),
                text => sanitized.push(text),
            }
        }
        user.content = UserMessageContent::Blocks(sanitized);
    }

    PendingSessionWrite::Message {
        message: loop_agent::types::AgentMessage::Llm(Message::User(user)),
    }
}

#[cfg(test)]
mod tests {
    use loop_agent::harness::session::types::{PendingSessionWrite, Session, SessionTreeEntry};
    use loop_agent::types::AgentMessage;
    use loop_ai::{
        ImageContent, Message, TextContent, UserContent, UserMessage, UserMessageContent,
    };

    use super::screenless_store;

    #[tokio::test]
    async fn screenshot_image_is_not_retained_in_entries_or_context() {
        let store = screenless_store();
        let reader = store
            .create(None, Some("ephemeral visual".to_owned()))
            .await
            .expect("create session");
        let screenshot = "screenshot-bytes-that-must-not-be-retained";
        store
            .append_entry(
                &reader.metadata().id,
                PendingSessionWrite::Message {
                    message: AgentMessage::Llm(Message::User(UserMessage {
                        content: UserMessageContent::Blocks(vec![
                            UserContent::Text(TextContent {
                                text: "Explain this screen".to_owned(),
                                text_signature: None,
                            }),
                            UserContent::Image(ImageContent {
                                data: screenshot.to_owned(),
                                mime_type: "image/png".to_owned(),
                            }),
                            UserContent::Text(TextContent {
                                text: "Keep the rest of the message".to_owned(),
                                text_signature: None,
                            }),
                        ]),
                        timestamp: 123,
                    })),
                },
            )
            .await
            .expect("append user message");

        let session = Session::new(store, reader);
        let entries = session.read_entries().await.expect("read entries");
        let context = session.build_context().await.expect("build context");

        let SessionTreeEntry::Message { message, .. } = &entries[0] else {
            panic!("expected message entry");
        };
        let AgentMessage::Llm(Message::User(user)) = message else {
            panic!("expected user message");
        };
        let UserMessageContent::Blocks(blocks) = &user.content else {
            panic!("expected block content");
        };
        assert!(blocks
            .iter()
            .all(|block| !matches!(block, UserContent::Image(_))));
        assert!(blocks.iter().any(
            |block| matches!(block, UserContent::Text(text) if text.text == "Explain this screen")
        ));
        assert!(blocks.iter().any(|block| matches!(block, UserContent::Text(text) if text.text == "Keep the rest of the message")));
        assert!(blocks.iter().any(
            |block| matches!(block, UserContent::Text(text) if text.text.contains("image omitted"))
        ));
        assert!(!format!("{entries:?}{context:?}").contains(screenshot));

        let AgentMessage::Llm(Message::User(context_user)) = &context.messages[0] else {
            panic!("expected user context message");
        };
        assert!(
            !matches!(context_user.content, UserMessageContent::Blocks(ref blocks) if blocks.iter().any(|block| matches!(block, UserContent::Image(_))))
        );
    }
}
