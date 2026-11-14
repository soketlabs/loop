//! Tests of the sequencer's durable half, with no Runtime.
use std::collections::HashMap;

use loop_protocol::{
    BackendInstanceId, Caller, ConfigChanged, ErrorCategory, ErrorInfo, InteractionId,
    InteractionOutcome, InteractionRequest, NoticeLevel, PlanEntry, PlanEntryStatus, Principal,
    SessionKey, StopReason, Tenant, Usage,
};
use loop_store::EventKind;
use serde_json::json;

use super::*;

fn who(uid: u32) -> Identity {
    Identity::acting(&Caller::host(Principal::local_uid(uid)), Tenant::local())
}

fn envelope(turn: Option<&str>, update: SessionUpdate) -> UpdateEnvelope {
    UpdateEnvelope::live(
        BackendInstanceId("native".into()),
        0,
        SessionKey("s".into()),
        turn.map(|t| TurnId(t.into())),
        0,
        update,
    )
}

fn mid(id: &str) -> MessageId {
    MessageId(id.into())
}

fn tid(id: &str) -> ToolCallId {
    ToolCallId(id.into())
}

fn text(id: &str, t: &str) -> SessionUpdate {
    SessionUpdate::MessageChunk {
        message_id: mid(id),
        role: Role::Agent,
        content: ContentBlock::text(t),
    }
}

fn thought(id: &str, t: &str) -> SessionUpdate {
    SessionUpdate::ThoughtChunk {
        message_id: mid(id),
        content: ContentBlock::text(t),
    }
}

fn started(id: &str) -> SessionUpdate {
    SessionUpdate::MessageStarted {
        message_id: mid(id),
        role: Role::Agent,
    }
}

fn ended(id: &str) -> SessionUpdate {
    SessionUpdate::MessageEnded {
        message_id: mid(id),
    }
}

fn call(id: &str) -> SessionUpdate {
    SessionUpdate::ToolCall(ToolCall {
        id: tid(id),
        name: Some("read".into()),
        title: format!("read {id}"),
        kind: ToolKind::Read,
        status: ToolCallStatus::Pending,
        content: vec![],
        locations: vec![],
        raw_input: Some(json!({"path": id})),
    })
}

fn progress(id: &str, out: &str) -> SessionUpdate {
    let mut change = ToolCallUpdate::new(tid(id));
    change.status = Some(ToolCallStatus::InProgress);
    change.content = Some(vec![ToolContent::text(out)]);
    SessionUpdate::ToolCallUpdate(change)
}

fn finish(id: &str, status: ToolCallStatus) -> SessionUpdate {
    let mut change = ToolCallUpdate::new(tid(id));
    change.status = Some(status);
    change.raw_output = Some(json!({"ok": true}));
    SessionUpdate::ToolCallUpdate(change)
}

fn preparing(id: &str) -> SessionUpdate {
    SessionUpdate::ToolCallPreparing {
        id: tid(id),
        name: "read".into(),
        kind: ToolKind::Read,
    }
}

fn turn_started(id: &str) -> SessionUpdate {
    SessionUpdate::TurnStarted {
        turn_id: TurnId(id.into()),
    }
}

fn turn_ended(id: &str, stop: StopReason) -> SessionUpdate {
    SessionUpdate::TurnEnded {
        turn_id: TurnId(id.into()),
        stop,
        error: (stop == StopReason::Error).then(|| ErrorInfo::new(ErrorCategory::Other, "boom")),
    }
}

fn config(revision: u64) -> SessionUpdate {
    SessionUpdate::ConfigChanged(ConfigChanged {
        revision,
        ..ConfigChanged::default()
    })
}

/// Commit `updates` in order as seq `H + 1, H + 2, ...`.
fn feed(seq: &mut Sequencer, updates: &[SessionUpdate]) -> Vec<Applied> {
    updates
        .iter()
        .map(|u| {
            let next = seq.through_seq() + 1;
            seq.committed(next, u)
        })
        .collect()
}

fn joined(blocks: &[ContentBlock]) -> String {
    blocks.iter().filter_map(ContentBlock::as_text).collect()
}

