//! Chat transcript view models.

use loop_agent::types::AgentMessage;
use loop_ai::{UserContent, UserMessageContent};

/// One row in the chat VirtualList.
#[derive(Debug, Clone)]
pub enum ChatRow {
    User {
        id: String,
        text: String,
    },
    Assistant {
        id: String,
        text: String,
        streaming: bool,
    },
    Thinking {
        id: String,
        text: String,
        done: bool,
    },
    Tool {
        id: String,
        name: String,
        summary: String,
        status: ToolCardStatus,
    },
    FileChange {
        id: String,
        path: String,
        added: usize,
        removed: usize,
    },
    System(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCardStatus {
    Pending,
    Running,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub name: Option<String>,
    pub cwd: String,
    pub updated_at: i64,
    pub active: bool,
    pub running: bool,
}

/// First user-visible prompt from an agent message, if this is a user turn.
pub fn first_user_prompt(message: &AgentMessage) -> Option<String> {
    if let AgentMessage::Llm(loop_ai::Message::User(u)) = message {
        let text = user_text(&u.content);
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    } else {
        None
    }
}

impl ChatRow {
    pub fn from_agent_message(message: &AgentMessage) -> Vec<Self> {
        let mut rows = Vec::new();
        if let AgentMessage::Llm(loop_ai::Message::User(u)) = message {
            rows.push(ChatRow::User {
                id: uuid_simple(),
                text: user_text(&u.content),
            });
        } else if let AgentMessage::Llm(loop_ai::Message::Assistant(a)) = message {
            let mut text = String::new();
            for block in &a.content {
                match block {
                    loop_ai::AssistantContent::Text(t) => text.push_str(&t.text),
                    loop_ai::AssistantContent::Thinking(t) => {
                        rows.push(ChatRow::Thinking {
                            id: uuid_simple(),
                            text: t.thinking.clone(),
                            done: true,
                        });
                    }
                    _ => {}
                }
            }
            if !text.is_empty() {
                rows.push(ChatRow::Assistant {
                    id: uuid_simple(),
                    text,
                    streaming: false,
                });
            }
        }
        rows
    }
}

fn user_text(content: &UserMessageContent) -> String {
    match content {
        UserMessageContent::Text(t) => t.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                UserContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn uuid_simple() -> String {
    uuid::Uuid::now_v7().to_string()
}
