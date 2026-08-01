//! Best-effort partial JSON parsing for streaming tool arguments.

use serde_json::Value;

/// Parse a (possibly incomplete) JSON string into a Value.
///
/// Returns the best-effort object/array so far; falls back to an empty object
/// when the buffer is not yet parseable.
pub fn parse_streaming_json(partial: &str) -> Value {
    let trimmed = partial.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return v;
    }
    // Try closing open braces/brackets/quotes heuristically.
    if let Some(repaired) = try_repair(trimmed) {
        if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
            return v;
        }
    }
    Value::Object(Default::default())
}

fn try_repair(input: &str) -> Option<String> {
    let mut out = String::from(input);
    let mut in_string = false;
    let mut escape = false;
    let mut stack: Vec<char> = Vec::new();

    for ch in input.chars() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.last() == Some(&ch) {
                    stack.pop();
                }
            }
            _ => {}
        }
    }

    if in_string {
        out.push('"');
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_json() {
        let v = parse_streaming_json(r#"{"a":1}"#);
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn repairs_partial_object() {
        let v = parse_streaming_json(r#"{"name":"wo"#);
        assert!(v.get("name").is_some());
    }
}
