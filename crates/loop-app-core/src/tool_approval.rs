//! Interactive tool approval (file edits + bash) with session auto-approve.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use loop_agent::harness::ExecutionEnv;
use loop_agent::{
    AfterToolCallContext, AfterToolCallResult, BeforeToolCallContext, BeforeToolCallResult,
};
use loop_ai::{TextContent, ToolResultContent};
use loop_protocol::HostServices;
use parking_lot::Mutex as SyncMutex;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::interaction::{native_decision, native_permission_request};

/// Label prefix persisted on the session tree for auto-approve.
pub const AUTO_APPROVE_LABEL_PREFIX: &str = "loop.toolApproval:auto:";

/// File-edit tools share one session auto-approve bucket.
pub const GROUP_FILE: &str = "file";
/// Bash tool group.
pub const GROUP_BASH: &str = "bash";
/// Web tool group (`web_fetch` + `web_search`).
pub const GROUP_WEB: &str = "web";

/// The old `fileEditReview` setting. The permission mode now decides when
/// to ask; only `never` still matters, as the starting mode
/// ([`crate::runtime::startup_permission_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// `newSession` (the default).
    NewSession,
    /// `always`.
    Always,
    /// `never`: start in `accept-edits` when no `permissionMode` is set.
    Never,
}

impl ApprovalPolicy {
    /// Parse settings value.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "on" | "true" => Self::Always,
            "never" | "off" | "false" => Self::Never,
            _ => Self::NewSession,
        }
    }

    /// Settings / status label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewSession => "newSession",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

pub use crate::permissions::ToolPermission;

/// Default per-tool permissions (others can be added in settings).
pub fn default_tool_permissions() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("write".into(), "ask".into()),
        ("edit".into(), "ask".into()),
        ("bash".into(), "ask".into()),
        ("read".into(), "allow".into()),
        ("grep".into(), "allow".into()),
        ("glob".into(), "allow".into()),
        ("ls".into(), "allow".into()),
        ("web_fetch".into(), "allow".into()),
        ("web_search".into(), "allow".into()),
    ])
}

/// Fill in permissions added by Loop after a settings file was created.
pub fn merge_default_tool_permissions(map: &mut BTreeMap<String, String>) {
    for (tool, permission) in default_tool_permissions() {
        map.entry(tool).or_insert(permission);
    }
}

/// Tools that currently have an interactive approval workflow.
pub fn interactive_tools() -> &'static [&'static str] {
    &[
        "write",
        "edit",
        "bash",
        "web_fetch",
        "web_search",
        loop_agent::harness::EXIT_PLAN_TOOL,
    ]
}

/// Map a tool name to its session auto-approve group.
pub fn approval_group(tool: &str) -> Option<&'static str> {
    match tool {
        "write" | "edit" => Some(GROUP_FILE),
        "bash" => Some(GROUP_BASH),
        "web_fetch" | "web_search" => Some(GROUP_WEB),
        _ => None,
    }
}

/// User decision for a pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Accept this one call.
    Accept,
    /// Accept this and all further calls in the same group for this session.
    AcceptSession,
    /// Reject; optional reason guides the model.
    Reject { reason: Option<String> },
}

/// Kind of approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// After write/edit — show diff, can revert.
    FileEdit,
    /// Before bash — approve the command.
    Bash,
    /// Before web_fetch / web_search — approve the outbound request.
    Web,
    /// `exit_plan` — approve the plan and leave plan mode.
    Plan,
}

impl ApprovalKind {
    /// Short label for the picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::FileEdit => "file edit",
            Self::Bash => "bash",
            Self::Web => "web",
            Self::Plan => "plan",
        }
    }

    /// Accept-all option text.
    pub fn accept_all_label(self) -> &'static str {
        match self {
            Self::FileEdit => "Accept all file changes for this session",
            Self::Bash => "Accept all bash for this session",
            Self::Web => "Accept all web requests for this session",
            Self::Plan => "Approve and accept edits from now on",
        }
    }

    /// Accept-once option text.
    pub fn accept_label(self) -> &'static str {
        match self {
            Self::Plan => "Approve the plan",
            _ => "Accept",
        }
    }

    /// Reject option text.
    pub fn reject_label(self) -> &'static str {
        match self {
            Self::Plan => "Keep planning",
            _ => "Reject",
        }
    }

    /// Session auto-approve group.
    pub fn group(self) -> &'static str {
        match self {
            Self::FileEdit => GROUP_FILE,
            Self::Bash => GROUP_BASH,
            Self::Web => GROUP_WEB,
            // Never granted: approving a plan is not remembered.
            Self::Plan => "plan",
        }
    }
}

