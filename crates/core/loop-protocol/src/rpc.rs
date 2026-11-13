//! The JSON-RPC methods that carry [`AgentBackend`](crate::AgentBackend)
//! and [`HostServices`](crate::HostServices) over a connection.
//!
//! This module only names the methods and shapes their params and results;
//! the connection and transports live in `loop-wire`. See
//! `docs/design/protocol-wire.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{BackendError, SessionRef, SessionSpec, TurnOutcome};
use crate::capabilities::Capabilities;
use crate::content::ContentBlock;
use crate::error::{ErrorCategory, ErrorInfo};
use crate::ids::{BackendInstanceId, SessionKey};
use crate::update::StopReason;

/// Method names.
pub mod methods {
    /// Current Session snapshot.
    pub const SESSION_CONFIG: &str = "session/config";
    /// Redacted workspace settings.
    pub const WORKSPACE_SETTINGS: &str = "workspace/settings";
    /// Session skill activation.
    pub const SESSION_SKILL: &str = "session/skill";
    /// Start a workflow; progress is streamed.
    pub const SESSION_WORKFLOW: &str = "session/workflow";
    // Session-tree operations.
    /// Session-tree RPC method.
    pub const SESSION_TREE: &str = "session/tree";
    /// Session-tree RPC method.
    pub const SESSION_VIEW: &str = "session/view";
    /// Session-tree RPC method.
    pub const SESSION_NAVIGATE: &str = "session/navigate";
    /// Session-tree RPC method.
    pub const SESSION_RENAME: &str = "session/rename";
    /// Session-tree RPC method.
    pub const SESSION_FORK: &str = "session/fork";
    /// Session-tree RPC method.
    pub const SESSION_CLONE: &str = "session/clone";
    /// Session-tree RPC method.
    pub const SESSION_EXPORT: &str = "session/export";
    /// Session-tree RPC method.
    pub const SESSION_IMPORT: &str = "session/import";
    /// Client → server request: agree on a version. [`super::InitializeParams`]
    /// → [`super::InitializeResult`].
    pub const INITIALIZE: &str = "initialize";
    /// Client → server request: [`crate::SessionSpec`] → [`crate::SessionRef`].
    pub const SESSION_NEW: &str = "session/new";
    /// Client → server request: `{}` → `[SessionInfo]`.
    pub const SESSION_LIST: &str = "session/list";
    /// Client → server request: [`super::CloseParams`] → `{}`.
    pub const SESSION_CLOSE: &str = "session/close";
    /// Client → server request: [`super::AttachParams`] → [`super::AttachResult`].
    pub const SESSION_ATTACH: &str = "session/attach";
    /// Client → server request: [`super::SessionParams`] → `{}`.
    pub const SESSION_DETACH: &str = "session/detach";
    /// Client → server request: [`super::LoadParams`] → [`crate::SessionRef`].
    pub const SESSION_LOAD: &str = "session/load";
    /// Client → server request: [`super::SessionParams`] →
    /// `[SessionUpdate]`.
    pub const SESSION_TRANSCRIPT: &str = "session/transcript";
    /// Client → server request: [`super::PromptParams`] →
    /// [`super::PromptResult`], sent after the turn's `turn_ended` update.
    pub const SESSION_PROMPT: &str = "session/prompt";
    /// Client → server notification: [`super::SessionParams`].
    pub const SESSION_CANCEL: &str = "session/cancel";
    /// Server → client notification: an [`crate::UpdateEnvelope`].
    pub const SESSION_UPDATE: &str = "session/update";
    /// Server → client request: [`super::InteractParams`] →
    /// [`crate::InteractionOutcome`].
    pub const HOST_INTERACT: &str = "host/interact";
    /// Server → client notification: [`super::InteractWithdrawnParams`].
    pub const HOST_INTERACT_WITHDRAWN: &str = "host/interact_withdrawn";
    /// Client → server request: `{}` → `{}`. The server shuts the backend
    /// down and closes the connection.
    pub const SHUTDOWN: &str = "shutdown";
    /// Client → server request: `{}` → [`super::RuntimeStatus`]. Served only
    /// by a shared Runtime.
    pub const RUNTIME_STATUS: &str = "runtime/status";
    /// Client → server request: `{}` → `{}`. The Runtime answers, then runs
    /// its normal signal shutdown: it closes every Session, removes its
    /// socket and exits. Served only by a shared Runtime.
    pub const RUNTIME_STOP: &str = "runtime/stop";
    /// Stop an empty owner atomically, for version replacement.
    pub const RUNTIME_STOP_EMPTY: &str = "runtime/stop_empty";
    /// Client → server request: [`super::SessionParams`] →
    /// [`super::ModelsResult`]. Needs the `set_model` capability.
    pub const SESSION_MODELS: &str = "session/models";
    /// Client → server request: [`super::SetModelParams`] →
    /// [`crate::MutationResult`]. Needs the `set_model` capability.
    pub const SESSION_SET_MODEL: &str = "session/set_model";
    /// Client → server request: [`super::SessionParams`] →
    /// [`super::ChoicesResult`]. Needs the `efforts` capability.
    pub const SESSION_EFFORTS: &str = "session/efforts";
    /// Client → server request: [`super::SetEffortParams`] →
    /// [`crate::MutationResult`]. Needs the `efforts` capability.
    pub const SESSION_SET_EFFORT: &str = "session/set_effort";
    /// Client → server request: [`super::SessionParams`] →
    /// [`super::ChoicesResult`]. Needs the `modes` capability.
    pub const SESSION_MODES: &str = "session/modes";
    /// Client → server request: [`super::SetModeParams`] →
    /// [`crate::MutationResult`]. Needs the `modes` capability.
    pub const SESSION_SET_MODE: &str = "session/set_mode";
    /// Client → server request: [`super::CompactParams`] →
    /// [`crate::CompactionReport`]. Needs the `compaction` capability.
    pub const SESSION_COMPACT: &str = "session/compact";
    /// Client → server request: [`super::SessionParams`] →
    /// [`super::StatsResult`]. Needs the `session_stats` capability.
    pub const SESSION_STATS: &str = "session/stats";
    /// Client → server request: [`super::ShellParams`] →
    /// [`crate::ShellResult`]. Needs the `local_shell` capability.
    pub const SESSION_SHELL: &str = "session/shell";
    /// Client → server request: [`super::TtfbParams`] →
    /// [`super::TtfbResult`]. Needs the `session_ttfb` capability.
    pub const SESSION_TTFB: &str = "session/ttfb";
    /// Client → server request: [`crate::admin::McpParams`] →
    /// [`crate::admin::McpResult`]. Needs `workspace_mcp`.
    pub const WORKSPACE_MCP: &str = "workspace/mcp";
    /// Client → server request: [`crate::admin::WebSearchParams`] →
    /// [`crate::admin::WebSearchResult`]. Needs `workspace_web_search`.
    pub const WORKSPACE_WEB_SEARCH: &str = "workspace/web_search";
    /// Client → server request: [`crate::admin::SkillsParams`] →
    /// [`crate::admin::SkillsResult`]. Needs `workspace_skills`.
    pub const WORKSPACE_SKILLS: &str = "workspace/skills";
    /// Client → server request: [`crate::admin::TemplatesParams`] →
    /// [`crate::admin::TemplatesResult`]. Needs `workspace_templates`.
    pub const WORKSPACE_TEMPLATES: &str = "workspace/templates";
    /// Client → server request: [`crate::admin::ReloadParams`] →
    /// [`crate::admin::ReloadResult`]. Needs `workspace_reload`.
    pub const WORKSPACE_RELOAD: &str = "workspace/reload";
    /// Client → server request: [`crate::admin::TrustParams`] →
    /// [`crate::admin::TrustResult`]. Needs `workspace_trust`; a write is
    /// host-only.
    pub const WORKSPACE_TRUST: &str = "workspace/trust";
    /// Client → server request: [`crate::admin::SandboxParams`] →
    /// [`crate::admin::SandboxResult`]. Needs `session_sandbox`.
    pub const SESSION_SANDBOX: &str = "session/sandbox";
    /// Client → server request: [`crate::admin::PermissionModeParams`] →
    /// [`crate::admin::PermissionModeResult`]. Needs `permission_mode`.
    pub const SESSION_PERMISSION_MODE: &str = "session/permission_mode";
    /// Host-only request: [`crate::admin::UsageParams`] →
    /// [`crate::admin::UsageResult`]. Needs `host_usage`.
    pub const RUNTIME_USAGE: &str = "runtime/usage";
    /// Host-only request: [`crate::admin::DefaultsParams`] →
    /// [`crate::admin::DefaultsResult`]. Needs `host_defaults`.
    pub const RUNTIME_DEFAULTS: &str = "runtime/defaults";
    /// Client → server request: `{}` → [`crate::admin::RuntimeSessionsResult`].
    /// Needs `runtime_sessions`.
    pub const RUNTIME_SESSIONS: &str = "runtime/sessions";
    /// Host-only provider login and logout.
    pub const RUNTIME_PROVIDERS: &str = "runtime/providers";
    /// Host-only MCP OAuth login and logout.
    pub const RUNTIME_MCP_AUTH: &str = "runtime/mcp_auth";
    /// Host-only Runtime tracing.
    pub const RUNTIME_TRACING: &str = "runtime/tracing";

