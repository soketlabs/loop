//! `subagent` tool behavior over a faux model that answers by task, so
//! concurrent Sessions never take each other's scripted responses.
use super::*;
use crate::log::{EventLog, LoggedEvent, MemoryEventLog};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall, ToolResultContent, UserContent, UserMessageContent,
};
use loop_app_core::permissions::{Capability, Layer, LayerSource, PermissionMode, Setting, State};
use serde_json::Value;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
};

const LIMIT: Duration = Duration::from_secs(60);

/// One model call as the faux model sees it.
#[derive(Clone)]
struct Turn {
    /// The Session's first prompt: its role in a test.
    task: String,
    /// 1-based model call within the run.
    call: usize,
}

type Script = Arc<dyn Fn(Turn) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> + Send + Sync>;

/// What every task saw: offered tools, tool results and model calls.
#[derive(Default)]
struct Seen {
    tools: Mutex<HashMap<String, Vec<String>>>,
    results: Mutex<HashMap<String, Vec<String>>>,
    calls: Mutex<HashMap<String, usize>>,
}
impl Seen {
    fn tools(&self, task: &str) -> Vec<String> {
        self.tools
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .unwrap_or_default()
    }
    fn results(&self, task: &str) -> Vec<String> {
        self.results
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .unwrap_or_default()
    }
    fn calls(&self, task: &str) -> usize {
        self.calls.lock().unwrap().get(task).copied().unwrap_or(0)
    }
}

fn user_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(match &user.content {
        UserMessageContent::Text(text) => text.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect(),
    })
}

fn observe(seen: &Seen, context: &Context) -> Turn {
    let task = context
        .messages
        .iter()
        .find_map(user_text)
        .unwrap_or_default();
    let call = 1 + context
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Assistant(_)))
        .count();
    let tools = context
        .tools
        .iter()
        .flatten()
        .map(|t| t.name.clone())
        .collect();
    seen.tools.lock().unwrap().insert(task.clone(), tools);
    *seen.calls.lock().unwrap().entry(task.clone()).or_default() += 1;
    let mut results = Vec::new();
    for message in context.messages.iter().rev() {
        let Message::ToolResult(result) = message else {
            break;
        };
        for content in &result.content {
            if let ToolResultContent::Text(t) = content {
                results.push(t.text.clone());
            }
        }
    }
    if !results.is_empty() {
        seen.results
            .lock()
            .unwrap()
            .entry(task.clone())
            .or_default()
            .extend(results);
    }
    Turn { task, call }
}

static IDS: AtomicU64 = AtomicU64::new(0);
fn calls(list: Vec<(&str, Value)>) -> FauxResponse {
    FauxResponse::ToolCalls(
        list.into_iter()
            .map(|(name, arguments)| ToolCall {
                id: format!("call-{}", IDS.fetch_add(1, Ordering::Relaxed)),
                name: name.into(),
                arguments,
                thought_signature: None,
            })
            .collect(),
    )
}
fn spawn(task: &str) -> (&'static str, Value) {
    ("subagent", json!({ "task": task }))
}
fn text(t: impl Into<String>) -> FauxResponse {
    FauxResponse::Text(t.into())
}
fn ready(r: FauxResponse) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> {
    Box::pin(std::future::ready(r))
}

fn opts(cwd: &std::path::Path, mode: &str) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some(mode.into()),
        agent: None,
    }
}

fn globals(dir: &std::path::Path, script: Script) -> (GlobalResources, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let watch = Arc::clone(&seen);
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(move |context: &Context| {
        script(observe(&watch, context))
    })));
    (
        GlobalResources::with_models(dir.join("agent"), models).unwrap(),
        seen,
    )
}

async fn runtime(
    dir: &std::path::Path,
    mode: &str,
    limits: RuntimeLimits,
    script: Script,
) -> (Arc<Runtime>, Arc<MemoryEventLog>, Arc<Seen>) {
    let (globals, seen) = globals(dir, script);
    let log = Arc::new(MemoryEventLog::new());
    let rt = Runtime::new(globals, opts(dir, mode), log.clone(), limits)
        .await
        .unwrap()
        .with_close_grace(Duration::from_millis(500))
        .into_shared();
    (rt, log, seen)
}

async fn run(rt: &Runtime, session: &SessionRef, prompt: &str) -> TurnOutcome {
    rt.prompt(session, vec![ContentBlock::text(prompt)])
        .await
        .unwrap()
        .outcome()
        .await
}

fn children(list: &[SessionInfo], parent: &SessionRef) -> Vec<SessionInfo> {
    list.iter()
        .filter(|s| s.parent.as_ref() == Some(parent))
        .cloned()
        .collect()
}

