//! The durable half of a Session's sequencer (ADR-0007 §3.2, step A0).
//!
//! Each loaded Session has one [`Sequencer`]. The Runtime pump (the delivery
//! half, `runtime/publish.rs`) puts every update of the Session through it,
//! one at a time, in the order that the backend made them:
//!
//! 1. [`Sequencer::record`] turns an update into the [`Event`] to commit. It
//!    adds who caused the update (see "Who caused an update"). It skips
//!    replayed history (`Origin::Replay`).
//! 2. The pump commits the event. The store gives the `session_seq` in the
//!    commit transaction (ADR-0007 §3.2 rule 5). The sequencer does not
//!    give it, so several writers stay correct.
//! 3. [`Sequencer::committed`] moves the high-water mark `H` (the last
//!    committed `session_seq`) and applies the update to the accumulators:
//!    the open turn, each open message (its text and thought so far), each
//!    open tool call (its merged state so far) and each tool call that is
//!    being prepared. Together they are the **in-progress snapshot**
//!    (ADR-0007 §2.3), which [`Sequencer::snapshot`] returns with `H`. An
//!    item ends at `message_ended`, at a terminal `tool_call_update`, at
//!    `turn_ended` (also of a cancelled or failed turn) and at
//!    [`Sequencer::reset`] (the Session failed).
//! 4. The pump then sends the update to subscribers.
//!
//! The pump keeps the sequencer in one mutex with the rest of the Session's
//! announced state, and calls `committed` and `snapshot` under that mutex.
//! So the snapshot always holds exactly the updates with `seq <= H`. A
//! reader that subscribes first, then takes `H` and the snapshot, then
//! reads the log up to `H`, gets each part exactly once (ADR-0007 §2.3).
//!
//! # Who caused an update
//!
//! The backend sends updates with no caller. The sequencer gives each one an
//! [`Identity`] when it records it, from its own state at that point in the
//! stream:
//!
//! - An update that the Runtime made for a request carries its identity
//!   (`cause`).
//! - Otherwise the **acting** identity: the caller of the turn or command in
//!   progress (or of the last one). The pump changes it only with
//!   [`Sequencer::act`], after it has recorded every update that the backend
//!   sent before the switch. So an update keeps its caller also when it
//!   waits in the queue while the next caller's turn is admitted.
//! - A `config_changed` with no turn id is the caller that made its
//!   revision: [`Sequencer::claim`] and [`Sequencer::end_claim`] bracket a
//!   setter. A claimed revision is kept until a newer revision commits, so a
//!   change that takes effect later (same revision) is still its setter's. A
//!   revision that no setter claimed is the acting caller's.
//!
//! # Limits
//!
//! The accumulators are bounded. One open message or tool call holds at most
//! [`Limits::item_bytes`] of content; all open items of the Session hold at
//! most [`Limits::session_bytes`]. An item that would go above a limit is
//! marked `overflowed`: it stops storing content, it keeps counting its
//! parts, and the snapshot says that its content is not complete. Nothing
//! is dropped from the durable path, and the Session does not fail.
//!
//! # Boundaries
//!
//! This module uses only `std`, `loop_protocol` (the update types that a
//! backend makes) and `loop_store` (the stored `Event` and `Identity`). It
//! has no tokio type, no channel, no clock and no wire type (`loop_wire`,
//! `session/delta`). Step B8 moves it to `loop-agent` as the
//! `SessionRecorder` of embedded owners.

use std::collections::BTreeMap;

use loop_protocol::{
    ContentBlock, MessageId, Origin, Role, SessionUpdate, ToolCall, ToolCallId, ToolCallStatus,
    ToolCallUpdate, ToolContent, ToolKind, TurnId, UpdateEnvelope,
};
use loop_store::class::{classify_update, Durability};
use loop_store::{Event, Identity};
use serde_json::Value;

/// Content limit of one open message or tool call: 8 MiB.
pub const ITEM_LIMIT_BYTES: usize = 8 << 20;
/// Content limit of all open items of one Session: 32 MiB.
pub const SESSION_LIMIT_BYTES: usize = 32 << 20;
/// Claimed revisions kept at most. A revision leaves the table when a newer
/// revision commits, so the table is small; this bounds a backend that
/// never announces its changes.
pub const CLAIMS_KEPT: usize = 4096;

