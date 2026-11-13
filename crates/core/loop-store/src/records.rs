//! Payloads of the log record kinds that ADR-0007 §1.2 adds.
//!
//! `policy_decision` is live: the Runtime writes one for every Policy gate
//! decision ([`PolicyDecisionRecord`], [`crate::EventKind::PolicyDecision`]).
//! `tree` and `session_deleted` are defined for later steps: nothing writes
//! them yet, and a stored row of one of them reads as `EventKind::Unknown`
//! (B5 adds their variants).

use loop_protocol::ToolKind;
use serde::{Deserialize, Serialize};

use crate::identity::{Principal, Tenant, Via};
use crate::tree::SessionTreeEntry;

/// Stored `kind` of a policy decision record.
pub const POLICY_DECISION: &str = "policy_decision";
/// Stored `kind` of a tree record.
pub const TREE: &str = "tree";
/// Stored `kind` of a Session tombstone.
pub const SESSION_DELETED: &str = "session_deleted";

/// `tree`: one Session tree entry, exactly as the Session store keeps it.
pub type TreeRecord = SessionTreeEntry;

/// The largest [`DecisionSubject`] a record keeps, in bytes of JSON
/// (S4 §12).
pub const MAX_SUBJECT_BYTES: usize = 4096;

/// `policy_decision`: one Policy gate decision (S4 §12, ADR-0009). Every
/// path writes one record for each call it decides: the agent loop, a
/// Subagent, a workflow task, an ACP agent's permission request, `loop mcp
/// serve` and a user command (`session/shell`).
///
/// It never holds secrets or file contents: the arguments are kept only as
/// a hash ([`Self::args_sha256`]), and [`Self::subject`] keeps a short,
/// stable summary of the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionRecord {
    /// A unique id, `pd-…`.
    pub id: String,
    /// When the gate decided, in Unix milliseconds.
    pub ts_ms: i64,
    /// The tenant the decision belongs to.
    pub tenant: Tenant,
    /// Who acted: the caller that sent the prompt or the command. A Subagent,
    /// a workflow task and an ACP agent act for the person who started the
    /// parent Session. A decision with no caller (an in-process call, work
    /// the Runtime started itself) has the Runtime's own principal.
    pub principal: Principal,
    /// How the principal reached the Runtime (`token`, `edge`), when not
    /// host-local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// The Session, when the decision is for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The turn, when a turn is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// The tool call id, when the call has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Where the call came from.
    pub source: DecisionSource,
    /// The tool name (`bash`, `write`, `mcp__github__create_issue`), or the
    /// agent's own name for an ACP call.
    pub tool: String,
    /// The tool's kind.
    pub kind: ToolKind,
    /// The Capability the call needs (`shell`, `write_files`, `mcp:github`).
    pub capability: String,
    /// A short, stable summary of the call, at most [`MAX_SUBJECT_BYTES`].
    #[serde(default, skip_serializing_if = "DecisionSubject::is_empty")]
    pub subject: DecisionSubject,
    /// SHA-256 of the call's arguments as JSON, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_sha256: Option<String>,
    /// What the gate decided.
    pub outcome: DecisionOutcome,
    /// For `outcome: ask`, the answer, when it came before the call ran.
    /// `None` for a file review after the write: the answer is in the
    /// `escalation_answered` record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<DecisionAnswer>,
    /// The step of the check order that decided.
    pub by: DecidedBy,
    /// The Policy layer that decided, when a layer did (`runtime`, `user`,
    /// `session`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// The `toolPermissions` rule that decided, when a rule did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// The permission mode in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The Escalation that asked, when the approver asked through one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<String>,
    /// Who answered the Escalation. It can differ from [`Self::principal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<Principal>,
    /// The refusal reason given to the model or the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PolicyDecisionRecord {
    /// Whether the call may run.
    pub fn allowed(&self) -> bool {
        match self.outcome {
            DecisionOutcome::Allow => true,
            DecisionOutcome::Deny => false,
            DecisionOutcome::Ask => self.answer != Some(DecisionAnswer::Deny),
        }
    }
}

/// Where a decided call came from (S4 §1). Stored as a string: `native`,
/// `mcp:<server>`, `agent:<id>`, `workflow`, `mcp_serve` or `user_shell`.
/// A value this reader does not know is kept as it was.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum DecisionSource {
    /// The model of a native Session, a Subagent included.
    Native,
    /// A tool of an MCP server, called by the model.
    Mcp(String),
    /// An ACP agent's permission request; the agent id.
    Agent(String),
    /// A `/workflow` shell task.
    Workflow,
    /// A client of `loop mcp serve`.
    McpServe,
    /// A command the user typed (`session/shell`, `!command`).
    UserShell,
    /// A source a newer writer knows, as it was stored.
    Other(String),
}