#[test]
fn every_update_kind_moves_h_and_only_open_and_end_kinds_change_the_snapshot() {
    let mut seq = Sequencer::new(10, who(1));
    let mut retitle = ToolCallUpdate::new(tid("t"));
    retitle.title = Some("x".into());
    let all = vec![
        SessionUpdate::BranchReset {
            revision: 1,
            branch_revision: 1,
            view_id: "v".into(),
            reason: "navigate".into(),
        },
        turn_started("turn-1"),
        started("m"),
        text("m", "a"),
        thought("m", "b"),
        ended("m"),
        preparing("t"),
        call("t"),
        SessionUpdate::ToolCallUpdate(retitle),
        finish("t", ToolCallStatus::Completed),
        SessionUpdate::Plan {
            entries: vec![PlanEntry {
                content: "p".into(),
                status: PlanEntryStatus::Pending,
            }],
        },
        SessionUpdate::Usage(Usage::default()),
        SessionUpdate::Notice {
            level: NoticeLevel::Info,
            text: "n".into(),
        },
        SessionUpdate::InteractionRequested {
            request: InteractionRequest::Question {
                id: InteractionId("i".into()),
                prompt: "ok?".into(),
                choices: None,
            },
        },
        SessionUpdate::InteractionResolved {
            id: InteractionId("i".into()),
            outcome: InteractionOutcome::Cancelled,
        },
        config(2),
        SessionUpdate::WorkflowProgress(
            serde_json::from_value(json!({"workflow_id": "w", "state": "planned"})).unwrap(),
        ),
        SessionUpdate::Unknown(json!({"type": "future_kind", "x": 1})),
        turn_ended("turn-1", StopReason::EndTurn),
    ];
    let applied = feed(&mut seq, &all);
    assert_eq!(seq.through_seq(), 10 + all.len() as u64);
    assert_eq!(seq.unexpected_seqs(), 0);
    let parts = applied
        .iter()
        .filter(|a| matches!(a, Applied::Part { .. }))
        .count();
    // Two chunks, `preparing` and the non-terminal update.
    assert_eq!(parts, 4);
    // Only the live-only kinds leave the durable mark behind.
    assert_eq!(seq.snapshot().durable_seq, seq.through_seq());
    let snapshot = seq.snapshot();
    assert_eq!(snapshot.through_seq, seq.through_seq());
    assert!(snapshot.turn.is_none() && snapshot.messages.is_empty());
    assert!(snapshot.tools.is_empty() && snapshot.preparing.is_empty());
    assert_eq!(seq.open_bytes(), 0);
}

#[test]
fn text_and_thought_interleave_and_share_one_part_counter() {
    let mut seq = Sequencer::new(0, who(1));
    let applied = feed(
        &mut seq,
        &[
            turn_started("turn-1"),
            started("m"),
            thought("m", "think "),
            text("m", "hel"),
            thought("m", "more"),
            text("m", "lo"),
        ],
    );
    let parts: Vec<u32> = applied
        .iter()
        .filter_map(|a| match a {
            Applied::Part { item, part } => {
                assert_eq!(
                    item,
                    &Item::Message {
                        anchor_seq: 2,
                        id: mid("m")
                    }
                );
                Some(*part)
            }
            Applied::Durable => None,
        })
        .collect();
    assert_eq!(parts, vec![0, 1, 2, 3]);
    let snapshot = seq.snapshot();
    // Chunks are live only: the durable mark stays at `message_started`.
    assert_eq!(snapshot.durable_seq, 2);
    assert_eq!(snapshot.through_seq, 6);
    let message = &snapshot.messages[0];
    assert_eq!(message.content, vec![ContentBlock::text("hello")]);
    assert_eq!(message.thought, vec![ContentBlock::text("think more")]);
    assert_eq!(message.parts, 4);
    assert_eq!(message.bytes, 15);
    assert_eq!(snapshot.turn.as_ref().map(|t| t.anchor_seq), Some(1));
    feed(&mut seq, &[ended("m")]);
    assert!(seq.snapshot().messages.is_empty());
    assert!(seq.snapshot().turn.is_some());
}

