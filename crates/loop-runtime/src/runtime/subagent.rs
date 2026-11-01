//! The `subagent` tool: a Session inside the Runtime opens a child Session,
//! hands it a task and receives its final answer.
//!
//! The child's Policy is its parent's plus one tightening layer, its turns
//! use the Runtime's limits, and the parent gives up its active-turn permit
//! while it waits, so parents and children cannot deadlock.
use super::{failed, now, LiveSession, Runtime};
use crate::log::{Event, EventKind};
use loop_agent::types::{AgentTool, AgentToolResult, ShouldStopAfterTurnContext};
use loop_ai::AssistantContent;
use loop_app_core::{
    permissions::{Capability, Layer, LayerSource, PermissionMode, State, ToolPermission},
    runtime::OpenedSession,
};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, SessionRef, SessionSpec, TurnOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex, Weak,
    },
};
use std::{future::Future, pin::Pin};
use tokio::sync::oneshot;

/// Resolves when the call that started a child must stop.
type Stop = Pin<Box<dyn Future<Output = ()> + Send>>;
const CANCELLED: &str = "Subagent was cancelled.";

/// Resolves once the parent turn that began at `epoch` is cancelled. The
/// epoch is the turn's starting cancellation count, so a cancellation that
/// landed before this is first polled still counts.
async fn turn_cancelled(parent: &LiveSession, epoch: u64) {
    let mut cancel = parent.cancel.subscribe();
    loop {
        if *cancel.borrow_and_update() != epoch || cancel.changed().await.is_err() {
            return;
        }
    }
}

impl LiveSession {
    fn cancelled_since(&self, epoch: u64) -> bool {
        *self.cancel.borrow() != epoch
    }
}

pub(super) const DEFAULT_DEPTH: u32 = 2;
const TOOL: &str = "subagent";

/// What `open` needs to create a child Session.
pub(super) struct ChildOpen {
    pub(super) parent: Arc<LiveSession>,
    pub(super) cwd: PathBuf,
    pub(super) max_turns: Option<u32>,
}

/// A child's final answer and model calls, read by the waiting parent.
#[derive(Default)]
pub(super) struct ChildState {
    answer: Mutex<String>,
    calls: AtomicU32,
    stopped: AtomicBool,
}

impl ChildState {
    /// Count model calls and keep the latest answer text. With `max_turns`,
    /// end the run once that many calls are made and more work is pending.
    pub(super) fn install(opened: &OpenedSession, max_turns: Option<u32>) -> Arc<Self> {
        let state = Arc::new(Self::default());
        let seen = Arc::clone(&state);
        opened.harness.set_should_stop_after_turn(Some(Arc::new(
            move |ctx: ShouldStopAfterTurnContext| {
                let calls = seen.calls.fetch_add(1, Ordering::AcqRel) + 1;
                let text: String = ctx
                    .message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text.trim().is_empty() {
                    *seen.answer.lock().unwrap() = text;
                }
                let pending = ctx
                    .message
                    .content
                    .iter()
                    .any(|c| matches!(c, AssistantContent::ToolCall(_)));
                let stop = pending && max_turns.is_some_and(|max| calls >= max);
                if stop {
                    seen.stopped.store(true, Ordering::Release);
                }
                Box::pin(std::future::ready(stop))
            },
        )));
        state
    }
}

