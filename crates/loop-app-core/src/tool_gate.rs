//! The one check every tool call passes, in every mode.
//!
//! Bootstrap installs the gate into the harness's before/after tool slots, so
//! `--print`, the TUI, the desktop app and workflow tasks all go through it.
//! Each call passes, in order:
//!
//! 1. `toolPermissions` deny rules and the `read-only` ceiling: refused;
//! 2. hooks (`tool_call`): a block refuses, an allow runs it without asking;
//! 3. the permission mode ([`crate::permissions`]): allow, or ask;
//! 4. "ask": the interactive approver when a surface attached one; with
//!    nobody to ask (`--print`, `mcp serve`) the call is refused.
//!
//! After the call, the approver's file review runs (unless step 2 or 3
//! allowed the call), then `tool_result` hooks, which may add context for
//! the model.
//!
//! Every decision is written as a decision record to the gate's
//! [`DecisionSink`] before the call runs ([`crate::permissions::decision`]).
//! A record that cannot be written refuses the call.
//!
//! A command the user typed (`session/shell`) passes the same Policy
//! layers, rules and mode as a `bash` call ([`ToolGate::check_user_command`]).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use loop_agent::harness::{
    AgentHarness, HarnessHookEvent, HookRegistry, SearchFilter, ToolCallInfo, ToolDecision,
    ToolSource, EXIT_PLAN_TOOL,
};
use loop_agent::{
    AfterToolCallFn, BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult, ToolCall,
};
use loop_ai::{TextContent, ToolResultContent};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use loop_protocol::ToolKind;

use crate::permissions::decision::{
    self, DecidedBy, DecisionAnswer, DecisionOutcome, DecisionSink, DecisionSource,
    PolicyDecisionRecord,
};
use crate::permissions::paths::PathLimit;
use crate::permissions::{
    Call, Decision, GatePaths, LayerSource, PathFacts, PermissionMode, Policy, Verdict,
};
use crate::tool_approval::{PlanReview, Review, ToolApprovalBridge, ToolPermission};
use loop_agent::harness::sandbox::PathNamespace;
use loop_protocol::Caller;

/// Why a call that needs approval is refused when nobody can approve it,
/// naming a mode that would allow it.
fn headless_refusal(tool: &str, mode: PermissionMode) -> String {
    let fix = if matches!(tool, "write" | "edit") && mode != PermissionMode::AcceptEdits {
        "Rerun with --permission-mode accept-edits (edits inside the workspace) or full-access"
            .to_owned()
    } else if tool == "bash" {
        "Rerun with --permission-mode full-access, or allow the command in \
         settings.toolPermissions, for example \"bash(cargo test*)\": \"allow\""
            .to_owned()
    } else {
        format!(
            "Rerun with --permission-mode full-access, or allow `{tool}` in \
             settings.toolPermissions"
        )
    };
    format!(
        "`{tool}` needs approval in {} mode, and nobody can approve it in this run. {fix}.",
        mode.as_str()
    )
}

/// Largest tool output passed to `tool_result` hooks.
const HOOK_OUTPUT_CAP: usize = 64 * 1024;

/// Deny rules, hooks and an optional interactive approver.
#[derive(Default)]
pub struct ToolGate {
    /// Shared, so the search filter takes the rules without copying them.
    permissions: Mutex<Arc<BTreeMap<String, ToolPermission>>>,
    approver: Mutex<Option<Arc<ToolApprovalBridge>>>,
    hooks: Mutex<Option<HookRegistry>>,
    harness: Mutex<Weak<AgentHarness>>,
    mode: Mutex<PermissionMode>,
    /// The mode plan mode was entered from, which approving a plan returns
    /// to.
    plan_return: Mutex<PermissionMode>,
    policy: Mutex<Policy>,
    workspace: Mutex<Option<PathBuf>>,
    /// The directory the tools resolve relative paths against (the
    /// harness's cwd); it can be below the workspace.
    cwd: Mutex<Option<PathBuf>>,
    /// More bases for relative deny and ask patterns ([`Call::rule_bases`]).
    rule_bases: Mutex<Vec<PathBuf>>,
    /// Calls allowed without asking, counted by [`call_key`]; their
    /// after-call review is skipped. Keyed by more than the id, which a
    /// server may leave empty or reuse across parallel workflow tasks.
    allowed: Mutex<HashMap<String, usize>>,
    /// MCP servers, for the tools they mark read-only.
    mcp: Mutex<Option<Arc<loop_mcp::McpClientManager>>>,
    /// Told the new mode when approving a plan changes it, so the Session
    /// can record it for every attached Surface.
    mode_recorder: Mutex<Option<ModeRecorder>>,
    /// The Runtime's tool lock: set once, never cleared. Its layer is added
    /// over the Policy on every check ([`Self::effective_policy`]), so
    /// [`Self::set_policy`] cannot drop it.
    tool_lock: std::sync::OnceLock<crate::permissions::tool_lock::ToolLock>,
    /// Under a tool lock: the Session's folder and root as they resolved
    /// when it opened. Set once; every path check compares against it.
    confinement: std::sync::OnceLock<Arc<crate::permissions::tool_lock::Confinement>>,
    /// Where the decision records go. `None` records nothing (a bare gate in
    /// tests); every Session the factory opens has one.
    decisions: Mutex<Option<Arc<dyn DecisionSink>>>,
    /// Where path checks get their facts: the Session's Workspace env.
    /// `None` uses the Runtime host's env ([`GatePaths::host`]).
    paths: Mutex<Option<GatePaths>>,
}

/// What the gate decided about one call, before it is recorded.
#[derive(Debug, Clone)]
pub(crate) struct Decided {
    pub(crate) outcome: DecisionOutcome,
    pub(crate) answer: Option<DecisionAnswer>,
    pub(crate) by: DecidedBy,
    pub(crate) layer: Option<LayerSource>,
    pub(crate) rule: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) escalation: Option<String>,
}

impl Decided {
    /// The mode's and the Policy's verdict as it stands.
    pub(crate) fn from_verdict(verdict: &Verdict) -> Self {
        let (outcome, reason) = match &verdict.decision {
            Decision::Allow => (DecisionOutcome::Allow, None),
            Decision::Ask => (DecisionOutcome::Ask, None),
            Decision::Deny(reason) => (DecisionOutcome::Deny, Some(reason.clone())),
        };
        Self {
            outcome,
            answer: None,
            by: verdict.by,
            layer: verdict.layer,
            rule: verdict.rule.clone(),
            reason,
            escalation: None,
        }
    }

    /// A refusal by `by`.
    pub(crate) fn deny(by: DecidedBy, reason: impl Into<String>) -> Self {
        Self {
            outcome: DecisionOutcome::Deny,
            answer: None,
            by,
            layer: None,
            rule: None,
            reason: Some(reason.into()),
            escalation: None,
        }
    }

    /// An allow by `by`.
    pub(crate) fn allow(by: DecidedBy) -> Self {
        Self {
            outcome: DecisionOutcome::Allow,
            answer: None,
            by,
            layer: None,
            rule: None,
            reason: None,
            escalation: None,
        }
    }

    /// An "ask" the approver answered (`allowed`), through `escalation`.
    pub(crate) fn answered(allowed: bool, escalation: Option<String>) -> Self {
        Self {
            outcome: DecisionOutcome::Ask,
            answer: Some(if allowed {
                DecisionAnswer::Allow
            } else {
                DecisionAnswer::Deny
            }),
            by: DecidedBy::Approver,
            layer: None,
            rule: None,
            reason: None,
            escalation,
        }
    }

    /// Keep the layer and the rule of `verdict`, which led here.
    pub(crate) fn after(mut self, verdict: &Verdict) -> Self {
        self.layer = self.layer.or(verdict.layer);
        self.rule = self.rule.or_else(|| verdict.rule.clone());
        self
    }

    /// With `reason`.
    pub(crate) fn because(mut self, reason: Option<String>) -> Self {
        self.reason = reason;
        self
    }
}