/// The bounds of the accumulators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Content bytes of one open message or tool call.
    pub item_bytes: usize,
    /// Content bytes of all open items of the Session.
    pub session_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            item_bytes: ITEM_LIMIT_BYTES,
            session_bytes: SESSION_LIMIT_BYTES,
        }
    }
}

/// An open item that a live-only part extends. `(anchor_seq, id)` is unique;
/// the id alone is not (ids repeat after a restart).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Item {
    /// An open message.
    Message {
        /// `session_seq` of its `message_started`, or the last durable
        /// `seq` before its first part when it had no `message_started`.
        anchor_seq: u64,
        /// The message id.
        id: MessageId,
    },
    /// An open tool call.
    Tool {
        /// `session_seq` of its `tool_call`; for a call that is being
        /// prepared, the last durable `seq` before it.
        anchor_seq: u64,
        /// The tool call id.
        id: ToolCallId,
    },
}

/// What [`Sequencer::committed`] did with one update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// A durable record (ADR-0007 §1.1). The accumulators changed only if
    /// it opens or ends an item.
    Durable,
    /// A live-only part (a chunk, a non-terminal tool update, `preparing`):
    /// part `part` of `item`. Text and thought of one message share one
    /// counter. In step A4 this becomes a delta.
    Part {
        /// The item it extends.
        item: Item,
        /// 0, 1, 2 ... with no gaps, per item.
        part: u32,
    },
    /// A non-terminal `tool_call_update` for a call that already ended in
    /// this turn. It changes nothing.
    Late,
}

/// The turn in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTurn {
    /// `session_seq` of its `turn_started`.
    pub anchor_seq: u64,
    /// The turn id.
    pub turn_id: TurnId,
}

/// A message that has started and has not ended.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenMessage {
    /// See [`Item::Message`].
    pub anchor_seq: u64,
    /// The message id.
    pub id: MessageId,
    /// The author.
    pub role: Role,
    /// Parts applied so far. The last part included is `parts - 1`
    /// (`parts_through` of ADR-0007 §2.3; -1 when this is 0).
    pub parts: u32,
    /// Text and other blocks so far, in order. Consecutive text joins into
    /// one block.
    pub content: Vec<ContentBlock>,
    /// Thought so far.
    pub thought: Vec<ContentBlock>,
    /// Content bytes held.
    pub bytes: usize,
    /// A limit was reached: `content` and `thought` are not complete.
    pub overflowed: bool,
}

/// A tool call that has started and has not reached a terminal status.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenTool {
    /// See [`Item::Tool`].
    pub anchor_seq: u64,
    /// Non-terminal updates applied so far (see [`OpenMessage::parts`]).
    pub parts: u32,
    /// The field-wise merge of the `tool_call` and every update since.
    pub state: ToolCallUpdate,
    /// Content bytes held.
    pub bytes: usize,
    /// A limit was reached: `state` is not complete.
    pub overflowed: bool,
    /// Bytes of each sized field of `state` (see [`field_bytes`]), so an
    /// update is sized from its own fields only.
    sizes: [usize; FIELDS],
}

/// A tool call that the model is writing (`tool_call_preparing`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preparing {
    /// The last durable `seq` before it.
    pub anchor_seq: u64,
    /// The tool call id.
    pub id: ToolCallId,
    /// The tool's own name.
    pub name: String,
    /// What kind of action the tool performs.
    pub kind: ToolKind,
}

/// The in-progress snapshot: the open work after the update `through_seq`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// `H`: the last committed `session_seq` that the snapshot includes.
    pub through_seq: u64,
    /// The last committed `seq` of a durable kind (the anchor of a part
    /// with no durable record of its own).
    pub durable_seq: u64,
    /// The open turn.
    pub turn: Option<OpenTurn>,
    /// Open messages, oldest first.
    pub messages: Vec<OpenMessage>,
    /// Open tool calls, oldest first.
    pub tools: Vec<OpenTool>,
    /// Tool calls being prepared, oldest first.
    pub preparing: Vec<Preparing>,
}