    /// Methods served only with host-local authority (the Runtime's 0600
    /// Unix socket or an embedded adapter), never over a WebSocket.
    pub const HOST_ONLY: &[&str] = &[
        RUNTIME_STOP,
        RUNTIME_STOP_EMPTY,
        RUNTIME_USAGE,
        RUNTIME_DEFAULTS,
        RUNTIME_PROVIDERS,
        RUNTIME_MCP_AUTH,
        RUNTIME_TRACING,
    ];
}

/// Who is on the other end, for logs. Never used for feature checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PeerInfo {
    /// Program name, e.g. `loop`.
    pub name: String,
    /// Program version.
    pub version: String,
}

/// What a client can do for the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ClientCapabilities {
    /// Answers `host/interact` (permission prompts, questions).
    pub interact: bool,
    /// Replaces its transcript when a branch reset arrives.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub branch_reset: bool,
}

/// `initialize` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InitializeParams {
    /// The highest version the client speaks.
    pub protocol_version: u32,
    /// The client.
    pub client_info: PeerInfo,
    /// What the client can do.
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InitializeResult {
    /// The version to use: the client's, when the server speaks it, or else
    /// the server's highest. A client that can't use it closes.
    pub protocol_version: u32,
    /// The server.
    pub server_info: PeerInfo,
    /// The backend instance behind the server.
    pub instance: BackendInstanceId,
    /// What the backend supports.
    pub capabilities: Capabilities,
    /// Who this connection acts for (ADR-0009), fixed when the connection
    /// was accepted. Absent from an older server, or from a connection that
    /// has no identity. A request field never changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<crate::Principal>,
    /// The tenant this connection's data belongs to. Present with
    /// `principal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<crate::Tenant>,
    /// How the principal reached the Runtime (`token`, `edge`); absent on a
    /// host-local connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<crate::Via>,
}