/// Tighten a child's gate under its parent's and return the child's
/// workspace. The child takes the parent's mode and rules, never its grants,
/// and the parent's Policy plus a Session layer that keeps whatever the
/// parent's mode turns off turned off, along with MCP servers the parent
/// does not have.
pub(super) async fn confine(parent: &LiveSession, child: &OpenedSession) -> PathBuf {
    let gate = &parent.opened.tool_gate;
    let target = &child.tool_gate;
    // A child cannot leave plan mode; it may only investigate. Its own
    // workspace may set a stricter mode than the parent's.
    let plain = |mode| match mode {
        PermissionMode::Plan => PermissionMode::ReadOnly,
        mode => mode,
    };
    let mode = plain(gate.mode()).min(plain(target.mode()));
    let parent_servers: BTreeSet<String> = parent
        .opened
        .workspace
        .mcp_client
        .connections()
        .read()
        .await
        .keys()
        .cloned()
        .collect();
    let child_servers: Vec<String> = child
        .workspace
        .mcp_client
        .connections()
        .read()
        .await
        .keys()
        .cloned()
        .collect();
    let mut layer = Layer::new(LayerSource::Session);
    let mut capabilities = vec![
        Capability::ReadFiles,
        Capability::WriteFiles,
        Capability::Shell,
        Capability::WebFetch,
        Capability::WebSearch,
        Capability::Other,
    ];
    capabilities.extend(parent_servers.iter().cloned().map(Capability::Mcp));
    for capability in capabilities {
        let setting = mode.setting(&capability);
        if setting.state == State::Off {
            layer = layer.set(capability, setting);
        }
    }
    for server in child_servers {
        if !parent_servers.contains(&server) {
            layer = layer.set(
                Capability::Mcp(server),
                loop_app_core::permissions::Setting::always(State::Off),
            );
        }
    }
    let (policy, _) = gate.policy().with(layer);
    target.set_permissions(merge_rules(&gate.permissions(), &target.permissions()));
    target.set_mode(mode);
    target.set_policy(policy);
    let workspace = if child.workspace.cwd == parent.opened.workspace.cwd {
        parent.workspace.clone()
    } else {
        child.workspace.cwd.clone()
    };
    target.set_workspace(workspace.clone());
    workspace
}

/// Offer the `subagent` tool to a live Session. Spawning is allowed unless
/// the user's rules say otherwise: the child can do no more than its parent.
pub(super) async fn offer(rt: Weak<Runtime>, live: &Arc<LiveSession>) -> Result<(), String> {
    let gate = &live.opened.tool_gate;
    let mut permissions = gate.permissions();
    permissions
        .entry(TOOL.into())
        .or_insert(ToolPermission::Allow);
    gate.set_permissions(permissions);
    let mut tools = live.opened.harness.get_tools().await;
    tools.retain(|tool| tool.name != TOOL);
    tools.push(tool(rt, Arc::downgrade(live)));
    live.opened
        .harness
        .set_tools(tools)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct Args {
    task: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
}

fn tool(rt: Weak<Runtime>, parent: Weak<LiveSession>) -> AgentTool {
    AgentTool::simple(
        TOOL,
        "Subagent",
        "Run a task in a new child Session and return its final answer. The child starts \
         without this conversation's history, so describe the task completely. It has at \
         most this Session's permissions. Several calls in one turn run in parallel.",
        json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "Complete instructions for the child"},
                "cwd": {"type": "string", "description": "Working directory inside this workspace; defaults to this Session's"},
                "max_turns": {"type": "integer", "minimum": 1, "description": "Stop the child after this many model calls"}
            },
            "required": ["task"]
        }),
        move |_id, args: Value, cancel, _update| {
            let rt = rt.clone();
            let parent = parent.clone();
            async move {
                let args: Args = serde_json::from_value(args)
                    .map_err(|e| format!("invalid subagent arguments: {e}"))?;
                let rt = rt.upgrade().ok_or("Runtime stopped")?;
                let parent = parent.upgrade().ok_or("Session closed")?;
                // Taken at dispatch: the turn's own cancellation count, so a
                // cancel before the detached task runs is not lost.
                let epoch = parent.turn_epoch.load(Ordering::Acquire);
                // The child's lifecycle belongs to the Runtime: it finishes,
                // closes the child and restores the permit even if this
                // call is dropped. Dropping it counts as cancellation, as do
                // the parent turn's cancellation and the harness's token.
                let (abandon, abandoned) = oneshot::channel::<()>();
                let token: Stop = match cancel {
                    Some(token) => Box::pin(async move { token.cancelled().await }),
                    None => Box::pin(std::future::pending()),
                };
                let watched = Arc::clone(&parent);
                let stop: Stop = Box::pin(async move {
                    tokio::select! {
                        _ = turn_cancelled(&watched, epoch) => {}
                        _ = token => {}
                        _ = abandoned => {}
                    }
                });
                let task =
                    tokio::spawn(async move { rt.run_subagent(parent, args, epoch, stop).await });
                let result = task.await.map_err(|e| e.to_string())?;
                drop(abandon);
                result.map(AgentToolResult::text)
            }
        },
    )
}