impl Snapshot {
    /// Content bytes held by the open items.
    pub fn bytes(&self) -> usize {
        self.messages.iter().map(|m| m.bytes).sum::<usize>()
            + self.tools.iter().map(|t| t.bytes).sum::<usize>()
    }
}

/// One Session's sequencer state (see the module documentation).
#[derive(Debug, Clone)]
pub struct Sequencer {
    open: Snapshot,
    /// Who causes updates that carry no cause and are not claimed.
    acting: Identity,
    /// Who made each claimed revision that has not been superseded.
    claims: BTreeMap<u64, Identity>,
    /// A setter running now: revisions above the first value are its.
    claim: Option<(u64, Identity)>,
    limits: Limits,
    /// Commits whose `seq` was not `H + 1` (another writer of the Session).
    unexpected: u64,
    /// Content bytes held by the open items (the sum of their `bytes`).
    held: usize,
    /// Tool calls that ended in the current turn: a late update of one
    /// does not open it again.
    ended_tools: Vec<ToolCallId>,
}

impl Sequencer {
    /// A Session whose last committed update is `through_seq`, whose
    /// updates `acting` causes until the first [`Sequencer::act`].
    pub fn new(through_seq: u64, acting: Identity) -> Self {
        Self::with_limits(through_seq, acting, Limits::default())
    }

    /// As [`Sequencer::new`], with other accumulator limits.
    pub fn with_limits(through_seq: u64, acting: Identity, limits: Limits) -> Self {
        Self {
            open: Snapshot {
                through_seq,
                durable_seq: through_seq,
                ..Snapshot::default()
            },
            acting,
            claims: BTreeMap::new(),
            claim: None,
            limits,
            unexpected: 0,
            held: 0,
            ended_tools: Vec::new(),
        }
    }

    /// `H`: the last committed `session_seq`.
    pub fn through_seq(&self) -> u64 {
        self.open.through_seq
    }

    /// Commits whose `seq` did not follow `H` directly. Zero while the pump
    /// is the only writer of the Session's updates.
    pub fn unexpected_seqs(&self) -> u64 {
        self.unexpected
    }

    /// The acting identity now.
    pub fn acting(&self) -> &Identity {
        &self.acting
    }

    /// Updates recorded from now on, with no cause of their own, are
    /// `identity`'s. The caller has recorded every update before the switch.
    pub fn act(&mut self, identity: Identity) {
        self.acting = identity;
    }

    /// A setter of `identity` starts while the Session revision is
    /// `before`: the revisions above it are its until [`Sequencer::end_claim`].
    pub fn claim(&mut self, before: u64, identity: Identity) {
        self.claim = Some((before, identity));
    }

    /// The setter ended with the Session revision at `after`.
    pub fn end_claim(&mut self, after: u64) {
        if let Some((before, identity)) = self.claim.take() {
            for revision in before.saturating_add(1)..=after {
                self.claims.insert(revision, identity.clone());
            }
            while self.claims.len() > CLAIMS_KEPT {
                self.claims.pop_first();
            }
        }
    }

    /// Who caused `envelope` (see the module documentation).
    pub fn cause_of(&self, envelope: &UpdateEnvelope) -> Identity {
        if envelope.turn.is_none() {
            if let SessionUpdate::ConfigChanged(changed) = &envelope.update {
                if let Some(identity) = self.claims.get(&changed.revision) {
                    return identity.clone();
                }
                if let Some((before, identity)) = &self.claim {
                    if changed.revision > *before {
                        return identity.clone();
                    }
                }
            }
        }
        self.acting.clone()
    }

    /// The record to commit for `envelope`, stamped `cause` or, without
    /// one, with who caused it. `None` for replayed history: it is never
    /// stored again and never published.
    pub fn record(
        &self,
        envelope: UpdateEnvelope,
        cause: Option<Identity>,
        generation: u64,
        ts_ms: i64,
    ) -> Option<Event> {
        if envelope.origin == Origin::Replay {
            return None;
        }
        let identity = cause.unwrap_or_else(|| self.cause_of(&envelope));
        Some(Event::update(generation, ts_ms, envelope, identity))
    }

