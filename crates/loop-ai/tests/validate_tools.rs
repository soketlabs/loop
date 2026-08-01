//! Tool argument validation tests.

use loop_ai::{validate_tool_call, Tool, ToolCall};
use serde_json::json;

fn time_tool() -> Tool {
    Tool {
        name: "get_time".into(),
        description: "Get time".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "timezone": { "type": "string" }
            },
            "required": ["timezone"],
            "additionalProperties": false
        }),
    }
}

#[test]
fn accepts_valid_args() {
    let call = ToolCall {
        id: "1".into(),
        name: "get_time".into(),
        arguments: json!({"timezone": "UTC"}),
        thought_signature: None,
    };
    let args = validate_tool_call(&[time_tool()], &call).unwrap();
    assert_eq!(args["timezone"], "UTC");
}

#[test]
fn coerces_json_string_arguments() {
    let call = ToolCall {
        id: "1".into(),
        name: "get_time".into(),
        arguments: json!("{\"timezone\":\"UTC\"}"),
        thought_signature: None,
    };
    let args = validate_tool_call(&[time_tool()], &call).unwrap();
    assert_eq!(args["timezone"], "UTC");
}

#[test]
fn rejects_missing_required() {
    let call = ToolCall {
        id: "1".into(),
        name: "get_time".into(),
        arguments: json!({}),
        thought_signature: None,
    };
    assert!(validate_tool_call(&[time_tool()], &call).is_err());
}

#[test]
fn rejects_unknown_tool() {
    let call = ToolCall {
        id: "1".into(),
        name: "missing".into(),
        arguments: json!({}),
        thought_signature: None,
    };
    assert!(validate_tool_call(&[time_tool()], &call).is_err());
}
