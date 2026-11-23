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

/// A client reads the identity of a newer server leniently: a value that
/// today's rules refuse reads as absent, and `initialize` still succeeds.
#[test]
fn a_client_reads_a_newer_server_s_identity_leniently() {
    let base = json!({
        "protocol_version": 1,
        "server_info": {"name": "loop", "version": "9.0.0"},
        "instance": "native",
        "capabilities": {}
    });
    let with = |fields: Value| {
        let mut value = base.clone();
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        serde_json::from_value::<InitializeResult>(value).unwrap()
    };
    let good = with(json!({
        "principal": {"kind": "person", "issuer": "https://idp", "id": "alice"},
        "tenant": "acme",
        "via": "edge"
    }));
    assert_eq!(good.principal.unwrap().id(), "alice");
    assert_eq!(good.tenant.unwrap().as_str(), "acme");
    assert_eq!(good.via.unwrap().as_str(), "edge");
    let newer = with(json!({
        "principal": {"kind": "person", "issuer": "https://idp", "id": "has space"},
        "tenant": "Upper/Case",
        "via": "a via with spaces"
    }));
    assert_eq!(
        (newer.principal, newer.tenant, newer.via),
        (None, None, None)
    );
    // A kind this client does not know is kept, not dropped.
    let robot = with(json!({"principal": {"kind": "robot", "issuer": "https://idp", "id": "r"}}));
    assert_eq!(robot.principal.unwrap().kind().as_str(), "robot");
    assert!(with(json!({"principal": null})).principal.is_none());
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
            ..Default::default()
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
        BackendError::Conflict { revision: 7 },
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
            summary: Default::default(),
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
            snapshot: None,
        },
        json!({"latest_seq":42,"generation":3}),
    );
    roundtrip(
        &SessionParams { session: session() },
        json!({"session":reference}),
    );
}

#[test]
fn runtime_status_wire() {
    use loop_protocol::rpc::RuntimeStatus;
    roundtrip(
        &RuntimeStatus {
            auto_started: false,
            pid: 42,
            socket: "/a/run/runtime.sock".into(),
            listen_url: None,
            generation: 3,
            loaded: 2,
            busy: 1,
            max_sessions: 16,
            max_active_turns: 8,
            max_sessions_from: None,
            max_active_turns_from: None,
            tool_lock: None,
        },
        json!({"pid":42,"socket":"/a/run/runtime.sock","generation":3,"loaded":2,"busy":1,
            "max_sessions":16,"max_active_turns":8}),
    );
    roundtrip(
        &RuntimeStatus {
            auto_started: true,
            pid: 42,
            socket: "/s".into(),
            listen_url: Some("ws://127.0.0.1:1234/".into()),
            generation: 3,
            loaded: 0,
            busy: 0,
            max_sessions: 3,
            max_active_turns: 8,
            max_sessions_from: Some("settings".into()),
            max_active_turns_from: Some("default".into()),
            tool_lock: None,
        },
        json!({"auto_started":true,"pid":42,"socket":"/s","listen_url":"ws://127.0.0.1:1234/","generation":3,"loaded":0,"busy":0,
            "max_sessions":3,"max_active_turns":8,
            "max_sessions_from":"settings","max_active_turns_from":"default"}),
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
        "terminal_client":false,"multi_session":true,"attach":true,"efforts":false,
        "attach_snapshot":false}),
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

#[test]
fn session_tree_v1_capabilities_and_event_goldens() {
    use loop_protocol::rpc::*;
    use loop_protocol::SessionUpdate;
    let legacy: ClientCapabilities = serde_json::from_value(json!({"interact":true})).unwrap();
    assert!(!legacy.branch_reset);
    let capabilities: Capabilities = serde_json::from_value(json!({})).unwrap();
    assert!(
        !capabilities.session_tree
            && !capabilities.branch_snapshot
            && !capabilities.session_transfer
    );
    roundtrip(
        &NavigateParams {
            session: session(),
            target_entry_id: None,
            summarize: false,
            expected_revision: Some(2),
        },
        json!({"session":session(),"target_entry_id":null,"summarize":false,"expected_revision":2}),
    );
    roundtrip(
        &SessionUpdate::BranchReset {
            revision: 5,
            branch_revision: 3,
            view_id: "view-1".into(),
            reason: "navigate".into(),
        },
        json!({"type":"branch_reset","revision":5,"branch_revision":3,"view_id":"view-1","reason":"navigate"}),
    );
    roundtrip(
        &NavigateResult {
            revision: 5,
            branch_revision: 3,
            seq: 9,
        },
        json!({"revision":5,"branch_revision":3,"seq":9}),
    );
}

