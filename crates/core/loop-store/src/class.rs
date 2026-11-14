//! Which records are durable and which are live only, as data (ADR-0007 §1).
//!
//! Defined for chain A (A0 and A4). Nothing uses it yet: today every update
//! is stored. Tests can read the tables, and [`classify_update`] gives the
//! class of one update value.

use loop_protocol::{SessionUpdate, ToolCallStatus};

/// Whether the log stores a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Durability {
    /// Stored. Reaches Surfaces only after its commit.
    Durable,
    /// Sent as a delta. Never stored.
    LiveOnly,
}

/// Which values of an update kind a row covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdateCase {
    /// Every value of the kind.
    Any,
    /// `tool_call_update` whose `status` is not `completed` or `failed`.
    NotTerminal,
    /// `tool_call_update` whose `status` is `completed` or `failed`.
    Terminal,
}

/// The class of one wire update kind (§1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UpdateClass {
    /// The serde `type` tag of the `SessionUpdate`.
    pub kind: &'static str,
    /// The values of the kind that this row covers.
    pub case: UpdateCase,
    /// Stored or live only.
    pub durability: Durability,
    /// The producer waits for the commit before it continues.
    pub barrier: bool,
}

const fn update(
    kind: &'static str,
    case: UpdateCase,
    durability: Durability,
    barrier: bool,
) -> UpdateClass {
    UpdateClass {
        kind,
        case,
        durability,
        barrier,
    }
}

use Durability::{Durable, LiveOnly};
use UpdateCase::{Any, NotTerminal, Terminal};

const BRANCH_RESET: UpdateClass = update("branch_reset", Any, Durable, true);
const TURN_STARTED: UpdateClass = update("turn_started", Any, Durable, true);
const MESSAGE_STARTED: UpdateClass = update("message_started", Any, Durable, false);
const MESSAGE_CHUNK: UpdateClass = update("message_chunk", Any, LiveOnly, false);
const THOUGHT_CHUNK: UpdateClass = update("thought_chunk", Any, LiveOnly, false);
const MESSAGE_ENDED: UpdateClass = update("message_ended", Any, Durable, false);
const TOOL_CALL_PREPARING: UpdateClass = update("tool_call_preparing", Any, LiveOnly, false);
const TOOL_CALL: UpdateClass = update("tool_call", Any, Durable, true);
const TOOL_CALL_UPDATE_OPEN: UpdateClass = update("tool_call_update", NotTerminal, LiveOnly, false);
const TOOL_CALL_UPDATE_END: UpdateClass = update("tool_call_update", Terminal, Durable, false);
const PLAN: UpdateClass = update("plan", Any, Durable, false);
const USAGE: UpdateClass = update("usage", Any, Durable, false);
const NOTICE: UpdateClass = update("notice", Any, Durable, false);
const INTERACTION_REQUESTED: UpdateClass = update("interaction_requested", Any, Durable, true);
const INTERACTION_RESOLVED: UpdateClass = update("interaction_resolved", Any, Durable, true);
const TURN_ENDED: UpdateClass = update("turn_ended", Any, Durable, true);
const CONFIG_CHANGED: UpdateClass = update("config_changed", Any, Durable, true);
const WORKFLOW_PROGRESS: UpdateClass = update("workflow_progress", Any, Durable, false);
const UNKNOWN: UpdateClass = update("unknown", Any, Durable, false);

/// §1.1, one row per kind and case.
pub const UPDATE_CLASSES: &[UpdateClass] = &[
    BRANCH_RESET,
    TURN_STARTED,
    MESSAGE_STARTED,
    MESSAGE_CHUNK,
    THOUGHT_CHUNK,
    MESSAGE_ENDED,
    TOOL_CALL_PREPARING,
    TOOL_CALL,
    TOOL_CALL_UPDATE_OPEN,
    TOOL_CALL_UPDATE_END,
    PLAN,
    USAGE,
    NOTICE,
    INTERACTION_REQUESTED,
    INTERACTION_RESOLVED,
    TURN_ENDED,
    CONFIG_CHANGED,
    WORKFLOW_PROGRESS,
    UNKNOWN,
];

