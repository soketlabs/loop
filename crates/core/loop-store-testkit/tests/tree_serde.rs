//! `loop-store` has copies of the `loop-agent` Session tree types (ADR-0007
//! §8 moves them in step B5). Both copies must keep one serde form.
#![cfg(all(feature = "event-log", feature = "session-store"))]

use loop_agent::harness::session::SessionMetadata as AgentMetadata;
use loop_agent::harness::SessionTreeEntry as AgentEntry;
use loop_agent::{AgentMessage, AgentThinkingLevel};
use serde_json::{json, Value};

fn agent_entries() -> Vec<AgentEntry> {
    let parent = Some("p".to_owned());
    let mut entries = vec![
        AgentEntry::Message {
            id: "e1".into(),
            parent_id: None,
            timestamp: 1,
            message: AgentMessage::user_text("hello"),
        },
        AgentEntry::ModelChange {
            id: "e3".into(),
            parent_id: parent.clone(),
            timestamp: 3,
            provider: "faux".into(),
            model_id: "m".into(),
        },
        AgentEntry::ActiveToolsChange {
            id: "e4".into(),
            parent_id: parent.clone(),
            timestamp: 4,
            tool_names: vec!["bash".into()],
        },
        AgentEntry::Compaction {
            id: "e5".into(),
            parent_id: parent.clone(),
            timestamp: 5,
            summary: "s".into(),
            first_kept_entry_id: Some("e1".into()),
            details: Some(json!({"k": 1})),
        },
        AgentEntry::Compaction {
            id: "e5b".into(),
            parent_id: parent.clone(),
            timestamp: 5,
            summary: "s".into(),
            first_kept_entry_id: None,
            details: None,
        },
        AgentEntry::BranchSummary {
            id: "e6".into(),
            parent_id: parent.clone(),
            timestamp: 6,
            summary: "b".into(),
        },
        AgentEntry::Custom {
            id: "e7".into(),
            parent_id: parent.clone(),
            timestamp: 7,
            custom_type: "loop.permission_mode".into(),
            data: json!({"mode": "ask"}),
        },
        AgentEntry::Label {
            id: "e8".into(),
            parent_id: parent.clone(),
            timestamp: 8,
            label: "l".into(),
        },
        AgentEntry::SessionInfo {
            id: "e9".into(),
            parent_id: parent.clone(),
            timestamp: 9,
            name: "n".into(),
        },
        AgentEntry::Leaf {
            id: "e10".into(),
            parent_id: parent.clone(),
            timestamp: 10,
            target_id: None,
        },
    ];
    for level in [
        AgentThinkingLevel::Off,
        AgentThinkingLevel::Minimal,
        AgentThinkingLevel::Low,
        AgentThinkingLevel::Medium,
        AgentThinkingLevel::High,
        AgentThinkingLevel::XHigh,
        AgentThinkingLevel::Max,
    ] {
        entries.push(AgentEntry::ThinkingLevelChange {
            id: "e2".into(),
            parent_id: parent.clone(),
            timestamp: 2,
            thinking_level: level,
        });
    }
    entries
}

#[test]
fn tree_entry_serde_is_the_same_in_both_crates() {
    for entry in agent_entries() {
        let agent = serde_json::to_value(&entry).unwrap();
        let store: loop_store::SessionTreeEntry = serde_json::from_value(agent.clone()).unwrap();
        assert_eq!(store.id(), entry.id());
        assert_eq!(store.parent_id(), entry.parent_id());
        let back = serde_json::to_value(&store).unwrap();
        assert_eq!(back, agent);
        let again: AgentEntry = serde_json::from_value(back).unwrap();
        assert_eq!(serde_json::to_value(again).unwrap(), agent);
    }
}

#[test]
fn session_metadata_serde_is_the_same_in_both_crates() {
    for raw in [
        json!({"id": "s", "createdAt": 1}),
        json!({"id": "s", "cwd": "/w", "name": "n", "parentSessionId": "p", "createdAt": 1, "path": "/f"}),
    ] {
        let agent: AgentMetadata = serde_json::from_value(raw.clone()).unwrap();
        let agent: Value = serde_json::to_value(agent).unwrap();
        let store: loop_store::SessionMetadata = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(store).unwrap(), agent);
        assert_eq!(agent, raw);
    }
}
