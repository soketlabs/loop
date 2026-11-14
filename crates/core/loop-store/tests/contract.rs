//! Checks of the data in the contract: classes, serde forms and error text.

use loop_protocol::{SessionUpdate, ToolCallStatus, ToolCallUpdate, UpdateEnvelope};
use loop_store::class::{classify_update, Durability, UpdateCase, RECORD_CLASSES, UPDATE_CLASSES};
use loop_store::{
    DecidedBy, DecisionAnswer, DecisionOutcome, DecisionSource, DecisionSubject, Event, EventKind,
    PolicyDecisionRecord, Principal, PrincipalKind, StoreError, Tenant,
};
use serde_json::{json, Value};
use std::collections::HashSet;

/// One value of each `SessionUpdate` kind, as wire JSON.
fn samples() -> Vec<Value> {
    vec![
        json!({"type":"branch_reset","revision":1,"branch_revision":1,"view_id":"v","reason":"r"}),
        json!({"type":"turn_started","turn_id":"t1"}),
        json!({"type":"message_started","message_id":"m1","role":"agent"}),
        json!({"type":"message_chunk","message_id":"m1","role":"agent","content":{"type":"text","text":"x"}}),
        json!({"type":"message_ended","message_id":"m1"}),
        json!({"type":"thought_chunk","message_id":"m1","content":{"type":"text","text":"x"}}),
        json!({"type":"tool_call_preparing","id":"c1","name":"bash","kind":"execute"}),
        json!({"type":"tool_call","id":"c1","title":"bash","kind":"execute","status":"pending"}),
        json!({"type":"tool_call_update","id":"c1","status":"in_progress"}),
        json!({"type":"plan","entries":[]}),
        json!({"type":"usage","input_tokens":1,"output_tokens":1,"total_tokens":2}),
        json!({"type":"notice","level":"info","text":"x"}),
        json!({"type":"interaction_requested","request":{"kind":"url","id":"i1","url":"u","purpose":"p"}}),
        json!({"type":"interaction_resolved","id":"i1","outcome":{"outcome":"cancelled"}}),
        json!({"type":"turn_ended","turn_id":"t1","stop":"end_turn"}),
        json!({"type":"config_changed","revision":1}),
        json!({"type":"workflow_progress","workflow_id":"w","state":"planned"}),
    ]
}

#[test]
fn every_update_kind_has_its_own_class() {
    let samples = samples();
    for sample in &samples {
        let tag = sample["type"].as_str().unwrap();
        let update: SessionUpdate = serde_json::from_value(sample.clone()).unwrap();
        assert!(
            !matches!(update, SessionUpdate::Unknown(_)),
            "sample {tag} did not decode: {sample}"
        );
        assert_eq!(classify_update(&update).kind, tag);
    }
    // The samples cover each kind of the table once.
    let tags: HashSet<_> = samples
        .iter()
        .map(|s| s["type"].as_str().unwrap())
        .collect();
    let kinds: HashSet<_> = UPDATE_CLASSES
        .iter()
        .map(|c| c.kind)
        .filter(|k| *k != "unknown")
        .collect();
    assert_eq!(tags, kinds);
    let unknown: SessionUpdate = serde_json::from_value(json!({"type":"later"})).unwrap();
    assert_eq!(classify_update(&unknown).kind, "unknown");
}

#[test]
fn table_rows_are_unique() {
    let rows: HashSet<_> = UPDATE_CLASSES.iter().map(|c| (c.kind, c.case)).collect();
    assert_eq!(rows.len(), UPDATE_CLASSES.len());
    let records: HashSet<_> = RECORD_CLASSES.iter().map(|c| c.kind).collect();
    assert_eq!(records.len(), RECORD_CLASSES.len());
}

#[test]
fn live_only_updates_are_the_adr_list() {
    let live: Vec<_> = UPDATE_CLASSES
        .iter()
        .filter(|c| c.durability == Durability::LiveOnly)
        .map(|c| (c.kind, c.case))
        .collect();
    assert_eq!(
        live,
        [
            ("message_chunk", UpdateCase::Any),
            ("thought_chunk", UpdateCase::Any),
            ("tool_call_preparing", UpdateCase::Any),
            ("tool_call_update", UpdateCase::NotTerminal),
        ]
    );
}