/// Params naming one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionParams {
    /// The session.
    pub session: SessionRef,
}

/// One Session in the Runtime's list, including closed Sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionInfo {
    /// The full Session reference.
    pub r#ref: SessionRef,
    /// Working directory.
    pub cwd: std::path::PathBuf,
    /// Display name, when set.
    #[serde(default)]
    pub name: Option<String>,
    /// Parent Session, when this is a Subagent.
    #[serde(default)]
    pub parent: Option<SessionRef>,
    /// Whether the Runtime currently holds this Session open.
    pub live: bool,
    /// Whether a turn is running.
    pub busy: bool,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// Last activity time in milliseconds since the Unix epoch.
    pub last_active_ms: u64,
    /// Public picker summary, read without writer admission.
    #[serde(default, flatten)]
    pub summary: SessionListSummary,
}

/// `runtime/status` result: the Runtime process and its Session counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeStatus {
    /// True only for an owner started automatically by a Surface.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_started: bool,
    /// The Runtime's process id.
    pub pid: u32,
    /// The socket this connection reached.
    pub socket: std::path::PathBuf,
    /// Loopback WebSocket endpoint, without credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_url: Option<String>,
    /// The Runtime generation.
    pub generation: u64,
    /// Sessions held in memory.
    pub loaded: u64,
    /// Loaded Sessions with a turn running.
    pub busy: u64,
    /// Most Sessions held at once.
    pub max_sessions: u64,
    /// Most turns running at once.
    pub max_active_turns: u64,
    /// Where `max_sessions` came from: `flag`, `settings` or `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions_from: Option<String>,
    /// Where `max_active_turns` came from: `flag`, `settings` or `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_turns_from: Option<String>,
    /// The Runtime's tool lock (`--tool-lock`), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_lock: Option<crate::ToolLock>,
}

/// `session/close` params. Closing a busy Session requires `force`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CloseParams {
    /// The Session to close.
    pub session: SessionRef,
    /// Cancel a running turn before closing. Defaults to false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// `session/attach` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachParams {
    /// The Session to attach.
    pub session: SessionRef,
    /// Last processed update sequence; zero replays from the beginning.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub after_seq: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// `session/attach` result, sent before replay and live updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachResult {
    /// Last update sequence in the replay snapshot, not an audit cursor.
    pub latest_seq: u64,
    /// Runtime generation of the snapshot.
    pub generation: u64,
    /// The Session's state at `latest_seq`, from servers with the
    /// `attach_snapshot` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SessionSnapshot>,
}

/// A Session's state at one committed update sequence. Excludes secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionSnapshot {
    /// Public startup notices, including credential-environment differences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
    /// The Session.
    pub session: SessionRef,
    /// Opaque host workspace identity, when the server reports one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    /// The update sequence the snapshot was taken at: the replay's last
    /// update. Not the client's processed cursor.
    pub at_seq: u64,
    /// The Session revision.
    pub revision: u64,
    /// The active branch's revision.
    #[serde(default)]
    pub branch_revision: u64,
    /// Immutable transcript view at `at_seq`, for servers with the
    /// `branch_snapshot` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<String>,
    /// Session metadata.
    #[serde(default)]
    pub metadata: crate::SessionMetadata,
    /// The selected config.
    #[serde(default)]
    pub config: crate::SessionConfig,
    /// The config the current turn runs with.
    #[serde(default)]
    pub effective: crate::SessionConfig,
    /// Part of `config` waits for the running turn to end.
    #[serde(default)]
    pub pending: bool,
    /// What this Session supports.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Idle, busy or parked, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_state: Option<String>,
    /// Public stats; unknown values are omitted.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub stats: Value,
    /// Revision of the workspace definitions the Session uses.
    #[serde(default)]
    pub workspace_revision: u64,
}

/// One model in `session/models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelInfo {
    /// Id to pass to `session/set_model`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The backend instance that runs the model.
    pub backend: BackendInstanceId,
}

/// `session/models` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelsResult {
    /// The current model's id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Every model on offer.
    pub models: Vec<ModelInfo>,
    /// The Session revision.
    pub revision: u64,
}

/// One effort level or mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChoiceInfo {
    /// Id to pass to the setter.
    pub id: String,
    /// Display name.
    pub name: String,
}

/// `session/efforts` and `session/modes` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChoicesResult {
    /// The current choice's id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Every choice on offer.
    pub choices: Vec<ChoiceInfo>,
    /// The Session revision.
    pub revision: u64,
}

/// `session/set_model` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetModelParams {
    /// The Session.
    pub session: SessionRef,
    /// A [`ModelInfo::id`].
    pub model: String,
    /// Fail with a conflict unless the Session is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `session/set_effort` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetEffortParams {
    /// The Session.
    pub session: SessionRef,
    /// A [`ChoiceInfo::id`] from `session/efforts`.
    pub effort: String,
    /// Fail with a conflict unless the Session is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `session/set_mode` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetModeParams {
    /// The Session.
    pub session: SessionRef,
    /// A [`ChoiceInfo::id`] from `session/modes`.
    pub mode: String,
    /// Fail with a conflict unless the Session is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

impl ModelsResult {
    /// The wire form of `choices`, offered by backend `backend`.
    pub fn from_choices(choices: crate::ModelChoices, backend: &BackendInstanceId) -> Self {
        Self {
            current: choices.current,
            models: choices
                .models
                .into_iter()
                .map(|m| ModelInfo {
                    id: m.id,
                    name: m.name,
                    description: m.description,
                    backend: backend.clone(),
                })
                .collect(),
            revision: choices.revision,
        }
    }
}

