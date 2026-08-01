//! Tool call argument validation against JSON Schema.

use serde_json::Value;
use thiserror::Error;

use crate::types::{Tool, ToolCall};

/// Tool validation failure.
#[derive(Debug, Error)]
pub enum ToolValidationError {
    /// Unknown tool name.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// Arguments failed JSON Schema validation.
    #[error("invalid arguments for tool {tool}: {message}")]
    InvalidArguments {
        /// Tool name.
        tool: String,
        /// Validation message.
        message: String,
    },
    /// Schema itself is invalid.
    #[error("invalid schema for tool {tool}: {message}")]
    InvalidSchema {
        /// Tool name.
        tool: String,
        /// Schema error.
        message: String,
    },
}

/// Validate a tool call against the matching tool's JSON Schema.
pub fn validate_tool_call(tools: &[Tool], call: &ToolCall) -> Result<Value, ToolValidationError> {
    let tool = tools
        .iter()
        .find(|t| t.name == call.name)
        .ok_or_else(|| ToolValidationError::UnknownTool(call.name.clone()))?;
    validate_tool_arguments(tool, &call.arguments)
}

/// Validate arguments against a tool's parameter schema.
pub fn validate_tool_arguments(tool: &Tool, arguments: &Value) -> Result<Value, ToolValidationError> {
    let validator = jsonschema::validator_for(&tool.parameters).map_err(|e| {
        ToolValidationError::InvalidSchema {
            tool: tool.name.clone(),
            message: e.to_string(),
        }
    })?;

    // Coerce: if arguments is a string that looks like JSON, parse it.
    let args = coerce_arguments(arguments);

    if let Err(error) = validator.validate(&args) {
        return Err(ToolValidationError::InvalidArguments {
            tool: tool.name.clone(),
            message: error.to_string(),
        });
    }
    Ok(args)
}

fn coerce_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| arguments.clone()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let tool = time_tool();
        let call = ToolCall {
            id: "1".into(),
            name: "get_time".into(),
            arguments: json!({"timezone": "UTC"}),
            thought_signature: None,
        };
        assert!(validate_tool_call(&[tool], &call).is_ok());
    }

    #[test]
    fn rejects_missing_required() {
        let tool = time_tool();
        let call = ToolCall {
            id: "1".into(),
            name: "get_time".into(),
            arguments: json!({}),
            thought_signature: None,
        };
        assert!(validate_tool_call(&[tool], &call).is_err());
    }
}