#[test]
fn a_non_text_block_keeps_its_place() {
    let mut seq = Sequencer::new(0, who(1));
    let image = ContentBlock::Image {
        data: "AAAA".into(),
        mime_type: "image/png".into(),
    };
    feed(
        &mut seq,
        &[
            started("m"),
            text("m", "a"),
            SessionUpdate::MessageChunk {
                message_id: mid("m"),
                role: Role::Agent,
                content: image.clone(),
            },
            text("m", "b"),
            text("m", "c"),
        ],
    );
    assert_eq!(
        seq.snapshot().messages[0].content,
        vec![ContentBlock::text("a"), image, ContentBlock::text("bc")]
    );
}

#[test]
fn parallel_tool_calls_merge_field_by_field_and_end_one_by_one() {
    let mut seq = Sequencer::new(0, who(1));
    let applied = feed(
        &mut seq,
        &[
            turn_started("turn-1"),
            preparing("a"),
            preparing("b"),
            call("a"),
            call("b"),
            progress("b", "b1"),
            progress("a", "a1"),
            progress("b", "b2"),
        ],
    );
    // `preparing` anchors at the last durable record (the turn start).
    assert_eq!(
        applied[1],
        Applied::Part {
            item: Item::Tool {
                anchor_seq: 1,
                id: tid("a")
            },
            part: 0
        }
    );
    assert_eq!(
        applied[7],
        Applied::Part {
            item: Item::Tool {
                anchor_seq: 5,
                id: tid("b")
            },
            part: 1
        }
    );
    let snapshot = seq.snapshot();
    assert!(snapshot.preparing.is_empty());
    assert_eq!(snapshot.tools.len(), 2);
    let b = &snapshot.tools[1];
    assert_eq!(b.anchor_seq, 5);
    assert_eq!(b.parts, 2);
    assert_eq!(b.state.content, Some(vec![ToolContent::text("b2")]));
    assert_eq!(b.state.title.as_deref(), Some("read b"));
    assert_eq!(b.state.status, Some(ToolCallStatus::InProgress));
    assert_eq!(b.state.raw_input, Some(json!({"path": "b"})));
    feed(&mut seq, &[finish("a", ToolCallStatus::Failed)]);
    let snapshot = seq.snapshot();
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools[0].state.id, tid("b"));
    feed(&mut seq, &[finish("b", ToolCallStatus::Completed)]);
    assert!(seq.snapshot().tools.is_empty());
}

#[test]
fn a_tool_call_reported_complete_does_not_open() {
    let mut seq = Sequencer::new(0, who(1));
    let SessionUpdate::ToolCall(mut done) = call("t") else {
        unreachable!()
    };
    done.status = ToolCallStatus::Completed;
    feed(&mut seq, &[SessionUpdate::ToolCall(done)]);
    assert!(seq.snapshot().tools.is_empty());
}

#[test]
fn a_preparing_call_is_open_until_its_tool_call_or_the_turn_end() {
    let mut seq = Sequencer::new(3, who(1));
    feed(
        &mut seq,
        &[SessionUpdate::ToolCallPreparing {
            id: tid("a"),
            name: "write".into(),
            kind: ToolKind::Edit,
        }],
    );
    assert_eq!(
        seq.snapshot().preparing,
        vec![Preparing {
            anchor_seq: 3,
            id: tid("a"),
            name: "write".into(),
            kind: ToolKind::Edit
        }]
    );
    // The model gave up writing the call: the turn end clears it.
    feed(&mut seq, &[turn_ended("turn-1", StopReason::EndTurn)]);
    assert!(seq.snapshot().preparing.is_empty());
}

#[test]
fn a_cancel_or_an_error_in_the_middle_of_a_message_ends_it_with_the_turn() {
    for stop in [StopReason::Cancelled, StopReason::Error] {
        let mut seq = Sequencer::new(0, who(1));
        feed(
            &mut seq,
            &[
                turn_started("turn-1"),
                started("m"),
                text("m", "half a sen"),
                call("t"),
                progress("t", "running"),
            ],
        );
        let open = seq.snapshot();
        assert_eq!(joined(&open.messages[0].content), "half a sen");
        assert_eq!(open.tools.len(), 1);
        feed(&mut seq, &[turn_ended("turn-1", stop)]);
        let snapshot = seq.snapshot();
        assert!(snapshot.messages.is_empty() && snapshot.tools.is_empty());
        assert!(snapshot.turn.is_none());
        assert_eq!(seq.open_bytes(), 0);
    }
}