impl From<ModelsResult> for crate::ModelChoices {
    fn from(result: ModelsResult) -> Self {
        Self {
            current: result.current,
            models: result
                .models
                .into_iter()
                .map(|m| crate::ModelChoice {
                    id: m.id,
                    name: m.name,
                    description: m.description,
                })
                .collect(),
            revision: result.revision,
        }
    }
}

impl From<crate::ModelChoices> for ChoicesResult {
    fn from(choices: crate::ModelChoices) -> Self {
        Self {
            current: choices.current,
            choices: choices
                .models
                .into_iter()
                .map(|m| ChoiceInfo {
                    id: m.id,
                    name: m.name,
                })
                .collect(),
            revision: choices.revision,
        }
    }
}

impl From<ChoicesResult> for crate::ModelChoices {
    fn from(result: ChoicesResult) -> Self {
        Self {
            current: result.current,
            models: result
                .choices
                .into_iter()
                .map(|c| crate::ModelChoice {
                    id: c.id,
                    name: c.name,
                    description: None,
                })
                .collect(),
            revision: result.revision,
        }
    }
}

/// `host/interact` params, preserving the v1 interaction shape.
///
/// Routing lives at the wire boundary so local backends can continue producing
/// Session-independent requests. Flattening keeps old v1 messages compatible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InteractParams {
    /// The Session awaiting an answer. Absent on single-Session v1 peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRef>,
    /// The backend's original request and native interaction id.
    #[serde(flatten)]
    pub request: crate::InteractionRequest,
}

/// `host/interact_withdrawn` notification params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InteractWithdrawnParams {
    /// The Session whose Escalation is no longer pending.
    pub session: SessionRef,
    /// The original interaction id, not a connection's JSON-RPC request id.
    pub id: crate::InteractionId,
}

/// `session/load` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoadParams {
    /// The session to reopen.
    pub session: SessionRef,
    /// How to open it.
    #[serde(default)]
    pub spec: SessionSpec,
}

/// `session/prompt` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PromptParams {
    /// The session.
    pub session: SessionRef,
    /// The prompt.
    pub content: Vec<ContentBlock>,
}

/// `session/prompt` result: how the turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PromptResult {
    /// Why the turn ended.
    pub stop: StopReason,
    /// Failure details when `stop` is `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    /// The backend lost contact with its agent mid-turn: edits or commands
    /// may already have run, so the prompt must not be re-sent automatically.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
}

impl From<TurnOutcome> for PromptResult {
    fn from(outcome: TurnOutcome) -> Self {
        let (stop, error, uncertain) = match outcome {
            TurnOutcome::Completed(stop) => (stop, None, false),
            TurnOutcome::Cancelled => (StopReason::Cancelled, None, false),
            TurnOutcome::Failed(error) => (StopReason::Error, Some(error), false),
            TurnOutcome::Uncertain => (StopReason::Error, None, true),
        };
        Self {
            stop,
            error,
            uncertain,
        }
    }
}

impl From<PromptResult> for TurnOutcome {
    fn from(result: PromptResult) -> Self {
        match result {
            PromptResult {
                uncertain: true, ..
            } => Self::Uncertain,
            PromptResult {
                error: Some(error), ..
            } => Self::Failed(error),
            PromptResult {
                stop: StopReason::Cancelled,
                ..
            } => Self::Cancelled,
            PromptResult {
                stop: StopReason::RuntimeRestarted,
                ..
            } => Self::Uncertain,
            PromptResult {
                stop: StopReason::Error,
                ..
            } => Self::Failed(ErrorInfo::new(ErrorCategory::Agent, "the turn failed")),
            PromptResult { stop, .. } => Self::Completed(stop),
        }
    }
}

/// JSON-RPC error codes for [`BackendError`]s.
pub mod error_codes {
    /// [`crate::BackendError::Busy`].
    pub const BUSY: i64 = -32001;
    /// [`crate::BackendError::Unsupported`].
    pub const UNSUPPORTED: i64 = -32002;
    /// [`crate::BackendError::SessionNotFound`].
    pub const SESSION_NOT_FOUND: i64 = -32003;
    /// [`crate::BackendError::Failed`].
    pub const FAILED: i64 = -32004;
    /// [`crate::BackendError::Conflict`]; data is `{"revision": n}`.
    pub const CONFLICT: i64 = -32005;
    /// [`crate::BackendError::Forbidden`]; data is the method name.
    pub const FORBIDDEN: i64 = -32006;
    /// [`crate::BackendError::ToolLocked`]; data is `{"what": …, "lock": …}`.
    pub const TOOL_LOCKED: i64 = -32007;
}

/// A JSON-RPC error: code, message and optional data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RpcErrorBody {
    /// Error code.
    pub code: i64,
    /// Message.
    pub message: String,
    /// Structured details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl From<&BackendError> for RpcErrorBody {
    fn from(error: &BackendError) -> Self {
        let (code, data) = match error {
            BackendError::Busy => (error_codes::BUSY, None),
            BackendError::Unsupported(what) => {
                (error_codes::UNSUPPORTED, Some(Value::from(what.as_str())))
            }
            BackendError::SessionNotFound(key) => (
                error_codes::SESSION_NOT_FOUND,
                Some(serde_json::json!({ "session": key })),
            ),
            BackendError::Conflict { revision } => (
                error_codes::CONFLICT,
                Some(serde_json::json!({ "revision": revision })),
            ),
            BackendError::Failed(info) => (error_codes::FAILED, serde_json::to_value(info).ok()),
            BackendError::Forbidden(method) => {
                (error_codes::FORBIDDEN, Some(Value::from(method.as_str())))
            }
            BackendError::ToolLocked { what, lock } => (
                error_codes::TOOL_LOCKED,
                Some(serde_json::json!({ "what": what, "lock": lock })),
            ),
        };
        Self {
            code,
            message: error.to_string(),
            data,
        }
    }
}