    /// The update `update` committed as `seq`: move `H` and apply it.
    pub fn committed(&mut self, seq: u64, update: &SessionUpdate) -> Applied {
        if seq != self.open.through_seq.saturating_add(1) {
            self.unexpected += 1;
        }
        self.open.through_seq = self.open.through_seq.max(seq);
        let anchor = self.open.durable_seq;
        if classify_update(update).durability == Durability::Durable {
            self.open.durable_seq = self.open.through_seq;
        }
        match update {
            SessionUpdate::TurnStarted { turn_id } => {
                self.open.turn = Some(OpenTurn {
                    anchor_seq: seq,
                    turn_id: turn_id.clone(),
                });
                Applied::Durable
            }
            SessionUpdate::TurnEnded { .. } => {
                self.clear();
                Applied::Durable
            }
            SessionUpdate::MessageStarted { message_id, role } => {
                self.end_message(message_id);
                self.open.messages.push(OpenMessage {
                    anchor_seq: seq,
                    id: message_id.clone(),
                    role: *role,
                    parts: 0,
                    content: Vec::new(),
                    thought: Vec::new(),
                    bytes: 0,
                    overflowed: false,
                });
                Applied::Durable
            }
            SessionUpdate::MessageChunk {
                message_id,
                role,
                content,
            } => self.append(anchor, message_id, *role, content, false),
            SessionUpdate::ThoughtChunk {
                message_id,
                content,
            } => self.append(anchor, message_id, Role::Agent, content, true),
            SessionUpdate::MessageEnded { message_id } => {
                self.end_message(message_id);
                Applied::Durable
            }
            SessionUpdate::ToolCallPreparing { id, name, kind } => {
                self.open.preparing.retain(|p| &p.id != id);
                self.open.preparing.push(Preparing {
                    anchor_seq: anchor,
                    id: id.clone(),
                    name: name.clone(),
                    kind: *kind,
                });
                Applied::Part {
                    item: Item::Tool {
                        anchor_seq: anchor,
                        id: id.clone(),
                    },
                    part: 0,
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.ended_tools.retain(|id| id != &call.id);
                self.open.preparing.retain(|p| p.id != call.id);
                self.end_tool(&call.id);
                if !terminal(Some(call.status)) {
                    let state = from_call(call);
                    self.open_tool(seq, state);
                }
                Applied::Durable
            }
            SessionUpdate::ToolCallUpdate(change) if terminal(change.status) => {
                self.open.preparing.retain(|p| p.id != change.id);
                self.end_tool(&change.id);
                if !self.ended_tools.contains(&change.id) {
                    self.ended_tools.push(change.id.clone());
                }
                Applied::Durable
            }
            SessionUpdate::ToolCallUpdate(change) if self.ended_tools.contains(&change.id) => {
                Applied::Late
            }
            SessionUpdate::ToolCallUpdate(change) => self.merge_tool(anchor, change),
            SessionUpdate::ConfigChanged(changed) => {
                // Older revisions can no longer appear.
                let kept = self.claims.split_off(&changed.revision);
                self.claims = kept;
                Applied::Durable
            }
            _ => Applied::Durable,
        }
    }

    /// The in-progress snapshot and `H` (its `through_seq`).
    pub fn snapshot(&self) -> Snapshot {
        self.open.clone()
    }

    /// Content bytes held by the open items.
    pub fn open_bytes(&self) -> usize {
        self.held
    }

    /// The Session failed: nothing is in progress any more. `H` stays.
    pub fn reset(&mut self) {
        self.clear();
    }

    fn clear(&mut self) {
        self.held = 0;
        self.ended_tools.clear();
        self.open.turn = None;
        self.open.messages.clear();
        self.open.tools.clear();
        self.open.preparing.clear();
    }

    fn room(&self, held: usize, more: usize) -> bool {
        held.saturating_add(more) <= self.limits.item_bytes
            && self.held.saturating_add(more) <= self.limits.session_bytes
    }

    fn end_message(&mut self, id: &MessageId) {
        let held = &mut self.held;
        self.open.messages.retain(|m| {
            let keep = &m.id != id;
            if !keep {
                *held -= m.bytes;
            }
            keep
        });
    }

    fn end_tool(&mut self, id: &ToolCallId) {
        let held = &mut self.held;
        self.open.tools.retain(|t| {
            let keep = &t.state.id != id;
            if !keep {
                *held -= t.bytes;
            }
            keep
        });
    }

    fn append(
        &mut self,
        anchor: u64,
        id: &MessageId,
        role: Role,
        block: &ContentBlock,
        thought: bool,
    ) -> Applied {
        let index = match self.open.messages.iter().rposition(|m| &m.id == id) {
            Some(index) => index,
            None => {
                // A part with no `message_started` (an ACP backend may send
                // one): it opens the message at the last durable record.
                self.open.messages.push(OpenMessage {
                    anchor_seq: anchor,
                    id: id.clone(),
                    role,
                    parts: 0,
                    content: Vec::new(),
                    thought: Vec::new(),
                    bytes: 0,
                    overflowed: false,
                });
                self.open.messages.len() - 1
            }
        };
        let size = block_bytes(block);
        let fits = {
            let message = &self.open.messages[index];
            !message.overflowed && self.room(message.bytes, size)
        };
        let message = &mut self.open.messages[index];
        if fits {
            let blocks = if thought {
                &mut message.thought
            } else {
                &mut message.content
            };
            join(blocks, block);
            message.bytes += size;
            self.held += size;
        } else {
            message.overflowed = true;
        }
        let part = message.parts;
        message.parts = message.parts.saturating_add(1);
        Applied::Part {
            item: Item::Message {
                anchor_seq: message.anchor_seq,
                id: id.clone(),
            },
            part,
        }
    }

    fn open_tool(&mut self, anchor_seq: u64, state: ToolCallUpdate) {
        let sizes = field_bytes(&state);
        let size = sizes.iter().sum();
        let fits = self.room(0, size);
        if fits {
            self.held += size;
        }
        self.open.tools.push(OpenTool {
            anchor_seq,
            parts: 0,
            bytes: if fits { size } else { 0 },
            overflowed: !fits,
            sizes: if fits { sizes } else { [0; FIELDS] },
            state: if fits {
                state
            } else {
                ToolCallUpdate::new(state.id)
            },
        });
    }

    fn merge_tool(&mut self, anchor: u64, change: &ToolCallUpdate) -> Applied {
        let index = match self
            .open
            .tools
            .iter()
            .rposition(|t| t.state.id == change.id)
        {
            Some(index) => index,
            None => {
                // An update for a call with no `tool_call`: it opens the call
                // at the last durable record.
                let anchor = self
                    .open
                    .preparing
                    .iter()
                    .find(|p| p.id == change.id)
                    .map_or(anchor, |p| p.anchor_seq);
                self.open_tool(anchor, ToolCallUpdate::new(change.id.clone()));
                self.open.tools.len() - 1
            }
        };
        // Sized from the fields that the update sets: O(update), not
        // O(state), under the Session's mutex.
        let tool = &self.open.tools[index];
        let fits = !tool.overflowed && {
            let mut sizes = tool.sizes;
            update_sizes(&mut sizes, change);
            let size: usize = sizes.iter().sum();
            let others = self.held - tool.bytes;
            let fits = size <= self.limits.item_bytes
                && others.saturating_add(size) <= self.limits.session_bytes;
            if fits {
                let tool = &mut self.open.tools[index];
                merge(&mut tool.state, change);
                tool.sizes = sizes;
                tool.bytes = size;
                self.held = others + size;
            }
            fits
        };
        let tool = &mut self.open.tools[index];
        if !fits && !tool.overflowed {
            self.held -= tool.bytes;
            tool.overflowed = true;
            tool.bytes = 0;
            tool.sizes = [0; FIELDS];
            tool.state = ToolCallUpdate::new(tool.state.id.clone());
        }
        let part = tool.parts;
        tool.parts = tool.parts.saturating_add(1);
        Applied::Part {
            item: Item::Tool {
                anchor_seq: tool.anchor_seq,
                id: change.id.clone(),
            },
            part,
        }
    }
}

fn terminal(status: Option<ToolCallStatus>) -> bool {
    matches!(
        status,
        Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
    )
}

/// The state of a call as first reported, as a full update.
fn from_call(call: &ToolCall) -> ToolCallUpdate {
    ToolCallUpdate {
        id: call.id.clone(),
        name: call.name.clone(),
        title: Some(call.title.clone()),
        kind: Some(call.kind),
        status: Some(call.status),
        content: Some(call.content.clone()),
        locations: Some(call.locations.clone()),
        raw_output: None,
        raw_input: call.raw_input.clone(),
    }
}

/// ADR-0007 §2.1: each field that `change` sets replaces the old value.
pub fn merge(state: &mut ToolCallUpdate, change: &ToolCallUpdate) {
    fn set<T: Clone>(slot: &mut Option<T>, value: &Option<T>) {
        if let Some(value) = value {
            *slot = Some(value.clone());
        }
    }
    set(&mut state.name, &change.name);
    set(&mut state.title, &change.title);
    set(&mut state.kind, &change.kind);
    set(&mut state.status, &change.status);
    set(&mut state.content, &change.content);
    set(&mut state.locations, &change.locations);
    set(&mut state.raw_output, &change.raw_output);
    set(&mut state.raw_input, &change.raw_input);
}

/// Append `block` to `blocks`: text joins the last text block.
pub fn join(blocks: &mut Vec<ContentBlock>, block: &ContentBlock) {
    if let (Some(ContentBlock::Text { text: last }), ContentBlock::Text { text }) =
        (blocks.last_mut(), block)
    {
        last.push_str(text);
    } else {
        blocks.push(block.clone());
    }
}

/// Approximate payload bytes of a block (text, data and names).
fn block_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::Image { data, mime_type } => data.len() + mime_type.len(),
        ContentBlock::ResourceLink { uri, name } => {
            uri.len() + name.as_ref().map_or(0, String::len)
        }
        ContentBlock::Unknown(value) => value_bytes(value),
    }
}

