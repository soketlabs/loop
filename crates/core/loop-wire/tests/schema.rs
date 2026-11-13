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
        let errors: Vec<String> = validator
            .iter_errors(&message)
            .map(|e| e.to_string())
            .collect();
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

#[test]
fn runtime_methods_and_messages_are_in_the_schema() {
    use loop_protocol::rpc::methods;
    let schema = json_schema();
    assert_eq!(schema["protocol_version"], 1);
    for method in [
        methods::SESSION_LIST,
        methods::SESSION_CLOSE,
        methods::SESSION_ATTACH,
        methods::SESSION_DETACH,
        methods::RUNTIME_STATUS,
        methods::RUNTIME_STOP,
    ] {
        assert_eq!(schema["methods"][method]["direction"], "client_to_server");
        assert_eq!(schema["methods"][method]["kind"], "request");
        assert!(schema["methods"][method].get("result").is_some());
    }
    assert_eq!(
        schema["methods"][methods::HOST_INTERACT_WITHDRAWN]["kind"],
        "notification"
    );
    assert_eq!(
        schema["methods"][methods::HOST_INTERACT_WITHDRAWN]["direction"],
        "server_to_client"
    );
    let session = json!({"instance":"loop","key":"s1","native_id":"s1"});
    for (definition, message) in [
        (
            "SessionInfo",
            json!({"ref":session,"cwd":"/work","name":null,"parent":null,"live":true,"busy":false,"created_ms":1,"last_active_ms":2}),
        ),
        ("CloseParams", json!({"session":session})),
        (
            "RuntimeStatus",
            json!({"pid":42,"socket":"/a/run/runtime.sock","generation":3,"loaded":2,"busy":1,"max_sessions":16,"max_active_turns":8}),
        ),
        ("AttachParams", json!({"session":session})),
        ("AttachResult", json!({"latest_seq":42,"generation":3})),
        (
            "InteractParams",
            json!({"kind":"permission","id":"p1","session":session,"options":[]}),
        ),
        (
            "InteractParams",
            json!({"kind":"question","id":"q1","prompt":"Which?"}),
        ),
        (
            "InteractWithdrawnParams",
            json!({"session":session,"id":"p1"}),
        ),
        ("StopReason", json!("runtime_restarted")),
    ] {
        assert!(
            validator(definition).is_valid(&message),
            "{definition}: {message}"
        );
    }
    assert!(!validator("RuntimeStatus").is_valid(&json!({"pid":42,"loaded":-1})));
    assert!(!validator("AttachParams").is_valid(&json!({"session":session,"after_seq":-1})));
    assert!(!validator("InteractParams")
        .is_valid(&json!({"kind":"permission","id":"p1","session":7,"options":[]})));
}

#[test]
fn session_tree_contracts_have_schema_entries_and_validate() {
    use loop_protocol::rpc::methods;
    let schema = json_schema();
    for method in [
        methods::SESSION_TREE,
        methods::SESSION_VIEW,
        methods::SESSION_NAVIGATE,
        methods::SESSION_RENAME,
        methods::SESSION_FORK,
        methods::SESSION_CLONE,
        methods::SESSION_EXPORT,
        methods::SESSION_IMPORT,
    ] {
        assert_eq!(schema["methods"][method]["kind"], "request");
    }
    let session = json!({"instance":"native","key":"s","native_id":"s"});
    for (definition, value) in [
        ("TreeParams", json!({"session":session,"limit":100})),
        ("ViewParams", json!({"session":session,"view_id":"view"})),
        ("NavigateParams", json!({"session":session})),
        ("RenameParams", json!({"session":session,"name":"new name"})),
        (
            "ForkParams",
            json!({"session":session,"before_entry_id":"user-1","request_id":"request-1"}),
        ),
        (
            "CloneParams",
            json!({"session":session,"request_id":"request-2"}),
        ),
        (
            "ImportParams",
            json!({"workspace_id":"workspace-1","jsonl":"payload","request_id":"request-3"}),
        ),
        (
            "SessionUpdate",
            json!({"type":"branch_reset","revision":1,"branch_revision":1,"view_id":"view","reason":"navigate"}),
        ),
    ] {
        validator(definition).validate(&value).unwrap();
    }
}

