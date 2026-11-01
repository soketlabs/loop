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

use crate::permissions::{Call, Decision, PermissionMode, Policy};
use crate::tool_approval::{PlanReview, ToolApprovalBridge, ToolPermission};

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
    permissions: Mutex<BTreeMap<String, ToolPermission>>,
    approver: Mutex<Option<Arc<ToolApprovalBridge>>>,
    hooks: Mutex<Option<HookRegistry>>,
    harness: Mutex<Weak<AgentHarness>>,
    mode: Mutex<PermissionMode>,
    /// The mode plan mode was entered from, which approving a plan returns
    /// to.
    plan_return: Mutex<PermissionMode>,
    policy: Mutex<Policy>,
    workspace: Mutex<Option<PathBuf>>,
    /// Calls allowed without asking, counted by [`call_key`]; their
    /// after-call review is skipped. Keyed by more than the id, which a
    /// server may leave empty or reuse across parallel workflow tasks.
    allowed: Mutex<HashMap<String, usize>>,
    /// MCP servers, for the tools they mark read-only.
    mcp: Mutex<Option<Arc<loop_mcp::McpClientManager>>>,
}

impl ToolGate {
    /// A gate enforcing `permissions` (from `settings.toolPermissions`).
    pub fn new(permissions: BTreeMap<String, ToolPermission>) -> Arc<Self> {
        Arc::new(Self {
            permissions: Mutex::new(permissions),
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
        gate
    }

    /// Install the gate into `harness`'s tool slots and run its hooks.
    pub fn install(self: &Arc<Self>, harness: &Arc<AgentHarness>) {
        self.workspace
            .lock()
            .get_or_insert_with(|| crate::permissions::workspace_root(&harness.cwd()));
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
        self.policy.lock().available(self.mode(), &call)
    }

    /// The Policy layers over the mode.
    pub fn policy(&self) -> Policy {
        self.policy.lock().clone()
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
        let call = self.call(tool, args).await;
        self.policy
            .lock()
            .decide(self.mode(), &self.permissions.lock(), &call)
    }

    async fn call(&self, tool: &str, args: &serde_json::Value) -> Call {
        Call::resolve(tool, self.kind(tool).await, args, &self.workspace())
    }

    /// Run `hooks` without a harness (tests, hosts that only gate agents).
    pub fn set_hooks(&self, hooks: HookRegistry) {
        *self.hooks.lock() = Some(hooks);
    }

    /// The `tool_call` hooks' decision about an external agent's call.
    /// Deny rules name Loop's own tools, so they do not apply here.
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
        Arc::new(move |tool: &str, path: &std::path::Path| {
            let Some(gate) = gate.upgrade() else {
                return true;
            };
            let workspace = gate.workspace();
            let rules = gate.permissions.lock();
            crate::permissions::search_may_show(&rules, tool, path, &workspace, home.as_deref())
        })
    }

    /// The `toolPermissions` rules in force.
    pub fn permissions(&self) -> BTreeMap<String, ToolPermission> {
        self.permissions.lock().clone()
    }

    /// Replace the per-tool permissions (settings changed).
    pub fn set_permissions(&self, permissions: BTreeMap<String, ToolPermission>) {
        *self.permissions.lock() = permissions;
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
    ) -> Option<BeforeToolCallResult> {
        let block = |reason: String| {
            Some(BeforeToolCallResult {
                block: true,
                reason: Some(reason),
            })
        };
        let Some(approver) = self.approver() else {
            return block(
                "Plan mode ends when the user approves the plan, and nobody can approve it in \
                 this run. Give the plan as your answer instead."
                    .into(),
            );
        };
        let plan = ctx
            .args
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let back = *self.plan_return.lock();
        match approver.review_plan(plan, back.as_str(), cancel).await {
            PlanReview::Approved { accept_edits } => {
                self.set_mode(if accept_edits {
                    PermissionMode::AcceptEdits
                } else {
                    back
                });
                None
            }
            PlanReview::KeepPlanning(reason) => block(match reason {
                Some(reason) => format!(
                    "The user wants changes to the plan: {reason}. Stay in plan mode, revise the \
                     plan and call {EXIT_PLAN_TOOL} again."
                ),
                None => format!(
                    "The user did not approve the plan. Stay in plan mode: ask what to change, \
                     or revise the plan and call {EXIT_PLAN_TOOL} again."
                ),
            }),
        }
    }

    /// The gate's before-call check, for callers outside the harness
    /// (`mcp serve`).
    pub fn before_hook(self: &Arc<Self>) -> BeforeToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let block = |reason: String| {
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                    })
                };
                let decision = gate.decide(&ctx.tool_call.name, &ctx.args).await;
                if let Decision::Deny(reason) = decision {
                    return block(reason);
                }
                // Only the user ends plan mode: hooks don't decide this one.
                if ctx.tool_call.name == EXIT_PLAN_TOOL {
                    return gate.review_plan(&ctx, cancel).await;
                }
                if let Some(hooks) = gate.hooks() {
                    let info = gate.call_info(&ctx.tool_call, &ctx.args).await;
                    let emit = hooks.emit(HarnessHookEvent::ToolCall(info));
                    // Cancelling the turn drops the emit, which kills any
                    // command hook still running.
                    let outcome = match &cancel {
                        Some(cancel) => tokio::select! {
                            outcome = emit => outcome,
                            () = cancel.cancelled() => return block("Operation aborted".into()),
                        },
                        None => emit.await,
                    };
                    match outcome.decision {
                        Some(ToolDecision::Block { reason }) => return block(reason),
                        Some(ToolDecision::Allow) => {
                            // A Policy layer's "ask" holds even against a hook.
                            let call = gate.call(&ctx.tool_call.name, &ctx.args).await;
                            if !gate.policy.lock().requires_ask(&call) {
                                gate.mark_allowed(&ctx.tool_call, &ctx.args);
                                return None;
                            }
                        }
                        None => {}
                    }
                }
                if decision == Decision::Allow {
                    gate.mark_allowed(&ctx.tool_call, &ctx.args);
                    return None;
                }
                match gate.approver() {
                    // Only some tools have an approval prompt; asking about
                    // the others would silently run them, so refuse instead.
                    Some(_)
                        if !crate::tool_approval::interactive_tools()
                            .contains(&ctx.tool_call.name.as_str()) =>
                    {
                        block(format!(
                            "`{}` needs approval, but Loop has no approval prompt for it yet. \
                             Set it to allow or deny in settings.toolPermissions.",
                            ctx.tool_call.name
                        ))
                    }
                    Some(approver) => {
                        let call = gate.call(&ctx.tool_call.name, &ctx.args).await;
                        let ignore_grants = gate.policy.lock().requires_ask(&call);
                        approver
                            .review_before_tool(ctx, cancel, ignore_grants)
                            .await
                    }
                    None => block(headless_refusal(&ctx.tool_call.name, gate.mode())),
                }
            })
        })
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
                        let call = gate.call(&ctx.tool_call.name, &ctx.args).await;
                        let ignore_grants = gate.policy.lock().requires_ask(&call);
                        approver
                            .review_after_file_tool(ctx.clone(), cancel, ignore_grants)
                            .await
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

    fn hooks(files: &[serde_json::Value]) -> (tempfile::TempDir, HookRegistry) {
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