#[test]
fn revision_conflicts_preserve_the_current_revision_over_the_wire() {
    let error = loop_protocol::BackendError::Conflict { revision: 7 };
    let body = loop_protocol::rpc::RpcErrorBody::from(&error);
    assert_eq!(body.code, loop_protocol::rpc::error_codes::CONFLICT);
    assert_eq!(body.data, Some(json!({"revision":7})));
    assert_eq!(body.into_backend_error(), error);
}

#[test]
fn model_control_methods_wire() {
    use loop_protocol::rpc::{
        methods, ChoiceInfo, ChoicesResult, ModelInfo, ModelsResult, SetEffortParams,
        SetModeParams, SetModelParams,
    };
    use loop_protocol::{ModelChoice, ModelChoices, MutationResult};
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    assert_eq!(
        [
            methods::SESSION_MODELS,
            methods::SESSION_SET_MODEL,
            methods::SESSION_EFFORTS,
            methods::SESSION_SET_EFFORT,
            methods::SESSION_MODES,
            methods::SESSION_SET_MODE,
        ],
        [
            "session/models",
            "session/set_model",
            "session/efforts",
            "session/set_effort",
            "session/modes",
            "session/set_mode",
        ]
    );
    let models = ModelsResult {
        current: Some("faux/one".into()),
        models: vec![ModelInfo {
            id: "faux/one".into(),
            name: "One".into(),
            description: None,
            backend: "loop".into(),
        }],
        revision: 4,
    };
    roundtrip(
        &models,
        json!({"current":"faux/one","models":[{"id":"faux/one","name":"One","backend":"loop"}],
            "revision":4}),
    );
    let choices = ModelChoices::from(models.clone());
    assert_eq!(
        choices,
        ModelChoices {
            current: Some("faux/one".into()),
            models: vec![ModelChoice {
                id: "faux/one".into(),
                name: "One".into(),
                description: None,
            }],
            revision: 4,
        }
    );
    assert_eq!(ModelsResult::from_choices(choices, &"loop".into()), models);
    let efforts = ChoicesResult {
        current: None,
        choices: vec![ChoiceInfo {
            id: "off".into(),
            name: "off".into(),
        }],
        revision: 0,
    };
    roundtrip(
        &efforts,
        json!({"choices":[{"id":"off","name":"off"}],"revision":0}),
    );
    assert_eq!(
        ChoicesResult::from(ModelChoices::from(efforts.clone())),
        efforts
    );
    roundtrip(
        &SetModelParams {
            session: session(),
            model: "faux/two".into(),
            expected_revision: Some(4),
        },
        json!({"session":reference,"model":"faux/two","expected_revision":4}),
    );
    roundtrip(
        &SetEffortParams {
            session: session(),
            effort: "off".into(),
            expected_revision: None,
        },
        json!({"session":reference,"effort":"off"}),
    );
    roundtrip(
        &SetModeParams {
            session: session(),
            mode: "plan".into(),
            expected_revision: None,
        },
        json!({"session":reference,"mode":"plan"}),
    );
    roundtrip(
        &MutationResult {
            revision: 5,
            seq: 12,
            pending: true,
            applies_after_turn: Some("turn-3".into()),
        },
        json!({"revision":5,"seq":12,"pending":true,"applies_after_turn":"turn-3"}),
    );
    let old: MutationResult = serde_json::from_value(json!({"revision":1,"seq":2})).unwrap();
    assert!(!old.pending);
}

