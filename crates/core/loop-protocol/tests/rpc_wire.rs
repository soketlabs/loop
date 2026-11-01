//! Method wire-format tests: the JSON here is the contract with clients of
//! `loop-wire` (and the SDK generated from the schema).

use loop_protocol::rpc::{
    error_codes, negotiate, InitializeParams, InitializeResult, PeerInfo, PromptParams,
    PromptResult, RpcErrorBody,
};
use loop_protocol::{
    BackendError, Capabilities, ContentBlock, ErrorCategory, ErrorInfo, SessionRef, SessionSpec,
    StopReason, TurnOutcome, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

fn session() -> SessionRef {
    SessionRef {
        instance: "native".into(),
        key: "s1".into(),
        native_id: "n1".into(),
    }
}

fn roundtrip<T>(value: &T, expected: Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(value).unwrap(), expected);
    assert_eq!(&serde_json::from_value::<T>(expected).unwrap(), value);
}

#[test]
fn initialize() {
    roundtrip(
        &InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: PeerInfo {
                name: "loop".into(),
                version: "0.5.0".into(),
            },
            client_capabilities: Default::default(),
        },
        json!({
            "protocol_version": 1,
            "client_info": {"name": "loop", "version": "0.5.0"},
            "client_capabilities": {"interact": false}
        }),
    );
    let result: InitializeResult = serde_json::from_value(json!({
        "protocol_version": 1,
        "server_info": {"name": "loop", "version": "0.5.0"},
        "instance": "native",
        "capabilities": {"usage": true, "a_future_capability": true}
    }))
    .unwrap();
    assert!(result.capabilities.usage);
    assert_eq!(
        result.capabilities,
        Capabilities {
            usage: true,
            ..Default::default()
        }
    );
}

#[test]
fn sessions_and_prompts() {
    roundtrip(
        &session(),
        json!({"instance": "native", "key": "s1", "native_id": "n1"}),
    );
    roundtrip(&SessionSpec::default(), json!({}));
    roundtrip(
        &SessionSpec {
            cwd: Some("/work".into()),
            name: Some("x".into()),
        },
        json!({"cwd": "/work", "name": "x"}),
    );
    roundtrip(
        &PromptParams {
            session: session(),
            content: vec![ContentBlock::text("hi")],
        },
        json!({
            "session": {"instance": "native", "key": "s1", "native_id": "n1"},
            "content": [{"type": "text", "text": "hi"}]
        }),
    );
}

#[test]
fn every_turn_outcome_survives_the_wire() {
    let failed = ErrorInfo::new(ErrorCategory::RateLimit, "slow down");
    let cases = [
        (
            TurnOutcome::Completed(StopReason::EndTurn),
            json!({"stop": "end_turn"}),
        ),
        (
            TurnOutcome::Completed(StopReason::MaxTokens),
            json!({"stop": "max_tokens"}),
        ),
        (TurnOutcome::Cancelled, json!({"stop": "cancelled"})),
        (
            TurnOutcome::Failed(failed.clone()),
            json!({"stop": "error", "error": {"category": "rate_limit", "retryable": true, "message": "slow down"}}),
        ),
        (
            TurnOutcome::Uncertain,
            json!({"stop": "error", "uncertain": true}),
        ),
    ];
    for (outcome, expected) in cases {
        let result = PromptResult::from(outcome.clone());
        assert_eq!(serde_json::to_value(&result).unwrap(), expected);
        let back: PromptResult = serde_json::from_value(expected).unwrap();
        assert_eq!(TurnOutcome::from(back), outcome);
    }
}

#[test]
fn every_backend_error_survives_the_wire() {
    let errors = [
        BackendError::Busy,
        BackendError::Unsupported("images".into()),
        BackendError::SessionNotFound("s9".into()),
        BackendError::failed(ErrorCategory::Auth, "sign in again"),
    ];
    for error in errors {
        let body = RpcErrorBody::from(&error);
        let json = serde_json::to_value(&body).unwrap();
        let back: RpcErrorBody = serde_json::from_value(json).unwrap();
        assert_eq!(back.into_backend_error(), error);
    }
    assert_eq!(
        RpcErrorBody::from(&BackendError::Busy).code,
        error_codes::BUSY
    );
    let unknown = RpcErrorBody {
        code: -32601,
        message: "Method not found".into(),
        data: None,
    };
    assert!(matches!(
        unknown.into_backend_error(),
        BackendError::Failed(ErrorInfo {
            category: ErrorCategory::Protocol,
            ..
        })
    ));
}