/// The class of one update value. A `tool_call_update` with no status is
/// not terminal.
pub fn classify_update(update: &SessionUpdate) -> UpdateClass {
    match update {
        SessionUpdate::BranchReset { .. } => BRANCH_RESET,
        SessionUpdate::TurnStarted { .. } => TURN_STARTED,
        SessionUpdate::MessageStarted { .. } => MESSAGE_STARTED,
        SessionUpdate::MessageChunk { .. } => MESSAGE_CHUNK,
        SessionUpdate::MessageEnded { .. } => MESSAGE_ENDED,
        SessionUpdate::ThoughtChunk { .. } => THOUGHT_CHUNK,
        SessionUpdate::ToolCallPreparing { .. } => TOOL_CALL_PREPARING,
        SessionUpdate::ToolCall(_) => TOOL_CALL,
        SessionUpdate::ToolCallUpdate(change) => match change.status {
            Some(ToolCallStatus::Completed | ToolCallStatus::Failed) => TOOL_CALL_UPDATE_END,
            _ => TOOL_CALL_UPDATE_OPEN,
        },
        SessionUpdate::Plan { .. } => PLAN,
        SessionUpdate::Usage(_) => USAGE,
        SessionUpdate::Notice { .. } => NOTICE,
        SessionUpdate::InteractionRequested { .. } => INTERACTION_REQUESTED,
        SessionUpdate::InteractionResolved { .. } => INTERACTION_RESOLVED,
        SessionUpdate::TurnEnded { .. } => TURN_ENDED,
        SessionUpdate::ConfigChanged(_) => CONFIG_CHANGED,
        SessionUpdate::WorkflowProgress(_) => WORKFLOW_PROGRESS,
        SessionUpdate::Unknown(_) => UNKNOWN,
    }
}

/// The class of one log record kind (§1.2). Every log record is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordClass {
    /// The stored `kind`.
    pub kind: &'static str,
    /// The writer waits for the commit. `None` for `update`: it depends on
    /// the update kind ([`UPDATE_CLASSES`]).
    pub barrier: Option<bool>,
    /// Only `update` records take a `session_seq`.
    pub session_seq: bool,
    /// `EventKind` has a variant for it today. `false` for the kinds that
    /// ADR-0007 adds; such rows read as `EventKind::Unknown` until then.
    pub in_event_kind: bool,
}

const fn record(kind: &'static str, barrier: Option<bool>, in_event_kind: bool) -> RecordClass {
    RecordClass {
        kind,
        barrier,
        session_seq: false,
        in_event_kind,
    }
}

/// §1.2, one row per stored kind. `unknown` is the catch-all and has no
/// stored kind of its own.
pub const RECORD_CLASSES: &[RecordClass] = &[
    RecordClass {
        kind: "update",
        barrier: None,
        session_seq: true,
        in_event_kind: true,
    },
    record("prompt_sent", Some(true), true),
    record("prompt_settled", Some(true), true),
    record("cancelled", Some(true), true),
    record("escalation_raised", Some(true), true),
    record("escalation_answered", Some(true), true),
    record("escalation_withdrawn", Some(true), true),
    record("session_opened", Some(true), true),
    record("session_closed", Some(true), true),
    record("subagent_started", Some(true), true),
    record("subagent_ended", Some(true), true),
    record("runtime_started", Some(true), true),
    record("runtime_stopped", Some(true), true),
    record(crate::records::POLICY_DECISION, Some(true), true),
    // A turn's tree records commit in one atomic request that the harness
    // awaits (§5.3).
    record(crate::records::TREE, Some(true), false),
    record(crate::records::SESSION_DELETED, Some(true), false),
];