#[test]
fn a_conflict_carries_the_current_revision() {
    let body = RpcErrorBody::from(&BackendError::Conflict { revision: 9 });
    assert_eq!(body.code, error_codes::CONFLICT);
    assert_eq!(body.data, Some(json!({"revision": 9})));
}

#[test]
fn config_changed_and_attach_snapshot_wire() {
    use loop_protocol::rpc::{AttachResult, SessionSnapshot};
    use loop_protocol::{ConfigChanged, SessionConfig, SessionMetadata, SessionUpdate};
    let config = SessionConfig {
        model: Some("faux/one".into()),
        effort: Some("off".into()),
        mode: None,
        response_timeout_ms: None,
        sandbox: None,
        active_skills: vec![],
    };
    let update = SessionUpdate::ConfigChanged(ConfigChanged {
        revision: 3,
        metadata: SessionMetadata {
            name: Some("Review".into()),
        },
        config: config.clone(),
        effective: SessionConfig {
            effort: Some("high".into()),
            ..config.clone()
        },
        pending: true,
        applies_after_turn: Some("turn-1".into()),
        workspace_revision: 0,
    });
    roundtrip(
        &update,
        json!({"type":"config_changed","revision":3,"metadata":{"name":"Review"},
            "config":{"model":"faux/one","effort":"off"},
            "effective":{"model":"faux/one","effort":"high"},
            "pending":true,"applies_after_turn":"turn-1","workspace_revision":0}),
    );
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    let attached = AttachResult {
        latest_seq: 8,
        generation: 2,
        snapshot: Some(SessionSnapshot {
            notices: vec![],
            session: session(),
            at_seq: 8,
            revision: 3,
            metadata: SessionMetadata::default(),
            config: config.clone(),
            effective: config,
            pending: false,
            capabilities: Capabilities::default(),
            workspace_id: String::new(),
            branch_revision: 0,
            view_id: None,
            turn_state: None,
            stats: serde_json::Value::Null,
            workspace_revision: 0,
        }),
    };
    let value = serde_json::to_value(&attached).unwrap();
    assert_eq!(value["snapshot"]["session"], reference);
    assert_eq!(value["snapshot"]["at_seq"], 8);
    assert_eq!(value["snapshot"]["config"]["effort"], "off");
    assert_eq!(
        serde_json::from_value::<AttachResult>(value).unwrap(),
        attached
    );
    // An old server's result has no snapshot.
    let old: AttachResult = serde_json::from_value(json!({"latest_seq":8,"generation":2})).unwrap();
    assert_eq!(old.snapshot, None);
}

#[test]
fn compact_stats_and_shell_methods_wire() {
    use loop_protocol::rpc::{methods, CompactParams, ShellParams, StatsResult};
    use loop_protocol::{Capabilities, CompactionReport, ShellResult, StatsSummary};
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    let session: loop_protocol::SessionRef = serde_json::from_value(reference.clone()).unwrap();
    assert_eq!(
        [
            methods::SESSION_COMPACT,
            methods::SESSION_STATS,
            methods::SESSION_SHELL
        ],
        ["session/compact", "session/stats", "session/shell"]
    );
    roundtrip(
        &CompactParams {
            session: session.clone(),
            instructions: Some("keep the plan".into()),
        },
        json!({"session":reference,"instructions":"keep the plan"}),
    );
    roundtrip(
        &CompactParams {
            session: session.clone(),
            instructions: None,
        },
        json!({"session":reference}),
    );
    roundtrip(
        &CompactionReport {
            tokens_before: Some(1200),
            summary: "Earlier work".into(),
            branch_revision: 2,
            seq: 9,
        },
        json!({"tokens_before":1200,"summary":"Earlier work","branch_revision":2,"seq":9}),
    );
    let stats = StatsResult {
        total_tokens: 40,
        context_tokens: Some(30),
        context_window: Some(200_000),
        report: "Tokens: 40".into(),
        turn_state: "idle".into(),
        usage: None,
        taken_at_ms: 5,
    };
    roundtrip(
        &stats,
        json!({"total_tokens":40,"context_tokens":30,"context_window":200000,
            "report":"Tokens: 40","turn_state":"idle","taken_at_ms":5}),
    );
    // Unknown values stay unknown.
    let unknown: StatsResult = serde_json::from_value(
        json!({"total_tokens":0,"report":"","turn_state":"busy","taken_at_ms":1}),
    )
    .unwrap();
    assert_eq!(unknown.context_window, None);
    let summary = StatsSummary::from(stats);
    assert_eq!(summary.context_window, Some(200_000));
    assert_eq!(summary.turn_state.as_deref(), Some("idle"));
    roundtrip(
        &ShellParams {
            session: session.clone(),
            command: "ls".into(),
            timeout_ms: Some(1000),
        },
        json!({"session":reference,"command":"ls","timeout_ms":1000}),
    );
    roundtrip(
        &ShellResult {
            stdout: "a\n".into(),
            stderr: String::new(),
            exit_code: 0,
            truncated: false,
        },
        json!({"stdout":"a\n","stderr":"","exit_code":0,"truncated":false}),
    );
    // Old servers omit the new flags; they mean false and are not written.
    let old: Capabilities = serde_json::from_value(json!({"attach":true})).unwrap();
    assert!(!old.compaction && !old.session_stats && !old.local_shell);
    let value = serde_json::to_value(Capabilities::default()).unwrap();
    assert!(value.get("compaction").is_none() && value.get("local_shell").is_none());
    let value = serde_json::to_value(Capabilities {
        compaction: true,
        session_stats: true,
        local_shell: true,
        ..Capabilities::default()
    })
    .unwrap();
    assert_eq!(value["compaction"], true);
    assert_eq!(value["session_stats"], true);
    assert_eq!(value["local_shell"], true);
}

