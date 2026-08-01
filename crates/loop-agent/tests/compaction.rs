//! Compaction helper tests.

use loop_agent::harness::compaction::{
    default_compaction_settings, find_cut_point, prepare_compaction, should_compact,
};
use loop_agent::{convert_to_llm, AgentMessage};
use loop_ai::Message;

#[test]
fn should_compact_threshold() {
    let settings = default_compaction_settings();
    // reserve_tokens default 16384 → 110_000 + 16384 >= 120_000
    assert!(should_compact(110_000, 120_000, &settings));
    assert!(!should_compact(1000, 120_000, &settings));
}

#[test]
fn cut_point_and_prepare() {
    let messages: Vec<AgentMessage> = (0..40)
        .map(|i| AgentMessage::user_text(format!("msg {i} {}", "x".repeat(800))))
        .collect();
    let llm = convert_to_llm(&messages);
    let cut = find_cut_point(&llm, 200);
    assert!(cut < llm.len());
    let mut settings = default_compaction_settings();
    settings.keep_recent_tokens = 200;
    let prep = prepare_compaction(&messages, &llm, &settings).unwrap();
    assert!(!prep.to_summarize.is_empty());
    assert!(!prep.retained.is_empty());
    let _ = Message::user_text("x");
}