#[test]
fn a_failure_resets_the_open_work_but_keeps_h() {
    let mut seq = Sequencer::new(0, who(1));
    feed(
        &mut seq,
        &[turn_started("turn-1"), started("m"), text("m", "x")],
    );
    seq.reset();
    let snapshot = seq.snapshot();
    assert_eq!(snapshot.through_seq, 3);
    assert!(snapshot.turn.is_none() && snapshot.messages.is_empty());
}

#[test]
fn a_branch_reset_a_config_change_and_an_unknown_kind_leave_open_work_alone() {
    let mut seq = Sequencer::new(0, who(1));
    feed(
        &mut seq,
        &[turn_started("turn-1"), started("m"), text("m", "x")],
    );
    let before = seq.snapshot();
    feed(
        &mut seq,
        &[
            SessionUpdate::BranchReset {
                revision: 4,
                branch_revision: 2,
                view_id: "v".into(),
                reason: "navigate".into(),
            },
            config(5),
            SessionUpdate::Unknown(json!({"type": "later", "data": [1, 2]})),
        ],
    );
    let after = seq.snapshot();
    assert_eq!(after.through_seq, 6);
    assert_eq!(after.durable_seq, 6);
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.turn, before.turn);
}

#[test]
fn a_part_with_no_start_opens_its_item_at_the_last_durable_record() {
    let mut seq = Sequencer::new(0, who(1));
    let applied = feed(
        &mut seq,
        &[
            turn_started("turn-1"),
            text("m", "a"),
            text("m", "b"),
            progress("t", "out"),
        ],
    );
    assert_eq!(
        applied[1],
        Applied::Part {
            item: Item::Message {
                anchor_seq: 1,
                id: mid("m")
            },
            part: 0
        }
    );
    assert_eq!(
        applied[3],
        Applied::Part {
            item: Item::Tool {
                anchor_seq: 1,
                id: tid("t")
            },
            part: 0
        }
    );
    let snapshot = seq.snapshot();
    assert_eq!(joined(&snapshot.messages[0].content), "ab");
    assert_eq!(
        snapshot.tools[0].state.content,
        Some(vec![ToolContent::text("out")])
    );
}

#[test]
fn replayed_history_is_not_recorded() {
    let seq = Sequencer::new(0, who(1));
    let mut replayed = envelope(None, text("m", "old"));
    replayed.origin = Origin::Replay;
    assert!(seq.record(replayed, None, 7, 1).is_none());
    let live = envelope(Some("turn-1"), text("m", "new"));
    let event = seq.record(live.clone(), None, 7, 1).unwrap();
    assert_eq!(event.generation, 7);
    assert_eq!(event.ts_ms, 1);
    assert_eq!(event.identity, who(1));
    assert_eq!(event.turn, live.turn);
    assert!(matches!(event.kind, EventKind::Update(e) if e == live));
}

#[test]
fn a_cause_the_runtime_gives_wins() {
    let mut seq = Sequencer::new(0, who(1));
    seq.claim(0, who(3));
    let event = seq
        .record(envelope(None, config(1)), Some(who(9)), 0, 0)
        .unwrap();
    assert_eq!(event.identity, who(9));
}

#[test]
fn a_queued_update_keeps_its_caller_when_the_next_caller_acts() {
    // The pump records everything the backend sent before `act`, so the
    // order of `record` and `act` is the order of the stream.
    let mut seq = Sequencer::new(0, who(1));
    let notice = envelope(
        None,
        SessionUpdate::Notice {
            level: NoticeLevel::Info,
            text: "from the first command".into(),
        },
    );
    let queued = seq.record(notice.clone(), None, 0, 0).unwrap();
    seq.act(who(2));
    let next = seq.record(notice, None, 0, 0).unwrap();
    assert_eq!(queued.identity, who(1));
    assert_eq!(next.identity, who(2));
    let turn = seq
        .record(envelope(Some("turn-2"), started("m")), None, 0, 0)
        .unwrap();
    assert_eq!(turn.identity, who(2));
    assert_eq!(seq.acting(), &who(2));
}