/// An effect of the approver's answer, made only once the decision is
/// recorded.
pub(crate) enum After {
    /// Approving a plan changes the mode.
    Mode(PermissionMode),
    /// "Accept all" grants a group for the rest of the Session.
    Grant(&'static str),
}

/// The call a decision is about.
pub(crate) struct Facts<'a> {
    pub(crate) source: DecisionSource,
    pub(crate) tool: &'a str,
    pub(crate) kind: ToolKind,
    pub(crate) call_id: Option<&'a str>,
    pub(crate) args: &'a serde_json::Value,
    /// Who acts, when the caller is known here (a user command); else the
    /// sink says.
    pub(crate) caller: Option<Caller>,
    /// The Session, when known here.
    pub(crate) session: Option<String>,
}

/// The rule name a record gives the `bash` block list.
const BLOCK_LIST: &str = "bash block list";

/// The `reason` a record keeps. An approver's or a hook's text can quote
/// the call (a URL with a token, a search query, a command), so those get
/// a fixed text; Loop's own reasons are scrubbed of URL credentials and
/// capped ([`decision::scrub`]).
fn record_reason(decided: &Decided) -> Option<String> {
    let reason = decided.reason.as_deref()?;
    Some(match decided.by {
        DecidedBy::Approver => "the approver rejected it".to_owned(),
        DecidedBy::Hook => "a tool_call hook blocked it".to_owned(),
        _ => decision::scrub(reason, decision::MAX_REASON_BYTES),
    })
}

/// Where a call reached the gate from, by its name and id: an MCP tool, a
/// `/workflow` shell task (`wf-shell-…`), or the model's own tool.
fn source_of(call: &ToolCall) -> DecisionSource {
    if call.id.starts_with("wf-shell-") {
        return DecisionSource::Workflow;
    }
    match loop_agent::harness::mcp::bridge::parse_mcp_tool_name(&call.name) {
        Some((server, _)) => DecisionSource::Mcp(server.to_owned()),
        None => DecisionSource::Native,
    }
}

/// See [`ToolGate::set_mode_recorder`].
pub type ModeRecorder = Arc<
    dyn Fn(PermissionMode) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// A recorder that writes the mode into `native`'s current Session. It holds
/// the backend weakly, so the gate never keeps it alive.
pub fn mode_recorder(native: &Arc<loop_agent::native_backend::NativeBackend>) -> ModeRecorder {
    let native = Arc::downgrade(native);
    Arc::new(move |mode| {
        let native = native.clone();
        Box::pin(async move {
            let Some(native) = native.upgrade() else {
                return;
            };
            if let Err(error) = native.record_gate_mode(mode.as_str()).await {
                tracing::warn!(%error, "record permission mode");
            }
        })
    })
}

impl ToolGate {
    /// A gate enforcing `permissions` (from `settings.toolPermissions`).
    pub fn new(permissions: BTreeMap<String, ToolPermission>) -> Arc<Self> {
        Arc::new(Self {
            permissions: Mutex::new(Arc::new(permissions)),
            ..Default::default()
        })
    }

    /// Create a gate bound to a Session's workspace before tools are built.
    /// Relative call paths and Policy checks never use the process cwd.
    pub fn new_for_workspace(
        permissions: BTreeMap<String, ToolPermission>,
        cwd: &std::path::Path,
    ) -> Arc<Self> {
        let gate = Self::new(permissions);
        gate.set_workspace(crate::permissions::workspace_root(cwd));
        *gate.cwd.lock() = Some(cwd.to_path_buf());
        gate
    }

    /// Create a gate bound to a Session's Workspace env: every path check
    /// gets its facts from `paths`. `cwd` is the Session's folder for a
    /// host-namespace env; a guest env's own cwd is used instead. The git
    /// root is found through the env.
    pub async fn new_for_env(
        permissions: BTreeMap<String, ToolPermission>,
        cwd: &std::path::Path,
        paths: GatePaths,
    ) -> Arc<Self> {
        let gate = Self::new(permissions);
        let cwd = match paths.namespace() {
            PathNamespace::Host => cwd.to_path_buf(),
            _ => paths.env().cwd().to_path_buf(),
        };
        gate.set_workspace(paths.workspace_root(&cwd).await);
        *gate.cwd.lock() = Some(cwd);
        gate.set_paths(paths);
        gate
    }

    /// Get path facts from `paths` from the next call on.
    pub fn set_paths(&self, paths: GatePaths) {
        *self.paths.lock() = Some(paths);
        self.allowed.lock().clear();
    }

    /// Where path checks get their facts.
    pub fn paths(&self) -> GatePaths {
        self.paths.lock().clone().unwrap_or_else(GatePaths::host)
    }

    /// Install the gate into `harness`'s tool slots and run its hooks.
    pub fn install(self: &Arc<Self>, harness: &Arc<AgentHarness>) {
        self.workspace
            .lock()
            .get_or_insert_with(|| crate::permissions::workspace_root(&harness.cwd()));
        self.cwd.lock().get_or_insert_with(|| harness.cwd());
        *self.hooks.lock() = Some(harness.hooks());
        *self.harness.lock() = Arc::downgrade(harness);
        harness.set_before_tool_call(Some(self.before_hook()));
        harness.set_after_tool_call(Some(self.after_hook()));
        let gate = Arc::clone(self);
        harness.set_tool_filter(Some(Arc::new(move |tool| gate.available(tool))));
    }

    /// Whether `tool` is offered to the model: a tool whose Capability is
    /// off is taken out of the toolset, not just refused.
    pub fn available(&self, tool: &str) -> bool {
        let call = Call {
            kind: self.kind_now(tool),
            ..Call::new(tool, serde_json::Value::Null, self.workspace())
        };
        self.effective_policy().available(self.mode(), &call)
    }

    /// Lock this gate's tools for good with the Runtime's tool lock
    /// ([`crate::permissions::tool_lock`]). A second lock is ignored; the
    /// first one stays.
    pub fn lock_tools(&self, lock: crate::permissions::tool_lock::ToolLock) {
        let _ = self.tool_lock.set(lock);
        self.allowed.lock().clear();
    }

    /// The tool lock in force, if any.
    pub fn tool_lock(&self) -> Option<crate::permissions::tool_lock::ToolLock> {
        self.tool_lock.get().copied()
    }

    /// Bind a locked gate's reads to `confinement` for good. A second one
    /// is ignored; the first one stays.
    pub fn confine(&self, confinement: crate::permissions::tool_lock::Confinement) {
        let _ = self.confinement.set(Arc::new(confinement));
        self.allowed.lock().clear();
    }

    /// Whether a locked gate may let a read of `path` through: inside the
    /// stored folder and root. A locked gate without a confinement refuses.
    fn locked_read_ok(&self, facts: &PathFacts, path: &std::path::Path) -> bool {
        match self.confinement.get() {
            Some(confined) => confined.allows_in(facts, path),
            None => false,
        }
    }

    /// The Policy every check uses: the one set, with the tool lock's layer
    /// on top when there is a lock.
    fn effective_policy(&self) -> Policy {
        let policy = self.policy.lock().clone();
        match self.tool_lock() {
            Some(lock) => policy.with_only(crate::permissions::tool_lock::layer(lock)),
            None => policy,
        }
    }

    /// The Policy layers over the mode, the tool lock's included.
    pub fn policy(&self) -> Policy {
        self.effective_policy()
    }

    /// Replace the Policy layers over the mode; applies to the next call.
    pub fn set_policy(&self, policy: Policy) {
        *self.policy.lock() = policy;
        self.allowed.lock().clear();
    }

    /// [`Self::kind`] without waiting. While the MCP server list is being
    /// changed, an MCP tool counts as a read, so it stays listed; the gate
    /// still checks each call.
    fn kind_now(&self, tool: &str) -> ToolKind {
        let kind = loop_agent::protocol_events::tool_kind(tool);
        let mcp = self.mcp.lock().clone();
        let (Some(mcp), Some((server, name))) = (
            mcp,
            loop_agent::harness::mcp::bridge::parse_mcp_tool_name(tool),
        ) else {
            return kind;
        };
        let read_only = match mcp.connections().try_read() {
            Ok(connections) => connections.get(server).is_some_and(|conn| {
                conn.tools.iter().any(|t| {
                    t.name == name
                        && t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
                })
            }),
            Err(_) => true,
        };
        if read_only {
            ToolKind::Read
        } else {
            kind
        }
    }