#[test]
fn shell_bounds() {
    use loop_protocol::{shell_timeout, ShellResult, SHELL_OUTPUT_LIMIT};
    // The default is the host's maximum (its bash.maxTimeoutMs).
    assert_eq!(shell_timeout(None, 600_000).unwrap(), 600_000);
    assert_eq!(shell_timeout(None, 30_000).unwrap(), 30_000);
    assert_eq!(shell_timeout(Some(1), 600_000).unwrap(), 1);
    assert_eq!(shell_timeout(Some(600_000), 600_000).unwrap(), 600_000);
    assert!(shell_timeout(Some(0), 600_000).is_err());
    assert!(shell_timeout(Some(30_001), 30_000).is_err());
    // A cut never splits a character, and only a cut marks truncation.
    let exact = ShellResult {
        stdout: "a".repeat(SHELL_OUTPUT_LIMIT),
        ..ShellResult::default()
    }
    .bounded();
    assert!(!exact.truncated);
    let mut long = "a".repeat(SHELL_OUTPUT_LIMIT - 1);
    long.push('é');
    let cut = ShellResult {
        stderr: long,
        ..ShellResult::default()
    }
    .bounded();
    assert!(cut.truncated);
    assert_eq!(cut.stderr.len(), SHELL_OUTPUT_LIMIT - 1);
}

#[test]
fn ttfb_method_wire() {
    use loop_protocol::rpc::{methods, TtfbParams, TtfbResult};
    use loop_protocol::Capabilities;
    assert_eq!(methods::SESSION_TTFB, "session/ttfb");
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    let session: loop_protocol::SessionRef = serde_json::from_value(reference.clone()).unwrap();
    let read: TtfbParams = serde_json::from_value(json!({"session":reference})).unwrap();
    assert_eq!(read.change().unwrap(), None);
    roundtrip(
        &TtfbParams {
            session: session.clone(),
            timeout_ms: Some(0),
            clear_override: false,
            expected_revision: Some(3),
        },
        json!({"session":reference,"timeout_ms":0,"clear_override":false,"expected_revision":3}),
    );
    let zero: TtfbParams =
        serde_json::from_value(json!({"session":reference,"timeout_ms":0})).unwrap();
    assert_eq!(zero.change().unwrap(), Some(Some(0)));
    let clear: TtfbParams =
        serde_json::from_value(json!({"session":reference,"clear_override":true})).unwrap();
    assert_eq!(clear.change().unwrap(), Some(None));
    let both: TtfbParams =
        serde_json::from_value(json!({"session":reference,"timeout_ms":5,"clear_override":true}))
            .unwrap();
    assert!(both.change().is_err());
    roundtrip(
        &TtfbResult {
            override_ms: Some(0),
            effective_ms: 0,
            revision: 4,
            seq: Some(9),
            pending: true,
        },
        json!({"override_ms":0,"effective_ms":0,"revision":4,"seq":9,"pending":true}),
    );
    roundtrip(
        &TtfbResult {
            override_ms: None,
            effective_ms: 120_000,
            revision: 4,
            seq: None,
            pending: false,
        },
        json!({"effective_ms":120000,"revision":4,"pending":false}),
    );
    let old: Capabilities = serde_json::from_value(json!({})).unwrap();
    assert!(!old.session_ttfb);
    let config: loop_protocol::SessionConfig =
        serde_json::from_value(json!({"model":"faux/one"})).unwrap();
    assert_eq!(config.response_timeout_ms, None);
    assert_eq!(
        serde_json::to_value(loop_protocol::SessionConfig {
            response_timeout_ms: Some(0),
            ..Default::default()
        })
        .unwrap(),
        json!({"response_timeout_ms":0})
    );
}