impl RpcErrorBody {
    /// The [`BackendError`] this error stands for. Codes Loop doesn't know,
    /// including JSON-RPC's own, become [`BackendError::Failed`] with a
    /// protocol category.
    pub fn into_backend_error(self) -> BackendError {
        let data = self.data.unwrap_or(Value::Null);
        match self.code {
            error_codes::BUSY => BackendError::Busy,
            error_codes::UNSUPPORTED => {
                BackendError::Unsupported(data.as_str().unwrap_or(&self.message).to_owned())
            }
            error_codes::SESSION_NOT_FOUND => BackendError::SessionNotFound(
                serde_json::from_value::<SessionKey>(data["session"].clone())
                    .unwrap_or_else(|_| SessionKey(String::new())),
            ),
            error_codes::CONFLICT => BackendError::Conflict {
                revision: data["revision"].as_u64().unwrap_or_default(),
            },
            error_codes::FAILED => BackendError::Failed(
                serde_json::from_value(data)
                    .unwrap_or_else(|_| ErrorInfo::new(ErrorCategory::Agent, self.message)),
            ),
            error_codes::FORBIDDEN => {
                BackendError::Forbidden(data.as_str().unwrap_or(&self.message).to_owned())
            }
            error_codes::TOOL_LOCKED => BackendError::ToolLocked {
                what: data["what"].as_str().unwrap_or(&self.message).to_owned(),
                lock: serde_json::from_value(data["lock"].clone())
                    .unwrap_or(crate::ToolLock::Unknown),
            },
            _ => BackendError::failed(ErrorCategory::Protocol, self.message),
        }
    }
}

/// The version to use: `client`'s when this side speaks it, or else this
/// side's highest, [`crate::PROTOCOL_VERSION`].
pub fn negotiate(client: u32) -> u32 {
    if (1..=crate::PROTOCOL_VERSION).contains(&client) {
        client
    } else {
        crate::PROTOCOL_VERSION
    }
}