#[test]
fn a_claimed_revision_stays_its_setter_s_until_a_newer_one_commits() {
    let mut seq = Sequencer::new(0, who(1));
    seq.act(who(2));
    // Setter 3 runs at revision 4 and makes revision 5.
    seq.claim(4, who(3));
    assert_eq!(seq.cause_of(&envelope(None, config(5))), who(3));
    seq.end_claim(5);
    // A pending change takes effect later, with the same revision.
    assert_eq!(seq.cause_of(&envelope(None, config(5))), who(3));
    feed(&mut seq, &[config(5)]);
    assert_eq!(seq.cause_of(&envelope(None, config(5))), who(3));
    // An update of a turn is the turn's.
    assert_eq!(seq.cause_of(&envelope(Some("turn-1"), config(5))), who(2));
    // A newer revision drops the old claim.
    feed(&mut seq, &[config(6)]);
    assert_eq!(seq.cause_of(&envelope(None, config(5))), who(2));
}

#[test]
fn a_revision_no_setter_claimed_is_the_acting_caller_s() {
    let mut seq = Sequencer::new(0, who(1));
    seq.claim(1, who(3));
    seq.end_claim(2);
    // Revision 3 was made with no setter: the backend made it during the
    // acting caller's turn or command.
    assert_eq!(seq.cause_of(&envelope(None, config(3))), who(1));
    seq.act(who(2));
    assert_eq!(seq.cause_of(&envelope(None, config(3))), who(2));
    // Claims are not forgotten by count while they wait to commit (the old
    // table kept 256).
    for revision in 3..1000 {
        seq.claim(revision - 1, who(4));
        seq.end_claim(revision);
    }
    assert_eq!(seq.cause_of(&envelope(None, config(2))), who(3));
}

#[test]
fn a_seq_that_does_not_follow_h_is_counted() {
    let mut seq = Sequencer::new(5, who(1));
    seq.committed(6, &config(1));
    seq.committed(9, &config(2));
    assert_eq!(seq.unexpected_seqs(), 1);
    assert_eq!(seq.through_seq(), 9);
}

#[test]
fn a_message_above_its_limit_overflows_and_keeps_counting_parts() {
    let limits = Limits {
        item_bytes: 10,
        session_bytes: 15,
    };
    let mut seq = Sequencer::with_limits(0, who(1), limits);
    let applied = feed(
        &mut seq,
        &[
            started("a"),
            text("a", "12345"),
            text("a", "678901"),
            text("a", "x"),
            started("b"),
            text("b", "1234"),
            text("b", "12345678"),
        ],
    );
    let snapshot = seq.snapshot();
    let (a, b) = (&snapshot.messages[0], &snapshot.messages[1]);
    // `a` would pass 10 bytes: it keeps what it had and stops.
    assert!(a.overflowed);
    assert_eq!(joined(&a.content), "12345");
    assert_eq!(a.parts, 3);
    // `b` would pass the Session's 15 bytes (5 + 4 + 8).
    assert!(b.overflowed);
    assert_eq!(joined(&b.content), "1234");
    assert_eq!(seq.open_bytes(), 9);
    assert!(matches!(applied[6], Applied::Part { part: 1, .. }));
    // The end of a message frees its bytes.
    feed(&mut seq, &[ended("a"), ended("b")]);
    assert_eq!(seq.open_bytes(), 0);
}

#[test]
fn a_tool_call_above_its_limit_overflows() {
    let limits = Limits {
        item_bytes: 40,
        session_bytes: 1000,
    };
    let mut seq = Sequencer::with_limits(0, who(1), limits);
    feed(&mut seq, &[call("t"), progress("t", "short")]);
    assert!(!seq.snapshot().tools[0].overflowed);
    feed(&mut seq, &[progress("t", &"y".repeat(100))]);
    let tool = &seq.snapshot().tools[0];
    assert!(tool.overflowed);
    assert_eq!(tool.bytes, 0);
    assert_eq!(tool.parts, 2);
    // A later, smaller update does not make an overflowed call complete.
    feed(&mut seq, &[progress("t", "z")]);
    assert!(seq.snapshot().tools[0].overflowed);
    feed(&mut seq, &[finish("t", ToolCallStatus::Completed)]);
    assert!(seq.snapshot().tools.is_empty());
}