/// The Session's chosen sandbox is an optional config field: absent means
/// the workspace default, and older peers' configs still read.
#[test]
fn the_chosen_sandbox_is_an_optional_config_field() {
    let old: loop_protocol::SessionConfig =
        serde_json::from_value(json!({"model":"faux/one"})).unwrap();
    assert_eq!(old.sandbox, None);
    let config = loop_protocol::SessionConfig {
        sandbox: Some("local/full/krun".into()),
        ..Default::default()
    };
    let wire = serde_json::to_value(&config).unwrap();
    assert_eq!(wire, json!({"sandbox":"local/full/krun"}));
    assert_eq!(
        serde_json::from_value::<loop_protocol::SessionConfig>(wire).unwrap(),
        config
    );
}

#[test]
fn workspace_admin_and_host_only_methods_wire() {
    use loop_protocol::admin::*;
    use loop_protocol::rpc::methods;
    assert_eq!(methods::WORKSPACE_MCP, "workspace/mcp");
    assert_eq!(methods::WORKSPACE_WEB_SEARCH, "workspace/web_search");
    assert_eq!(methods::WORKSPACE_SKILLS, "workspace/skills");
    assert_eq!(methods::WORKSPACE_TEMPLATES, "workspace/templates");
    assert_eq!(methods::WORKSPACE_RELOAD, "workspace/reload");
    assert_eq!(methods::WORKSPACE_TRUST, "workspace/trust");
    assert_eq!(methods::SESSION_SANDBOX, "session/sandbox");
    assert_eq!(methods::SESSION_PERMISSION_MODE, "session/permission_mode");
    assert_eq!(methods::RUNTIME_USAGE, "runtime/usage");
    assert_eq!(methods::RUNTIME_DEFAULTS, "runtime/defaults");
    assert_eq!(methods::RUNTIME_SESSIONS, "runtime/sessions");
    assert!(!methods::HOST_ONLY.contains(&methods::RUNTIME_SESSIONS));
    for method in [
        methods::RUNTIME_STOP,
        methods::RUNTIME_USAGE,
        methods::RUNTIME_DEFAULTS,
        methods::RUNTIME_PROVIDERS,
        methods::RUNTIME_MCP_AUTH,
        methods::RUNTIME_TRACING,
    ] {
        assert!(methods::HOST_ONLY.contains(&method), "{method}");
    }
    assert!(!methods::HOST_ONLY.contains(&methods::WORKSPACE_TRUST));

    let list: McpParams = serde_json::from_value(json!({"workspace_id":"w"})).unwrap();
    assert_eq!(list.action, AdminAction::List);
    roundtrip(
        &McpParams {
            workspace_id: "w".into(),
            action: AdminAction::Disable,
            name: Some("docs".into()),
            expected_revision: Some(2),
        },
        json!({"workspace_id":"w","action":"disable","name":"docs","expected_revision":2}),
    );
    roundtrip(
        &McpResult {
            revision: 3,
            servers: vec![McpServerStatus {
                name: "docs".into(),
                enabled: true,
                connected: false,
                tools: 0,
                error: Some("refused".into()),
            }],
            pending_sessions: 1,
        },
        json!({"revision":3,"servers":[{"name":"docs","enabled":true,"connected":false,
            "tools":0,"error":"refused"}],"pending_sessions":1}),
    );
    roundtrip(
        &TrustParams {
            workspace_id: "w".into(),
            trusted: None,
            expected_revision: None,
        },
        json!({"workspace_id":"w"}),
    );
    roundtrip(
        &TemplatesResult {
            revision: 1,
            templates: None,
            text: Some("Review x".into()),
        },
        json!({"revision":1,"text":"Review x"}),
    );
    roundtrip(
        &ReloadResult {
            revision: 4,
            notices: vec!["reloaded".into()],
            pending_sessions: 0,
        },
        json!({"revision":4,"notices":["reloaded"],"pending_sessions":0}),
    );
    let reference = json!({"instance":"native","key":"s1","native_id":"n1"});
    let session: loop_protocol::SessionRef = serde_json::from_value(reference.clone()).unwrap();
    roundtrip(
        &SandboxParams {
            session: session.clone(),
            action: SandboxAction::Set,
            config: Some(SandboxChoice {
                mode: "local".into(),
                isolation: Some("partial".into()),
                runtime: None,
            }),
            expected_revision: None,
        },
        json!({"session":reference,"action":"set","config":{"mode":"local","isolation":"partial"}}),
    );
    roundtrip(
        &PermissionModeResult {
            mode: "ask".into(),
            effective_policy: "asks".into(),
            revision: 5,
            seq: Some(9),
            pending: false,
        },
        json!({"mode":"ask","effective_policy":"asks","revision":5,"seq":9,"pending":false}),
    );
    roundtrip(
        &UsageParams {
            days: Some(7),
            cursor: None,
            limit: Some(10),
        },
        json!({"days":7,"limit":10}),
    );
    roundtrip(
        &DefaultsParams {
            workspace_id: None,
            patch: Some(DefaultsPatch {
                response_header_timeout_ms: Some(0),
                ..DefaultsPatch::default()
            }),
            expected_revision: None,
        },
        json!({"patch":{"response_header_timeout_ms":0}}),
    );
    roundtrip(
        &DefaultsPatch {
            model: Some("faux/second".into()),
            effort: Some("high".into()),
            response_header_timeout_ms: None,
        },
        json!({"model":"faux/second","effort":"high"}),
    );
}