#[test]
fn terminal_tool_update_is_durable() {
    let mut change = ToolCallUpdate::new("c1".into());
    assert_eq!(
        classify_update(&SessionUpdate::ToolCallUpdate(change.clone())).durability,
        Durability::LiveOnly
    );
    for (status, durability) in [
        (ToolCallStatus::Pending, Durability::LiveOnly),
        (ToolCallStatus::InProgress, Durability::LiveOnly),
        (ToolCallStatus::Completed, Durability::Durable),
        (ToolCallStatus::Failed, Durability::Durable),
    ] {
        change.status = Some(status);
        let class = classify_update(&SessionUpdate::ToolCallUpdate(change.clone()));
        assert_eq!(class.durability, durability, "{status:?}");
    }
}

#[test]
fn record_kinds_match_event_kind() {
    let envelope: UpdateEnvelope = serde_json::from_value(json!({
        "v": 1, "instance": "i", "session": "s", "seq": 0, "generation": 1, "origin": "live",
        "update": {"type":"turn_started","turn_id":"t1"}
    }))
    .unwrap();
    let present = [
        EventKind::Update(envelope),
        EventKind::PromptSent(json!({})),
        EventKind::PromptSettled(json!({})),
        EventKind::Cancelled(json!({})),
        EventKind::EscalationRaised(json!({})),
        EventKind::EscalationAnswered(json!({})),
        EventKind::EscalationWithdrawn(json!({})),
        EventKind::SessionOpened(json!({})),
        EventKind::SessionClosed(json!({})),
        EventKind::SubagentStarted(json!({})),
        EventKind::SubagentEnded(json!({})),
        EventKind::RuntimeStarted(json!({})),
        EventKind::RuntimeStopped(json!({})),
        EventKind::PolicyDecision(json!({})),
    ];
    let names: Vec<String> = present
        .iter()
        .map(|k| {
            serde_json::to_value(k).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let table: Vec<&str> = RECORD_CLASSES
        .iter()
        .filter(|c| c.in_event_kind)
        .map(|c| c.kind)
        .collect();
    assert_eq!(names, table);
    // Only updates take a Session sequence.
    for class in RECORD_CLASSES {
        assert_eq!(class.session_seq, class.kind == "update", "{}", class.kind);
    }
    // The kinds that ADR-0007 adds and nothing writes yet are not variants:
    // they still read as Unknown, so defining them changed nothing.
    for class in RECORD_CLASSES.iter().filter(|c| !c.in_event_kind) {
        let raw = json!({"kind": class.kind, "payload": {}});
        let kind: EventKind = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(kind, EventKind::Unknown(raw), "{}", class.kind);
    }
}

#[test]
fn live_error_text_is_unchanged() {
    assert_eq!(
        StoreError::Invalid("x".into()).to_string(),
        "invalid Event Log append: x"
    );
    assert_eq!(
        StoreError::Storage("x".into()).to_string(),
        "Event Log storage: x"
    );
    assert_eq!(StoreError::Stopped.to_string(), "Event Log writer stopped");
}

#[test]
fn identity_serde_matches_the_s4_record() {
    let principal = Principal::new(PrincipalKind::Person, "local", "uid:501").unwrap();
    assert_eq!(
        serde_json::to_value(&principal).unwrap(),
        json!({"kind":"person","issuer":"local","id":"uid:501"})
    );
    assert_eq!(
        serde_json::to_value(Tenant::local()).unwrap(),
        json!("local")
    );
}

/// The store's identity types are the protocol's: one definition each.
#[test]
fn identity_types_are_the_protocol_types() {
    fn same<T>(_: &T, _: &T) {}
    let principal: loop_protocol::Principal = loop_protocol::Principal::local_uid(501);
    same(&principal, &Principal::local_uid(501));
    same(&loop_protocol::Tenant::local(), &Tenant::local());
}

fn decision() -> PolicyDecisionRecord {
    PolicyDecisionRecord {
        id: "pd-1".into(),
        ts_ms: 1_762_862_400_000,
        tenant: Tenant::local(),
        principal: Principal::local_uid(501),
        via: None,
        session: Some("s1".into()),
        turn: Some("turn-7".into()),
        call_id: Some("c3".into()),
        source: DecisionSource::Agent("chatgpt".into()),
        tool: "bash".into(),
        kind: loop_protocol::ToolKind::Execute,
        capability: "shell".into(),
        subject: DecisionSubject {
            programs: vec!["touch".into()],
            ..DecisionSubject::default()
        },
        args_sha256: Some("ab".into()),
        outcome: DecisionOutcome::Deny,
        answer: None,
        by: DecidedBy::Rule,
        layer: Some("project".into()),
        rule: Some("bash(touch*)".into()),
        mode: Some("full-access".into()),
        escalation: None,
        answered_by: None,
        reason: Some("Tool `bash` is denied by settings.toolPermissions".into()),
    }
}

/// The record's JSON follows S4 §12, and it reads back unchanged.
#[test]
fn a_decision_record_has_the_s4_shape() {
    let record = decision();
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(
        value,
        json!({
            "id": "pd-1", "ts_ms": 1_762_862_400_000_i64, "tenant": "local",
            "principal": {"kind": "person", "issuer": "local", "id": "uid:501"},
            "session": "s1", "turn": "turn-7", "call_id": "c3",
            "source": "agent:chatgpt", "tool": "bash", "kind": "execute",
            "capability": "shell", "subject": {"programs": ["touch"]},
            "args_sha256": "ab", "outcome": "deny", "by": "rule",
            "layer": "project", "rule": "bash(touch*)", "mode": "full-access",
            "reason": "Tool `bash` is denied by settings.toolPermissions"
        })
    );
    let back: PolicyDecisionRecord = serde_json::from_value(value).unwrap();
    assert_eq!(back, record);
    for (source, text) in [
        (DecisionSource::Native, "native"),
        (DecisionSource::Mcp("github".into()), "mcp:github"),
        (DecisionSource::Workflow, "workflow"),
        (DecisionSource::McpServe, "mcp_serve"),
        (DecisionSource::UserShell, "user_shell"),
        (DecisionSource::Other("later".into()), "later"),
    ] {
        assert_eq!(serde_json::to_value(&source).unwrap(), json!(text));
        assert_eq!(
            serde_json::from_value::<DecisionSource>(json!(text)).unwrap(),
            source
        );
    }
    // An answered ask runs only when it was approved.
    let mut asked = decision();
    asked.outcome = DecisionOutcome::Ask;
    assert!(asked.allowed());
    asked.answer = Some(DecisionAnswer::Deny);
    assert!(!asked.allowed());
}

/// The subject is capped at 4 KB of JSON.
#[test]
fn a_decision_subject_is_capped() {
    let subject = DecisionSubject {
        programs: (0..2000).map(|i| format!("p{i}")).collect(),
        path: Some("x".repeat(10_000)),
        host: None,
    }
    .capped();
    assert!(serde_json::to_vec(&subject).unwrap().len() <= loop_store::records::MAX_SUBJECT_BYTES);
}

/// A `policy_decision` row reads as its own kind and passes the append
/// rules; a reader from before this kind reads it as Unknown, and an
/// unknown kind still reads as Unknown.
#[test]
fn a_policy_decision_row_stays_readable_by_old_and_new_readers() {
    let payload = serde_json::to_value(decision()).unwrap();
    let raw = json!({"kind": "policy_decision", "payload": payload});
    let kind: EventKind = serde_json::from_value(raw.clone()).unwrap();
    let EventKind::PolicyDecision(value) = &kind else {
        panic!("policy_decision did not decode: {kind:?}");
    };
    assert_eq!(
        serde_json::from_value::<PolicyDecisionRecord>(value.clone()).unwrap(),
        decision()
    );
    assert_eq!(serde_json::to_value(&kind).unwrap(), raw);
    let event = Event {
        generation: 1,
        session_key: Some("s1".into()),
        turn: None,
        ts_ms: 1,
        kind,
    };
    loop_store::validate_batch(&[event]).unwrap();
    assert!(!loop_store::is_recovery_record(&EventKind::PolicyDecision(
        json!({})
    )));

    // The shape of `EventKind` before this kind existed.
    #[derive(serde::Deserialize, Debug, PartialEq)]
    #[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
    enum OldKind {
        RuntimeStopped(Value),
        #[serde(untagged)]
        Unknown(Value),
    }
    let old: OldKind = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(old, OldKind::Unknown(raw));
    let later = json!({"kind": "later_kind", "payload": {"x": 1}});
    let kind: EventKind = serde_json::from_value(later.clone()).unwrap();
    assert_eq!(kind, EventKind::Unknown(later));
}
