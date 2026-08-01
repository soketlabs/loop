//! Mid-session / cross-model message hand-off.

use crate::types::{
    AssistantContent, AssistantMessage, Message, Model, StopReason, TextContent,
    ToolResultContent, ToolResultMessage, UserContent, UserMessageContent,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

/// Normalize messages for the target model (thinking strip, image downgrade, orphans).
pub fn transform_messages(messages: &[Message], model: &Model) -> Vec<Message> {
    let normalized: Vec<Message> = messages.to_vec();
    let image_aware = downgrade_unsupported_images(&normalized, model);

    let mut tool_call_id_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut transformed = Vec::new();
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new(); // id, name

    for msg in image_aware {
        match msg {
            Message::User(u) => {
                // Synthesize empty tool results for orphaned tool calls before a new user turn.
                for (id, name) in pending_tool_calls.drain(..) {
                    transformed.push(Message::ToolResult(ToolResultMessage {
                        tool_call_id: id,
                        tool_name: name,
                        content: vec![ToolResultContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        })],
                        details: None,
                        usage: None,
                        added_tool_names: None,
                        is_error: false,
                        timestamp: crate::utils::id::now_ms(),
                    }));
                }
                transformed.push(Message::User(u));
            }
            Message::ToolResult(mut t) => {
                if let Some(mapped) = tool_call_id_map.get(&t.tool_call_id) {
                    t.tool_call_id = mapped.clone();
                }
                pending_tool_calls.retain(|(id, _)| id != &t.tool_call_id);
                transformed.push(Message::ToolResult(t));
            }
            Message::Assistant(a) => {
                // Drop error/aborted turns.
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }

                let is_same_model =
                    a.provider == model.provider && a.api == model.api && a.model == model.id;

                let mut new_content = Vec::new();
                for block in a.content.into_iter() {
                    match block {
                        AssistantContent::Thinking(t) => {
                            if is_same_model {
                                new_content.push(AssistantContent::Thinking(t));
                            } else if t.redacted == Some(true) || t.thinking.trim().is_empty() {
                                // drop
                            } else {
                                new_content.push(AssistantContent::Text(TextContent {
                                    text: format!("<thinking>\n{}\n</thinking>", t.thinking),
                                    text_signature: None,
                                }));
                            }
                        }
                        AssistantContent::ToolCall(mut tc) => {
                            // Strip Google-style thought signatures on cross-model handoff.
                            if !is_same_model {
                                tc.thought_signature = None;
                            }
                            let normalized_id = normalize_tool_call_id(&tc.id);
                            if normalized_id != tc.id {
                                tool_call_id_map.insert(tc.id.clone(), normalized_id.clone());
                                tc.id = normalized_id;
                            }
                            pending_tool_calls.push((tc.id.clone(), tc.name.clone()));
                            new_content.push(AssistantContent::ToolCall(tc));
                        }
                        other => new_content.push(other),
                    }
                }

                let AssistantMessage {
                    api,
                    provider,
                    model: model_id,
                    response_model,
                    response_id,
                    usage,
                    stop_reason,
                    error_message,
                    raw_stop_reason,
                    timestamp,
                    ..
                } = a;
                transformed.push(Message::Assistant(AssistantMessage {
                    content: new_content,
                    api,
                    provider,
                    model: model_id,
                    response_model,
                    response_id,
                    usage,
                    stop_reason,
                    error_message,
                    raw_stop_reason,
                    timestamp,
                }));
            }
        }
    }

    // Trailing orphaned tool calls → empty results.
    for (id, name) in pending_tool_calls {
        transformed.push(Message::ToolResult(ToolResultMessage {
            tool_call_id: id,
            tool_name: name,
            content: vec![ToolResultContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: crate::utils::id::now_ms(),
        }));
    }

    transformed
}

fn normalize_tool_call_id(id: &str) -> String {
    let filtered: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if filtered.is_empty() {
        return crate::utils::id::new_id().replace('-', "").chars().take(64).collect();
    }
    filtered.chars().take(64).collect()
}

fn downgrade_unsupported_images(messages: &[Message], model: &Model) -> Vec<Message> {
    if model.supports_images() {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|msg| match msg {
            Message::User(u) => {
                let content = match &u.content {
                    UserMessageContent::Text(t) => UserMessageContent::Text(t.clone()),
                    UserMessageContent::Blocks(blocks) => {
                        UserMessageContent::Blocks(replace_user_images(
                            blocks,
                            NON_VISION_USER_IMAGE_PLACEHOLDER,
                        ))
                    }
                };
                Message::User(crate::types::UserMessage {
                    content,
                    timestamp: u.timestamp,
                })
            }
            Message::ToolResult(t) => Message::ToolResult(ToolResultMessage {
                tool_call_id: t.tool_call_id.clone(),
                tool_name: t.tool_name.clone(),
                content: replace_tool_images(&t.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER),
                details: t.details.clone(),
                usage: t.usage.clone(),
                added_tool_names: t.added_tool_names.clone(),
                is_error: t.is_error,
                timestamp: t.timestamp,
            }),
            other => other.clone(),
        })
        .collect()
}

fn replace_user_images(content: &[UserContent], placeholder: &str) -> Vec<UserContent> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserContent::Image(_) => {
                if !previous_was_placeholder {
                    result.push(UserContent::Text(TextContent {
                        text: placeholder.to_string(),
                        text_signature: None,
                    }));
                }
                previous_was_placeholder = true;
            }
            UserContent::Text(t) => {
                previous_was_placeholder = t.text == placeholder;
                result.push(UserContent::Text(t.clone()));
            }
        }
    }
    result
}

fn replace_tool_images(content: &[ToolResultContent], placeholder: &str) -> Vec<ToolResultContent> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            ToolResultContent::Image(_) => {
                if !previous_was_placeholder {
                    result.push(ToolResultContent::Text(TextContent {
                        text: placeholder.to_string(),
                        text_signature: None,
                    }));
                }
                previous_was_placeholder = true;
            }
            ToolResultContent::Text(t) => {
                previous_was_placeholder = t.text == placeholder;
                result.push(ToolResultContent::Text(t.clone()));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        InputModality, ModelCost, ThinkingContent, ToolCall, Usage,
    };
    use serde_json::json;

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: "openai-completions".into(),
            provider: provider.into(),
            base_url: "http://localhost".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 128000,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn strips_thinking_across_models() {
        let source = model("a", "p1");
        let target = model("b", "p2");
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "secret".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: None,
                }),
                AssistantContent::Text(crate::types::TextContent {
                    text: "hello".into(),
                    text_signature: None,
                }),
            ],
            api: source.api.clone(),
            provider: source.provider.clone(),
            model: source.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        })];

        let out = transform_messages(&messages, &target);
        match &out[0] {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 2);
                match &a.content[0] {
                    AssistantContent::Text(t) => assert!(t.text.contains("secret")),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn drops_error_turns() {
        let target = model("a", "p");
        let mut err = AssistantMessage::pending(&target);
        err.stop_reason = StopReason::Error;
        err.error_message = Some("boom".into());
        let out = transform_messages(&[Message::Assistant(err)], &target);
        assert!(out.is_empty());
    }

    #[test]
    fn fills_orphan_tool_results() {
        let target = model("a", "p");
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "get_time".into(),
                arguments: json!({}),
                thought_signature: None,
            })],
            api: target.api.clone(),
            provider: target.provider.clone(),
            model: target.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        })];
        let out = transform_messages(&messages, &target);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[1], Message::ToolResult(_)));
    }
}
