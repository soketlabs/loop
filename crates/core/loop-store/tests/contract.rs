//! Checks of the data in the contract: classes, serde forms and error text.

use loop_protocol::{SessionUpdate, ToolCallStatus, ToolCallUpdate, UpdateEnvelope};
use loop_store::class::{classify_update, Durability, UpdateCase, RECORD_CLASSES, UPDATE_CLASSES};
use loop_store::{EventKind, Principal, PrincipalKind, StoreError, Tenant};
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
    // The kinds that ADR-0007 adds are not variants yet: they still read as
    // Unknown, so defining them changed nothing.
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
    let principal = Principal::new(PrincipalKind::Person, "local", "uid:501");
    assert_eq!(
        serde_json::to_value(&principal).unwrap(),
        json!({"kind":"person","issuer":"local","id":"uid:501"})
    );
    assert_eq!(
        serde_json::to_value(Tenant::local()).unwrap(),
        json!("local")
    );
}