#[test]
fn forbidden_error_and_admin_capabilities_wire() {
    use loop_protocol::rpc::{error_codes, RpcErrorBody};
    use loop_protocol::{BackendError, Capabilities};
    let error = BackendError::Forbidden("runtime/usage".into());
    let body = RpcErrorBody::from(&error);
    assert_eq!(error_codes::FORBIDDEN, -32006);
    assert_eq!(
        serde_json::to_value(&body).unwrap(),
        json!({"code":-32006,
            "message":"runtime/usage is only available on the Runtime host; run it there",
            "data":"runtime/usage"})
    );
    assert_eq!(body.into_backend_error(), error);
    // New flags are left out until offered, so older goldens are unchanged.
    let none = serde_json::to_value(Capabilities::default()).unwrap();
    assert!(none.get("workspace_reload").is_none());
    let some = serde_json::to_value(Capabilities {
        workspace_reload: true,
        host_usage: true,
        ..Capabilities::default()
    })
    .unwrap();
    assert_eq!(some["workspace_reload"], json!(true));
    assert_eq!(some["host_usage"], json!(true));
}

#[test]
fn tool_locked_error_wire() {
    use loop_protocol::rpc::{error_codes, RpcErrorBody};
    use loop_protocol::{BackendError, ToolLock};
    let error = BackendError::ToolLocked {
        what: "session/shell".into(),
        lock: ToolLock::Chat,
    };
    let body = RpcErrorBody::from(&error);
    assert_eq!(error_codes::TOOL_LOCKED, -32007);
    assert_eq!(
        serde_json::to_value(&body).unwrap(),
        json!({"code":-32007,
            "message":"session/shell is off: this Runtime runs with the `chat` tool lock",
            "data":{"what":"session/shell","lock":"chat"}})
    );
    assert_eq!(body.into_backend_error(), error);
}