#[test]
fn model_control_methods_and_messages_are_in_the_schema() {
    use loop_protocol::rpc::methods;
    let schema = json_schema();
    for method in [
        methods::SESSION_MODELS,
        methods::SESSION_SET_MODEL,
        methods::SESSION_EFFORTS,
        methods::SESSION_SET_EFFORT,
        methods::SESSION_MODES,
        methods::SESSION_SET_MODE,
    ] {
        assert_eq!(schema["methods"][method]["direction"], "client_to_server");
        assert_eq!(schema["methods"][method]["kind"], "request");
        assert!(
            schema["methods"][method].get("result").is_some(),
            "{method}"
        );
    }
    let reference = json!({"instance": "loop", "key": "s", "native_id": "s"});
    let cases = [
        (
            "UpdateEnvelope",
            json!({"v": 1, "instance": "loop", "generation": 2, "session": "s", "seq": 9,
                   "origin": "live",
                   "update": {"type": "config_changed", "revision": 3, "metadata": {"name": "Review"},
                              "config": {"model": "faux/one", "effort": "off"},
                              "effective": {"model": "faux/one", "effort": "high"},
                              "pending": true, "applies_after_turn": "turn-2",
                              "workspace_revision": 0}}),
        ),
        (
            "ModelsResult",
            json!({"current": "faux/one", "revision": 3,
                   "models": [{"id": "faux/one", "name": "One", "backend": "loop"}]}),
        ),
        (
            "ChoicesResult",
            json!({"current": "off", "revision": 3, "choices": [{"id": "off", "name": "off"}]}),
        ),
        (
            "SetModelParams",
            json!({"session": reference, "model": "faux/two", "expected_revision": 3}),
        ),
        (
            "SetEffortParams",
            json!({"session": reference, "effort": "high"}),
        ),
        (
            "SetModeParams",
            json!({"session": reference, "mode": "plan"}),
        ),
        (
            "MutationResult",
            json!({"revision": 4, "seq": 10, "pending": true, "applies_after_turn": "turn-2"}),
        ),
        (
            "AttachResult",
            json!({"latest_seq": 9, "generation": 2,
                   "snapshot": {"session": reference, "at_seq": 9, "revision": 3,
                                "metadata": {}, "config": {"effort": "off"},
                                "effective": {"effort": "off"}, "pending": false,
                                "capabilities": {"attach_snapshot": true}}}),
        ),
        ("AttachResult", json!({"latest_seq": 9, "generation": 2})),
    ];
    for (definition, message) in cases {
        let validator = validator(definition);
        let errors: Vec<String> = validator
            .iter_errors(&message)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{definition}: {errors:?}\n{message}");
    }
}

#[test]
fn compact_stats_and_shell_are_in_the_schema() {
    use loop_protocol::rpc::methods;
    let schema = json_schema();
    for method in [
        methods::SESSION_COMPACT,
        methods::SESSION_STATS,
        methods::SESSION_SHELL,
        methods::SESSION_TTFB,
    ] {
        assert_eq!(schema["methods"][method]["direction"], "client_to_server");
        assert_eq!(schema["methods"][method]["kind"], "request");
        assert!(
            schema["methods"][method].get("result").is_some(),
            "{method}"
        );
    }
    assert_eq!(schema["limits"]["shell_output_bytes"], 1024 * 1024);
    let reference = json!({"instance": "loop", "key": "s", "native_id": "s"});
    let cases = [
        (
            "CompactParams",
            json!({"session": reference, "instructions": "keep the plan"}),
        ),
        ("CompactParams", json!({"session": reference})),
        (
            "CompactionReport",
            json!({"tokens_before": 900, "summary": "Earlier work", "branch_revision": 2, "seq": 7}),
        ),
        (
            "StatsResult",
            json!({"total_tokens": 40, "context_tokens": 30, "context_window": 200000,
                   "report": "Tokens", "turn_state": "idle", "taken_at_ms": 5}),
        ),
        (
            "StatsResult",
            json!({"total_tokens": 0, "report": "", "turn_state": "busy", "taken_at_ms": 5}),
        ),
        (
            "ShellParams",
            json!({"session": reference, "command": "ls", "timeout_ms": 1000}),
        ),
        (
            "ShellResult",
            json!({"stdout": "a", "stderr": "", "exit_code": 0, "truncated": true}),
        ),
        (
            "Capabilities",
            json!({"compaction": true, "session_stats": true, "local_shell": true}),
        ),
    ];
    for (definition, message) in cases {
        let validator = validator(definition);
        let errors: Vec<String> = validator
            .iter_errors(&message)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{definition}: {errors:?}\n{message}");
    }
}

