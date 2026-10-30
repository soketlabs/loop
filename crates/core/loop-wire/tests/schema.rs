//! The checked-in JSON Schema matches the types, and real messages validate
//! against it. Regenerate with `UPDATE_SCHEMA=1 cargo test -p loop-wire --test schema`.

use std::path::PathBuf;

use loop_protocol::rpc::json_schema;
use serde_json::{json, Value};

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../loop-protocol/schema/loop-protocol.json")
}

/// `value` with every object's keys sorted, so the file doesn't depend on
/// whether some crate in the build turns on serde_json's `preserve_order`.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let entries: std::collections::BTreeMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

#[test]
fn the_checked_in_schema_is_up_to_date() {
    let generated = sorted(json_schema());
    let path = schema_path();
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        let text = serde_json::to_string_pretty(&generated).unwrap() + "\n";
        std::fs::write(&path, text).unwrap();
    }
    let checked_in: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    assert!(
        checked_in == generated,
        "{} is out of date; run `UPDATE_SCHEMA=1 cargo test -p loop-wire --test schema`",
        path.display()
    );
}

/// A validator for one definition, resolving `$ref`s inside the document.
fn validator(definition: &str) -> jsonschema::Validator {
    let mut document = json_schema();
    document["$ref"] = json!(format!("#/$defs/{definition}"));
    jsonschema::validator_for(&document).unwrap()
}

#[test]
fn real_messages_validate() {
    let cases: [(&str, Value); 6] = [
        (
            "UpdateEnvelope",
            json!({"v": 1, "instance": "native", "generation": 0, "session": "s", "turn": "t",
                   "seq": 3, "origin": "live",
                   "update": {"type": "message_chunk", "message_id": "m", "role": "agent",
                              "content": {"type": "text", "text": "hi"}}}),
        ),
        (
            "UpdateEnvelope",
            json!({"v": 1, "instance": "native", "generation": 0, "session": "s", "seq": 4,
                   "origin": "live",
                   "update": {"type": "turn_ended", "turn_id": "t", "stop": "error",
                              "error": {"category": "rate_limit", "retryable": true, "message": "slow"}}}),
        ),
        (
            "PromptParams",
            json!({"session": {"instance": "native", "key": "s", "native_id": "n"},
                   "content": [{"type": "text", "text": "go"}]}),
        ),
        ("PromptResult", json!({"stop": "end_turn"})),
        (
            "InteractionRequest",
            json!({"kind": "question", "id": "q", "prompt": "Which?", "choices": null}),
        ),
        (
            "InitializeParams",
            json!({"protocol_version": 1, "client_info": {"name": "sdk", "version": "1"}}),
        ),
    ];
    for (definition, message) in cases {
        let validator = validator(definition);
        let errors: Vec<String> = validator.iter_errors(&message).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{definition}: {errors:?}\n{message}");
    }
    assert!(
        !validator("PromptResult").is_valid(&json!({"stop": 7})),
        "the schema rejects a wrong type"
    );
}

#[test]
fn tagged_enums_are_discriminated_unions() {
    let schema = json_schema();
    let update = &schema["$defs"]["SessionUpdate"];
    let variants = update["oneOf"]
        .as_array()
        .or(update["anyOf"].as_array())
        .expect("SessionUpdate is a union");
    let tags: Vec<&str> = variants
        .iter()
        .filter_map(|v| v["properties"]["type"]["const"].as_str())
        .collect();
    for tag in ["turn_started", "message_chunk", "tool_call", "turn_ended"] {
        assert!(tags.contains(&tag), "{tag} missing from {tags:?}");
    }
    let methods = schema["methods"].as_object().unwrap();
    assert_eq!(methods["session/prompt"]["kind"], "request");
    assert_eq!(methods["host/interact"]["direction"], "server_to_client");
}