    /// The active permission mode.
    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock()
    }

    /// Switch the permission mode; applies to the next call.
    pub fn set_mode(&self, mode: PermissionMode) {
        let before = std::mem::replace(&mut *self.mode.lock(), mode);
        if mode == PermissionMode::Plan && before != PermissionMode::Plan {
            *self.plan_return.lock() = before;
        }
        // A call allowed under the old mode is reviewed under the new one.
        self.allowed.lock().clear();
    }

    /// Call `recorder` with the new mode when approving a plan changes it.
    pub fn set_mode_recorder(&self, recorder: Option<ModeRecorder>) {
        *self.mode_recorder.lock() = recorder;
    }

    /// Look up MCP tools' `readOnlyHint` in `mcp`'s servers.
    pub fn set_mcp(&self, mcp: Arc<loop_mcp::McpClientManager>) {
        *self.mcp.lock() = Some(mcp);
    }

    /// The tool's kind; an MCP tool its server marks read-only is a read.
    async fn kind(&self, tool: &str) -> ToolKind {
        let mcp = self.mcp.lock().clone();
        match (
            mcp,
            loop_agent::harness::mcp::bridge::parse_mcp_tool_name(tool),
        ) {
            (Some(mcp), Some((server, name))) if mcp.read_only_hint(server, name).await => {
                ToolKind::Read
            }
            _ => loop_agent::protocol_events::tool_kind(tool),
        }
    }

    /// More directories relative `deny` and `ask` patterns match against:
    /// a subagent bound to its own cwd keeps its parent's git root and cwd.
    pub fn set_rule_bases(&self, bases: Vec<PathBuf>) {
        *self.rule_bases.lock() = bases;
    }

    /// The directory `accept-edits` may edit (default: the git root of the
    /// harness's cwd).
    pub fn set_workspace(&self, workspace: PathBuf) {
        *self.workspace.lock() = Some(workspace);
    }

    fn workspace(&self) -> PathBuf {
        self.workspace
            .lock()
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// What the mode and the Policy say about a call, before hooks and the
    /// approver.
    pub async fn decide(&self, tool: &str, args: &serde_json::Value) -> Decision {
        self.verdict(tool, args).await.decision
    }

    /// [`Self::decide`], with the step and the layer that decided.
    pub async fn verdict(&self, tool: &str, args: &serde_json::Value) -> Verdict {
        let policy = self.effective_policy();
        let mode = self.mode();
        let rules = self.rules();
        self.with_call(tool, args, |call| {
            if let Some(confined) = &call.confinement {
                if confined.moved_in(&call.paths) {
                    return Verdict {
                        decision: Decision::Deny(format!(
                            "`{tool}` is refused: this Session's folder {} was moved or \
                             replaced after the Session opened, so the tool lock refuses \
                             every call of this Session. Open a new Session.",
                            confined.folder().display()
                        )),
                        by: DecidedBy::Off,
                        rule: None,
                        layer: Some(LayerSource::Runtime),
                    };
                }
            }
            policy.decide_why(mode, &rules, call)
        })
        .await
        .unwrap_or_else(|limit| Verdict {
            // Whatever the rules say: the gate could not check the path.
            decision: Decision::Deny(format!("`{tool}` is refused: {limit}.")),
            by: DecidedBy::Unreadable,
            rule: None,
            layer: None,
        })
    }

    /// Whether a Policy layer's "ask" holds for the call. A path the gate
    /// cannot check asks (fail closed).
    async fn requires_ask(&self, tool: &str, args: &serde_json::Value) -> bool {
        let policy = self.effective_policy();
        self.with_call(tool, args, |call| policy.requires_ask(call))
            .await
            .unwrap_or(true)
    }

    /// Run `decide` on the call, with its path facts from the Session's
    /// Workspace env ([`GatePaths::run`]). `decide` may run more than once.
    /// Past a limit the result is [`PathLimit`]: the caller refuses.
    async fn with_call<T>(
        &self,
        tool: &str,
        args: &serde_json::Value,
        decide: impl Fn(&Call) -> T,
    ) -> Result<T, PathLimit> {
        let workspace = self.workspace();
        let cwd = self.cwd(&workspace);
        let kind = self.kind(tool).await;
        let bases = self.rule_bases.lock().clone();
        let confinement = self.confinement.get().cloned();
        self.paths()
            .run(|facts| {
                let call = Call {
                    rule_bases: bases.clone(),
                    confinement: confinement.clone(),
                    ..Call::resolve_with(facts, tool, kind, args, &workspace, &cwd)
                };
                decide(&call)
            })
            .await
    }

    /// The directory the tools open relative paths against: the harness's
    /// cwd, or the workspace when none is known.
    fn cwd(&self, workspace: &std::path::Path) -> PathBuf {
        self.cwd
            .lock()
            .clone()
            .unwrap_or_else(|| workspace.to_path_buf())
    }

    /// Run `hooks` without a harness (tests, hosts that only gate agents).
    pub fn set_hooks(&self, hooks: HookRegistry) {
        *self.hooks.lock() = Some(hooks);
    }

    /// The `toolPermissions` deny rule that refuses an external agent's
    /// call, if any. Deny rules name Loop's own tools, so the call is
    /// checked as each Loop tool of its kind (`bash` for a command, `write`
    /// and `edit` for an edit, and so on) with the agent's raw input. A
    /// relative path in that input is joined to the agent's `cwd`.
    pub fn agent_call_denied(
        &self,
        kind: ToolKind,
        args: &serde_json::Value,
        cwd: &std::path::Path,
    ) -> Option<String> {
        self.agent_call_denied_by(kind, args, cwd)
            .map(|(reason, _)| reason)
    }

    /// [`Self::agent_call_denied`], with the `toolPermissions` key that
    /// denied it.
    pub fn agent_call_denied_by(
        &self,
        kind: ToolKind,
        args: &serde_json::Value,
        cwd: &std::path::Path,
    ) -> Option<(String, Option<String>)> {
        let tools: &[&str] = match kind {
            ToolKind::Execute => &["bash"],
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move => &["write", "edit"],
            ToolKind::Read => &["read"],
            ToolKind::Search => &["grep", "glob", "ls"],
            ToolKind::Fetch => &["web_fetch", "web_search"],
            ToolKind::Think | ToolKind::Other => &[],
        };
        let rules = self.rules();
        let workspace = self.workspace();
        // An external agent runs on the Runtime host, in its own folder
        // there: its paths are host paths.
        let host = PathFacts::runtime_host();
        tools.iter().find_map(|tool| {
            let call = Call {
                rule_bases: self.rule_bases.lock().clone(),
                ..Call::resolve_with(&host, tool, kind, args, &workspace, cwd)
            };
            let (rule, key) = crate::permissions::rule_and_key(&rules, &call);
            (rule.permission == ToolPermission::Deny).then(|| {
                (
                    format!("Tool `{tool}` is denied by settings.toolPermissions"),
                    key,
                )
            })
        })
    }

    /// The `tool_call` hooks' decision about an external agent's call.
    pub async fn review_agent_call(&self, info: ToolCallInfo) -> Option<ToolDecision> {
        let hooks = self.hooks()?;
        hooks.emit(HarnessHookEvent::ToolCall(info)).await.decision
    }

    /// Tell hooks about something that already happened; their answer is
    /// ignored (an agent's finished tool call).
    pub async fn observe(&self, event: HarnessHookEvent) {
        if let Some(hooks) = self.hooks() {
            let _ = hooks.emit(event).await;
        }
    }

    /// The hook registry, when it has handlers.
    fn hooks(&self) -> Option<HookRegistry> {
        self.hooks.lock().clone().filter(|hooks| !hooks.is_empty())
    }

    /// What hooks are told about `tool_call`.
    async fn call_info(&self, tool_call: &ToolCall, args: &serde_json::Value) -> ToolCallInfo {
        let harness = self.harness.lock().upgrade();
        let (session_id, cwd) = match harness {
            Some(harness) => (harness.session_id().await, harness.cwd()),
            None => (String::new(), std::path::PathBuf::new()),
        };
        ToolCallInfo {
            source: ToolSource::of_native_tool(&tool_call.name),
            tool: tool_call.name.clone(),
            kind: self.kind(&tool_call.name).await,
            args: args.clone(),
            call_id: tool_call.id.clone(),
            session_id,
            cwd,
            mode: Some(self.mode().as_str().to_owned()),
        }
    }

    /// Ask the user through `approver` (TUI, desktop); `None` runs headless.
    pub fn set_approver(&self, approver: Option<Arc<ToolApprovalBridge>>) {
        *self.approver.lock() = approver;
    }

    /// What the search tools hide: files the current `toolPermissions` deny
    /// to `read` or to the tool itself ([`crate::permissions::search_may_show`]).
    pub fn search_filter(self: &Arc<Self>) -> SearchFilter {
        let gate = Arc::downgrade(self);
        let home = dirs::home_dir();
        let memo = Arc::new(crate::permissions::paths::SearchMemo::default());
        Arc::new(move |tool: &str, path: &std::path::Path| {
            let Some(gate) = gate.upgrade() else {
                return true;
            };
            let workspace = gate.workspace();
            let cwd = gate.cwd(&workspace);
            // The filter cannot wait for the env (`SearchFilter` is sync):
            // host facts for a host-namespace Session, none (fail closed)
            // for a guest one.
            let facts = gate.paths().sync_facts(&memo);
            // Under a tool lock every walked entry must resolve inside the
            // Session's folder: a symlink or hard link out is hidden, and a
            // walk never enters a linked folder outside.
            if gate.tool_lock().is_some() && !gate.locked_read_ok(&facts, path) {
                return false;
            }
            let bases = gate.rule_bases.lock().clone();
            // Not under the lock: matching reads the file system.
            let rules = gate.rules();
            crate::permissions::rules::search_may_show_with(
                &facts,
                &rules,
                tool,
                path,
                &workspace,
                &cwd,
                &bases,
                home.as_deref(),
            )
        })
    }

    /// The `toolPermissions` rules in force.
    pub fn permissions(&self) -> BTreeMap<String, ToolPermission> {
        (*self.rules()).clone()
    }

    /// The rules in force, shared.
    fn rules(&self) -> Arc<BTreeMap<String, ToolPermission>> {
        Arc::clone(&self.permissions.lock())
    }

    /// Replace the per-tool permissions (settings changed).
    pub fn set_permissions(&self, permissions: BTreeMap<String, ToolPermission>) {
        *self.permissions.lock() = Arc::new(permissions);
        self.allowed.lock().clear();
    }

    fn approver(&self) -> Option<Arc<ToolApprovalBridge>> {
        self.approver.lock().clone()
    }

    fn mark_allowed(&self, call: &ToolCall, args: &serde_json::Value) {
        *self.allowed.lock().entry(call_key(call, args)).or_default() += 1;
    }

    /// Whether `call` was allowed without asking; consumes the mark.
    fn take_allowed(&self, call: &ToolCall, args: &serde_json::Value) -> bool {
        let mut allowed = self.allowed.lock();
        let key = call_key(call, args);
        match allowed.get_mut(&key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                allowed.remove(&key);
                true
            }
            None => false,
        }
    }

    /// Ask the user to approve the plan in an `exit_plan` call. Approved,
    /// the call runs and the mode goes back to the one plan mode was entered
    /// from (or to `accept-edits`, if the user chose that).
    async fn review_plan(
        &self,
        ctx: &BeforeToolCallContext,
        cancel: Option<CancellationToken>,
    ) -> (Option<BeforeToolCallResult>, Decided, Option<After>) {
        let block = |reason: String| {
            Some(BeforeToolCallResult {
                block: true,
                reason: Some(reason),
            })
        };
        let Some(approver) = self.approver() else {
            let reason = "Plan mode ends when the user approves the plan, and nobody can \
                          approve it in this run. Give the plan as your answer instead.";
            return (
                block(reason.into()),
                Decided::deny(DecidedBy::NoApprover, reason),
                None,
            );
        };
        let plan = ctx
            .args
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let back = *self.plan_return.lock();
        let (review, escalation) = approver.review_plan(plan, back.as_str(), cancel).await;
        match review {
            PlanReview::Approved { accept_edits } => {
                let mode = if accept_edits {
                    PermissionMode::AcceptEdits
                } else {
                    back
                };
                // The mode changes once the decision is recorded.
                (
                    None,
                    Decided::answered(true, escalation),
                    Some(After::Mode(mode)),
                )
            }
            PlanReview::KeepPlanning(reason) => {
                let reason = match reason {
                    Some(reason) => format!(
                        "The user wants changes to the plan: {reason}. Stay in plan mode, \
                         revise the plan and call {EXIT_PLAN_TOOL} again."
                    ),
                    None => format!(
                        "The user did not approve the plan. Stay in plan mode: ask what to \
                         change, or revise the plan and call {EXIT_PLAN_TOOL} again."
                    ),
                };
                let decided = Decided::answered(false, escalation).because(Some(reason.clone()));
                (block(reason), decided, None)
            }
        }
    }

    /// Write the decision record for `facts` and `decided` to the sink, and
    /// wait for it. With no sink, nothing is written. `Err` is the refusal
    /// to give when the record could not be written.
    pub(crate) async fn record(&self, facts: Facts<'_>, decided: &Decided) -> Result<(), String> {
        let Some(sink) = self.decisions.lock().clone() else {
            return Ok(());
        };
        let context = sink.context(decided.escalation.as_deref());
        let caller = facts.caller.unwrap_or(context.caller);
        let session = match facts.session.or(context.session) {
            Some(session) => Some(session),
            None => self.harness_session().await,
        };
        let capability = Call {
            kind: facts.kind,
            ..Call::new(facts.tool, serde_json::Value::Null, PathBuf::new())
        };
        let record = PolicyDecisionRecord {
            id: decision::record_id(),
            ts_ms: decision::now_ms(),
            tenant: caller.tenant().clone(),
            principal: caller.principal().clone(),
            via: caller.via().cloned(),
            session,
            turn: context.turn,
            call_id: facts.call_id.filter(|id| !id.is_empty()).map(str::to_owned),
            source: facts.source,
            tool: decision::scrub(facts.tool, decision::MAX_NAME_BYTES),
            kind: facts.kind,
            capability: crate::permissions::Capability::of(&capability).id(),
            subject: decision::subject_of(facts.kind, facts.args),
            args_sha256: Some(decision::args_sha256(facts.args)),
            outcome: decided.outcome,
            answer: decided.answer,
            by: decided.by,
            layer: decided.layer.map(|layer| layer.id().to_owned()),
            rule: decided
                .rule
                .as_deref()
                .map(|rule| decision::scrub(rule, decision::MAX_NAME_BYTES)),
            mode: Some(self.mode().as_str().to_owned()),
            escalation: decided.escalation.clone(),
            answered_by: (decided.by == DecidedBy::Approver)
                .then_some(context.answered_by)
                .flatten()
                .map(|caller| caller.principal().clone()),
            reason: record_reason(decided),
        };
        sink.record(&record).await.map_err(|error| {
            tracing::warn!(%error, "Policy decision not recorded; the call is refused");
            format!("`{}` is refused: {error}", facts.tool)
        })
    }

    /// The Session id of the harness, when there is one.
    async fn harness_session(&self) -> Option<String> {
        let harness = self.harness.lock().upgrade()?;
        let id = harness.session_id().await;
        (!id.is_empty()).then_some(id)
    }

    /// Write every decision record to `sink` from now on (`None`: write
    /// none).
    pub fn set_decision_sink(&self, sink: Option<Arc<dyn DecisionSink>>) {
        *self.decisions.lock() = sink;
    }

    /// The sink decision records go to, if any.
    pub fn decision_sink(&self) -> Option<Arc<dyn DecisionSink>> {
        self.decisions.lock().clone()
    }

    /// The Policy check for a command the user typed (`session/shell`,
    /// `!command`), which runs in the Session's tool environment as `bash`
    /// does. The tool lock and the outside-user refusal come before it.
    ///
    /// The command is judged as a `bash` call with the same Policy layers,
    /// `toolPermissions` rules and mode: a layer that turns the shell off, a
    /// deny rule (plain or pattern) and `read-only` or `plan` mode (except
    /// read-only commands) refuse it. Where the `bash` tool would ask, the
    /// user who typed the command is the approver, so it runs without a
    /// prompt, when the caller is this host's user or an in-process call. A
    /// caller from outside the host cannot approve, so an "ask" refuses it.
    /// Hooks are not run: they review the agent's tool calls, and a typed
    /// command is the user's own action. The `bash` block list refuses as it
    /// does for the tool.
    ///
    /// The decision is recorded before `Ok` returns. `Err` is the refusal.
    pub async fn check_user_command(
        &self,
        command: &str,
        session: Option<String>,
        caller: Option<Caller>,
    ) -> Result<(), loop_agent::native_backend::CommandRefused> {
        use loop_agent::native_backend::CommandRefused;
        let args = serde_json::json!({ "command": command });
        let mut verdict = self.verdict("bash", &args).await;
        // The `bash` block list is part of the decision, so the record says
        // what happened.
        if !matches!(verdict.decision, Decision::Deny(_)) {
            if let Err(reason) = loop_agent::harness::tools::check_command_policy(command, &[]) {
                verdict = Verdict {
                    decision: Decision::Deny(reason),
                    by: DecidedBy::Rule,
                    rule: Some(BLOCK_LIST.to_owned()),
                    layer: None,
                };
            }
        }
        let host = caller.as_ref().is_none_or(Caller::is_host_user);
        let (result, decided) = match &verdict.decision {
            Decision::Deny(reason) => (
                Err(CommandRefused(reason.clone())),
                Decided::from_verdict(&verdict),
            ),
            Decision::Allow => (Ok(()), Decided::from_verdict(&verdict)),
            Decision::Ask if host => (Ok(()), Decided::allow(DecidedBy::User).after(&verdict)),
            Decision::Ask => {
                let reason = format!(
                    "this command needs approval in {} mode, and a user from outside this \
                     host cannot approve it",
                    self.mode().as_str()
                );
                let decided = Decided::deny(DecidedBy::NoApprover, reason.clone()).after(&verdict);
                (Err(CommandRefused(reason)), decided)
            }
        };
        let facts = Facts {
            source: DecisionSource::UserShell,
            tool: "bash",
            kind: ToolKind::Execute,
            call_id: None,
            args: &args,
            caller,
            session,
        };
        self.record(facts, &decided).await.map_err(CommandRefused)?;
        result
    }

    /// [`Self::check_user_command`] as the backend's check for user
    /// commands. It holds the gate weakly: a dropped gate refuses.
    pub fn command_gate(self: &Arc<Self>) -> loop_agent::native_backend::CommandGate {
        let gate = Arc::downgrade(self);
        Arc::new(move |command: loop_agent::native_backend::UserCommand| {
            let gate = gate.upgrade();
            Box::pin(async move {
                let Some(gate) = gate else {
                    return Err(loop_agent::native_backend::CommandRefused(
                        "the Session's permission gate is gone".to_owned(),
                    ));
                };
                gate.check_user_command(
                    &command.command,
                    Some(command.session.0.clone()),
                    command.caller,
                )
                .await
            })
        })
    }

    /// The gate's before-call check, for callers outside the harness
    /// (`mcp serve`).
    pub fn before_hook(self: &Arc<Self>) -> BeforeToolCallFn {
        self.before_hook_from(None)
    }

    /// [`Self::before_hook`] for calls that all come from `source` (such as
    /// `loop mcp serve`'s clients); `None` tells the source by the call.
    pub fn before_hook_from(self: &Arc<Self>, source: Option<DecisionSource>) -> BeforeToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            let source = source.clone();
            Box::pin(async move {
                let tool_call = ctx.tool_call.clone();
                let args = ctx.args.clone();
                let source = source.unwrap_or_else(|| source_of(&tool_call));
                let kind = gate.kind(&tool_call.name).await;
                let (result, decided, unasked, after) = gate.check(ctx, cancel).await;
                let facts = Facts {
                    source,
                    tool: &tool_call.name,
                    kind,
                    call_id: Some(&tool_call.id),
                    args: &args,
                    caller: None,
                    session: None,
                };
                if let Err(reason) = gate.record(facts, &decided).await {
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                    });
                }
                if unasked {
                    gate.mark_allowed(&tool_call, &args);
                }
                // Effects of the answer come only after it is recorded.
                match after {
                    Some(After::Mode(mode)) => {
                        gate.set_mode(mode);
                        let recorder = gate.mode_recorder.lock().clone();
                        if let Some(recorder) = recorder {
                            recorder(mode).await;
                        }
                    }
                    Some(After::Grant(group)) => {
                        if let Some(approver) = gate.approver() {
                            approver.grant_and_persist(group).await;
                        }
                    }
                    None => {}
                }
                result
            })
        })
    }

    /// The decision on one call, in the gate's check order: the result for
    /// the harness, what to record, and whether it runs without asking (so
    /// the after-call review is skipped).
    async fn check(
        &self,
        ctx: BeforeToolCallContext,
        cancel: Option<CancellationToken>,
    ) -> (Option<BeforeToolCallResult>, Decided, bool, Option<After>) {
        let block = |reason: String| {
            Some(BeforeToolCallResult {
                block: true,
                reason: Some(reason),
            })
        };
        let verdict = self.verdict(&ctx.tool_call.name, &ctx.args).await;
        if let Decision::Deny(reason) = &verdict.decision {
            return (
                block(reason.clone()),
                Decided::from_verdict(&verdict),
                false,
                None,
            );
        }
        // Only the user ends plan mode: hooks don't decide this one.
        if ctx.tool_call.name == EXIT_PLAN_TOOL {
            let (result, decided, after) = self.review_plan(&ctx, cancel).await;
            return (result, decided, false, after);
        }
        if let Some(hooks) = self.hooks() {
            let info = self.call_info(&ctx.tool_call, &ctx.args).await;
            let emit = hooks.emit(HarnessHookEvent::ToolCall(info));
            // Cancelling the turn drops the emit, which kills any command
            // hook still running.
            let outcome = match &cancel {
                Some(cancel) => tokio::select! {
                    outcome = emit => outcome,
                    () = cancel.cancelled() => {
                        let reason = "Operation aborted";
                        return (
                            block(reason.into()),
                            Decided::deny(DecidedBy::Hook, reason),
                            false,
                            None,
                        );
                    }
                },
                None => emit.await,
            };
            let hook_allows = matches!(outcome.decision, Some(ToolDecision::Allow));
            if let Some(ToolDecision::Block { reason }) = outcome.decision {
                let decided = Decided::deny(DecidedBy::Hook, reason.clone());
                return (block(reason), decided, false, None);
            }
            // A Policy layer's "ask" holds even against a hook.
            if hook_allows && !self.requires_ask(&ctx.tool_call.name, &ctx.args).await {
                return (None, Decided::allow(DecidedBy::Hook), true, None);
            }
        }
        if verdict.decision == Decision::Allow {
            return (None, Decided::from_verdict(&verdict), true, None);
        }
        match self.approver() {
            // Only some tools have an approval prompt; asking about the
            // others would silently run them, so refuse instead.
            Some(_)
                if !crate::tool_approval::interactive_tools()
                    .contains(&ctx.tool_call.name.as_str()) =>
            {
                let reason = format!(
                    "`{}` needs approval, but Loop has no approval prompt for it yet. Set it \
                     to allow or deny in settings.toolPermissions.",
                    ctx.tool_call.name
                );
                let decided = Decided::deny(DecidedBy::NoApprover, reason.clone()).after(&verdict);
                (block(reason), decided, false, None)
            }
            Some(approver) => {
                let ignore_grants = self.requires_ask(&ctx.tool_call.name, &ctx.args).await;
                let (result, review) = approver
                    .review_before_tool(ctx, cancel, ignore_grants)
                    .await;
                let mut after = None;
                let decided = match review {
                    Review::Granted => Decided::allow(DecidedBy::Grant).after(&verdict),
                    // A file tool: the review comes after the write, and its
                    // answer gets its own record then.
                    Review::Later => Decided::from_verdict(&verdict),
                    Review::Asked { escalation, grant } => {
                        after = grant.map(After::Grant);
                        let reason = result.as_ref().and_then(|r| r.reason.clone());
                        Decided::answered(result.is_none(), escalation)
                            .after(&verdict)
                            .because(reason)
                    }
                };
                (result, decided, false, after)
            }
            None => {
                let reason = headless_refusal(&ctx.tool_call.name, self.mode());
                let decided = Decided::deny(DecidedBy::NoApprover, reason.clone()).after(&verdict);
                (block(reason), decided, false, None)
            }
        }
    }

    /// The gate's after-call step, for callers outside the harness.
    pub fn after_hook(self: &Arc<Self>) -> AfterToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let allowed = gate.take_allowed(&ctx.tool_call, &ctx.args);
                let patch = match gate.approver() {
                    Some(approver) if !allowed => {
                        let ignore_grants = gate.requires_ask(&ctx.tool_call.name, &ctx.args).await;
                        let (patch, answer) = approver
                            .review_after_file_tool(ctx.clone(), cancel, ignore_grants)
                            .await;
                        // The answer to a review after the write gets its
                        // own record. The write already happened; a reject
                        // reverted it.
                        if let Some(answer) = answer {
                            let decided = Decided::answered(answer.accepted, answer.escalation);
                            let facts = Facts {
                                source: source_of(&ctx.tool_call),
                                tool: &ctx.tool_call.name,
                                kind: gate.kind(&ctx.tool_call.name).await,
                                call_id: Some(&ctx.tool_call.id),
                                args: &ctx.args,
                                caller: None,
                                session: None,
                            };
                            if let Err(error) = gate.record(facts, &decided).await {
                                tracing::warn!(%error, "file review answer not recorded");
                            }
                        }
                        patch
                    }
                    _ => None,
                };
                let Some(hooks) = gate.hooks() else {
                    return patch;
                };
                let mut patch = patch.unwrap_or_default();
                let content = patch
                    .content
                    .clone()
                    .unwrap_or_else(|| ctx.result.content.clone());
                let event = HarnessHookEvent::ToolResult {
                    call: gate.call_info(&ctx.tool_call, &ctx.args).await,
                    is_error: patch.is_error.unwrap_or(ctx.is_error),
                    output: capped_text(&content),
                };
                if let Some(extra) = hooks.emit(event).await.append_context {
                    let mut content = content;
                    content.push(ToolResultContent::Text(TextContent {
                        text: extra,
                        text_signature: None,
                    }));
                    patch.content = Some(content);
                }
                Some(patch)
            })
        })
    }
}

