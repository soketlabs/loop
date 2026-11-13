//! Live-only deltas and the new `message_ended` fields (ADR-0007 §1.1, §2.1, §4).
//!
//! Defined for chain A (A2 to A4). Nothing uses them yet.
//!
//! These are wire types. They belong in `loop-protocol`, and the protocol
//! lane will move them there (with `message_ended.content`, `thought` and
//! `interrupted`, and `ClientCapabilities.live_deltas`). They are here so
//! that chain A can code against them before that change. The serde form
//! is not final until A3.

use loop_protocol::{
    ContentBlock, MessageId, Role, SessionKey, ToolCallId, ToolCallUpdate, ToolKind,
};
use serde::{Deserialize, Serialize};

/// Name of the client capability that a Runtime with deltas requires.
pub const LIVE_DELTAS_CAPABILITY: &str = "live_deltas";
/// The protocol version that brings `session/delta` (from 1 to 2).
pub const DELTA_PROTOCOL_VERSION: u32 = 2;
/// The error code for a client without `live_deltas`.
pub const UPGRADE_REQUIRED: &str = "upgrade_required";
/// The notification that carries a delta.
pub const DELTA_METHOD: &str = "session/delta";

/// A live-only part of an open message or tool call. It has no `seq` of its
/// own and is never stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    /// The Session.
    pub session: SessionKey,
    /// The Runtime generation that sent it.
    pub generation: u64,
    /// `session_seq` of the durable record it extends: `message_started`,
    /// `tool_call`, or for `Preparing` the last durable `seq` of the Session.
    pub anchor_seq: u64,
    /// The open item. `(anchor_seq, item)` is unique.
    pub item: DeltaItem,
    /// 0, 1, 2 ... with no gaps, per item. The Runtime gives it at send time.
    pub part: u32,
    /// What changed.
    pub body: DeltaBody,
}

/// The item that a delta extends.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaItem {
    /// An open message.
    Message(MessageId),
    /// An open tool call.
    Tool(ToolCallId),
}

/// The change that a delta carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeltaBody {
    /// Text to append.
    Text {
        /// Author.
        role: Role,
        /// Content to append.
        content: ContentBlock,
    },
    /// Thought to append. Shares the `part` counter with `Text`.
    Thought {
        /// Content to append.
        content: ContentBlock,
    },
    /// Changed fields of the call. Each set field replaces the old value.
    Tool(ToolCallUpdate),
    /// The model is writing a tool call. Always part 0 of its item.
    Preparing {
        /// The tool's own name.
        name: String,
        /// What kind of action the tool performs.
        kind: ToolKind,
    },
}

/// The fields that `message_ended` gains (§1.1). On the wire, `content` and
/// `thought` are optional: a stored row without `content` is an old-format
/// row whose text is in the chunk rows before it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageEndedFields {
    /// All text and other blocks of the message, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    /// The thought blocks of the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<Vec<ContentBlock>>,
    /// True only when recovery closed the message (§2.4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}