/// The sized fields of a tool call's state: name, title, content,
/// locations, raw input, raw output.
const FIELDS: usize = 6;

/// Approximate payload bytes of each sized field of `state`.
fn field_bytes(state: &ToolCallUpdate) -> [usize; FIELDS] {
    let mut sizes = [0; FIELDS];
    update_sizes(&mut sizes, state);
    sizes
}

/// Replace the size of each field that `change` sets.
fn update_sizes(sizes: &mut [usize; FIELDS], change: &ToolCallUpdate) {
    if let Some(name) = &change.name {
        sizes[0] = name.len();
    }
    if let Some(title) = &change.title {
        sizes[1] = title.len();
    }
    if let Some(content) = &change.content {
        sizes[2] = content.iter().map(content_bytes).sum();
    }
    if let Some(locations) = &change.locations {
        sizes[3] = locations.iter().map(|l| l.path.len() + 4).sum();
    }
    if let Some(raw_input) = &change.raw_input {
        sizes[4] = value_bytes(raw_input);
    }
    if let Some(raw_output) = &change.raw_output {
        sizes[5] = value_bytes(raw_output);
    }
}

/// Approximate payload bytes of one piece of tool output.
fn content_bytes(content: &ToolContent) -> usize {
    match content {
        ToolContent::Content { content } => block_bytes(content),
        ToolContent::Diff {
            path,
            old_text,
            new_text,
        } => path.len() + old_text.as_ref().map_or(0, String::len) + new_text.len(),
        ToolContent::UnifiedDiff { path, diff } => path.len() + diff.len(),
        ToolContent::Terminal { terminal_id } => terminal_id.len(),
        ToolContent::Unknown(value) => value_bytes(value),
    }
}

/// Approximate bytes of a JSON value, without serializing it.
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => 4,
        Value::Number(_) => 8,
        Value::String(s) => s.len() + 2,
        Value::Array(items) => 2 + items.iter().map(value_bytes).sum::<usize>(),
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| k.len() + 3 + value_bytes(v))
                .sum::<usize>()
        }
    }
}

#[cfg(test)]
#[path = "sequencer_tests.rs"]
mod tests;