#[test]
fn the_fixed_cost_of_one_open_message_is_small() {
    // For the memory report: bytes held per open message besides its text.
    let fixed = std::mem::size_of::<OpenMessage>();
    assert!(fixed <= 160, "{fixed}");
}

// ---- property tests ----

/// A small deterministic generator (xorshift64*), so a failure names its case.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const PIECES: &[&str] = &["a", "bc", " ", "héllo", "\n", "ü", "日本", "{}", "x y"];

/// A random stream: turns with interleaved messages, parallel tool calls
/// and other kinds; a turn may end (cancel, error) with items still open,
/// and the stream may stop in the middle of a turn.
fn stream(rng: &mut Rng) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    let turns = 1 + rng.below(3);
    let mut next_id = 0;
    for t in 0..turns {
        let turn = format!("turn-{t}");
        out.push(turn_started(&turn));
        let mut messages: Vec<String> = Vec::new();
        let mut tools: Vec<String> = Vec::new();
        for _ in 0..rng.below(60) {
            match rng.below(12) {
                0 => {
                    next_id += 1;
                    let id = format!("msg-{next_id}");
                    out.push(started(&id));
                    messages.push(id);
                }
                1..=3 if !messages.is_empty() => {
                    let id = rng.pick(&messages).clone();
                    out.push(text(&id, rng.pick(PIECES)));
                }
                4 if !messages.is_empty() => {
                    let id = rng.pick(&messages).clone();
                    out.push(thought(&id, rng.pick(PIECES)));
                }
                5 if !messages.is_empty() => {
                    let id = messages.remove(rng.below(messages.len() as u64) as usize);
                    out.push(ended(&id));
                }
                6 => {
                    next_id += 1;
                    let id = format!("call-{next_id}");
                    if rng.chance(50) {
                        out.push(preparing(&id));
                    }
                    out.push(call(&id));
                    tools.push(id);
                }
                7 | 8 if !tools.is_empty() => {
                    let id = rng.pick(&tools).clone();
                    let mut change = ToolCallUpdate::new(tid(&id));
                    if rng.chance(50) {
                        let out = format!("out {}", rng.below(1000));
                        change.content = Some(vec![ToolContent::text(out)]);
                    }
                    if rng.chance(30) {
                        change.title = Some(format!("title {}", rng.below(10)));
                    }
                    if rng.chance(30) {
                        change.status = Some(ToolCallStatus::InProgress);
                    }
                    out.push(SessionUpdate::ToolCallUpdate(change));
                }
                9 if !tools.is_empty() => {
                    let id = tools.remove(rng.below(tools.len() as u64) as usize);
                    out.push(finish(&id, ToolCallStatus::Completed));
                }
                10 => out.push(SessionUpdate::Usage(Usage::default())),
                _ => out.push(config(rng.below(5))),
            }
        }
        if t + 1 < turns || rng.chance(70) {
            let stop = *rng.pick(&[
                StopReason::EndTurn,
                StopReason::Cancelled,
                StopReason::Error,
            ]);
            out.push(turn_ended(&turn, stop));
        }
    }
    out
}

/// What a reader holds of one item.
#[derive(Debug, Default, Clone, PartialEq)]
struct Seen {
    content: Vec<ContentBlock>,
    thought: Vec<ContentBlock>,
    tool: Option<ToolCallUpdate>,
    /// The next part the reader expects.
    next_part: u32,
}

fn apply(seen: &mut Seen, update: &SessionUpdate) {
    match update {
        SessionUpdate::MessageChunk { content, .. } => join(&mut seen.content, content),
        SessionUpdate::ThoughtChunk { content, .. } => join(&mut seen.thought, content),
        SessionUpdate::ToolCall(call) => seen.tool = Some(from_call(call)),
        SessionUpdate::ToolCallUpdate(change) => {
            let state = seen
                .tool
                .get_or_insert_with(|| ToolCallUpdate::new(change.id.clone()));
            merge(state, change);
        }
        _ => {}
    }
}