/// The JSON Schema of the wire format: every type as a definition, and
/// each method's direction, kind, params and result. The checked-in
/// `schema/loop-protocol.json` is this value.
#[cfg(feature = "schema")]
pub fn json_schema() -> Value {
    use crate::{InteractionOutcome, SessionUpdate, UpdateEnvelope, PROTOCOL_VERSION};

    let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let reference = |schema: schemars::Schema| serde_json::to_value(schema).unwrap_or_default();
    let initialize_params = reference(generator.subschema_for::<InitializeParams>());
    let initialize_result = reference(generator.subschema_for::<InitializeResult>());
    // The stored and audit form of a connection's identity (ADR-0009).
    let _caller = generator.subschema_for::<crate::Caller>();
    let spec = reference(generator.subschema_for::<SessionSpec>());
    let session = reference(generator.subschema_for::<SessionRef>());
    let load = reference(generator.subschema_for::<LoadParams>());
    let session_params = reference(generator.subschema_for::<SessionParams>());
    let session_list = reference(generator.subschema_for::<Vec<SessionInfo>>());
    let close = reference(generator.subschema_for::<CloseParams>());
    let status = reference(generator.subschema_for::<RuntimeStatus>());
    let attach = reference(generator.subschema_for::<AttachParams>());
    let attached = reference(generator.subschema_for::<AttachResult>());
    let withdrawn = reference(generator.subschema_for::<InteractWithdrawnParams>());
    let transcript = reference(generator.subschema_for::<Vec<SessionUpdate>>());
    let prompt_params = reference(generator.subschema_for::<PromptParams>());
    let prompt_result = reference(generator.subschema_for::<PromptResult>());
    let envelope = reference(generator.subschema_for::<UpdateEnvelope>());
    let interaction = reference(generator.subschema_for::<InteractParams>());
    let interaction_outcome = reference(generator.subschema_for::<InteractionOutcome>());
    let error = reference(generator.subschema_for::<RpcErrorBody>());
    let tree_params = reference(generator.subschema_for::<TreeParams>());
    let tree_result = reference(generator.subschema_for::<TreeResult>());
    let view_params = reference(generator.subschema_for::<ViewParams>());
    let view_result = reference(generator.subschema_for::<ViewResult>());
    let navigate_params = reference(generator.subschema_for::<NavigateParams>());
    let navigate_result = reference(generator.subschema_for::<NavigateResult>());
    let rename_params = reference(generator.subschema_for::<RenameParams>());
    let fork_params = reference(generator.subschema_for::<ForkParams>());
    let clone_params = reference(generator.subschema_for::<CloneParams>());
    let destination = reference(generator.subschema_for::<DestinationResult>());
    let export = reference(generator.subschema_for::<ExportResult>());
    let import = reference(generator.subschema_for::<ImportParams>());
    let models = reference(generator.subschema_for::<ModelsResult>());
    let choices = reference(generator.subschema_for::<ChoicesResult>());
    let set_model = reference(generator.subschema_for::<SetModelParams>());
    let set_effort = reference(generator.subschema_for::<SetEffortParams>());
    let set_mode = reference(generator.subschema_for::<SetModeParams>());
    let mutation = reference(generator.subschema_for::<crate::MutationResult>());
    let compact = reference(generator.subschema_for::<CompactParams>());
    let compacted = reference(generator.subschema_for::<crate::CompactionReport>());
    let stats = reference(generator.subschema_for::<StatsResult>());
    let shell = reference(generator.subschema_for::<ShellParams>());
    let shell_result = reference(generator.subschema_for::<crate::ShellResult>());
    let ttfb = reference(generator.subschema_for::<TtfbParams>());
    let ttfb_result = reference(generator.subschema_for::<TtfbResult>());
    let skill = reference(generator.subschema_for::<crate::session_services::SkillParams>());
    let workflow = reference(generator.subschema_for::<crate::session_services::WorkflowParams>());
    let workflow_result =
        reference(generator.subschema_for::<crate::session_services::WorkflowResult>());
    let snapshot = reference(generator.subschema_for::<SessionSnapshot>());
    let admin = {
        use crate::admin::*;
        use crate::host::*;
        let r = |schema| reference(schema);
        vec![
            (
                methods::RUNTIME_PROVIDERS,
                r(generator.subschema_for::<ProvidersParams>()),
                r(generator.subschema_for::<ProvidersResult>()),
            ),
            (
                methods::RUNTIME_TRACING,
                r(generator.subschema_for::<TracingParams>()),
                r(generator.subschema_for::<TracingResult>()),
            ),
            (
                methods::RUNTIME_MCP_AUTH,
                r(generator.subschema_for::<McpAuthParams>()),
                r(generator.subschema_for::<McpAuthResult>()),
            ),
            (
                methods::WORKSPACE_SETTINGS,
                r(generator.subschema_for::<SettingsParams>()),
                r(generator.subschema_for::<SettingsResult>()),
            ),
            (
                methods::WORKSPACE_MCP,
                r(generator.subschema_for::<McpParams>()),
                r(generator.subschema_for::<McpResult>()),
            ),
            (
                methods::WORKSPACE_WEB_SEARCH,
                r(generator.subschema_for::<WebSearchParams>()),
                r(generator.subschema_for::<WebSearchResult>()),
            ),
            (
                methods::WORKSPACE_SKILLS,
                r(generator.subschema_for::<SkillsParams>()),
                r(generator.subschema_for::<SkillsResult>()),
            ),
            (
                methods::WORKSPACE_TEMPLATES,
                r(generator.subschema_for::<TemplatesParams>()),
                r(generator.subschema_for::<TemplatesResult>()),
            ),
            (
                methods::WORKSPACE_RELOAD,
                r(generator.subschema_for::<ReloadParams>()),
                r(generator.subschema_for::<ReloadResult>()),
            ),
            (
                methods::WORKSPACE_TRUST,
                r(generator.subschema_for::<TrustParams>()),
                r(generator.subschema_for::<TrustResult>()),
            ),
            (
                methods::SESSION_SANDBOX,
                r(generator.subschema_for::<SandboxParams>()),
                r(generator.subschema_for::<SandboxResult>()),
            ),
            (
                methods::SESSION_PERMISSION_MODE,
                r(generator.subschema_for::<PermissionModeParams>()),
                r(generator.subschema_for::<PermissionModeResult>()),
            ),
            (
                methods::RUNTIME_USAGE,
                r(generator.subschema_for::<UsageParams>()),
                r(generator.subschema_for::<UsageResult>()),
            ),
            (
                methods::RUNTIME_DEFAULTS,
                r(generator.subschema_for::<DefaultsParams>()),
                r(generator.subschema_for::<DefaultsResult>()),
            ),
            (
                methods::RUNTIME_SESSIONS,
                serde_json::json!({"type": "object"}),
                r(generator.subschema_for::<RuntimeSessionsResult>()),
            ),
        ]
    };
    let empty = serde_json::json!({"type": "object"});
    let method = |direction: &str, kind: &str, params: &Value, result: Option<&Value>| {
        let mut method = serde_json::json!({
            "direction": direction,
            "kind": kind,
            "params": params,
        });
        if let Some(result) = result {
            method["result"] = result.clone();
        }
        method
    };
    let up = "client_to_server";
    let down = "server_to_client";
    let mut schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "loop-protocol",
        "description": "Loop's wire format: JSON-RPC 2.0 methods and the event model. See docs/design/protocol-wire.md.",
        "protocol_version": PROTOCOL_VERSION,
        "limits": {"session_transfer_bytes":SESSION_TRANSFER_LIMIT,"encoded_response_bytes":SESSION_RESPONSE_LIMIT,"page_default":100,"page_max":1000,"shell_output_bytes":crate::SHELL_OUTPUT_LIMIT},
        "methods": {
            methods::INITIALIZE: method(up, "request", &initialize_params, Some(&initialize_result)),
            methods::SESSION_NEW: method(up, "request", &spec, Some(&session)),
            methods::SESSION_TREE: method(up, "request", &tree_params, Some(&tree_result)),
            methods::SESSION_VIEW: method(up, "request", &view_params, Some(&view_result)),
            methods::SESSION_NAVIGATE: method(up, "request", &navigate_params, Some(&navigate_result)),
            methods::SESSION_RENAME: method(up, "request", &rename_params, Some(&mutation)),
            methods::SESSION_FORK: method(up, "request", &fork_params, Some(&destination)),
            methods::SESSION_CLONE: method(up, "request", &clone_params, Some(&destination)),
            methods::SESSION_EXPORT: method(up, "request", &session_params, Some(&export)),
            methods::SESSION_IMPORT: method(up, "request", &import, Some(&destination)),
            methods::SESSION_LIST: method(up, "request", &empty, Some(&session_list)),
            methods::SESSION_CLOSE: method(up, "request", &close, Some(&empty)),
            methods::SESSION_ATTACH: method(up, "request", &attach, Some(&attached)),
            methods::SESSION_DETACH: method(up, "request", &session_params, Some(&empty)),
            methods::SESSION_LOAD: method(up, "request", &load, Some(&session)),
            methods::SESSION_TRANSCRIPT: method(up, "request", &session_params, Some(&transcript)),
            methods::SESSION_PROMPT: method(up, "request", &prompt_params, Some(&prompt_result)),
            methods::SESSION_CANCEL: method(up, "notification", &session_params, None),
            methods::SESSION_UPDATE: method(down, "notification", &envelope, None),
            methods::HOST_INTERACT: method(down, "request", &interaction, Some(&interaction_outcome)),
            methods::HOST_INTERACT_WITHDRAWN: method(down, "notification", &withdrawn, None),
            methods::SHUTDOWN: method(up, "request", &empty, Some(&empty)),
            methods::RUNTIME_STATUS: method(up, "request", &empty, Some(&status)),
            methods::RUNTIME_STOP: method(up, "request", &empty, Some(&empty)),
            methods::RUNTIME_STOP_EMPTY: method(up, "request", &empty, Some(&empty)),
        },
        "error": error,
        "$defs": generator.definitions(),
    });
    // Added separately: one `json!` this large exceeds the macro recursion limit.
    let s3_methods = [
        (
            methods::SESSION_CONFIG,
            method(up, "request", &session_params, Some(&snapshot)),
        ),
        (
            methods::SESSION_SKILL,
            method(up, "request", &skill, Some(&mutation)),
        ),
        (
            methods::SESSION_WORKFLOW,
            method(up, "request", &workflow, Some(&workflow_result)),
        ),
        (
            methods::SESSION_MODELS,
            method(up, "request", &session_params, Some(&models)),
        ),
        (
            methods::SESSION_SET_MODEL,
            method(up, "request", &set_model, Some(&mutation)),
        ),
        (
            methods::SESSION_EFFORTS,
            method(up, "request", &session_params, Some(&choices)),
        ),
        (
            methods::SESSION_SET_EFFORT,
            method(up, "request", &set_effort, Some(&mutation)),
        ),
        (
            methods::SESSION_MODES,
            method(up, "request", &session_params, Some(&choices)),
        ),
        (
            methods::SESSION_SET_MODE,
            method(up, "request", &set_mode, Some(&mutation)),
        ),
        (
            methods::SESSION_COMPACT,
            method(up, "request", &compact, Some(&compacted)),
        ),
        (
            methods::SESSION_STATS,
            method(up, "request", &session_params, Some(&stats)),
        ),
        (
            methods::SESSION_SHELL,
            method(up, "request", &shell, Some(&shell_result)),
        ),
        (
            methods::SESSION_TTFB,
            method(up, "request", &ttfb, Some(&ttfb_result)),
        ),
    ];
    for (name, entry) in s3_methods {
        schema["methods"][name] = entry;
    }
    for (name, params, result) in admin {
        let mut entry = method(up, "request", &params, Some(&result));
        if methods::HOST_ONLY.contains(&name) {
            entry["host_only"] = Value::Bool(true);
        }
        schema["methods"][name] = entry;
    }
    schema["methods"][methods::RUNTIME_STOP]["host_only"] = Value::Bool(true);
    schema["methods"][methods::RUNTIME_STOP_EMPTY]["host_only"] = Value::Bool(true);
    schema
}