#[test]
fn skill_and_workflow_requests_and_progress_have_stable_wire_shapes() {
    use loop_protocol::session_services::*;
    let session =
        serde_json::from_value(json!({"instance":"loop","key":"s","native_id":"s"})).unwrap();
    let request = WorkflowParams {
        session,
        goal: "inspect".into(),
        concurrency: Some(2),
        request_id: "once".into(),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"session":{"instance":"loop","key":"s","native_id":"s"},"goal":"inspect","concurrency":2,"request_id":"once"})
    );
    let progress: loop_protocol::SessionUpdate = serde_json::from_value(json!({"type":"workflow_progress","workflow_id":"wf","state":"task_started","task_id":"t","description":"Inspect"})).unwrap();
    assert!(matches!(
        progress,
        loop_protocol::SessionUpdate::WorkflowProgress(WorkflowProgress {
            state: WorkflowState::TaskStarted,
            ..
        })
    ));
    let flags: loop_protocol::Capabilities = serde_json::from_value(json!({})).unwrap();
    assert!(!flags.workflow && !flags.session_skills);
}

#[test]
fn host_credentials_have_redacted_debug_and_explicit_wire_actions() {
    use loop_protocol::host::*;
    let request = ProvidersParams {
        action: HostAction::Connect,
        provider: None,
        login: Some(json!({"Preset":{"id":"faux","api_key":"secret-value"}})),
    };
    assert!(!format!("{request:?}").contains("secret-value"));
    assert_eq!(serde_json::to_value(&request).unwrap()["action"], "connect");
    let tracing = TracingParams {
        action: HostAction::Setup,
        config: Some(json!({"authorization":"secret-value"})),
    };
    assert!(!format!("{tracing:?}").contains("secret-value"));
    let flags: loop_protocol::Capabilities = serde_json::from_value(json!({})).unwrap();
    assert!(!flags.host_providers && !flags.host_tracing && !flags.host_mcp_auth);
}

#[test]
fn caller_environment_is_optional_roundtrips_and_is_redacted_in_debug() {
    let spec: SessionSpec = serde_json::from_value(
        json!({"cwd":"/work", "env":{"TASK_ENV":"caller", "OPENAI_API_KEY":"secret-value"}}),
    )
    .unwrap();
    assert_eq!(spec.env.as_ref().unwrap()["TASK_ENV"], "caller");
    assert!(!format!("{spec:?}").contains("secret-value"));
    assert_eq!(
        serde_json::to_value(&spec).unwrap(),
        json!({"cwd":"/work", "env":{"TASK_ENV":"caller", "OPENAI_API_KEY":"secret-value"}})
    );
    let capabilities = Capabilities {
        session_environment: true,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(capabilities).unwrap()["session_environment"],
        true
    );
    assert!(!Capabilities::default().session_environment);
}

#[test]
fn empty_owner_stop_is_an_optional_host_only_capability() {
    assert!(loop_protocol::rpc::methods::HOST_ONLY.contains(&"runtime/stop_empty"));
    let offered = Capabilities {
        runtime_stop_empty: true,
        ..Default::default()
    };
    let value = serde_json::to_value(offered).unwrap();
    assert_eq!(value["runtime_stop_empty"], true);
    assert!(serde_json::to_value(Capabilities::default())
        .unwrap()
        .get("runtime_stop_empty")
        .is_none());
}

#[test]
fn session_environment_debug_exposes_only_the_key_count() {
    let spec = SessionSpec {
        env: Some(std::collections::BTreeMap::from([(
            "SECRET_NAME".into(),
            "sentinel-secret".into(),
        )])),
        ..Default::default()
    };
    let debug = format!("{spec:?}");
    assert!(debug.contains("env_keys: Some(1)"), "{debug}");
    assert!(!debug.contains("SECRET_NAME") && !debug.contains("sentinel-secret"));
}