impl Runtime {
    async fn run_subagent(
        self: Arc<Self>,
        parent: Arc<LiveSession>,
        args: Args,
        epoch: u64,
        mut stop: Stop,
    ) -> Result<String, String> {
        if parent.depth >= self.subagent_depth {
            return Err(format!(
                "Subagent depth limit reached: a Session at depth {} cannot spawn another",
                parent.depth
            ));
        }
        let cwd = resolve_cwd(&parent, args.cwd.as_deref())?;
        if parent.cancelled_since(epoch) {
            return Err(CANCELLED.into());
        }
        self.enter_wait(&parent).await;
        let (result, halted) = self.child_turn(&parent, cwd, args, epoch, &mut stop).await;
        let back = self.leave_wait(&parent, epoch, halted).await;
        match (result, back) {
            (Ok(answer), Ok(())) => Ok(answer),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(format!("parent turn ended: {error}")),
        }
    }

    // The first waiting call gives up the parent's active-turn permit; the
    // last one takes it back. Per-call release would let a finished sibling
    // reacquire while another child still queues for a permit.
    async fn enter_wait(&self, parent: &LiveSession) {
        let mut waiting = parent.waiting.lock().await;
        if *waiting == 0 {
            parent.permit.lock().await.take();
        }
        *waiting += 1;
    }

    // A cancelled or halted turn never takes a permit back: it would queue
    // behind unrelated turns only to end. The permit mutex is not held while
    // waiting, so the turn's own cleanup is never blocked.
    async fn leave_wait(
        &self,
        parent: &LiveSession,
        epoch: u64,
        halted: bool,
    ) -> Result<(), BackendError> {
        let mut waiting = parent.waiting.lock().await;
        *waiting -= 1;
        if *waiting > 0 {
            return Ok(());
        }
        let ended = || failed("Session turn ended");
        if halted || parent.cancelled_since(epoch) {
            return Err(ended());
        }
        let permit = tokio::select! {
            permit = self.turns.clone().acquire_owned() => permit.map_err(failed)?,
            _ = turn_cancelled(parent, epoch) => return Err(ended()),
        };
        let mut slot = parent.permit.lock().await;
        if parent.cancelled_since(epoch)
            || parent.closing.load(Ordering::Acquire)
            || !parent.busy.load(Ordering::Acquire)
        {
            return Err(ended());
        }
        *slot = Some(permit);
        Ok(())
    }

    async fn child_turn(
        &self,
        parent: &Arc<LiveSession>,
        cwd: PathBuf,
        args: Args,
        epoch: u64,
        stop: &mut Stop,
    ) -> (Result<String, String>, bool) {
        let open = ChildOpen {
            parent: Arc::clone(parent),
            cwd: cwd.clone(),
            max_turns: args.max_turns,
        };
        let spec = SessionSpec {
            name: Some(title(&args.task)),
            ..Default::default()
        };
        let child = match self.open(None, spec, Some(open)).await {
            Ok(child) => child,
            Err(BackendError::Busy) => {
                let error = format!(
                    "the Runtime is at its limit of {} live Sessions; run fewer subagents at once",
                    self.limits.max_sessions
                );
                return (Err(error), false);
            }
            Err(error) => {
                return (
                    Err(format!("could not open a child Session: {error}")),
                    false,
                )
            }
        };
        let mut halted = false;
        self.log_parent(
            parent,
            EventKind::SubagentStarted(json!({
                "child": child.key, "task": args.task, "cwd": cwd, "max_turns": args.max_turns,
                "depth": parent.depth + 1,
            })),
        )
        .await;
        let outcome = async {
            // Cancelled while the child was opening: never prompt it.
            if parent.cancelled_since(epoch) {
                halted = true;
                return Ok(TurnOutcome::Cancelled);
            }
            let turn = self
                .prompt(&child, vec![ContentBlock::text(args.task.clone())])
                .await
                .map_err(|e| format!("could not start the child: {e}"))?;
            tokio::select! {
                outcome = turn.outcome() => Ok(outcome),
                _ = stop => {
                    // Cancel cascades: the child's own subagent calls watch
                    // its cancellation in turn.
                    halted = true;
                    self.cancel(&child);
                    Ok(TurnOutcome::Cancelled)
                }
            }
        }
        .await;
        let state = self
            .route(&child)
            .ok()
            .and_then(|live| live.child.clone())
            .unwrap_or_default();
        let force = !matches!(outcome, Ok(TurnOutcome::Completed(_)));
        let closed = self.close_child(&child, force).await;
        let calls = state.calls.load(Ordering::Acquire);
        let answer = state.answer.lock().unwrap().clone();
        let stopped = state.stopped.load(Ordering::Acquire);
        let (status, result) = match outcome {
            Ok(TurnOutcome::Completed(_)) if stopped => (
                "stopped",
                Ok(format!(
                    "Subagent stopped after {calls} model calls (max_turns) before finishing.{}",
                    last_output(&answer)
                )),
            ),
            Ok(TurnOutcome::Completed(_)) if answer.is_empty() => (
                "completed",
                Ok("Subagent finished without an answer.".into()),
            ),
            Ok(TurnOutcome::Completed(_)) => ("completed", Ok(answer)),
            Ok(TurnOutcome::Cancelled) => ("cancelled", Err(CANCELLED.into())),
            Ok(TurnOutcome::Failed(info)) => {
                ("failed", Err(format!("Subagent failed: {}", info.message)))
            }
            Ok(TurnOutcome::Uncertain) => (
                "uncertain",
                Err("Subagent's outcome is uncertain; it may have made changes.".into()),
            ),
            Err(error) => ("failed", Err(error)),
        };
        // `closed` is the close error, or null when the child closed; older
        // logs only have that field. `closed_ok` states the outcome directly.
        let ended = json!({
            "child": child.key, "status": status, "model_calls": calls,
            "closed": closed.as_ref().map_err(ToString::to_string).err(),
            "closed_ok": closed.is_ok(),
        });
        self.log_parent(parent, EventKind::SubagentEnded(ended))
            .await;
        (result, halted)
    }