/// Identifies a call between the before and after hooks.
fn call_key(call: &ToolCall, args: &serde_json::Value) -> String {
    format!("{}\u{0}{}\u{0}{args}", call.id, call.name)
}

/// The text parts of a tool result, capped at [`HOOK_OUTPUT_CAP`] bytes.
fn capped_text(content: &[ToolResultContent]) -> String {
    let mut text = String::new();
    for part in content {
        if let ToolResultContent::Text(t) = part {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }
    if text.len() > HOOK_OUTPUT_CAP {
        let mut end = HOOK_OUTPUT_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_agent::BeforeToolCallContext;
    use loop_ai::{AssistantMessage, StopReason, ToolCall};

    fn call(name: &str) -> BeforeToolCallContext {
        let tool_call = ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: serde_json::json!({ "command": "ls" }),
            thought_signature: None,
        };
        BeforeToolCallContext {
            assistant_message: AssistantMessage {
                content: Vec::new(),
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                response_model: None,
                response_id: None,
                usage: Default::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: None,
                timestamp: 0,
            },
            args: tool_call.arguments.clone(),
            tool_call,
            context: Default::default(),
        }
    }

    #[tokio::test]
    async fn headless_gate_refuses_what_needs_approval() {
        let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
            &BTreeMap::from([
                ("bash".into(), "deny".into()),
                ("write".into(), "ask".into()),
            ]),
        ));
        let before = gate.before_hook();
        let blocked = before(call("bash"), None).await.expect("bash is denied");
        assert!(blocked.block);
        assert!(blocked.reason.unwrap().contains("denied"));
        let refused = before(call("write"), None)
            .await
            .expect("nobody can approve");
        assert!(refused
            .reason
            .unwrap()
            .contains("needs approval in ask mode"));
        assert!(before(call("read"), None).await.is_none());
    }

    #[tokio::test]
    async fn permission_changes_apply_to_the_next_call() {
        let gate = ToolGate::new(BTreeMap::new());
        let before = gate.before_hook();
        assert!(before(call("bash"), None).await.is_none());
        gate.set_permissions(BTreeMap::from([("bash".into(), ToolPermission::Deny)]));
        assert!(before(call("bash"), None).await.is_some());
    }

    pub(super) fn hooks(files: &[serde_json::Value]) -> (tempfile::TempDir, HookRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(i, body)| {
                let path = dir.path().join(format!("h{i}.json"));
                std::fs::write(&path, body.to_string()).unwrap();
                path
            })
            .collect();
        let registry = HookRegistry::default();
        crate::hooks_load::register_hooks(&registry, crate::hooks_load::load_hooks(&paths));
        (dir, registry)
    }

    /// A gate with `bash: ask` and a prompting approver; returns the prompt
    /// receiver so tests can see whether the user was asked.
    fn gate_with_approver(
        permissions: &[(&str, &str)],
    ) -> (
        Arc<ToolGate>,
        tokio::sync::mpsc::UnboundedReceiver<crate::tool_approval::ApprovalPrompt>,
    ) {
        let permissions = crate::tool_approval::permissions_from_settings(
            &permissions
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        let gate = ToolGate::new(permissions);
        let env: Arc<dyn loop_agent::harness::ExecutionEnv> = Arc::new(
            loop_agent::harness::HostExecutionEnv::new(std::env::temp_dir()),
        );
        let (bridge, prompts) = ToolApprovalBridge::new(env, None, Default::default());
        gate.set_approver(Some(Arc::new(bridge)));
        (gate, prompts)
    }

    /// The second layer: a locked tool that reaches the gate anyway is
    /// refused, whatever the mode, the rules, a hook, a grant or a new
    /// Policy say.
    #[tokio::test]
    async fn a_tool_lock_refuses_calls_nothing_can_widen() {
        use crate::permissions::tool_lock::{layer, ToolLock};
        use crate::permissions::{Capability, Layer, LayerSource, Setting, State};
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        std::fs::write(ws.join("a.txt"), "a").unwrap();
        let (gate, mut prompts) = gate_with_approver(&[
            ("bash", "allow"),
            ("write", "allow"),
            ("edit", "allow"),
            ("subagent", "allow"),
            ("mcp__fs__write", "allow"),
        ]);
        gate.set_workspace(ws.clone());
        *gate.cwd.lock() = Some(ws.clone());
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        gate.set_mode(PermissionMode::FullAccess);
        let before = gate.before_hook();
        // Control: unlocked, full access runs everything.
        assert!(before(call("bash"), None).await.is_none());
        assert!(gate.available("bash"));

        gate.lock_tools(ToolLock::Chat);
        let removed = [
            "bash",
            "write",
            "edit",
            "subagent",
            "todo",
            "exit_plan",
            "spawn_task",
            "mcp__fs__write",
            "brand_new_tool",
        ];
        type Change = Box<dyn Fn(&ToolGate)>;
        let widen: [(&str, Change); 5] = [
            (
                "full-access",
                Box::new(|g| g.set_mode(PermissionMode::FullAccess)),
            ),
            ("plan", Box::new(|g| g.set_mode(PermissionMode::Plan))),
            (
                "allow rules",
                Box::new(|g| {
                    g.set_permissions(BTreeMap::from([
                        ("bash".into(), ToolPermission::Allow),
                        ("write".into(), ToolPermission::Allow),
                    ]))
                }),
            ),
            // What `subagent::confine` and a future settings loader do.
            (
                "a new Policy",
                Box::new(|g| {
                    let on = Setting::always(State::On);
                    let (policy, _) =
                        crate::permissions::Policy::new(vec![Layer::new(LayerSource::Session)
                            .set(Capability::Shell, on.clone())
                            .set(Capability::Other, on)]);
                    g.set_policy(policy)
                }),
            ),
            (
                "a Policy without the lock",
                Box::new(|g| g.set_policy(Default::default())),
            ),
        ];
        for (what, change) in widen {
            change(&gate);
            for tool in removed {
                assert!(!gate.available(tool), "{what}: {tool} offered");
                let mut ctx = call(tool);
                ctx.args = serde_json::json!({"command": "touch x", "path": "a.txt"});
                let result = before(ctx, None).await.expect("refused");
                assert!(result.block, "{what}: {tool}");
                let reason = result.reason.unwrap_or_default();
                assert!(
                    reason.contains("the Runtime's tool lock"),
                    "{what}: {reason}"
                );
            }
            for tool in ["read", "grep", "glob", "ls", "web_fetch", "web_search"] {
                assert!(gate.available(tool), "{what}: {tool} removed");
            }
        }
        assert!(prompts.try_recv().is_err(), "nobody was asked");
        // A second lock does not replace the first, and the Policy shows it.
        gate.lock_tools(ToolLock::Unknown);
        assert_eq!(gate.tool_lock(), Some(ToolLock::Chat));
        let (with_lock, _) = gate.policy().with(Layer::new(LayerSource::Session));
        assert_eq!(
            gate.policy().with_only(layer(ToolLock::Chat)),
            gate.policy()
        );
        assert_ne!(with_lock, crate::permissions::Policy::default());
    }

    #[tokio::test]
    async fn hook_allow_skips_the_prompt() {
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask")]);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "match": {"tool": "bash"}, "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        assert!(gate.before_hook()(call("bash"), None).await.is_none());
        assert!(prompts.try_recv().is_err(), "the user was not asked");
    }

    #[tokio::test]
    async fn deny_rules_beat_hook_allow() {
        let (gate, _prompts) = gate_with_approver(&[("bash", "deny")]);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("bash"), None).await.unwrap();
        assert!(result.block);
        assert!(result.reason.unwrap().contains("toolPermissions"));
    }

    #[tokio::test]
    async fn hook_block_reaches_the_model_without_asking() {
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask")]);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"name": "no-ls", "on": "tool_call", "match": {"args": {"command": "ls*"}},
             "action": {"block": true, "reason": "use read"}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("bash"), None).await.unwrap();
        assert!(result.block);
        assert_eq!(
            result.reason.as_deref(),
            Some("Blocked by hook no-ls: use read")
        );
        assert!(prompts.try_recv().is_err());
    }

    #[tokio::test]
    async fn tool_result_hooks_append_context() {
        let gate = ToolGate::new(BTreeMap::new());
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_result", "match": {"tool": "bash"},
             "action": {"appendContext": "note from hook"}}
        )]);
        gate.set_hooks(registry);
        let before = call("bash");
        let ctx = loop_agent::AfterToolCallContext {
            assistant_message: before.assistant_message,
            tool_call: before.tool_call,
            args: before.args,
            result: loop_agent::AgentToolResult::text("files"),
            is_error: false,
            context: Default::default(),
        };
        let patch = gate.after_hook()(ctx, None).await.unwrap();
        let texts: Vec<_> = patch
            .content
            .unwrap()
            .into_iter()
            .filter_map(|c| match c {
                ToolResultContent::Text(t) => Some(t.text),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["files", "note from hook"]);
    }

    #[test]
    fn hook_output_is_capped_on_a_char_boundary() {
        let long = "é".repeat(HOOK_OUTPUT_CAP);
        let text = capped_text(&[ToolResultContent::Text(TextContent {
            text: long,
            text_signature: None,
        })]);
        assert!(text.len() <= HOOK_OUTPUT_CAP);
    }

    #[tokio::test]
    async fn the_headless_refusal_names_a_mode_that_would_allow_the_call() {
        let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
            &Default::default(),
        ));
        gate.set_mode(PermissionMode::AcceptEdits);
        let bash = gate.before_hook()(call("bash"), None).await.unwrap();
        let reason = bash.reason.unwrap();
        assert!(reason.contains("full-access"), "{reason}");
        assert!(!reason.contains("accept-edits ("), "{reason}");
        gate.set_mode(PermissionMode::Ask);
        let write = gate.before_hook()(call("write"), None).await.unwrap();
        assert!(write.reason.unwrap().contains("accept-edits"));
    }

    #[tokio::test]
    async fn read_only_is_a_ceiling_even_for_hook_allow() {
        let (gate, _prompts) = gate_with_approver(&[]);
        gate.set_mode(PermissionMode::ReadOnly);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("write"), None).await.unwrap();
        assert!(result.reason.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn full_access_skips_the_prompt_and_the_edit_review() {
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask"), ("write", "ask")]);
        gate.set_mode(PermissionMode::FullAccess);
        assert!(gate.before_hook()(call("bash"), None).await.is_none());
        assert!(prompts.try_recv().is_err());

        let before = call("write");
        assert!(gate.before_hook()(before.clone(), None).await.is_none());
        assert_eq!(gate.allowed.lock().len(), 2, "bash and the write");
        let ctx = loop_agent::AfterToolCallContext {
            assistant_message: before.assistant_message,
            tool_call: before.tool_call,
            args: before.args,
            result: loop_agent::AgentToolResult::text("wrote"),
            is_error: false,
            context: Default::default(),
        };
        assert!(gate.after_hook()(ctx, None).await.is_none());
        assert!(
            prompts.try_recv().is_err(),
            "no review after an allowed edit"
        );
        assert_eq!(
            gate.allowed.lock().len(),
            1,
            "the write's mark was consumed"
        );
    }

    #[tokio::test]
    async fn hooks_see_the_mode() {
        let gate = ToolGate::new(BTreeMap::new());
        gate.set_mode(PermissionMode::AcceptEdits);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "match": {"mode": "accept-edits"},
             "action": {"block": true, "reason": "seen"}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("read"), None).await.unwrap();
        assert!(result.reason.unwrap().contains("seen"));
    }

    #[tokio::test]
    async fn a_shared_call_id_does_not_skip_another_calls_review() {
        let dir = tempfile::tempdir().unwrap();
        let (gate, _prompts) = gate_with_approver(&[("write", "ask")]);
        gate.set_mode(PermissionMode::AcceptEdits);
        gate.set_workspace(dir.path().to_path_buf());
        let with_path = |path: &str| {
            let mut ctx = call("write");
            ctx.tool_call.id = String::new();
            ctx.args = serde_json::json!({ "path": path, "content": "x" });
            ctx.tool_call.arguments = ctx.args.clone();
            ctx
        };
        let inside = with_path("notes.txt");
        let outside = with_path("/etc/elsewhere.txt");
        assert!(gate.before_hook()(inside.clone(), None).await.is_none());
        assert!(gate.take_allowed(&inside.tool_call, &inside.args));
        assert!(gate.before_hook()(inside.clone(), None).await.is_none());
        assert!(
            !gate.take_allowed(&outside.tool_call, &outside.args),
            "the outside write shares the empty id but was not allowed"
        );
        gate.set_mode(PermissionMode::Ask);
        assert!(
            !gate.take_allowed(&inside.tool_call, &inside.args),
            "changing mode forgets earlier allows"
        );
    }

    #[tokio::test]
    async fn read_only_takes_off_tools_out_of_the_toolset() {
        let gate = ToolGate::new(BTreeMap::new());
        gate.set_mode(PermissionMode::ReadOnly);
        for (tool, offered) in [
            ("read", true),
            ("bash", true),
            ("web_fetch", true),
            ("write", false),
            ("edit", false),
            ("mcp__github__create_issue", false),
        ] {
            assert_eq!(gate.available(tool), offered, "{tool}");
        }
        let refused = gate.before_hook()(call("write"), None).await.unwrap();
        assert!(refused.block, "a call to a removed tool is still refused");
        gate.set_mode(PermissionMode::Ask);
        assert!(gate.available("write") && gate.available("mcp__github__create_issue"));
    }

    #[tokio::test]
    async fn policy_layers_beat_the_mode_and_hooks() {
        use crate::permissions::{Capability, Layer, LayerSource, Policy, Setting, State};
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask")]);
        gate.set_mode(PermissionMode::FullAccess);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        let (policy, _) = Policy::new(vec![
            Layer::new(LayerSource::Org).set(Capability::WriteFiles, Setting::always(State::Off)),
            Layer::new(LayerSource::Org).set(Capability::Shell, Setting::always(State::Ask)),
        ]);
        gate.set_policy(policy);

        let write = gate.before_hook()(call("write"), None).await.unwrap();
        assert!(write.reason.unwrap().contains("Org Policy turns off"));
        assert!(
            !gate.available("write"),
            "off in a layer leaves the toolset"
        );

        gate.approver().unwrap().grant_session("bash");
        let gate_for_bash = Arc::clone(&gate);
        let bash =
            tokio::spawn(async move { gate_for_bash.before_hook()(call("bash"), None).await });
        let prompt = prompts
            .recv()
            .await
            .expect("a layer's ask is not skipped by a hook");
        assert_eq!(prompt.tool_name, "bash");
        drop(prompt);
        let _ = bash.await;
    }

    fn exit_plan() -> BeforeToolCallContext {
        let mut ctx = call("exit_plan");
        ctx.args = serde_json::json!({"plan": "1. Edit main.rs\n2. Run the tests"});
        ctx.tool_call.arguments = ctx.args.clone();
        ctx
    }

    #[tokio::test]
    async fn the_user_approves_the_plan_to_leave_plan_mode() {
        use crate::tool_approval::{ApprovalDecision, ApprovalKind};
        let (gate, mut prompts) = gate_with_approver(&[]);
        for (decision, then) in [
            (ApprovalDecision::Accept, PermissionMode::Ask),
            (ApprovalDecision::AcceptSession, PermissionMode::AcceptEdits),
        ] {
            gate.set_mode(PermissionMode::Ask);
            gate.set_mode(PermissionMode::Plan);
            let before = gate.before_hook();
            let pending = tokio::spawn(async move { before(exit_plan(), None).await });
            let prompt = prompts.recv().await.unwrap();
            assert_eq!(prompt.kind, ApprovalKind::Plan);
            assert!(prompt.detail.contains("Run the tests"), "{}", prompt.detail);
            prompt.response_tx.send(decision).unwrap();
            assert!(pending.await.unwrap().is_none(), "approved: the call runs");
            assert_eq!(gate.mode(), then);
        }

        gate.set_mode(PermissionMode::Plan);
        let before = gate.before_hook();
        let pending = tokio::spawn(async move { before(exit_plan(), None).await });
        let prompt = prompts.recv().await.unwrap();
        prompt
            .response_tx
            .send(ApprovalDecision::Reject {
                reason: Some("also update the docs".into()),
            })
            .unwrap();
        let blocked = pending.await.unwrap().expect("kept planning");
        assert!(blocked.reason.unwrap().contains("also update the docs"));
        assert_eq!(gate.mode(), PermissionMode::Plan);
    }

    #[tokio::test]
    async fn approving_a_plan_tells_the_recorder_the_new_mode() {
        let (gate, mut prompts) = gate_with_approver(&[]);
        let told = Arc::new(Mutex::new(Vec::new()));
        let sink = told.clone();
        gate.set_mode_recorder(Some(Arc::new(move |mode| {
            let sink = sink.clone();
            Box::pin(async move { sink.lock().push(mode) })
        })));
        gate.set_mode(PermissionMode::FullAccess);
        gate.set_mode(PermissionMode::Plan);
        let before = gate.before_hook();
        let pending = tokio::spawn(async move { before(exit_plan(), None).await });
        let prompt = prompts.recv().await.unwrap();
        assert!(told.lock().is_empty(), "entering plan is not the gate's");
        prompt
            .response_tx
            .send(crate::tool_approval::ApprovalDecision::Accept)
            .unwrap();
        assert!(pending.await.unwrap().is_none());
        assert_eq!(*told.lock(), vec![PermissionMode::FullAccess]);
    }

    #[tokio::test]
    async fn plan_mode_returns_to_the_mode_it_was_entered_from() {
        let (gate, mut prompts) = gate_with_approver(&[]);
        gate.set_mode(PermissionMode::FullAccess);
        gate.set_mode(PermissionMode::Plan);
        gate.set_mode(PermissionMode::Plan);
        let before = gate.before_hook();
        let pending = tokio::spawn(async move { before(exit_plan(), None).await });
        let prompt = prompts.recv().await.unwrap();
        prompt
            .response_tx
            .send(crate::tool_approval::ApprovalDecision::Accept)
            .unwrap();
        assert!(pending.await.unwrap().is_none());
        assert_eq!(gate.mode(), PermissionMode::FullAccess);
    }

    #[tokio::test]
    async fn nobody_can_approve_a_plan_headless() {
        let gate = ToolGate::new(BTreeMap::new());
        gate.set_mode(PermissionMode::Plan);
        let blocked = gate.before_hook()(exit_plan(), None)
            .await
            .expect("refused");
        assert!(blocked.reason.unwrap().contains("nobody can approve"));
        assert_eq!(gate.mode(), PermissionMode::Plan);
    }

    #[test]
    fn the_search_filter_hides_what_the_rules_deny() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
            &BTreeMap::from([("read(secret/*)".into(), "deny".into())]),
        ));
        gate.set_workspace(dir.path().to_path_buf());
        let filter = gate.search_filter();
        assert!(!filter("grep", &dir.path().join("secret/key")));
        assert!(filter("grep", &dir.path().join("src/main.rs")));
        gate.set_permissions(BTreeMap::new());
        assert!(
            filter("grep", &dir.path().join("secret/key")),
            "the filter reads the rules at each call"
        );
    }

    /// The cwd can be below the workspace (the git root); a relative path
    /// is checked where the tools open it, against the cwd.
    #[cfg(unix)]
    #[tokio::test]
    async fn relative_paths_are_checked_against_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let sub = repo.join("sub");
        let secret = root.join("secret");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("key"), "k").unwrap();
        std::os::unix::fs::symlink(secret.join("key"), sub.join("notes")).unwrap();
        let gate = ToolGate::new_for_workspace(
            crate::tool_approval::permissions_from_settings(&BTreeMap::from([(
                format!("read({}/*)", secret.display()),
                "deny".into(),
            )])),
            &sub,
        );
        gate.set_mode(PermissionMode::FullAccess);
        let read = |path: &str| serde_json::json!({ "path": path });
        assert!(matches!(
            gate.decide("read", &read("notes")).await,
            Decision::Deny(_)
        ));
        assert_eq!(
            gate.decide("read", &read("other.txt")).await,
            Decision::Allow
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_search_filter_matches_relative_rules_against_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        let svc = repo.join("svc");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(svc.join("secrets")).unwrap();
        let gate = ToolGate::new_for_workspace(
            crate::tool_approval::permissions_from_settings(&BTreeMap::from([(
                "read(secrets/*)".into(),
                "deny".into(),
            )])),
            &svc,
        );
        let filter = gate.search_filter();
        assert!(!filter("grep", &svc.join("secrets/key")));
        assert!(filter("grep", &svc.join("main.rs")));
    }

    /// An ACP agent's relative path is checked against its cwd.
    #[cfg(unix)]
    #[test]
    fn agent_calls_are_checked_against_the_agent_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        let svc = repo.join("svc");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(svc.join("secrets")).unwrap();
        let gate = ToolGate::new_for_workspace(
            crate::tool_approval::permissions_from_settings(&BTreeMap::from([(
                format!("read({}/*)", svc.join("secrets").display()),
                "deny".into(),
            )])),
            &repo,
        );
        let args = serde_json::json!({ "path": "secrets/key" });
        assert!(gate
            .agent_call_denied(ToolKind::Read, &args, &svc)
            .is_some());
        assert!(gate
            .agent_call_denied(ToolKind::Read, &args, &repo)
            .is_none());
    }

    /// The parent's git root stays a base for relative deny rules in a
    /// subagent gate bound to its own cwd.
    #[cfg(unix)]
    #[tokio::test]
    async fn rule_bases_reach_the_gate_decision() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        let svc = repo.join("svc");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("secrets")).unwrap();
        std::fs::create_dir_all(&svc).unwrap();
        std::fs::write(repo.join("secrets/key"), "k").unwrap();
        let gate = ToolGate::new_for_workspace(
            crate::tool_approval::permissions_from_settings(&BTreeMap::from([(
                "read(secrets/*)".into(),
                "deny".into(),
            )])),
            &svc,
        );
        gate.set_workspace(svc.clone());
        gate.set_mode(PermissionMode::FullAccess);
        let read = serde_json::json!({ "path": "../secrets/key" });
        assert_eq!(gate.decide("read", &read).await, Decision::Allow);
        gate.set_rule_bases(vec![repo.clone()]);
        assert!(matches!(
            gate.decide("read", &read).await,
            Decision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn ask_for_a_tool_without_a_prompt_is_refused_not_run() {
        let (gate, mut prompts) = gate_with_approver(&[("mcp__github__*", "ask")]);
        let mut ctx = call("mcp__github__create_issue");
        ctx.tool_call.name = "mcp__github__create_issue".into();
        let result = gate.before_hook()(ctx, None).await.expect("refused");
        assert!(result.reason.unwrap().contains("no approval prompt"));
        assert!(prompts.try_recv().is_err());
    }
}

#[cfg(test)]
#[path = "tool_gate_decision_tests.rs"]
mod decision_tests;