/// How the prompt step before a call went, for the decision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Review {
    /// Not asked now: a file tool, whose review comes after the write.
    Later,
    /// Not asked: a Session grant ("accept all") covers it.
    Granted,
    /// The user was asked, through the Escalation `escalation` when the
    /// approver is a Runtime host.
    Asked { escalation: Option<String> },
}

/// The user's answer to a plan (`exit_plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReview {
    /// Approved; with `accept_edits`, also switch to `accept-edits`.
    Approved {
        /// The user chose "Approve and accept edits".
        accept_edits: bool,
    },
    /// The user wants changes; the reason, if they gave one.
    KeepPlanning(Option<String>),
}

/// Prompt shown in the TUI while the agent waits.
#[derive(Debug)]
pub struct ApprovalPrompt {
    /// File edit vs bash (extensible).
    pub kind: ApprovalKind,
    /// Tool name.
    pub tool_name: String,
    /// Primary display line (path or command summary).
    pub summary: String,
    /// Optional detail (full command, absolute path).
    pub detail: String,
    /// Reply channel.
    pub response_tx: oneshot::Sender<ApprovalDecision>,
}

/// Shared bridge between agent hooks and the TUI.
#[derive(Clone)]
pub struct ToolApprovalBridge {
    /// Session auto-approve groups (`file`, `bash`, …).
    auto_approve: Arc<SyncMutex<HashSet<String>>>,
    prompts: PromptSink,
    env: Arc<dyn ExecutionEnv>,
    diff_editor: Arc<Mutex<Option<String>>>,
    /// When false, skip spawning an external `--diff` editor (desktop reviews in-app).
    open_external_diff: Arc<AtomicBool>,
    gate: Arc<Mutex<()>>,
    /// Persist auto-approve labels onto the session (optional).
    persist: Arc<SyncMutex<Option<PersistAutoApprove>>>,
}

/// Where approval prompts go.
#[derive(Clone)]
enum PromptSink {
    /// Raw prompts for a surface that renders [`ApprovalPrompt`] itself.
    Prompts(mpsc::UnboundedSender<ApprovalPrompt>),
    /// Interaction requests through Loop's [`HostServices`], shared with
    /// every other backend.
    Host(Arc<dyn HostServices>),
}

/// Callback used to append a session label (and optionally notify).
pub type PersistAutoApprove = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

impl ToolApprovalBridge {
    /// Create a bridge; `prompt_rx` is drained by the TUI.
    pub fn new(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        session_auto_approve: HashSet<String>,
    ) -> (Self, mpsc::UnboundedReceiver<ApprovalPrompt>) {
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        (
            Self::with_sink(
                env,
                diff_editor,
                session_auto_approve,
                PromptSink::Prompts(prompt_tx),
            ),
            prompt_rx,
        )
    }

    /// Create a bridge that asks through `host` as
    /// [`InteractionRequest::Permission`](loop_protocol::InteractionRequest)s
    /// with the options in [`crate::interaction::native_options`].
    pub fn with_host(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        session_auto_approve: HashSet<String>,
        host: Arc<dyn HostServices>,
    ) -> Self {
        Self::with_sink(
            env,
            diff_editor,
            session_auto_approve,
            PromptSink::Host(host),
        )
    }

    fn with_sink(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        session_auto_approve: HashSet<String>,
        prompts: PromptSink,
    ) -> Self {
        Self {
            auto_approve: Arc::new(SyncMutex::new(session_auto_approve)),
            prompts,
            env,
            diff_editor: Arc::new(Mutex::new(diff_editor)),
            open_external_diff: Arc::new(AtomicBool::new(true)),
            gate: Arc::new(Mutex::new(())),
            persist: Arc::new(SyncMutex::new(None)),
        }
    }

    /// Replace the Surface transport while preserving the execution environment.
    pub fn set_host(&mut self, host: Arc<dyn HostServices>) {
        self.prompts = PromptSink::Host(host);
    }

    /// Whether file-edit review should spawn an external editor with `--diff`.
    /// Desktop disables this and shows diffs in its own panel instead.
    pub fn set_open_external_diff(&self, open: bool) {
        self.open_external_diff.store(open, Ordering::SeqCst);
    }

    /// Install a persistence callback for Accept-all labels.
    pub fn set_persist(&self, persist: PersistAutoApprove) {
        *self.persist.lock() = Some(persist);
    }

