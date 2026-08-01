//! Integration tests for mid-session message hand-off.

use loop_ai::{
    transform_messages, AssistantContent, AssistantMessage, ImageContent, InputModality, Message,
    Model, ModelCost, StopReason, TextContent, ThinkingContent, ToolCall, ToolResultContent,
    Usage, UserContent, UserMessage, UserMessageContent,
};
use serde_json::json;

fn model(id: &str, provider: &str, images: bool) -> Model {
    let mut input = vec![InputModality::Text];
    if images {
        input.push(InputModality::Image);
    }
    Model {
        id: id.into(),
        name: id.into(),
        api: "openai-completions".into(),
        provider: provider.into(),
        base_url: "http://localhost".into(),
        reasoning: true,
        thinking_level_map: None,
        input,
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

#[test]
fn strips_thinking_across_models() {
    let source = model("a", "p1", false);
    let target = model("b", "p2", false);
    let messages = vec![Message::Assistant(AssistantMessage {
        content: vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "secret".into(),
                thinking_signature: Some("sig".into()),
                redacted: None,
            }),
            AssistantContent::Text(TextContent {
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
    let Message::Assistant(a) = &out[0] else {
        panic!("expected assistant");
    };
    assert_eq!(a.content.len(), 2);
    let AssistantContent::Text(t) = &a.content[0] else {
        panic!("expected thinking downgraded to text");
    };
    assert!(t.text.contains("secret"));
    assert!(t.text.contains("<thinking>"));
}

#[test]
fn keeps_thinking_for_same_model() {
    let m = model("a", "p", false);
    let messages = vec![Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::Thinking(ThinkingContent {
            thinking: "keep me".into(),
            thinking_signature: Some("sig".into()),
            redacted: None,
        })],
        api: m.api.clone(),
        provider: m.provider.clone(),
        model: m.id.clone(),
        response_model: None,
        response_id: None,
        usage: Usage::empty(),
        stop_reason: StopReason::Stop,
        error_message: None,
        raw_stop_reason: None,
        timestamp: 0,
    })];

    let out = transform_messages(&messages, &m);
    let Message::Assistant(a) = &out[0] else {
        panic!("expected assistant");
    };
    assert!(matches!(&a.content[0], AssistantContent::Thinking(_)));
}

#[test]
fn drops_error_and_aborted_turns() {
    let target = model("a", "p", false);
    let mut err = AssistantMessage::pending(&target);
    err.stop_reason = StopReason::Error;
    err.error_message = Some("boom".into());

    let mut aborted = AssistantMessage::pending(&target);
    aborted.stop_reason = StopReason::Aborted;
    aborted.error_message = Some("aborted".into());

    let out = transform_messages(
        &[Message::Assistant(err), Message::Assistant(aborted)],
        &target,
    );
    assert!(out.is_empty());
}

#[test]
fn fills_orphan_tool_results() {
    let target = model("a", "p", false);
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
    assert!(matches!(&out[1], Message::ToolResult(t) if t.tool_call_id == "call_1"));
}

#[test]
fn normalizes_tool_call_ids_across_handoff() {
    let source = model("a", "p1", false);
    let target = model("b", "p2", false);
    let weird_id = "call|abc!with extras that are way too long and should be truncated to sixty four chars_____EXTRA";
    let messages = vec![
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: weird_id.into(),
                name: "t".into(),
                arguments: json!({}),
                thought_signature: Some("google-sig".into()),
            })],
            api: source.api.clone(),
            provider: source.provider.clone(),
            model: source.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        }),
        Message::ToolResult(loop_ai::ToolResultMessage {
            tool_call_id: weird_id.into(),
            tool_name: "t".into(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "ok".into(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 0,
        }),
    ];

    let out = transform_messages(&messages, &target);
    let Message::Assistant(a) = &out[0] else {
        panic!("expected assistant");
    };
    let AssistantContent::ToolCall(tc) = &a.content[0] else {
        panic!("expected tool call");
    };
    assert!(tc.id.len() <= 64);
    assert!(!tc.id.contains('|'));
    assert!(!tc.id.contains('!'));
    assert!(tc.thought_signature.is_none());

    let Message::ToolResult(tr) = &out[1] else {
        panic!("expected tool result");
    };
    assert_eq!(tr.tool_call_id, tc.id);
}

#[test]
fn downgrades_images_for_non_vision_models() {
    let target = model("text-only", "p", false);
    let messages = vec![
        Message::User(UserMessage {
            content: UserMessageContent::Blocks(vec![
                UserContent::Text(TextContent {
                    text: "look".into(),
                    text_signature: None,
                }),
                UserContent::Image(ImageContent {
                    data: "abc".into(),
                    mime_type: "image/png".into(),
                }),
            ]),
            timestamp: 0,
        }),
        Message::ToolResult(loop_ai::ToolResultMessage {
            tool_call_id: "1".into(),
            tool_name: "shot".into(),
            content: vec![ToolResultContent::Image(ImageContent {
                data: "xyz".into(),
                mime_type: "image/jpeg".into(),
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 0,
        }),
    ];

    let out = transform_messages(&messages, &target);
    let Message::User(u) = &out[0] else {
        panic!("expected user");
    };
    let UserMessageContent::Blocks(blocks) = &u.content else {
        panic!("expected blocks");
    };
    assert!(blocks.iter().all(|b| !matches!(b, UserContent::Image(_))));
    assert!(blocks.iter().any(|b| matches!(
        b,
        UserContent::Text(t) if t.text.contains("image omitted")
    )));

    let Message::ToolResult(tr) = &out[1] else {
        panic!("expected tool result");
    };
    assert!(tr
        .content
        .iter()
        .all(|b| !matches!(b, ToolResultContent::Image(_))));
    assert!(tr.content.iter().any(|b| matches!(
        b,
        ToolResultContent::Text(t) if t.text.contains("tool image omitted")
    )));
}

#[test]
fn preserves_images_for_vision_models() {
    let target = model("vision", "p", true);
    let messages = vec![Message::User(UserMessage {
        content: UserMessageContent::Blocks(vec![UserContent::Image(ImageContent {
            data: "abc".into(),
            mime_type: "image/png".into(),
        })]),
        timestamp: 0,
    })];
    let out = transform_messages(&messages, &target);
    let Message::User(u) = &out[0] else {
        panic!("expected user");
    };
    let UserMessageContent::Blocks(blocks) = &u.content else {
        panic!("expected blocks");
    };
    assert!(matches!(blocks[0], UserContent::Image(_)));
}
