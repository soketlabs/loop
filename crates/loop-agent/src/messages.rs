//! Custom message helpers and default harness convert_to_llm.

use loop_ai::{now_ms, Message, TextContent, UserMessage, UserMessageContent};

use crate::types::{AgentMessage, CustomAgentMessage};

/// Prefix wrapping compaction summaries for the LLM.
pub const COMPACTION_SUMMARY_PREFIX: &str = "<compaction-summary>\n";
/// Suffix wrapping compaction summaries for the LLM.
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</compaction-summary>";
/// Prefix wrapping branch summaries for the LLM.
pub const BRANCH_SUMMARY_PREFIX: &str = "<branch-summary>\n";
/// Suffix wrapping branch summaries for the LLM.
pub const BRANCH_SUMMARY_SUFFIX: &str = "\n</branch-summary>";

/// Create a compaction summary custom message.
pub fn create_compaction_summary_message(summary: impl Into<String>) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::CompactionSummary {
        summary: summary.into(),
        timestamp: now_ms(),
    })
}

/// Create a branch summary custom message.
pub fn create_branch_summary_message(summary: impl Into<String>) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::BranchSummary {
        summary: summary.into(),
        timestamp: now_ms(),
    })
}

/// Create a custom app message.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: impl Into<String>,
) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::Custom {
        custom_type: custom_type.into(),
        content: content.into(),
        timestamp: now_ms(),
        details: None,
    })
}

/// Format a bash execution custom message as text for the LLM.
pub fn bash_execution_to_text(command: &str, output: &str, exit_code: Option<i32>) -> String {
    let code = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    format!("$ {command}\n{output}\n[exit {code}]")
}

/// Default harness converter: maps custom roles to LLM-visible messages.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    let mut out = Vec::new();
    for m in messages {
        match m {
            AgentMessage::Llm(msg) => out.push(msg.clone()),
            AgentMessage::Custom(CustomAgentMessage::BashExecution {
                command,
                output,
                exit_code,
                timestamp,
            }) => {
                out.push(Message::User(UserMessage {
                    content: UserMessageContent::Text(bash_execution_to_text(
                        command, output, *exit_code,
                    )),
                    timestamp: *timestamp,
                }));
            }
            AgentMessage::Custom(CustomAgentMessage::Custom {
                content,
                timestamp,
                ..
            }) => {
                out.push(Message::User(UserMessage {
                    content: UserMessageContent::Text(content.clone()),
                    timestamp: *timestamp,
                }));
            }
            AgentMessage::Custom(CustomAgentMessage::BranchSummary { summary, timestamp }) => {
                out.push(Message::User(UserMessage {
                    content: UserMessageContent::Text(format!(
                        "{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
                    )),
                    timestamp: *timestamp,
                }));
            }
            AgentMessage::Custom(CustomAgentMessage::CompactionSummary {
                summary,
                timestamp,
            }) => {
                out.push(Message::User(UserMessage {
                    content: UserMessageContent::Text(format!(
                        "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
                    )),
                    timestamp: *timestamp,
                }));
            }
        }
    }
    out
}

/// Build a user message with optional images.
pub fn user_message_with_images(
    text: impl Into<String>,
    images: Vec<loop_ai::ImageContent>,
) -> AgentMessage {
    use loop_ai::UserContent;
    let mut blocks = vec![UserContent::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })];
    for img in images {
        blocks.push(UserContent::Image(img));
    }
    AgentMessage::Llm(Message::User(UserMessage {
        content: UserMessageContent::Blocks(blocks),
        timestamp: now_ms(),
    }))
}