    /// Snapshot of session auto-approve groups.
    pub fn auto_approve_groups(&self) -> HashSet<String> {
        self.auto_approve.lock().clone()
    }

    /// Mark a group as auto-approved for this session (in-memory).
    pub fn grant_session(&self, group: &str) {
        self.auto_approve.lock().insert(group.to_string());
    }

    /// Clear session auto-approve (e.g. `/new`).
    pub fn clear_session_grants(&self) {
        self.auto_approve.lock().clear();
    }

    /// Update preferred diff editor.
    pub async fn set_diff_editor(&self, editor: Option<String>) {
        *self.diff_editor.lock().await = editor;
    }

    /// Whether to prompt for a call the gate routed here as "ask": not when
    /// the session already accepted its group.
    pub fn should_ask(&self, tool: &str) -> bool {
        self.asks(tool, false)
    }

    /// [`Self::should_ask`]; with `ignore_grants`, the session's "accept
    /// all" grants don't count (a Policy layer says to ask).
    fn asks(&self, tool: &str, ignore_grants: bool) -> bool {
        if !interactive_tools().contains(&tool) {
            return false;
        }
        // The tool gate decides allow / ask / deny (modes, rules, Policy)
        // and only hands "ask" calls here.
        ignore_grants
            || approval_group(tool).is_none_or(|group| !self.auto_approve.lock().contains(group))
    }

    /// The prompt step of [`crate::tool_gate::ToolGate`] before a call
    /// (bash, web), and how it went, for the decision record.
    pub(crate) async fn review_before_tool(
        &self,
        ctx: BeforeToolCallContext,
        cancel: Option<CancellationToken>,
        ignore_grants: bool,
    ) -> (Option<BeforeToolCallResult>, Review) {
        let name = ctx.tool_call.name.as_str();
        // Not asked now: a Session grant covers it, or it is a file tool,
        // whose review comes after the write.
        let not_asked = |name: &str| {
            if interactive_tools().contains(&name) && !matches!(name, "write" | "edit") {
                Review::Granted
            } else {
                Review::Later
            }
        };
        if !self.asks(name, ignore_grants) {
            return (None, not_asked(name));
        }
        // Build the (kind, summary, detail, reject subject) tuple for the
        // prompt; only tools with an interactive workflow get here
        // (`should_ask` filters the rest).
        let (kind, summary, detail, subject) = match name {
            "bash" => {
                let command = ctx
                    .args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let cwd = ctx
                    .args
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string();
                let summary = truncate_cmd(&command, 72);
                let detail = if cwd == "." {
                    command.clone()
                } else {
                    format!("cwd: {cwd}\n{command}")
                };
                (
                    ApprovalKind::Bash,
                    summary,
                    detail,
                    "bash command".to_string(),
                )
            }
            "web_fetch" => {
                let url = ctx
                    .args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let summary = truncate_cmd(&url, 72);
                (
                    ApprovalKind::Web,
                    summary,
                    url.clone(),
                    format!("fetch of {url}"),
                )
            }
            "web_search" => {
                let query = ctx
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let summary = truncate_cmd(&query, 72);
                (
                    ApprovalKind::Web,
                    summary,
                    query.clone(),
                    format!("search for '{query}'"),
                )
            }
            _ => return (None, Review::Later),
        };

        let _lock = self.gate.lock().await;
        // Re-check after waiting for the gate (Accept-all may have landed).
        if !self.asks(name, ignore_grants) {
            return (None, Review::Granted);
        }
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            let blocked = BeforeToolCallResult {
                block: true,
                reason: Some("Approval cancelled".into()),
            };
            return (Some(blocked), Review::Asked { escalation: None });
        }

        let (decision, escalation) = self
            .ask_user(kind, name, summary, detail, cancel.as_ref())
            .await;
        let asked = Review::Asked { escalation };

