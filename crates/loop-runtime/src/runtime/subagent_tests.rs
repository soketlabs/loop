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
        assert_eq!(
            rt.sessions.lock().unwrap().len(),
            1,
            "only the root stays loaded"
        );
        for child in &after {
            let lease = rt.store.lease(&child.r#ref.native_id).await.unwrap();
            assert!(lease.is_none(), "a finished child releases its lease");
        }

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

/// Wait until `session` has logged an event whose JSON contains `needle`.
async fn logged(
    log: &MemoryEventLog,
    session: impl Fn(&[SessionInfo]) -> Option<SessionKey>,
    rt: &Runtime,
    needle: &str,
) -> SessionKey {
    loop {
        if let Some(key) = session(&rt.list().await.unwrap()) {
            let events = log.read_runtime_after(0).await.unwrap();
            if events.iter().any(|e| {
                e.event.session_key.as_ref() == Some(&key)
                    && serde_json::to_string(&e.event.kind)
                        .unwrap()
                        .contains(needle)
            }) {
                return key;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn payload<'a>(events: &'a [LoggedEvent], key: &SessionKey, kind: &str) -> Option<&'a Value> {
    events.iter().rev().find_map(|e| {
        if e.event.session_key.as_ref() != Some(key) {
            return None;
        }
        match (&e.event.kind, kind) {
            (EventKind::SessionClosed(p), "session_closed") => Some(p),
            (EventKind::SubagentEnded(p), "subagent_ended") => Some(p),
            _ => None,
        }
    })
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_parent_cancels_its_descendants() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![spawn("middle")])),
            ("middle", 1) => ready(calls(vec![spawn("leaf")])),
            ("leaf", 1) => ready(calls(vec![("bash", json!({"command": "sleep 30"}))])),
            _ => ready(text("done")),
        });
        let (globals, _seen) = globals(dir.path(), script);
        let log = Arc::new(MemoryEventLog::new());
        // The default close grace: a cascade that relied on aborting stuck
        // turns would take seconds per level and record `aborted`.
        let rt = Runtime::new(
            globals,
            opts(dir.path(), "full-access"),
            log.clone(),
            RuntimeLimits::default(),
        )
        .await
        .unwrap()
        .into_shared();
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&root, vec![ContentBlock::text("root")])
            .await
            .unwrap();
        let r = root.clone();
        let leaf = logged(
            &log,
            move |list| {
                let middle = children(list, &r).pop()?;
                Some(children(list, &middle.r#ref).pop()?.r#ref.key)
            },
            &rt,
            "sleep 30",
        )
        .await;
        let list = rt.list().await.unwrap();
        let middle = children(&list, &root).pop().unwrap().r#ref.key;

        let started = std::time::Instant::now();
        rt.cancel(&root);
        assert!(matches!(turn.outcome().await, TurnOutcome::Cancelled));
        loop {
            let ended = kinds(&log.read_runtime_after(0).await.unwrap(), &root.key)
                .contains(&"subagent_ended".to_owned());
            if ended && rt.sessions.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let events = log.read_runtime_after(0).await.unwrap();
        for key in [&middle, &leaf] {
            let kinds = kinds(&events, key);
            assert!(kinds.contains(&"cancelled".to_owned()), "{kinds:?}");
            let closed = payload(&events, key, "session_closed").unwrap();
            assert_eq!(closed["aborted"], json!(false), "{closed}");
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_mode_spawns_without_asking_and_cancel_withdraws_child_escalations() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![spawn("helper")])),
            ("asking root", 1) => ready(calls(vec![spawn("asker")])),
            ("asker", 1) => ready(calls(vec![("bash", json!({"command": "echo hi"}))])),
            ("helper", _) => ready(text("helped")),
            _ => ready(text("done")),
        });
        // `loop serve` defaults to ask mode; no Surface is attached here.
        let (rt, log, seen) = runtime(dir.path(), "ask", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        assert_eq!(seen.results("root"), vec!["helped".to_owned()]);

        let other = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&other, vec![ContentBlock::text("asking root")])
            .await
            .unwrap();
        let o = other.clone();
        let asker = logged(
            &log,
            move |list| Some(children(list, &o).pop()?.r#ref.key),
            &rt,
            "escalation_raised",
        )
        .await;
        rt.cancel(&other);
        assert!(matches!(turn.outcome().await, TurnOutcome::Cancelled));
        loop {
            let kinds = kinds(&log.read_runtime_after(0).await.unwrap(), &asker);
            if kinds.contains(&"session_closed".to_owned()) {
                assert!(
                    kinds.contains(&"escalation_withdrawn".to_owned()),
                    "{kinds:?}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_parent_does_not_wait_for_an_unrelated_turn() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let kid_started = Arc::new(tokio::sync::Notify::new());
        let other_started = Arc::new(tokio::sync::Notify::new());
        let release_other = Arc::new(tokio::sync::Notify::new());
        let (k, o, r) = (
            kid_started.clone(),
            other_started.clone(),
            release_other.clone(),
        );
        let script: Script = Arc::new(move |turn: Turn| {
            let (k, o, r) = (k.clone(), o.clone(), r.clone());
            match (turn.task.as_str(), turn.call) {
                ("root", 1) => ready(calls(vec![spawn("kid")])),
                ("kid", _) => Box::pin(async move {
                    k.notify_one();
                    std::future::pending().await
                }),
                ("other", _) => Box::pin(async move {
                    o.notify_one();
                    r.notified().await;
                    text("other done")
                }),
                _ => ready(text("done")),
            }
        });
        let limits = RuntimeLimits {
            max_sessions: 16,
            max_active_turns: 1,
        };
        let (rt, _log, _seen) = runtime(dir.path(), "full-access", limits, script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let other = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&root, vec![ContentBlock::text("root")])
            .await
            .unwrap();
        kid_started.notified().await;
        // Queued behind the child for the only permit.
        let other_turn = rt
            .prompt(&other, vec![ContentBlock::text("other")])
            .await
            .unwrap();
        rt.cancel(&root);
        other_started.notified().await;
        let outcome = tokio::time::timeout(Duration::from_secs(5), turn.outcome())
            .await
            .expect("cancelled parent must not wait for the unrelated turn's permit");
        assert!(matches!(outcome, TurnOutcome::Cancelled));
        assert!(!rt.is_busy(&root));
        release_other.notify_one();
        assert!(matches!(
            other_turn.outcome().await,
            TurnOutcome::Completed(_)
        ));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_the_subagent_task_runs_is_honored() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let in_call = Arc::new(tokio::sync::Notify::new());
        let signal = in_call.clone();
        let script: Script = Arc::new(move |turn: Turn| {
            let signal = signal.clone();
            match turn.task.as_str() {
                "root" => Box::pin(async move {
                    signal.notify_one();
                    std::future::pending().await
                }),
                _ => ready(text("should not run")),
            }
        });
        let (rt, _log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let _turn = rt
            .prompt(&root, vec![ContentBlock::text("root")])
            .await
            .unwrap();
        in_call.notified().await;
        let live = rt.route(&root).unwrap();
        let tool = live
            .opened
            .harness
            .get_tools()
            .await
            .into_iter()
            .find(|t| t.name == "subagent")
            .unwrap();
        let call = (tool.execute)("id".into(), json!({"task": "kid"}), None, None);
        tokio::pin!(call);
        // Dispatch: the detached task is spawned but has not run yet.
        assert!(futures::poll!(&mut call).is_pending());
        live.cancel.send_modify(|n| *n += 1);
        let result = call.await;
        assert!(result.is_err(), "a cancelled turn must not get a result");
        assert_eq!(
            seen.calls("kid"),
            0,
            "no child model call after cancellation"
        );
        assert!(children(&rt.list().await.unwrap(), &root).is_empty());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_keeps_its_own_workspace_restrictions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(sub.join(".loop")).unwrap();
        std::fs::write(
            sub.join(".loop/settings.json"),
            r#"{"toolPermissions":{"bash":"deny"}}"#,
        )
        .unwrap();
        loop_app_core::config::TrustStore::load(loop_app_core::config::trust_path(
            &dir.path().join("agent"),
        ))
        .unwrap()
        .set(&sub, true)
        .unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![(
                "subagent",
                json!({"task": "sub worker", "cwd": "sub"}),
            )])),
            ("sub worker", 1) => ready(calls(vec![("bash", json!({"command": "touch made"}))])),
            _ => ready(text("done")),
        });
        let (rt, _log, seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        assert_eq!(seen.calls("sub worker"), 2);
        assert!(
            !sub.join("made").exists(),
            "the child workspace denies bash"
        );
        let results = seen.results("sub worker");
        assert!(results[0].contains("denied"), "{results:?}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[test]
fn merged_rules_keep_the_stricter_and_drop_child_only_allows() {
    use loop_app_core::permissions::ToolPermission::*;
    let parent = std::collections::BTreeMap::from([
        ("bash".to_owned(), Allow),
        ("write".to_owned(), Ask),
        ("subagent".to_owned(), Allow),
    ]);
    let child = std::collections::BTreeMap::from([
        ("bash".to_owned(), Deny),
        ("write".to_owned(), Allow),
        ("bash(rm *)".to_owned(), Allow),
        ("web_fetch".to_owned(), Ask),
    ]);
    let merged = subagent::merge_rules(&parent, &child);
    assert_eq!(merged["bash"], Deny);
    assert_eq!(merged["write"], Ask);
    assert_eq!(merged["subagent"], Allow);
    assert_eq!(merged["web_fetch"], Ask);
    assert!(!merged.contains_key("bash(rm *)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_child_cannot_be_reopened_as_an_unrestricted_root() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![spawn("kid")])),
            _ => ready(text("done")),
        });
        let (rt, _log, _seen) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        rt.route(&root)
            .unwrap()
            .opened
            .tool_gate
            .set_mode(PermissionMode::ReadOnly);
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        let kid = children(&rt.list().await.unwrap(), &root).pop().unwrap();
        assert!(!kid.live);
        let error = rt
            .load_session(&kid.r#ref, SessionSpec::default())
            .await
            .expect_err("a Subagent must not reopen without its parent's ceiling");
        assert!(error.to_string().contains("Subagent"), "{error}");
        assert!(!rt
            .list()
            .await
            .unwrap()
            .iter()
            .any(|s| s.r#ref == kid.r#ref && s.live));

        // A fork also records a parent but is not a Subagent; it still reopens.
        rt.close(&root.key, false).await.unwrap();
        let fork = rt
            .store
            .fork(
                &root.native_id,
                loop_agent::harness::session::SessionForkSelection::All,
                None,
                None,
            )
            .await
            .unwrap();
        let fork = reference(&fork.metadata().id);
        rt.load_session(&fork, SessionSpec::default())
            .await
            .unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_subagent_children_list_a_parent_and_their_end_says_they_closed() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![spawn("kid")])),
            ("root", _) => ready(text("root done")),
            _ => ready(text("kid done")),
        });
        let (rt, log, _) =
            runtime(dir.path(), "full-access", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        // A fork made elsewhere also records a parent, but it is no Subagent.
        let fork = rt
            .store
            .create_child(None, Some("fork".into()), &root.native_id)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        assert!(matches!(
            run(&rt, &root, "root").await,
            TurnOutcome::Completed(_)
        ));
        let list = rt.list().await.unwrap();
        let fork = list.iter().find(|s| s.r#ref.native_id == fork).unwrap();
        assert_eq!(fork.parent, None, "{fork:?}");
        let kids = children(&list, &root);
        assert_eq!(kids.len(), 1);
        assert_eq!(
            kids[0].name.as_deref(),
            Some("kid"),
            "the task names the child"
        );

        let events = log.read_runtime_after(0).await.unwrap();
        let ended = payload(&events, &root.key, "subagent_ended").unwrap();
        // `closed` keeps its original meaning: the close error, or null.
        assert_eq!(ended["closed"], json!(null), "{ended}");
        assert_eq!(ended["closed_ok"], json!(true), "{ended}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

type Asked = (
    loop_protocol::rpc::InteractParams,
    oneshot::Sender<loop_protocol::InteractionOutcome>,
    oneshot::Receiver<()>,
);
/// A Surface that hands every Escalation to the test.
struct Watcher(tokio::sync::mpsc::UnboundedSender<Asked>);
#[async_trait]
impl EscalationRecipient for Watcher {
    fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
    async fn interact(
        &self,
        params: loop_protocol::rpc::InteractParams,
        withdrawn: oneshot::Receiver<()>,
    ) -> Result<loop_protocol::InteractionOutcome, ()> {
        let (tx, rx) = oneshot::channel();
        self.0.send((params, tx, withdrawn)).map_err(|_| ())?;
        rx.await.map_err(|_| ())
    }
}
fn watcher() -> (Arc<Watcher>, tokio::sync::mpsc::UnboundedReceiver<Asked>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (Arc::new(Watcher(tx)), rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grandchilds_escalation_reaches_surfaces_watching_any_ancestor() {
    use loop_wire::RuntimeControl;
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
            ("root", 1) => ready(calls(vec![spawn("middle")])),
            ("middle", 1) => ready(calls(vec![spawn("asker")])),
            ("asker", 1) => ready(calls(vec![("bash", json!({"command": "echo hi"}))])),
            (task, _) => ready(text(format!("{task} done"))),
        });
        let (rt, log, seen) = runtime(dir.path(), "ask", RuntimeLimits::default(), script).await;
        let root = rt.new_session(SessionSpec::default()).await.unwrap();
        let (early, mut early_rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 1, Some(early))
            .await
            .unwrap();
        let turn = rt
            .prompt(&root, vec![ContentBlock::text("root")])
            .await
            .unwrap();

        let (params, early_answer, early_withdrawn) = early_rx.recv().await.unwrap();
        let asker = params
            .session
            .clone()
            .expect("the request names its Session");
        assert_ne!(asker, root, "the request comes from the grandchild");
        let list = rt.list().await.unwrap();
        let info = list.iter().find(|s| s.r#ref == asker).unwrap();
        assert_eq!(info.name.as_deref(), Some("asker"));

        // A Surface attaching to the root later still gets the parked request.
        let (late, mut late_rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 2, Some(late))
            .await
            .unwrap();
        let (late_params, late_answer, _) = late_rx.recv().await.unwrap();
        assert_eq!(late_params.session.as_ref(), Some(&asker));
        let loop_protocol::InteractionRequest::Permission { options, .. } = &late_params.request
        else {
            panic!("{late_params:?}")
        };
        let allow = options
            .iter()
            .find(|o| o.native_id == loop_app_core::interaction::native_options::ACCEPT)
            .unwrap_or(&options[0])
            .native_id
            .clone();
        late_answer
            .send(loop_protocol::InteractionOutcome::Selected {
                native_id: allow,
                note: None,
            })
            .unwrap();
        // First answer wins; the other Surface is withdrawn.
        early_withdrawn
            .await
            .expect("the early Surface is withdrawn");
        drop(early_answer);
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        assert_eq!(seen.results("root"), vec!["middle done".to_owned()]);

        let events = log.read_runtime_after(0).await.unwrap();
        let kinds = kinds(&events, &asker.key);
        assert!(kinds.contains(&"escalation_raised".to_owned()), "{kinds:?}");
        assert!(
            kinds.contains(&"escalation_answered".to_owned()),
            "{kinds:?}"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A root in ask mode whose only child asks to run `bash`.
async fn asking_child(
    dir: &std::path::Path,
) -> (Arc<Runtime>, SessionRef, loop_protocol::TurnHandle) {
    let script: Script = Arc::new(|turn: Turn| match (turn.task.as_str(), turn.call) {
        ("root", 1) => ready(calls(vec![spawn("asker")])),
        ("asker", 1) => ready(calls(vec![("bash", json!({"command": "echo hi"}))])),
        (task, _) => ready(text(format!("{task} done"))),
    });
    let (rt, _, _) = runtime(dir, "ask", RuntimeLimits::default(), script).await;
    let root = rt.new_session(SessionSpec::default()).await.unwrap();
    let turn = rt
        .prompt(&root, vec![ContentBlock::text("root")])
        .await
        .unwrap();
    (rt, root, turn)
}
fn allow(params: &loop_protocol::rpc::InteractParams) -> loop_protocol::InteractionOutcome {
    let loop_protocol::InteractionRequest::Permission { options, .. } = &params.request else {
        panic!("{params:?}")
    };
    let native_id = options
        .iter()
        .find(|o| o.native_id == loop_app_core::interaction::native_options::ACCEPT)
        .unwrap_or(&options[0])
        .native_id
        .clone();
    loop_protocol::InteractionOutcome::Selected {
        native_id,
        note: None,
    }
}
/// Nothing more arrives for a moment.
async fn quiet(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Asked>) {
    let more = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(more.is_err(), "unexpected second delivery");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detaching_from_an_ancestor_withdraws_only_that_surface() {
    use loop_wire::RuntimeControl;
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, root, turn) = asking_child(dir.path()).await;
        let (one, mut one_rx) = watcher();
        let (two, mut two_rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 1, Some(one))
            .await
            .unwrap();
        RuntimeControl::recipient(rt.as_ref(), &root, 2, Some(two))
            .await
            .unwrap();
        let (_, _one_answer, one_withdrawn) = one_rx.recv().await.unwrap();
        let (params, two_answer, _) = two_rx.recv().await.unwrap();
        RuntimeControl::recipient(rt.as_ref(), &root, 1, None)
            .await
            .unwrap();
        one_withdrawn
            .await
            .expect("the detached Surface is withdrawn");
        // A Surface that attaches later is withdrawn when another answers.
        let (three, mut three_rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 3, Some(three))
            .await
            .unwrap();
        let (_, _three_answer, three_withdrawn) = three_rx.recv().await.unwrap();
        two_answer.send(allow(&params)).unwrap();
        three_withdrawn
            .await
            .expect("the late Surface is withdrawn");
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_on_child_and_ancestor_gets_one_prompt() {
    use loop_wire::RuntimeControl;
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, root, turn) = asking_child(dir.path()).await;
        let (surface, mut rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 1, Some(surface.clone()))
            .await
            .unwrap();
        let (params, answer, mut withdrawn) = rx.recv().await.unwrap();
        let child = params.session.clone().unwrap();
        RuntimeControl::recipient(rt.as_ref(), &child, 1, Some(surface))
            .await
            .unwrap();
        quiet(&mut rx).await;
        // Still attached to the child itself: leaving the root withdraws nothing.
        RuntimeControl::recipient(rt.as_ref(), &root, 1, None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            withdrawn.try_recv().is_err(),
            "kept by its direct attachment"
        );
        quiet(&mut rx).await;
        answer.send(allow(&params)).unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ancestors_surface_disconnecting_leaves_the_request_parked() {
    use loop_wire::RuntimeControl;
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, root, turn) = asking_child(dir.path()).await;
        let (gone, mut gone_rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 1, Some(gone))
            .await
            .unwrap();
        let (_, _gone_answer, gone_withdrawn) = gone_rx.recv().await.unwrap();
        RuntimeControl::disconnected(rt.as_ref(), 1).await;
        gone_withdrawn
            .await
            .expect("the disconnected Surface is withdrawn");
        assert!(rt.is_busy(&root), "the request stays parked");
        let (next, mut next_rx) = watcher();
        RuntimeControl::recipient(rt.as_ref(), &root, 2, Some(next))
            .await
            .unwrap();
        let (params, answer, _) = next_rx.recv().await.unwrap();
        answer.send(allow(&params)).unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