#[test]
fn negotiation_picks_a_version_both_speak() {
    assert_eq!(negotiate(PROTOCOL_VERSION), PROTOCOL_VERSION);
    assert_eq!(
        negotiate(PROTOCOL_VERSION + 5),
        PROTOCOL_VERSION,
        "a newer client"
    );
    assert_eq!(
        negotiate(0),
        PROTOCOL_VERSION,
        "a version that never existed"
    );
}

#[test]
fn session_list_info_wire() {
    use loop_protocol::rpc::SessionInfo;
    roundtrip(
        &vec![SessionInfo {
            r#ref: session(),
            cwd: "/work/repo".into(),
            name: Some("Review".into()),
            parent: Some(session()),
            live: true,
            busy: false,
            created_ms: 1793400000000,
            last_active_ms: 1793400001000,
        }],
        json!([{"ref": {"instance":"native","key":"s1","native_id":"n1"},
            "cwd":"/work/repo", "name":"Review", "parent":{"instance":"native","key":"s1","native_id":"n1"},
            "live":true,"busy":false,"created_ms":1793400000000u64,"last_active_ms":1793400001000u64}]),
    );
    let minimal: SessionInfo = serde_json::from_value(json!({
        "ref":session(),"cwd":"/work","live":false,"busy":false,
        "created_ms":0,"last_active_ms":0,"future":true
    }))
    .unwrap();
    assert_eq!(minimal.name, None);
    assert_eq!(minimal.parent, None);
    assert_eq!(
        serde_json::to_value(minimal).unwrap()["parent"],
        Value::Null
    );
}

#[test]
fn session_control_wire_and_defaults() {
    use loop_protocol::rpc::{AttachParams, AttachResult, CloseParams, SessionParams};
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    roundtrip(
        &CloseParams {
            session: session(),
            force: true,
        },
        json!({"session":reference,"force":true}),
    );
    let close: CloseParams =
        serde_json::from_value(json!({"session":reference,"future":true})).unwrap();
    assert!(!close.force);
    roundtrip(&close, json!({"session":reference}));
    roundtrip(
        &AttachParams {
            session: session(),
            after_seq: 40,
        },
        json!({"session":reference,"after_seq":40}),
    );
    let attach: AttachParams =
        serde_json::from_value(json!({"session":reference,"future":true})).unwrap();
    assert_eq!(attach.after_seq, 0);
    roundtrip(&attach, json!({"session":reference}));
    roundtrip(
        &AttachResult {
            latest_seq: 42,
            generation: 3,
        },
        json!({"latest_seq":42,"generation":3}),
    );
    roundtrip(
        &SessionParams { session: session() },
        json!({"session":reference}),
    );
}

#[test]
fn runtime_capabilities_are_opt_in() {
    let old: Capabilities = serde_json::from_value(json!({"usage":true,"future":true})).unwrap();
    assert!(!old.multi_session);
    assert!(!old.attach);
    let capabilities = Capabilities {
        multi_session: true,
        attach: true,
        ..Default::default()
    };
    roundtrip(
        &capabilities,
        json!({"load_session":false,"resume_session":false,"set_model":false,
        "modes":false,"usage":false,"images":false,"steering":false,"fs_client":false,
        "terminal_client":false,"multi_session":true,"attach":true}),
    );
}

#[test]
fn interactions_route_to_a_session_and_withdraw_by_native_id() {
    use loop_protocol::rpc::{InteractParams, InteractWithdrawnParams};
    use loop_protocol::InteractionRequest;
    let cases = [
        json!({"kind":"permission","id":"p1","options":[]}),
        json!({"kind":"question","id":"p1","prompt":"Which?"}),
        json!({"kind":"form","id":"p1","schema":{"type":"object"}}),
        json!({"kind":"url","id":"p1","url":"https://example.com","purpose":"Sign in"}),
        json!({"kind":"unsupported","id":"p1","method":"future","raw":{}}),
    ];
    for legacy in cases {
        let request: InteractionRequest = serde_json::from_value(legacy.clone()).unwrap();
        let old: InteractParams = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(old.session, None);
        roundtrip(&old, legacy.clone());
        let mut routed = legacy;
        routed["session"] = serde_json::to_value(session()).unwrap();
        roundtrip(
            &InteractParams {
                session: Some(session()),
                request: request.clone(),
            },
            routed.clone(),
        );
        routed["future"] = json!(true);
        let extended: InteractParams = serde_json::from_value(routed.clone()).unwrap();
        assert_eq!(extended.request, request);
        // Old v1 readers still understand the interaction and ignore routing.
        assert_eq!(
            serde_json::from_value::<InteractionRequest>(routed).unwrap(),
            request
        );
    }
    roundtrip(
        &InteractWithdrawnParams {
            session: session(),
            id: "p1".into(),
        },
        json!({"session":{"instance":"native","key":"s1","native_id":"n1"},"id":"p1"}),
    );
}