        match decision {
            ApprovalDecision::Accept => (None, asked),
            ApprovalDecision::AcceptSession => {
                self.grant_and_persist(kind.group()).await;
                (None, asked)
            }
            ApprovalDecision::Reject { reason } => {
                let msg = match reason.filter(|r| !r.trim().is_empty()) {
                    Some(r) => format!("User rejected {subject}. Reason: {r}"),
                    None => format!("User rejected {subject}."),
                };
                let blocked = BeforeToolCallResult {
                    block: true,
                    reason: Some(msg),
                };
                (Some(blocked), asked)
            }
        }
    }

    /// Ask the user to approve `plan`; `then` names the mode approving it
    /// returns to.
    pub(crate) async fn review_plan(
        &self,
        plan: &str,
        then: &str,
        cancel: Option<CancellationToken>,
    ) -> (PlanReview, Option<String>) {
        let _lock = self.gate.lock().await;
        let summary = format!("approve to continue in {then} mode");
        let (decision, escalation) = self
            .ask_user(
                ApprovalKind::Plan,
                loop_agent::harness::EXIT_PLAN_TOOL,
                summary,
                plan.to_owned(),
                cancel.as_ref(),
            )
            .await;
        let review = match decision {
            ApprovalDecision::Accept => PlanReview::Approved {
                accept_edits: false,
            },
            ApprovalDecision::AcceptSession => PlanReview::Approved { accept_edits: true },
            ApprovalDecision::Reject { reason } => {
                PlanReview::KeepPlanning(reason.filter(|r| !r.trim().is_empty()))
            }
        };
        (review, escalation)
    }

    /// The review step of [`crate::tool_gate::ToolGate`] after a file edit.
    pub(crate) async fn review_after_file_tool(
        &self,
        ctx: AfterToolCallContext,
        cancel: Option<CancellationToken>,
        ignore_grants: bool,
    ) -> Option<AfterToolCallResult> {
        if ctx.is_error {
            return None;
        }
        let name = ctx.tool_call.name.as_str();
        if name != "write" && name != "edit" {
            return None;
        }
        if !self.asks(name, ignore_grants) {
            // Still clean up review snapshots when not asking.
            if let Some(prev) = ctx
                .result
                .details
                .get("previousPath")
                .and_then(|v| v.as_str())
            {
                cleanup_snapshot(Some(Path::new(prev)));
            }
            return None;
        }

        let path = ctx
            .result
            .details
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)?;
        let previous_path = ctx
            .result
            .details
            .get("previousPath")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let review = FileReview::from_details(&ctx.result.details);

        let _lock = self.gate.lock().await;
        if !self.asks(name, ignore_grants) {
            cleanup_snapshot(previous_path.as_deref());
            return None;
        }
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            let _ = self
                .revert_change(&path, previous_path.as_deref(), &review)
                .await;
            return Some(reject_file_patch(
                "File edit review cancelled",
                &path,
                previous_path.as_deref(),
            ));
        }

        if self.open_external_diff.load(Ordering::SeqCst) {
            if let Some(prev) = &previous_path {
                if let Some(editor) = self.resolve_editor().await {
                    if let Err(e) = open_diff(&editor, prev, &path) {
                        tracing::warn!("failed to open diff editor ({editor}): {e}");
                    }
                } else {
                    tracing::warn!(
                        "no diff editor found (set settings.diffEditor or install cursor/code)"
                    );
                }
            }
        }

        let target = review.target_path.as_deref().filter(|_| review.via_link);
        let (summary, detail) = file_edit_prompt(&path, target);
        let (decision, _escalation) = self
            .ask_user(
                ApprovalKind::FileEdit,
                name,
                summary,
                detail,
                cancel.as_ref(),
            )
            .await;

        match decision {
            ApprovalDecision::Accept => {
                cleanup_snapshot(previous_path.as_deref());
                None
            }
            ApprovalDecision::AcceptSession => {
                cleanup_snapshot(previous_path.as_deref());
                self.grant_and_persist(ApprovalKind::FileEdit.group()).await;
                None
            }
            ApprovalDecision::Reject { reason } => {
                if let Err(e) = self
                    .revert_change(&path, previous_path.as_deref(), &review)
                    .await
                {
                    tracing::warn!("revert after reject failed: {e}");
                }
                cleanup_snapshot(previous_path.as_deref());
                let msg = match reason.filter(|r| !r.trim().is_empty()) {
                    Some(r) => format!("User rejected the edit to {}. Reason: {r}", path.display()),
                    None => format!("User rejected the edit to {}.", path.display()),
                };
                Some(reject_file_patch(&msg, &path, None))
            }
        }
    }

    async fn ask_user(
        &self,
        kind: ApprovalKind,
        tool_name: &str,
        summary: String,
        detail: String,
        cancel: Option<&CancellationToken>,
    ) -> (ApprovalDecision, Option<String>) {
        let mut escalation = None;
        let response_rx = match &self.prompts {
            PromptSink::Prompts(tx) => {
                let (response_tx, response_rx) = oneshot::channel();
                if tx
                    .send(ApprovalPrompt {
                        kind,
                        tool_name: tool_name.to_string(),
                        summary,
                        detail,
                        response_tx,
                    })
                    .is_err()
                {
                    let refused = ApprovalDecision::Reject {
                        reason: Some("approval UI unavailable".into()),
                    };
                    return (refused, None);
                }
                response_rx
            }
            PromptSink::Host(host) => {
                let request = native_permission_request(kind, tool_name, summary, detail);
                escalation = Some(request.id().to_string());
                let host = Arc::clone(host);
                let (response_tx, response_rx) = oneshot::channel();
                tokio::spawn(async move {
                    let _ = response_tx.send(native_decision(host.interact(request).await));
                });
                response_rx
            }
        };

        let decision = tokio::select! {
            biased;
            _ = async {
                if let Some(c) = cancel {
                    c.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => ApprovalDecision::Reject { reason: Some("approval cancelled".into()) },
            res = response_rx => res.unwrap_or(ApprovalDecision::Reject {
                reason: Some("approval UI closed".into()),
            }),
        };
        (decision, escalation)
    }

    async fn grant_and_persist(&self, group: &str) {
        self.grant_session(group);
        let label = format!("{AUTO_APPROVE_LABEL_PREFIX}{group}");
        let persist = self.persist.lock().clone();
        if let Some(persist) = persist {
            persist(label).await;
        }
    }

    /// Undo a reviewed write. A created file is removed only at the path
    /// recorded at write time, only while it is still a regular file with
    /// the identity the write gave it; nothing is resolved now, so a link
    /// retargeted during review cannot redirect the removal. An existing
    /// file is restored from its snapshot at the path the write resolved to.
    pub(crate) async fn revert_change(
        &self,
        path: &Path,
        previous_path: Option<&Path>,
        review: &FileReview,
    ) -> Result<(), String> {
        if review.created {
            let (Some(created), Some(identity)) = (&review.created_path, &review.created_identity)
            else {
                return Err(format!(
                    "no identity recorded for {}; left in place",
                    path.display()
                ));
            };
            return match self.env.file_identity(created).await {
                Ok(Some(now)) if now.matches(identity) => {
                    self.env.remove(created).await.map_err(|e| e.to_string())
                }
                _ => Err(format!(
                    "{} changed during review; left in place",
                    created.display()
                )),
            };
        }
        let Some(prev) = previous_path else {
            return Err("missing previous snapshot".into());
        };
        let bytes = tokio::fs::read(prev).await.map_err(|e| e.to_string())?;
        let target = review.target_path.as_deref().unwrap_or(path);
        self.env
            .write_file(target, &bytes)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resolve_editor(&self) -> Option<String> {
        if let Some(custom) = self.diff_editor.lock().await.clone() {
            if !custom.trim().is_empty() {
                return Some(custom);
            }
        }
        detect_diff_editor()
    }
}

/// Parse auto-approve groups from session tree labels.
pub fn auto_approve_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a loop_agent::harness::SessionTreeEntry>,
) -> HashSet<String> {
    use loop_agent::harness::SessionTreeEntry;
    let mut out = HashSet::new();
    for e in entries {
        if let SessionTreeEntry::Label { label, .. } = e {
            if let Some(group) = label.strip_prefix(AUTO_APPROVE_LABEL_PREFIX) {
                if !group.is_empty() {
                    out.insert(group.to_string());
                }
            }
        }
    }
    out
}

/// Build permission map from settings JSON strings.
pub fn permissions_from_settings(
    map: &BTreeMap<String, String>,
) -> BTreeMap<String, ToolPermission> {
    let mut merged = map.clone();
    merge_default_tool_permissions(&mut merged);
    let mut out = BTreeMap::new();
    for (k, v) in merged {
        out.insert(k, ToolPermission::parse(&v));
    }
    out
}

/// The title and detail of a file-edit prompt for `path`. A write through
/// a symlink names its real `target` first, because the TUI shows only the
/// title, on one line: `writes to "/real/key" via link "notes"`. The
/// detail gives the target and the link on two lines. Names a model or a
/// repository chose are quoted with control and bidi characters escaped, so a
/// newline, an escape sequence or a fake ` → ` in a name cannot hide the
/// target; a long name is shortened in the middle in the title.
pub(crate) fn file_edit_prompt(path: &Path, target: Option<&Path>) -> (String, String) {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let full = path.display().to_string();
    match target {
        Some(target) => {
            let target = target.display().to_string();
            (
                format!(
                    "writes to {} via link {}",
                    quoted(&shorten(&target, 80)),
                    quoted(&shorten(&name, 40))
                ),
                format!(
                    "writes to {}\nthrough link {}",
                    quoted(&target),
                    quoted(&full)
                ),
            )
        }
        None => (escaped(&name), escaped(&full)),
    }
}

/// `s` as is, or [`quoted`] when it has a character a terminal would not
/// show as itself.
fn escaped(s: &str) -> String {
    if s.chars().any(hidden) {
        quoted(s)
    } else {
        s.to_owned()
    }
}

/// `s` in double quotes, with Rust's escapes (`\n`, `\u{1b}`) and every
/// other [`hidden`] character as `\u{…}`.
fn quoted(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        if hidden(c) && !c.is_control() {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.extend(c.escape_debug());
        }
    }
    out.push('"');
    out
}