/// Maximum UTF-8 JSONL transfer size, in bytes.
pub const SESSION_TRANSFER_LIMIT: usize = 8 * 1024 * 1024;
/// Maximum encoded transfer response size, in bytes.
pub const SESSION_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

/// Optional picker data added to v1 Session listings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionListSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Opaque host workspace identity.
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Backend kind.
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// First user prompt preview.
    pub first_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Number of user and assistant messages.
    pub message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Selected provider/model id.
    pub selected_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Current lease owner kind, when leased.
    pub owner_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Loaded, loadable or owned_elsewhere.
    pub availability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Source Session for a fork or clone.
    pub fork_source: Option<SessionRef>,
}

/// A page of tree entries; cursors are offsets within the captured tree revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TreeParams {
    /// Session reference.
    pub session: SessionRef,
    #[serde(default)]
    /// Opaque continuation cursor.
    pub cursor: Option<String>,
    #[serde(default)]
    /// Page size: defaults to 100, at most 1000.
    pub limit: Option<u32>,
}
/// An immutable transcript page request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ViewParams {
    /// Session reference.
    pub session: SessionRef,
    /// Immutable transcript view id; expires explicitly.
    pub view_id: String,
    #[serde(default)]
    /// Opaque continuation cursor.
    pub cursor: Option<String>,
    #[serde(default)]
    /// Page size: defaults to 100, at most 1000.
    pub limit: Option<u32>,
}
/// Tree rows and fork points use the native tree's versioned JSON entry format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TreeResult {
    /// Tree rows in display order.
    pub entries: Vec<TreeEntry>,
    /// User-message fork points.
    pub fork_points: Vec<ForkPoint>,
    /// Related Session references.
    pub family: Vec<SessionRef>,
    /// Committed transcript revision.
    pub branch_revision: u64,
    /// Continuation cursor, absent at the end.
    pub next_cursor: Option<String>,
}
/// A page from an immutable transcript view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ViewResult {
    /// Transcript projection for this page.
    pub updates: Vec<crate::SessionUpdate>,
    /// Continuation cursor, absent at the end.
    pub next_cursor: Option<String>,
    /// Committed transcript revision.
    pub branch_revision: u64,
    /// Event Log high-water mark at capture.
    pub at_seq: u64,
}
/// `session/compact` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CompactParams {
    /// Session reference.
    pub session: SessionRef,
    /// Guidance for the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}