/// The item of an update that opens, extends or ends one.
fn item_of(
    update: &SessionUpdate,
    applied: &Applied,
    opened: &mut HashMap<String, Item>,
    seq: u64,
) -> Option<Item> {
    let item = match (update, applied) {
        (_, Applied::Part { item, .. }) => item.clone(),
        (SessionUpdate::MessageStarted { message_id, .. }, _) => Item::Message {
            anchor_seq: seq,
            id: message_id.clone(),
        },
        (SessionUpdate::ToolCall(call), _) => Item::Tool {
            anchor_seq: seq,
            id: call.id.clone(),
        },
        (SessionUpdate::ToolCallUpdate(change), _) => return opened.get(&change.id.0).cloned(),
        _ => return None,
    };
    let id = match &item {
        Item::Message { id, .. } => id.0.clone(),
        Item::Tool { id, .. } => id.0.clone(),
    };
    opened.insert(id, item.clone());
    Some(item)
}

fn open_items(snapshot: &Snapshot) -> Vec<Item> {
    let messages = snapshot.messages.iter().map(|m| Item::Message {
        anchor_seq: m.anchor_seq,
        id: m.id.clone(),
    });
    let tools = snapshot.tools.iter().map(|t| Item::Tool {
        anchor_seq: t.anchor_seq,
        id: t.state.id.clone(),
    });
    messages.chain(tools).collect()
}

/// R1 for A0: for every cut `H` of a random stream, the durable replay up
/// to `H`, the snapshot at `H` and the live tail after `H` give each part
/// exactly once, and every item ends in the state the whole stream gives.
#[test]
fn property_snapshot_at_any_cut_plus_the_live_tail_rebuilds_every_item_once() {
    for case in 0..400u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (case + 1));
        let updates = stream(&mut rng);
        let mut seq = Sequencer::new(0, who(1));
        let (mut applied, mut items) = (Vec::new(), Vec::new());
        let mut opened = HashMap::new();
        let mut expected: HashMap<Item, Seen> = HashMap::new();
        let mut snapshots = vec![seq.snapshot()];
        for (i, update) in updates.iter().enumerate() {
            let n = i as u64 + 1;
            let a = seq.committed(n, update);
            let item = item_of(update, &a, &mut opened, n);
            if let Some(item) = &item {
                apply(expected.entry(item.clone()).or_default(), update);
            }
            applied.push(a);
            items.push(item);
            snapshots.push(seq.snapshot());
        }
        assert_eq!(seq.unexpected_seqs(), 0);
        for (cut, snapshot) in snapshots.iter().enumerate() {
            assert_eq!(snapshot.through_seq, cut as u64);
            let open = open_items(snapshot);
            let mut reader: HashMap<Item, Seen> = HashMap::new();
            // 1. Durable replay up to H. A chunk row of an item that is
            //    still open at H is not replayed: the snapshot holds it
            //    (in A4 such rows do not exist).
            for i in 0..cut {
                if let Some(item) = items[i].as_ref().filter(|item| !open.contains(item)) {
                    apply(reader.entry(item.clone()).or_default(), &updates[i]);
                }
            }
            // 2. The snapshot.
            for m in &snapshot.messages {
                let seen = reader
                    .entry(Item::Message {
                        anchor_seq: m.anchor_seq,
                        id: m.id.clone(),
                    })
                    .or_default();
                seen.content = m.content.clone();
                seen.thought = m.thought.clone();
                seen.next_part = m.parts;
            }
            for t in &snapshot.tools {
                let seen = reader
                    .entry(Item::Tool {
                        anchor_seq: t.anchor_seq,
                        id: t.state.id.clone(),
                    })
                    .or_default();
                seen.tool = Some(t.state.clone());
                seen.next_part = t.parts;
            }
            // 3. The live tail after H: each part must be the next one.
            for i in cut..updates.len() {
                let Some(item) = &items[i] else { continue };
                let seen = reader.entry(item.clone()).or_default();
                if let Applied::Part { part, .. } = &applied[i] {
                    if !matches!(updates[i], SessionUpdate::ToolCallPreparing { .. }) {
                        assert_eq!(
                            *part, seen.next_part,
                            "case {case} cut {cut} update {i}: a part is missing or repeated"
                        );
                        seen.next_part += 1;
                    }
                }
                apply(seen, &updates[i]);
            }
            for (item, want) in &expected {
                let got = reader.get(item).cloned().unwrap_or_default();
                assert_eq!(
                    (&got.content, &got.thought, &got.tool),
                    (&want.content, &want.thought, &want.tool),
                    "case {case} cut {cut} item {item:?}"
                );
            }
        }
    }
}

