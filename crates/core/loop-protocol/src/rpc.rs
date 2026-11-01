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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachResult {
    /// Last update sequence in the replay snapshot, not an audit cursor.
    pub latest_seq: u64,
    /// Runtime generation of the snapshot.
    pub generation: u64,
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
            BackendError::Failed(info) => (error_codes::FAILED, serde_json::to_value(info).ok()),
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
            error_codes::FAILED => BackendError::Failed(
                serde_json::from_value(data)
                    .unwrap_or_else(|_| ErrorInfo::new(ErrorCategory::Agent, self.message)),
            ),
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
    let spec = reference(generator.subschema_for::<SessionSpec>());
    let session = reference(generator.subschema_for::<SessionRef>());
    let load = reference(generator.subschema_for::<LoadParams>());
    let session_params = reference(generator.subschema_for::<SessionParams>());
    let session_list = reference(generator.subschema_for::<Vec<SessionInfo>>());
    let close = reference(generator.subschema_for::<CloseParams>());
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
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "loop-protocol",
        "description": "Loop's wire format: JSON-RPC 2.0 methods and the event model. See docs/design/protocol-wire.md.",
        "protocol_version": PROTOCOL_VERSION,
        "methods": {
            methods::INITIALIZE: method(up, "request", &initialize_params, Some(&initialize_result)),
            methods::SESSION_NEW: method(up, "request", &spec, Some(&session)),
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
        },
        "error": error,
        "$defs": generator.definitions(),
    })
}