impl std::fmt::Display for DecisionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native => f.write_str("native"),
            Self::Mcp(server) => write!(f, "mcp:{server}"),
            Self::Agent(id) => write!(f, "agent:{id}"),
            Self::Workflow => f.write_str("workflow"),
            Self::McpServe => f.write_str("mcp_serve"),
            Self::UserShell => f.write_str("user_shell"),
            Self::Other(text) => f.write_str(text),
        }
    }
}

impl From<DecisionSource> for String {
    fn from(source: DecisionSource) -> Self {
        source.to_string()
    }
}

impl From<String> for DecisionSource {
    fn from(text: String) -> Self {
        match text.as_str() {
            "native" => Self::Native,
            "workflow" => Self::Workflow,
            "mcp_serve" => Self::McpServe,
            "user_shell" => Self::UserShell,
            _ => match text.split_once(':') {
                Some(("mcp", server)) => Self::Mcp(server.to_owned()),
                Some(("agent", id)) => Self::Agent(id.to_owned()),
                _ => Self::Other(text),
            },
        }
    }
}

/// What the gate decided (S4 §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    /// The call runs without asking.
    Allow,
    /// The call is refused.
    Deny,
    /// The call went to the approver ([`PolicyDecisionRecord::answer`]).
    Ask,
}

/// The approver's answer to an "ask".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAnswer {
    /// Approved.
    Allow,
    /// Rejected or cancelled.
    Deny,
}

/// The step of the check order that decided (S4 §1, §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    /// A Capability or tool that a Policy layer, the tool lock or the mode
    /// turns off.
    Off,
    /// A protected path (S4 §5).
    Protected,
    /// A `toolPermissions` rule.
    Rule,
    /// A command that could not be checked under a pattern (S4 §7).
    Unreadable,
    /// A Policy layer's "ask".
    LayerAsk,
    /// A `tool_call` hook.
    Hook,
    /// A Session grant ("accept all").
    Grant,
    /// The permission mode's preset.
    Preset,
    /// The person the approver asked.
    Approver,
    /// Nobody could be asked, so the call was refused.
    NoApprover,
    /// The user typed the command, so the user approved it.
    User,
}

/// A short, stable summary of a call: never secrets or file contents, and
/// at most [`MAX_SUBJECT_BYTES`] of JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionSubject {
    /// The program of each part of a command (`git`, `cargo`), without
    /// arguments, which can hold secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
    /// The file path the call names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The host of a URL, without its path or query, which can hold tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl DecisionSubject {
    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty() && self.path.is_none() && self.host.is_none()
    }

    /// This subject cut to at most [`MAX_SUBJECT_BYTES`] of JSON: programs
    /// are dropped from the end, then a long path or host is cut.
    pub fn capped(mut self) -> Self {
        let size = |s: &Self| serde_json::to_vec(s).map_or(usize::MAX, |v| v.len());
        while size(&self) > MAX_SUBJECT_BYTES && !self.programs.is_empty() {
            self.programs.pop();
        }
        for text in [&mut self.path, &mut self.host].into_iter().flatten() {
            if text.len() > MAX_SUBJECT_BYTES / 4 {
                let mut end = MAX_SUBJECT_BYTES / 4;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
        }
        self
    }
}

/// `session_deleted`: the tombstone that stays after a delete (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeletedRecord {
    /// The deleted Session id.
    pub id: String,
    /// Who deleted it.
    pub by: Principal,
}

/// How a Session came to exist. A new field of `session_opened` (§1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// A new, empty Session.
    Create,
    /// A fork: copies of the selected entries of `source`.
    Fork,
    /// A clone: copies of the whole active branch of `source`.
    Clone,
    /// A `loop_session` JSONL import.
    Import,
    /// A Subagent child Session.
    Subagent,
    /// The one-time import of a Session tree written before ADR-0007 (§6.2).
    LegacyImport,
}

/// The fields that `session_opened` gains (§1.2). The present payload keeps
/// `cwd`, `name`, `parent` and `created_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpenedOrigin {
    /// How the Session came to exist.
    pub origin: SessionOrigin,
    /// The source Session of a fork or clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The last copied entry of the source, for a fork or clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_entry: Option<String>,
}