fn kinds(events: &[LoggedEvent], key: &SessionKey) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event.session_key.as_ref() == Some(key))
        .map(|e| {
            serde_json::to_value(&e.event.kind).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_children_complete_and_the_parent_gets_both_answers() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Barrier::new(3));
        let release = Arc::new(tokio::sync::Barrier::new(3));
        let (b1, b2) = (started.clone(), release.clone());
        let script: Script = Arc::new(move |turn: Turn| {
            let (b1, b2) = (b1.clone(), b2.clone());
            match (turn.task.as_str(), turn.call) {
                ("root", 1) => ready(calls(vec![spawn("child A"), spawn("child B")])),
                ("root", _) => ready(text("root done")),
                (task, _) => {
                    let task = task.to_owned();
                    Box::pin(async move {
                        // Both children must be in a model call at once.
                        b1.wait().await;
                        b2.wait().await;
                        text(format!("answer from {task}"))
                    })
                }
            }
        });
        let (rt, log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&root, vec![ContentBlock::text("root")])
            .await
            .unwrap();
        started.wait().await;
        let live = children(&rt.list().await.unwrap(), &root);
        assert_eq!(live.len(), 2, "both children are listed under the parent");
        assert!(live.iter().all(|c| c.live));
        release.wait().await;
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));

        let results = seen.results("root");
        assert!(
            results.contains(&"answer from child A".to_owned()),
            "{results:?}"
        );
        assert!(
            results.contains(&"answer from child B".to_owned()),
            "{results:?}"
        );
        let after = children(&rt.list().await.unwrap(), &root);
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|c| !c.live), "finished children close");

        let events = log.read_runtime_after(0).await.unwrap();
        let parent = kinds(&events, &root.key);
        assert_eq!(
            parent.iter().filter(|k| *k == "subagent_started").count(),
            2
        );
        assert_eq!(parent.iter().filter(|k| *k == "subagent_ended").count(), 2);
        for child in &after {
            let kinds = kinds(&events, &child.r#ref.key);
            assert_eq!(kinds.first().map(String::as_str), Some("session_opened"));
            assert!(kinds.contains(&"prompt_sent".to_owned()));
            assert!(kinds.contains(&"update".to_owned()));
            assert_eq!(kinds.last().map(String::as_str), Some("session_closed"));
            let opened = events
                .iter()
                .find(|e| e.event.session_key.as_ref() == Some(&child.r#ref.key))
                .unwrap();
            let EventKind::SessionOpened(payload) = &opened.event.kind else {
                panic!()
            };
            assert_eq!(payload["parent"], json!(root.key));
            assert_eq!(payload["depth"], json!(1));
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_cannot_exceed_its_parents_capabilities() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![
                spawn("writer"),
                ("subagent", json!({"task": "escape", "cwd": "/"})),
            ])),
            ("writer", 1) => ready(calls(vec![(
                "bash",
                json!({"command": "touch child-made"}),
            )])),
            _ => ready(text("done")),
        });
        // The Runtime default would give a child full access.
        let (rt, _log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let gate = rt.route(&root).unwrap().opened.tool_gate.clone();
        gate.set_mode(PermissionMode::ReadOnly);
        let (policy, _) = gate.policy().with(
            Layer::new(LayerSource::User).set(Capability::WebFetch, Setting::always(State::Off)),
        );
        gate.set_policy(policy);
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));

        let tools = seen.tools("writer");
        assert!(tools.contains(&"read".to_owned()), "{tools:?}");
        assert!(
            !tools.contains(&"write".to_owned()),
            "read-only parent: {tools:?}"
        );
        assert!(
            !tools.contains(&"web_fetch".to_owned()),
            "parent Policy: {tools:?}"
        );
        assert!(!dir.path().join("child-made").exists());
        let writer = seen.results("writer");
        assert_eq!(writer.len(), 1);
        assert!(writer[0].contains("read-only"), "{writer:?}");
        assert_eq!(
            seen.calls("escape"),
            0,
            "a cwd outside the workspace is refused"
        );
        let root_results = seen.results("root");
        assert!(
            root_results.iter().any(|r| r.contains("outside")),
            "{root_results:?}"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

fn nest() -> Script {
    Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
        ("root", 1) => ready(calls(vec![spawn("level1")])),
        ("level1", 1) => ready(calls(vec![spawn("level2")])),
        ("level2", 1) => ready(calls(vec![spawn("level3")])),
        (task, _) => ready(text(format!("{task} done"))),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn depth_is_limited_to_two_below_the_root() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), nest()).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        assert!(seen.tools("root").contains(&"subagent".to_owned()));
        assert!(seen.tools("level1").contains(&"subagent".to_owned()));
        assert!(!seen.tools("level2").contains(&"subagent".to_owned()));
        assert_eq!(seen.calls("level2"), 2, "depth 2 still works");
        assert_eq!(seen.calls("level3"), 0);
        let list = rt.list().await.unwrap();
        assert_eq!(list.len(), 3);
        let level1 = children(&list, &root);
        assert_eq!(level1.len(), 1);
        assert_eq!(children(&list, &level1[0].r#ref).len(), 1);
        assert!(seen.results("level1")[0].contains("level2 done"));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn depth_limit_is_configurable() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (globals, seen) = globals(dir.path(), nest());
        let rt = Runtime::new(
            globals,
            opts(dir.path(), "full-access"),
            Arc::new(MemoryEventLog::new()),
            RuntimeLimits::default(),
        )
        .await
        .unwrap()
        .with_subagent_depth(1)
        .into_shared();
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        assert!(!seen.tools("level1").contains(&"subagent".to_owned()));
        assert_eq!(seen.calls("level2"), 0);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_turns_stops_the_child_after_that_many_model_calls() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![(
                "subagent",
                json!({"task": "busy", "max_turns": 2}),
            )])),
            ("busy", _) => ready(calls(vec![("ls", json!({"path": "."}))])),
            _ => ready(text("done")),
        });
        let (rt, _log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        assert_eq!(seen.calls("busy"), 2);
        let results = seen.results("root");
        assert!(
            results[0].contains("stopped after 2 model calls"),
            "{results:?}"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parents_at_the_active_turn_cap_do_not_deadlock() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        // All eight children must hold turn permits at once, which only
        // works if each waiting parent gave its permit up.
        let together = Arc::new(tokio::sync::Barrier::new(8));
        let script: Script = Arc::new(move |turn: Turn| {
            let together = together.clone();
            match (turn.task.as_str(), turn.call) {
                ("root", 1) => ready(calls(vec![spawn("kid")])),
                ("kid", _) => Box::pin(async move {
                    together.wait().await;
                    text("kid done")
                }),
                _ => ready(text("done")),
            }
        });
        let (rt, _log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let mut roots = Vec::new();
        for _ in 0..8 {
            roots.push(rt.new_session(SessionSpec::default()).await.unwrap());
        }
        let outcomes =
            futures::future::join_all(roots.iter().map(|root| run(&rt, root, "root"))).await;
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, TurnOutcome::Completed(_))));
        assert_eq!(seen.calls("kid"), 8);
        assert_eq!(
            seen.results("root")
                .iter()
                .filter(|r| *r == "kid done")
                .count(),
            8
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_parent_cancels_its_descendants() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let stuck = Arc::new(tokio::sync::Notify::new());
        let signal = stuck.clone();
        let script: Script = Arc::new(move |turn: Turn| {
            let signal = signal.clone();
            match (turn.task.as_str(), turn.call) {
                ("root", 1) => ready(calls(vec![spawn("middle")])),
                ("middle", 1) => ready(calls(vec![spawn("stuck")])),
                ("stuck", _) => Box::pin(async move {
                    signal.notify_one();
                    std::future::pending().await
                }),
                _ => ready(text("done")),
            }
        });
        let (rt, log, _seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&root, vec![ContentBlock::text("root")])
            .await
            .unwrap();
        stuck.notified().await;
        let list = rt.list().await.unwrap();
        let middle = children(&list, &root);
        assert_eq!(middle.len(), 1);
        let leaf = children(&list, &middle[0].r#ref);
        assert_eq!(leaf.len(), 1);
        assert!(leaf[0].live && leaf[0].busy);

        rt.cancel(&root);
        assert!(matches!(turn.outcome().await, TurnOutcome::Cancelled));
        // Every descendant closes, and the root learns its child ended.
        loop {
            let ended = kinds(&log.read_runtime_after(0).await.unwrap(), &root.key)
                .contains(&"subagent_ended".to_owned());
            if ended && rt.sessions.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let events = log.read_runtime_after(0).await.unwrap();
        for session in [&middle[0].r#ref, &leaf[0].r#ref] {
            let kinds = kinds(&events, &session.key);
            assert!(kinds.contains(&"cancelled".to_owned()), "{kinds:?}");
            assert!(kinds.contains(&"session_closed".to_owned()), "{kinds:?}");
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tool_is_offered_only_inside_a_shared_runtime() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|_| ready(text("hi")));
        let names = |tools: Vec<loop_agent::types::AgentTool>| {
            tools.into_iter().map(|t| t.name).collect::<Vec<_>>()
        };

        // Standalone Surfaces build Sessions through the factory directly.
        let (standalone, _) = globals(dir.path(), script.clone());
        let factory = SessionFactory::new(Arc::new(standalone));
        let o = opts(dir.path(), "full-access");
        let workspace = factory.workspace(&o).await.unwrap();
        let opened = factory.open(workspace, &o).await.unwrap();
        assert!(!names(opened.harness.get_tools().await).contains(&"subagent".to_owned()));

        let other = tempfile::tempdir().unwrap();
        let (globals, _) = globals(other.path(), script);
        let plain = Runtime::new(
            globals,
            opts(other.path(), "full-access"),
            Arc::new(MemoryEventLog::new()),
            RuntimeLimits::default(),
        )
        .await
        .unwrap();
        let session = plain.new_session(SessionSpec::default()).await.unwrap();
        let tools = names(
            plain
                .route(&session)
                .unwrap()
                .opened
                .harness
                .get_tools()
                .await,
        );
        assert!(!tools.contains(&"subagent".to_owned()));
        let shared = plain.into_shared();
        let second = shared.new_session(SessionSpec::default()).await.unwrap();
        let tools = names(
            shared
                .route(&second)
                .unwrap()
                .opened
                .harness
                .get_tools()
                .await,
        );
        assert!(tools.contains(&"subagent".to_owned()));
        shared.shutdown().await;
    })
    .await
    .unwrap();
}
