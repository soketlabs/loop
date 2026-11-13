#![recursion_limit = "256"]
//! Versioned event model shared by every Loop agent backend and surface.
//!
//! Backends (Loop's own agent, or an external agent over the Agent Client
//! Protocol) report each turn as a stream of [`UpdateEnvelope`]s. Surfaces
//! render only these events. See the crate README for the wire format.

#![deny(missing_docs)]

pub mod admin;
pub mod backend;
mod capabilities;
mod content;
mod error;
pub mod host;
mod ids;
mod interaction;
pub mod rpc;
pub mod session_services;
mod update;

pub use backend::shell_timeout;
pub use backend::{
    AgentBackend, BackendError, BackendResult, Compaction, CompactionReport, HostFileSystem,
    HostServices, HostTerminals, LocalShell, ModelChoice, ModelChoices, ModelControl,
    MutationResult, SessionOptions, SessionRef, SessionSpec, SessionStats, ShellResult,
    StatsSummary, TerminalExit, TerminalOutput, TerminalRequest, TurnHandle, TurnOutcome,
    SHELL_OUTPUT_LIMIT,
};
pub use capabilities::{Capabilities, ToolLock};
pub use content::ContentBlock;
pub use error::{ErrorCategory, ErrorInfo};
pub use ids::{BackendInstanceId, InteractionId, MessageId, SessionKey, ToolCallId, TurnId};
pub use interaction::{
    Choice, InteractionOutcome, InteractionRequest, PermissionKind, PermissionOption,
    DECLINE_CHOICE,
};
pub use update::{
    ConfigChanged, NoticeLevel, Origin, PlanEntry, PlanEntryStatus, Role, SessionConfig,
    SessionMetadata, SessionUpdate, StopReason, ToolCall, ToolCallStatus, ToolCallUpdate,
    ToolContent, ToolKind, ToolLocation, UpdateEnvelope, Usage,
};

/// Version of the event model. Bumped only for changes an older reader cannot
/// tolerate; additive changes keep the version.
pub const PROTOCOL_VERSION: u32 = 1;