/// Control characters, and the format characters that reorder or hide
/// text: bidi controls, zero-width characters, line and paragraph
/// separators, the byte order mark.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// `s` cut to about `max` characters, keeping its start and its end.
fn shorten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_owned();
    }
    let keep = max.saturating_sub(1) / 2;
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{head}…{tail}")
}

/// What a write or edit result recorded for its review.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileReview {
    created: bool,
    target_path: Option<PathBuf>,
    /// Whether a symlink took the write somewhere its path does not show.
    via_link: bool,
    created_path: Option<PathBuf>,
    created_identity: Option<loop_agent::harness::types::FileIdentity>,
}

impl FileReview {
    pub(crate) fn from_details(details: &serde_json::Value) -> Self {
        let path = |key: &str| details.get(key).and_then(|v| v.as_str()).map(PathBuf::from);
        Self {
            created: details
                .get("created")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            target_path: path("targetPath"),
            via_link: details
                .get("viaLink")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            created_path: path("createdPath"),
            created_identity: details
                .get("createdIdentity")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
        }
    }
}

fn reject_file_patch(
    message: &str,
    path: &Path,
    previous_path: Option<&Path>,
) -> AfterToolCallResult {
    cleanup_snapshot(previous_path);
    AfterToolCallResult {
        content: Some(vec![ToolResultContent::Text(TextContent {
            text: message.to_string(),
            text_signature: None,
        })]),
        details: Some(json!({
            "path": path,
            "rejected": true,
        })),
        is_error: Some(true),
        usage: None,
        terminate: None,
    }
}