/// `session/stats` result. Unknown values are omitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StatsResult {
    /// All tokens billed in the Session.
    pub total_tokens: u64,
    /// Tokens in the current context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The Session model's context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Multi-line report for display.
    pub report: String,
    /// Idle, busy or parked.
    pub turn_state: String,
    /// Usage of the latest model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::Usage>,
    /// When the server read these values, in Unix milliseconds.
    pub taken_at_ms: u64,
}
impl From<StatsResult> for crate::StatsSummary {
    fn from(r: StatsResult) -> Self {
        Self {
            total_tokens: r.total_tokens,
            context_tokens: r.context_tokens,
            context_window: r.context_window,
            report: r.report,
            turn_state: Some(r.turn_state),
            usage: r.usage,
        }
    }
}
/// `session/shell` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ShellParams {
    /// Session reference.
    pub session: SessionRef,
    /// The command line, run by the Session's shell.
    pub command: String,
    /// At most this long; the default and the maximum are the host's
    /// `bash.maxTimeoutMs` (600 s unless configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}
/// `session/ttfb` params. Without `timeout_ms` or `clear_override` it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TtfbParams {
    /// Session reference.
    pub session: SessionRef,
    /// New override in ms; 0 waits indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Remove the override and inherit the default. Not with `timeout_ms`.
    #[serde(default)]
    pub clear_override: bool,
    /// Fail with a conflict unless the Session is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}
impl TtfbParams {
    /// The requested change: `None` reads, `Some(None)` clears.
    pub fn change(&self) -> Result<Option<Option<u64>>, crate::BackendError> {
        match (self.timeout_ms, self.clear_override) {
            (Some(_), true) => Err(crate::BackendError::failed(
                crate::ErrorCategory::Protocol,
                "give either timeout_ms or clear_override, not both",
            )),
            (Some(ms), false) => Ok(Some(Some(ms))),
            (None, true) => Ok(Some(None)),
            (None, false) => Ok(None),
        }
    }
}
/// `session/ttfb` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TtfbResult {
    /// The Session's override, when it has one; 0 waits indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_ms: Option<u64>,
    /// The timeout the next turn uses; 0 waits indefinitely.
    pub effective_ms: u64,
    /// The Session revision.
    pub revision: u64,
    /// The update sequence that announced a change; absent for a read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// The change waits for the running turn to end.
    #[serde(default)]
    pub pending: bool,
}
/// Move the active branch. None means before its first entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigateParams {
    /// Session reference.
    pub session: SessionRef,
    #[serde(default)]
    /// Destination entry, or None for the root.
    pub target_entry_id: Option<String>,
    #[serde(default)]
    /// Add a summary of the previous branch.
    pub summarize: bool,
    #[serde(default)]
    /// Fail with a conflict unless the Session is at this revision.
    pub expected_revision: Option<u64>,
}
/// Ordered branch mutation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigateResult {
    /// The Session revision after the branch change.
    pub revision: u64,
    /// Committed transcript revision.
    pub branch_revision: u64,
    /// Committed update sequence.
    pub seq: u64,
}
/// Change only the Session display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RenameParams {
    /// Session reference.
    pub session: SessionRef,
    /// Session display name.
    pub name: String,
    #[serde(default)]
    /// Fail with a conflict unless the Session is at this revision.
    pub expected_revision: Option<u64>,
}
pub use crate::MutationResult;
/// Create a new Session before a user message on the source branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForkParams {
    /// Session reference.
    pub session: SessionRef,
    /// User message excluded from the destination branch.
    pub before_entry_id: String,
    #[serde(default)]
    /// Session display name.
    pub name: Option<String>,
    /// Stable retry id; reuse only for identical parameters.
    pub request_id: String,
}
/// Copy a whole committed active branch into a new Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CloneParams {
    /// Session reference.
    pub session: SessionRef,
    #[serde(default)]
    /// Session display name.
    pub name: Option<String>,
    /// Stable retry id; reuse only for identical parameters.
    pub request_id: String,
}
/// A newly admitted destination; source subscriptions are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DestinationResult {
    /// Session reference.
    pub session: SessionRef,
    /// Opaque host workspace identity.
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Source revision captured by a copy.
    pub source_branch_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Excluded user message to restore in the initiating composer.
    pub draft: Option<String>,
}
/// Bounded payloads written to files by the Surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportResult {
    /// Session JSONL format version.
    pub format_version: u32,
    /// Readable branch transcript.
    pub markdown: String,
    /// UTF-8 versioned Session payload.
    pub jsonl: String,
    /// Committed transcript revision.
    pub branch_revision: u64,
}
/// Import bytes into a known host workspace, never a supplied host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImportParams {
    /// Opaque host workspace identity.
    pub workspace_id: String,
    /// UTF-8 versioned Session payload.
    pub jsonl: String,
    /// Stable retry id; reuse only for identical parameters.
    pub request_id: String,
}

/// One tree row, including its branch membership and editable user text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TreeEntry {
    /// Persisted entry id.
    pub entry_id: String,
    /// Parent entry, or the root.
    pub parent_id: Option<String>,
    /// Indentation level.
    pub depth: usize,
    /// User, assistant or summary.
    pub kind: String,
    /// Short display text.
    pub preview: String,
    /// Full editable prompt, for user rows.
    pub text: Option<String>,
    /// Whether this row is on the active branch.
    pub on_branch: bool,
    /// Whether this is the current position.
    pub current: bool,
}
/// A user-message fork point returned with its tree page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForkPoint {
    /// Persisted user-message entry id.
    pub entry_id: String,
    /// One-based user-message index.
    pub index: usize,
    /// Full draft text.
    pub text: String,
    /// Short display text.
    pub preview: String,
}