#[test]
fn admin_methods_are_in_the_schema_and_host_only_ones_are_marked() {
    use loop_protocol::rpc::methods;
    let schema = json_schema();
    for method in [
        methods::WORKSPACE_MCP,
        methods::WORKSPACE_WEB_SEARCH,
        methods::WORKSPACE_SKILLS,
        methods::WORKSPACE_TEMPLATES,
        methods::WORKSPACE_RELOAD,
        methods::WORKSPACE_TRUST,
        methods::SESSION_SANDBOX,
        methods::SESSION_PERMISSION_MODE,
        methods::RUNTIME_USAGE,
        methods::RUNTIME_DEFAULTS,
        methods::RUNTIME_SESSIONS,
    ] {
        let entry = &schema["methods"][method];
        assert_eq!(entry["kind"], "request", "{method}");
        assert!(entry.get("result").is_some(), "{method}");
        let host_only = methods::HOST_ONLY.contains(&method);
        assert_eq!(entry.get("host_only").is_some(), host_only, "{method}");
    }
    assert_eq!(schema["methods"][methods::RUNTIME_STOP]["host_only"], true);
    let session = json!({"instance": "loop", "key": "s", "native_id": "s"});
    for (definition, message) in [
        (
            "McpParams",
            json!({"workspace_id": "w", "action": "enable", "name": "docs"}),
        ),
        ("TrustParams", json!({"workspace_id": "w", "trusted": true})),
        (
            "ReloadResult",
            json!({"revision": 1, "notices": [], "pending_sessions": 1}),
        ),
        (
            "SandboxParams",
            json!({"session": session, "action": "set", "config": {"mode": "off"}}),
        ),
        (
            "PermissionModeParams",
            json!({"session": session, "mode": "plan"}),
        ),
        (
            "UsageResult",
            json!({"rows": [{"session_id": "a"}], "totals": {}, "taken_at_ms": 1,
                               "log_status": "ok"}),
        ),
    ] {
        let validator = validator(definition);
        assert!(validator.is_valid(&message), "{definition}: {message}");
    }
    assert!(!validator("McpParams").is_valid(&json!({"workspace_id": "w", "action": "nuke"})));
}

#[test]
fn identity_in_the_initialize_result_validates() {
    use loop_protocol::{Caller, Principal, PrincipalKind, Tenant};
    let edge = Caller::asserted(
        Principal::new(PrincipalKind::Person, "https://accounts.google.com", "42").unwrap(),
        Tenant::new("acme").unwrap(),
    )
    .unwrap();
    let principal = validator("Principal");
    let tenant = validator("Tenant");
    let caller = validator("Caller");
    for value in [
        serde_json::to_value(Caller::host(Principal::local_uid(501))).unwrap(),
        serde_json::to_value(Caller::local_token(Principal::local_uid(501))).unwrap(),
        serde_json::to_value(&edge).unwrap(),
    ] {
        assert!(caller.is_valid(&value), "{value}");
        assert!(principal.is_valid(&value["principal"]), "{value}");
        assert!(tenant.is_valid(&value["tenant"]), "{value}");
    }
    let result = validator("InitializeResult");
    let mut message = json!({
        "protocol_version": 1, "server_info": {"name": "loop", "version": "1"},
        "instance": "runtime", "capabilities": {},
        "principal": {"kind": "person", "issuer": "local", "id": "uid:501"},
        "tenant": "local"
    });
    let errors: Vec<String> = result
        .iter_errors(&message)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    message["tenant"] = json!("A/B");
    assert!(
        !result.is_valid(&message),
        "the schema rejects a bad tenant"
    );
    assert!(!principal.is_valid(&json!({"kind": "person", "issuer": "local", "id": "a b"})));
}