/// At each `message_ended`, and for each message open at the end, the
/// accumulator holds exactly the joined chunks.
#[test]
fn property_final_content_equals_the_concatenated_chunks() {
    for case in 0..400u64 {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03 ^ (case + 1));
        let updates = stream(&mut rng);
        let mut seq = Sequencer::new(0, who(1));
        let mut text_of: HashMap<String, (String, String)> = HashMap::new();
        for (i, update) in updates.iter().enumerate() {
            match update {
                SessionUpdate::MessageChunk {
                    message_id,
                    content,
                    ..
                } => {
                    let entry = text_of.entry(message_id.0.clone()).or_default();
                    entry.0.push_str(content.as_text().unwrap_or_default());
                }
                SessionUpdate::ThoughtChunk {
                    message_id,
                    content,
                } => {
                    let entry = text_of.entry(message_id.0.clone()).or_default();
                    entry.1.push_str(content.as_text().unwrap_or_default());
                }
                SessionUpdate::MessageEnded { message_id } => {
                    let snapshot = seq.snapshot();
                    let message = snapshot.messages.iter().find(|m| &m.id == message_id);
                    let (text, thought) = text_of.remove(&message_id.0).unwrap_or_default();
                    let message = message.expect("an ended message was open");
                    assert_eq!(joined(&message.content), text, "case {case}");
                    assert_eq!(joined(&message.thought), thought, "case {case}");
                    // Text joins into one block.
                    assert!(message.content.len() <= 1, "case {case}");
                }
                SessionUpdate::TurnEnded { .. } => text_of.clear(),
                _ => {}
            }
            seq.committed(i as u64 + 1, update);
        }
        for message in seq.snapshot().messages {
            let (text, thought) = text_of.remove(&message.id.0).unwrap_or_default();
            assert_eq!(joined(&message.content), text, "case {case}");
            assert_eq!(joined(&message.thought), thought, "case {case}");
        }
    }
}

/// The hot-path cost of the durable half: `record` and `committed` of one
/// 16-byte text chunk. Ignored; run it in an optimised profile:
/// `cargo test -p loop-runtime --lib sequencer::tests::cost -- --ignored --nocapture`.
#[test]
#[ignore]
fn cost_of_one_chunk() {
    const N: u64 = 2_000_000;
    let mut seq = Sequencer::new(0, who(1));
    feed(&mut seq, &[turn_started("turn-1"), started("m")]);
    let chunk = envelope(Some("turn-1"), text("m", "0123456789abcdef"));
    let start = std::time::Instant::now();
    for _ in 0..N {
        let event = seq.record(chunk.clone(), None, 1, 0).unwrap();
        let next = seq.through_seq() + 1;
        if let EventKind::Update(envelope) = &event.kind {
            std::hint::black_box(seq.committed(next, &envelope.update));
        }
        // A message ends every 100 chunks, as in the load tool.
        if next.is_multiple_of(100) {
            let next = seq.through_seq() + 1;
            seq.committed(next, &ended("m"));
            let next = seq.through_seq() + 1;
            seq.committed(next, &started("m"));
        }
    }
    let per = start.elapsed().as_nanos() as f64 / N as f64;
    // The same work without the sequencer: the envelope clone and the Event.
    let start = std::time::Instant::now();
    for _ in 0..N {
        std::hint::black_box(Event::update(1, 0, chunk.clone(), who(1)));
    }
    let base = start.elapsed().as_nanos() as f64 / N as f64;
    let snapshot = seq.snapshot();
    let message = &snapshot.messages[0];
    let held = std::mem::size_of::<OpenMessage>()
        + message
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.capacity(),
                _ => 0,
            })
            .sum::<usize>()
        + message.content.capacity() * std::mem::size_of::<ContentBlock>()
        + message.id.0.capacity();
    eprintln!(
        "SEQ|COST| record+committed per chunk {per:.0} ns (Event::update alone {base:.0} ns); \
         OpenMessage fixed {} B; one open message with {} B of text holds {held} B",
        std::mem::size_of::<OpenMessage>(),
        message.bytes
    );
}