    async fn close_child(&self, child: &SessionRef, force: bool) -> Result<(), BackendError> {
        match self.close(&child.key, force).await {
            Err(BackendError::Busy) => self.close(&child.key, true).await,
            other => other,
        }
    }

    async fn log_parent(&self, parent: &LiveSession, kind: EventKind) {
        let ts_ms = now();
        if let Err(error) = self
            .log
            .append_batch(vec![Event {
                generation: self.generation,
                session_key: Some(parent.reference.key.clone()),
                turn: None,
                ts_ms,
                kind,
            }])
            .await
        {
            super::fail(parent, failed(error));
        }
    }
}

/// A child's display name: the first line of its task, shortened.
fn title(task: &str) -> String {
    const MAX: usize = 60;
    let line = task
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Subagent")
        .trim();
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let mut short: String = line.chars().take(MAX - 1).collect();
    short.push('…');
    short
}

fn last_output(answer: &str) -> String {
    if answer.is_empty() {
        String::new()
    } else {
        format!(" Last output:\n{answer}")
    }
}

/// The child's cwd: the parent's by default, relative paths from it, and
/// never outside the parent's workspace.
fn resolve_cwd(parent: &LiveSession, cwd: Option<&str>) -> Result<PathBuf, String> {
    let base = &parent.opened.workspace.cwd;
    let Some(cwd) = cwd else {
        return Ok(base.clone());
    };
    let path = Path::new(cwd);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let resolved = joined
        .canonicalize()
        .map_err(|e| format!("subagent cwd {}: {e}", joined.display()))?;
    let root = parent
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| parent.workspace.clone());
    if !resolved.is_dir() {
        return Err(format!(
            "subagent cwd {} is not a directory",
            resolved.display()
        ));
    }
    if !resolved.starts_with(&root) {
        return Err(format!(
            "subagent cwd {} is outside this Session's workspace {}",
            resolved.display(),
            root.display()
        ));
    }
    Ok(resolved)
}

/// A child's `toolPermissions`: the stricter of its parent's and its own
/// workspace's rule for each key. A child workspace's own `allow` rules are
/// dropped, since they could lift an ask its parent keeps.
pub(super) fn merge_rules(
    parent: &BTreeMap<String, ToolPermission>,
    child: &BTreeMap<String, ToolPermission>,
) -> BTreeMap<String, ToolPermission> {
    let mut merged = parent.clone();
    for (key, &own) in child {
        match merged.get(key) {
            Some(inherited) if own.strictness() > inherited.strictness() => {
                merged.insert(key.clone(), own);
            }
            Some(_) => {}
            None if own != ToolPermission::Allow => {
                merged.insert(key.clone(), own);
            }
            None => {}
        }
    }
    merged
}