fn cleanup_snapshot(path: Option<&Path>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

fn truncate_cmd(cmd: &str, max: usize) -> String {
    let one = cmd.replace('\n', " ");
    if one.chars().count() <= max {
        return one;
    }
    let mut out: String = one.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Prefer Cursor, then VS Code-style binaries.
pub fn detect_diff_editor() -> Option<String> {
    for candidate in ["cursor", "code", "code-insiders", "codium"] {
        if which_ok(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn which_ok(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Open a side-by-side diff (`--diff left right`).
pub fn open_diff(editor: &str, left: &Path, right: &Path) -> Result<(), String> {
    Command::new(editor)
        .arg("--diff")
        .arg(left)
        .arg(right)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {editor}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {

    mod file_edit_prompt {
        use super::super::file_edit_prompt;
        use std::path::Path;

        fn linked(name: &str) -> (String, String) {
            let path = Path::new("/ws").join(name);
            file_edit_prompt(&path, Some(Path::new("/outside/key")))
        }

        #[test]
        fn the_target_comes_first_and_names_are_quoted() {
            let (summary, detail) = linked("notes");
            assert_eq!(summary, "writes to \"/outside/key\" via link \"notes\"");
            assert_eq!(
                detail,
                "writes to \"/outside/key\"\nthrough link \"/ws/notes\""
            );
        }

        #[test]
        fn a_long_link_name_is_shortened_in_the_middle() {
            let name = format!("{}END", "a".repeat(200));
            let (summary, detail) = linked(&name);
            assert!(
                summary.starts_with("writes to \"/outside/key\""),
                "{summary}"
            );
            assert!(summary.chars().count() < 100, "{summary}");
            assert!(
                summary.contains('…') && summary.contains("END\""),
                "{summary}"
            );
            assert!(detail.contains(&name), "the detail keeps the full path");
        }

        #[test]
        fn a_name_cannot_fake_the_target() {
            // A file name cannot hold `/`; the directories of the path can.
            let (summary, detail) = linked("notes → /tmp/notes.txt");
            assert!(
                summary.starts_with("writes to \"/outside/key\""),
                "{summary}"
            );
            assert!(
                detail.starts_with("writes to \"/outside/key\"\n"),
                "{detail}"
            );
            assert!(
                detail.ends_with("through link \"/ws/notes → /tmp/notes.txt\""),
                "{detail}"
            );
            let (summary, _) = linked("notes → x");
            assert!(summary.ends_with("via link \"notes → x\""), "{summary}");
        }

        #[test]
        fn bidi_and_zero_width_characters_are_escaped() {
            for name in ["a\u{202e}txt.exe", "pla\u{200b}in.txt", "a\u{2028}b"] {
                let path = Path::new("/ws").join(name);
                let (plain, detail) = file_edit_prompt(&path, None);
                assert!(plain.starts_with('"') && plain.is_ascii(), "{plain}");
                assert!(detail.is_ascii(), "{detail}");
                let (summary, detail) = linked(name);
                assert!(
                    summary.is_ascii() && detail.is_ascii(),
                    "{summary} {detail}"
                );
            }
        }

        #[test]
        fn control_characters_are_escaped() {
            let (summary, detail) = linked("notes\n\u{1b}[2Kok");
            assert!(
                !summary.contains('\n') && !summary.contains('\u{1b}'),
                "{summary}"
            );
            assert!(
                summary.contains("\\n") && summary.contains("\\u{1b}"),
                "{summary}"
            );
            assert_eq!(detail.matches('\n').count(), 1, "{detail}");
            let (plain, plain_detail) = file_edit_prompt(Path::new("/ws/a\nb.txt"), None);
            assert_eq!(plain, "\"a\\nb.txt\"");
            assert!(!plain_detail.contains('\n'), "{plain_detail}");
            let (plain, plain_detail) = file_edit_prompt(Path::new("/ws/plain.txt"), None);
            assert_eq!(
                (plain.as_str(), plain_detail.as_str()),
                ("plain.txt", "/ws/plain.txt")
            );
        }
    }
    use super::*;

    #[test]
    fn grant_labels_live_in_the_namespace_copies_drop() {
        assert!(AUTO_APPROVE_LABEL_PREFIX
            .starts_with(loop_agent::harness::session::TOOL_APPROVAL_LABEL_NAMESPACE));
    }

    #[test]
    fn policy_and_permissions() {
        assert_eq!(ApprovalPolicy::parse("off"), ApprovalPolicy::Never);
        assert_eq!(ApprovalPolicy::parse("x"), ApprovalPolicy::NewSession);
        assert_eq!(ToolPermission::parse("ASK"), ToolPermission::Ask);
        assert_eq!(approval_group("write"), Some(GROUP_FILE));
        assert_eq!(approval_group("bash"), Some(GROUP_BASH));
    }

    #[test]
    fn legacy_permission_maps_get_new_web_defaults() {
        let permissions =
            permissions_from_settings(&BTreeMap::from([("bash".into(), "allow".into())]));
        assert_eq!(permissions.get("bash"), Some(&ToolPermission::Allow));
        assert_eq!(permissions.get("web_fetch"), Some(&ToolPermission::Allow));
        assert_eq!(permissions.get("web_search"), Some(&ToolPermission::Allow));
        let chosen =
            permissions_from_settings(&BTreeMap::from([("web_fetch".into(), "ask".into())]));
        assert_eq!(chosen.get("web_fetch"), Some(&ToolPermission::Ask));
    }

    /// Run the write tool in `ws`, then undo it as a rejected review does.
    #[cfg(unix)]
    mod revert {
        use super::*;
        use loop_agent::harness::tools::create_write_tool;
        use loop_agent::harness::HostExecutionEnv;
        use std::os::unix::fs::symlink;

        fn bridge(ws: &Path) -> (ToolApprovalBridge, Arc<dyn ExecutionEnv>) {
            let env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(ws.to_path_buf()));
            let (bridge, _rx) = ToolApprovalBridge::new(Arc::clone(&env), None, HashSet::new());
            // Never open a GUI diff editor from a test.
            bridge.set_open_external_diff(false);
            (bridge, env)
        }

        async fn write(env: &Arc<dyn ExecutionEnv>, path: &str) -> serde_json::Value {
            let tool = create_write_tool(Arc::clone(env));
            let args = json!({"path": path, "content": "agent text"});
            (tool.execute)("w".into(), args, None, None)
                .await
                .expect("write")
                .details
        }

        async fn reject(bridge: &ToolApprovalBridge, details: &serde_json::Value) {
            let path = PathBuf::from(details["path"].as_str().unwrap());
            let prev = details["previousPath"].as_str().map(PathBuf::from);
            let _ = bridge
                .revert_change(&path, prev.as_deref(), &FileReview::from_details(details))
                .await;
        }

        #[tokio::test]
        async fn binary_file_is_restored_not_deleted() {
            let ws = tempfile::tempdir().unwrap();
            let file = ws.path().join("data.bin");
            let bytes = vec![0xff, 0xfe, 0x00, 0x80, 0x81];
            std::fs::write(&file, &bytes).unwrap();
            let (bridge, env) = bridge(ws.path());
            let details = write(&env, "data.bin").await;
            assert_eq!(details["created"], false);
            reject(&bridge, &details).await;
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
        }

        #[tokio::test]
        async fn rejected_document_overwrite_restores_the_original_bytes() {
            loop_agent::harness::set_document_reader(
                loop_agent::harness::DocumentReader::InProcess,
            );
            let ws = tempfile::tempdir().unwrap();
            let (bridge, env) = bridge(ws.path());
            let tool = create_write_tool(Arc::clone(&env));
            let args = json!({"path": "report.docx", "content": "# Original\n\n- kept"});
            (tool.execute)("w1".into(), args, None, None).await.unwrap();
            let file = ws.path().join("report.docx");
            let original = std::fs::read(&file).unwrap();
            let args = json!({"path": "report.docx", "content": "# Rewritten"});
            let details = (tool.execute)("w2".into(), args, None, None)
                .await
                .unwrap()
                .details;
            assert_eq!(details["created"], false);
            assert!(details["diff"].as_str().unwrap().contains("rebuilt"));
            assert_ne!(std::fs::read(&file).unwrap(), original);
            reject(&bridge, &details).await;
            assert_eq!(std::fs::read(&file).unwrap(), original);
        }

        #[tokio::test]
        async fn link_to_existing_outside_file_is_restored() {
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("data.bin");
            std::fs::write(&target, [0xffu8, 0x00, 0x01]).unwrap();
            symlink(&target, ws.path().join("link")).unwrap();
            let (bridge, env) = bridge(ws.path());
            let details = write(&env, "link").await;
            reject(&bridge, &details).await;
            assert_eq!(std::fs::read(&target).unwrap(), [0xffu8, 0x00, 0x01]);
            assert!(ws.path().join("link").is_symlink());
        }

        #[tokio::test]
        async fn link_retargeted_during_review_keeps_the_new_target() {
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let link = ws.path().join("link");
            symlink(outside.path().join("new.txt"), &link).unwrap();
            let (bridge, env) = bridge(ws.path());
            let details = write(&env, "link").await;
            assert_eq!(details["created"], true);
            // During review the link is pointed at a precious file.
            let precious = outside.path().join("precious.txt");
            std::fs::write(&precious, "keep me").unwrap();
            std::fs::remove_file(&link).unwrap();
            symlink(&precious, &link).unwrap();
            reject(&bridge, &details).await;
            assert_eq!(std::fs::read_to_string(&precious).unwrap(), "keep me");
        }

        #[tokio::test]
        async fn dangling_link_create_removes_the_created_file_only() {
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("new.txt");
            let link = ws.path().join("link");
            symlink(&target, &link).unwrap();
            let (bridge, env) = bridge(ws.path());
            let details = write(&env, "link").await;
            assert!(target.exists());
            reject(&bridge, &details).await;
            assert!(!target.exists());
            assert!(link.is_symlink());
        }

        #[tokio::test]
        async fn created_file_replaced_by_a_directory_is_kept() {
            let ws = tempfile::tempdir().unwrap();
            let (bridge, env) = bridge(ws.path());
            let details = write(&env, "new.txt").await;
            let file = ws.path().join("new.txt");
            std::fs::remove_file(&file).unwrap();
            std::fs::create_dir(&file).unwrap();
            std::fs::write(file.join("inner"), "keep").unwrap();
            reject(&bridge, &details).await;
            assert!(file.join("inner").exists());
        }

        #[tokio::test]
        async fn plain_create_is_removed() {
            let ws = tempfile::tempdir().unwrap();
            let (bridge, env) = bridge(ws.path());
            let details = write(&env, "new.txt").await;
            reject(&bridge, &details).await;
            assert!(!ws.path().join("new.txt").exists());
        }
    }

    #[test]
    fn auto_approve_label_roundtrip() {
        use loop_agent::harness::SessionTreeEntry;
        let entries = vec![SessionTreeEntry::Label {
            id: "1".into(),
            parent_id: None,
            timestamp: 0,
            label: format!("{AUTO_APPROVE_LABEL_PREFIX}{GROUP_FILE}"),
        }];
        let set = auto_approve_from_entries(&entries);
        assert!(set.contains(GROUP_FILE));
    }
}
