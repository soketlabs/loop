//! A recorded ("golden") trace of the update pump.
//!
//! One Runtime runs a fixed script of turns against a faux model. The test
//! writes down what the pump did, one line per item, and compares the lines
//! with `tests/data/pump_trace.golden`. The golden file was recorded on
//! `staging` (b71d013). A change to the pump that keeps its behaviour must
//! give the same lines. Set `LOOP_PUMP_TRACE_WRITE=1` to record the file
//! again. This file uses only public API that `staging` has.
//!
//! ## What the script covers
//!
//! Two callers act on one root Session: the host user (`Caller::host`) and
//! the same user on a token connection (`Caller::local_token`). So the
//! identity columns tell them apart. In order:
//!
//! 1. host: a model answer with three thought chunks and twelve text
//!    chunks;
//! 2. token: one `write` call, then three calls in one model message that
//!    run in parallel (`read`, `read`, `ls`), then text;
//! 3. host sets the permission mode to `ask`; token: a `write` call that
//!    asks for approval. An in-process Surface, registered with the public
//!    Escalation API (`RuntimeControl::recipient`), answers "accept";
//! 4. host: a turn with a `write` call whose approval the Surface holds
//!    open; while it waits, token cancels the turn. Then host sets the
//!    mode back to `full-access`;
//! 5. host: a turn whose model call waits on a gate; token calls
//!    `set_model` during the turn (a pending change), then the gate opens;
//! 6. token: a `subagent` call; the child Session answers with text;
//! 7. host: `compact` while idle (the summary is a model call);
//! 8. token: a model error (`FauxResponse::Error`);
//! 9. the Runtime shuts down.
//!
//! Notes on what the trace shows:
//!
//! - The approval leaves no `interaction_requested` update: the Runtime's
//!   Escalations are Event Log rows (`escalation_raised`,
//!   `escalation_answered`, `escalation_withdrawn`), so those rows are the
//!   approval's trace.
//! - The cancel is gated on a held approval, not on a held model call. The
//!   faux model does not stop for a cancel, so a turn cancelled inside a
//!   model call waits until the call returns, and when it then stops is
//!   timing. A held approval is withdrawn by the cancel itself, in a fixed
//!   order. On `staging` this cancel gives a `cancelled` row and an
//!   `escalation_withdrawn` row; the tool call fails ("rejected"), the
//!   model is called once more, and the turn's `turn_ended` says
//!   `end_turn` (the Runtime still reports the outcome as cancelled). The
//!   trace records that as it is.
//!
//! ## What is recorded
//!
//! Per Session (root first, then its child), three lanes:
//!
//! - `log`: the Update rows of the Event Log, in `session_seq` order. Each
//!   line has the row's `session_seq`, turn, identity (tenant, principal,
//!   via, actor) and the full `UpdateEnvelope` JSON.
//! - `rec`: the other rows of the Session (prompt sent, policy decision,
//!   escalation, cancel, subagent, opened, closed), in Event Log order,
//!   with turn, identity and payload.
//! - `bus`: the updates a subscriber of `Runtime::subscribe()` received
//!   for the Session, in the order received, as JSON. The subscriber
//!   subscribes before the first Session opens. The test fails if it lags
//!   or if it did not get every committed update.
//!
//! Records with no Session (Runtime started and stopped) are not recorded.
//!
//! ## Why the lanes are split, and other choices that keep it stable
//!
//! - Update rows go through the pump; the other rows are written to the
//!   Event Log directly by other tasks (the prompt, the gate, the
//!   Escalation host, the subagent tool), and the pump picks its command
//!   (a cancel) or the next update at random when both are ready. So where
//!   a `rec` row falls between Update rows is timing, not behaviour. The
//!   `rec` lane keeps only the order of those rows among themselves.
//! - Sessions are not interleaved: the child runs at the same time as its
//!   parent's tool call.
//! - Race window (lines marked `~` at the start): the three parallel tool
//!   calls of step 2 finish in any order. In each lane, a contiguous run of
//!   items about those calls (their `tool_call_update` updates, and their
//!   `policy_decision` rows) is sorted by tool call id with a stable sort,
//!   so the items of one call keep their order. The run's sequence numbers
//!   are then given back in ascending order, and in the JSON of a sorted
//!   update the `seq` is written `"$SEQ"` (the `seq=` column has it). Only
//!   ids that start with `call-par-` are in a race window.
//! - Each step ends at a Runtime barrier: a turn handle, a setter and
//!   `compact` return only after the pump has committed what came before.
//!   The one update that can come after that barrier is the deferred
//!   `config_changed` of step 5, and its setter claimed it, so its identity
//!   is fixed. So no unclaimed update with no turn id waits in the pump's
//!   queue when the other caller acts: the two identity races that a
//!   per-Session sequencer removes are not in this trace. The test also
//!   waits (`settle`) until the subscriber has every update the Event Log
//!   holds, so the `bus` lane is complete before the next step.
//! - The model's answer is chosen from the request (the turn's prompt and
//!   how many model answers follow it), never from a call counter, and
//!   each waiting turn has its own gate.
//! - There is only one test in this file: interaction ids come from a
//!   counter that is global to the test process.
//!
//! ## Masking
//!
//! Values that change from run to run become placeholders. Ids of one
//! class are numbered in the order they first appear in the lines (which
//! are in a fixed order): Session keys and native ids (`$SESSION1` root,
//! `$SESSION2` child), the backend instance (`$INSTANCE`), the test's temp
//! folder in its `/var` and `/private/var` forms (`$TMP`), the system temp
//! folder (`$SYSTMP`), the write tool's review folders as whole paths, with
//! the folder they are in (`$REVIEW1`...; that folder is the platform's or a
//! sandbox's temp folder), the home folder (`$HOME`),
//! branch view ids (`$VIEW1`...), interaction and Escalation ids
//! (`$INTERACTION1`...), policy decision ids (`$DECISION1`...) and the
//! Runtime generation (`$GEN`). The number of any JSON field whose name
//! ends in `_ms` or `_ns`, or is `timestamp`, `ts`, `dev` or `ino`, becomes
//! `0`. Sequence numbers, `turn-N` and `msg-N` counters, tool call ids,
//! kinds and order are kept as they are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use loop_ai::providers::{FauxResponder, FauxResponse};
use loop_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, Model, StopReason,
    TextContent, ThinkingContent, ToolCall, UserContent, UserMessageContent,
};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{
    AgentBackend, Caller, ContentBlock, ModelControl, Principal, SessionKey, SessionRef,
    SessionSpec, SessionUpdate, TurnOutcome, UpdateEnvelope,
};
use loop_runtime::log::{EventKind, EventLog, LoggedEvent, MemoryEventLog, Scope, SystemScope};
use loop_runtime::{Runtime, RuntimeLimits};
use tokio::sync::{broadcast, Notify};

const GOLDEN: &str = "tests/data/pump_trace.golden";
const WRITE_ENV: &str = "LOOP_PUMP_TRACE_WRITE";
/// The id prefix of the step 2 parallel tool calls (the race window).
const PARALLEL: &str = "call-par-";

fn host() -> Caller {
    Caller::host(Principal::local_uid(501))
}

/// The host user on a token connection: another identity (`via: token`).
fn token() -> Caller {
    Caller::local_token(Principal::local_uid(501))
}

fn opts(cwd: &Path) -> BootstrapOpts {
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
        permission_mode: Some("full-access".into()),
        agent: None,
    }
}

/// Gates for the turns that wait inside their model call.
#[derive(Default)]
struct Gates {
    config_held: Notify,
    config_release: Notify,
}

fn text_of(message: &Message) -> Option<String> {
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

/// The turn's prompt (the last user message) and how many model answers
/// follow it in this request.
fn step_of(context: &Context) -> (String, usize) {
    let last = context
        .messages
        .iter()
        .rposition(|m| matches!(m, Message::User(_)));
    let Some(last) = last else {
        return (String::new(), 0);
    };
    let prompt = text_of(&context.messages[last]).unwrap_or_default();
    let answers = context.messages[last..]
        .iter()
        .filter(|m| matches!(m, Message::Assistant(_)))
        .count();
    (prompt, answers)
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
    }
}

/// A streamed answer: thought chunks, then text chunks.
fn streamed(model: &Model, thoughts: &[&str], chunks: &[&str]) -> FauxResponse {
    let mut events = Vec::new();
    let mut partial = AssistantMessage::pending(model);
    partial.timestamp = 0;
    events.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });
    partial
        .content
        .push(AssistantContent::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: None,
            redacted: None,
        }));
    events.push(AssistantMessageEvent::ThinkingStart {
        content_index: 0,
        partial: partial.clone(),
    });
    for delta in thoughts {
        if let AssistantContent::Thinking(t) = &mut partial.content[0] {
            t.thinking.push_str(delta);
        }
        events.push(AssistantMessageEvent::ThinkingDelta {
            content_index: 0,
            delta: (*delta).into(),
            partial: partial.clone(),
        });
    }
    events.push(AssistantMessageEvent::ThinkingEnd {
        content_index: 0,
        content: thoughts.concat(),
        partial: partial.clone(),
    });
    partial.content.push(AssistantContent::Text(TextContent {
        text: String::new(),
        text_signature: None,
    }));
    events.push(AssistantMessageEvent::TextStart {
        content_index: 1,
        partial: partial.clone(),
    });
    for delta in chunks {
        if let AssistantContent::Text(t) = &mut partial.content[1] {
            t.text.push_str(delta);
        }
        events.push(AssistantMessageEvent::TextDelta {
            content_index: 1,
            delta: (*delta).into(),
            partial: partial.clone(),
        });
    }
    events.push(AssistantMessageEvent::TextEnd {
        content_index: 1,
        content: chunks.concat(),
        partial: partial.clone(),
    });
    partial.usage.output = 15;
    partial.usage.total_tokens = 15;
    partial.stop_reason = StopReason::Stop;
    events.push(AssistantMessageEvent::Done {
        reason: StopReason::Stop,
        message: partial,
    });
    FauxResponse::Events(events)
}

/// The faux model's answer to one request.
async fn answer(model: Model, gates: Arc<Gates>, context: Context) -> FauxResponse {
    use loop_agent::harness::compaction::SUMMARY_SYSTEM_PROMPT;
    use serde_json::json;
    if context.system_prompt.as_deref() == Some(SUMMARY_SYSTEM_PROMPT) {
        return FauxResponse::Text("SUMMARY: the pump trace so far.".into());
    }
    let (prompt, step) = step_of(&context);
    let text = |t: &str| FauxResponse::Text(t.into());
    match (prompt.as_str(), step) {
        ("stream", 0) => streamed(
            &model,
            &["Let me ", "think about ", "the pump."],
            &[
                "The ", "pump ", "commits ", "one ", "event ", "per ", "update, ", "then ", "it ",
                "sends ", "it ", "on.",
            ],
        ),
        ("tools", 0) => FauxResponse::ToolCalls(vec![call(
            "call-write-1",
            "write",
            json!({"path": "notes.txt", "content": "alpha\nbeta\n"}),
        )]),
        ("tools", 1) => FauxResponse::ToolCalls(vec![
            call(
                &format!("{PARALLEL}1"),
                "read",
                json!({"path": "notes.txt"}),
            ),
            call(&format!("{PARALLEL}2"), "read", json!({"path": "seed.txt"})),
            call(&format!("{PARALLEL}3"), "ls", json!({"path": "."})),
        ]),
        ("tools", _) => text("tools done"),
        ("approve", 0) => FauxResponse::ToolCalls(vec![call(
            "call-approve-1",
            "write",
            json!({"path": "approved.txt", "content": "yes\n"}),
        )]),
        ("approve", _) => text("approved and written"),
        ("cancel", 0) => FauxResponse::ToolCalls(vec![call(
            "call-cancel-1",
            "write",
            json!({"path": "never.txt", "content": "no\n"}),
        )]),
        ("cancel", _) => text("stopped"),
        ("config", 0) => {
            gates.config_held.notify_one();
            gates.config_release.notified().await;
            text("config done")
        }
        ("spawn", 0) => FauxResponse::ToolCalls(vec![call(
            "call-spawn-1",
            "subagent",
            json!({"task": "child work"}),
        )]),
        ("spawn", _) => text("spawn done"),
        ("child work", 0) => text("child done"),
        ("error", 0) => FauxResponse::Error("faux model failure: the service broke".into()),
        _ => text("unexpected request"),
    }
}

async fn runtime(dir: &Path, log: Arc<dyn EventLog>) -> (Arc<Runtime>, Arc<Gates>) {
    let gates = Arc::new(Gates::default());
    // A first provider only to read the faux model's description.
    let probe = loop_ai::providers::faux_provider_fn(Arc::new(|_: &Context| {
        Box::pin(std::future::ready(FauxResponse::Text(String::new())))
    }));
    let model = probe.get_model("faux-model").unwrap();
    let seen = gates.clone();
    let responder: FauxResponder = Arc::new(move |context: &Context| {
        Box::pin(answer(model.clone(), seen.clone(), context.clone()))
    });
    let provider = loop_ai::providers::faux_provider_fn(responder);
    let template = provider.get_model("faux-model").unwrap();
    let mut second = template.clone();
    second.id = "second".into();
    second.name = "second".into();
    provider.set_models(vec![template, second]);
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(provider);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("seed.txt"), "seed line\n").unwrap();
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let store = globals.open_runtime_session_store().unwrap();
    let rt = Runtime::new(globals, opts(&ws), log, store, RuntimeLimits::default())
        .await
        .unwrap()
        .into_shared();
    (rt, gates)
}

/// An in-process Surface. It accepts the first approval it is asked for.
/// It holds the second one open (so the test can cancel the turn while it
/// waits) until the Runtime withdraws it.
struct Approver {
    asked: Mutex<usize>,
    holding: Notify,
}

#[async_trait::async_trait]
impl loop_wire::control::EscalationRecipient for Approver {
    fn caller(&self) -> Option<Caller> {
        None
    }

    fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}

    async fn interact(
        &self,
        params: loop_protocol::rpc::InteractParams,
        withdrawn: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<loop_protocol::InteractionOutcome, ()> {
        let n = {
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            *asked
        };
        let loop_protocol::InteractionRequest::Permission { options, .. } = &params.request else {
            return Err(());
        };
        let accept = loop_app_core::interaction::native_options::ACCEPT;
        assert!(options.iter().any(|o| o.native_id == accept));
        if n == 1 {
            return Ok(loop_protocol::InteractionOutcome::Selected {
                native_id: accept.into(),
                note: None,
            });
        }
        self.holding.notify_one();
        let _ = withdrawn.await;
        Err(())
    }
}

/// What the subscriber received, in order.
#[derive(Default)]
struct Received {
    updates: Vec<UpdateEnvelope>,
    lagged: Option<String>,
}

fn collect(mut rx: broadcast::Receiver<UpdateEnvelope>) -> Arc<Mutex<Received>> {
    let received = Arc::new(Mutex::new(Received::default()));
    let sink = received.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(envelope) => sink.lock().unwrap().updates.push(envelope),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    sink.lock().unwrap().lagged = Some(format!("lagged by {n}"));
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    received
}

fn all() -> Scope<'static> {
    static SCOPE: std::sync::OnceLock<SystemScope> = std::sync::OnceLock::new();
    Scope::System(SCOPE.get_or_init(SystemScope::for_runtime_startup))
}

async fn records(log: &dyn EventLog) -> Vec<LoggedEvent> {
    log.read_runtime_after(all(), 0).await.unwrap()
}

/// Wait until the subscriber has every update the Event Log holds for
/// every Session, so nothing waits in a pump's queue.
async fn settle(log: &dyn EventLog, received: &Mutex<Received>) {
    loop {
        log.sync().await.unwrap();
        let mut latest: BTreeMap<SessionKey, u64> = BTreeMap::new();
        for record in records(log).await {
            if let (EventKind::Update(e), Some(_)) = (&record.event.kind, record.session_seq) {
                let seq = latest.entry(e.session.clone()).or_default();
                *seq = (*seq).max(e.seq);
            }
        }
        let done = {
            let received = received.lock().unwrap();
            assert!(received.lagged.is_none(), "{:?}", received.lagged);
            latest.iter().all(|(key, seq)| {
                received
                    .updates
                    .iter()
                    .any(|e| &e.session == key && e.seq >= *seq)
            })
        };
        if done {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn prompt(rt: &Runtime, caller: &Caller, session: &SessionRef, text: &str) -> TurnOutcome {
    caller
        .clone()
        .scope(rt.prompt(session, vec![ContentBlock::text(text)]))
        .await
        .unwrap()
        .outcome()
        .await
}

/// Change the Session's permission mode (`session/permission_mode`).
async fn set_mode(rt: &Runtime, caller: &Caller, session: &SessionRef, mode: &str) {
    let params = loop_protocol::admin::PermissionModeParams {
        session: session.clone(),
        mode: Some(mode.into()),
        expected_revision: None,
    };
    caller
        .clone()
        .scope(loop_wire::RuntimeAdmin::permission_mode(rt, params))
        .await
        .unwrap();
}

/// Run the script; return the root Session, the Event Log rows and the
/// updates the subscriber received.
async fn run(
    dir: &Path,
) -> (
    Arc<Runtime>,
    SessionRef,
    Vec<LoggedEvent>,
    Vec<UpdateEnvelope>,
) {
    let log = Arc::new(MemoryEventLog::new());
    let (rt, gates) = runtime(dir, log.clone()).await;
    let received = collect(rt.subscribe());
    let (me, other) = (host(), token());

    let root = me
        .clone()
        .scope(rt.new_session(SessionSpec::default()))
        .await
        .unwrap();
    let approver = Arc::new(Approver {
        asked: Mutex::new(0),
        holding: Notify::new(),
    });
    loop_wire::RuntimeControl::recipient(rt.as_ref(), &root, 1, Some(approver.clone()))
        .await
        .unwrap();
    settle(log.as_ref(), &received).await;

    // 1. Thought and text in many chunks.
    let outcome = prompt(&rt, &me, &root, "stream").await;
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
    settle(log.as_ref(), &received).await;

    // 2. One call, then three in parallel.
    let outcome = prompt(&rt, &other, &root, "tools").await;
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
    settle(log.as_ref(), &received).await;

    // 3. An approval.
    set_mode(&rt, &me, &root, "ask").await;
    settle(log.as_ref(), &received).await;
    let outcome = prompt(&rt, &other, &root, "approve").await;
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
    assert_eq!(*approver.asked.lock().unwrap(), 1, "one approval was asked");
    settle(log.as_ref(), &received).await;

    // 4. A cancel during a turn, while its tool call waits for approval.
    let turn = me
        .clone()
        .scope(rt.prompt(&root, vec![ContentBlock::text("cancel")]))
        .await
        .unwrap();
    approver.holding.notified().await;
    other.clone().scope(async { rt.cancel(&root) }).await;
    let outcome = turn.outcome().await;
    assert!(matches!(outcome, TurnOutcome::Cancelled), "{outcome:?}");
    assert_eq!(
        *approver.asked.lock().unwrap(),
        2,
        "two approvals were asked"
    );
    settle(log.as_ref(), &received).await;
    set_mode(&rt, &me, &root, "full-access").await;
    settle(log.as_ref(), &received).await;

    // 5. A config change during a turn.
    let turn = me
        .clone()
        .scope(rt.prompt(&root, vec![ContentBlock::text("config")]))
        .await
        .unwrap();
    gates.config_held.notified().await;
    let changed = other
        .clone()
        .scope(rt.set_model(&root, "faux/second", None))
        .await
        .unwrap();
    assert!(changed.pending, "the change waits for the turn");
    gates.config_release.notify_one();
    let outcome = turn.outcome().await;
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
    settle(log.as_ref(), &received).await;

    // 6. A subagent.
    let outcome = prompt(&rt, &other, &root, "spawn").await;
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");
    settle(log.as_ref(), &received).await;

    // 7. A compaction.
    me.clone().scope(rt.compact(&root, None)).await.unwrap();
    settle(log.as_ref(), &received).await;

    // 8. A model error.
    let outcome = prompt(&rt, &other, &root, "error").await;
    assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
    settle(log.as_ref(), &received).await;

    // 9. Shut down; the subscriber then has every committed update.
    rt.shutdown().await;
    settle(log.as_ref(), &received).await;
    let all_records = records(log.as_ref()).await;
    let committed = all_records
        .iter()
        .filter(|r| matches!(r.event.kind, EventKind::Update(_)))
        .count();
    let updates = received.lock().unwrap().updates.clone();
    assert_eq!(updates.len(), committed, "the subscriber got every update");
    (rt, root, all_records, updates)
}

/// The tool call id an update or record is about, when it is one of the
/// step 2 parallel calls.
fn parallel_id(json: &str) -> Option<String> {
    let at = json.find(PARALLEL)?;
    let rest = &json[at..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// One item of a lane before it becomes a line.
struct Item {
    /// The sequence number shown, or `None`.
    seq: Option<u64>,
    /// The rest of the line.
    body: String,
    /// The parallel tool call this item is about, when in a race window.
    race: Option<String>,
}

/// Sort each contiguous run of items of the parallel calls by call id and
/// give the run's sequence numbers back in ascending order.
fn settle_races(items: &mut [Item]) {
    let mut i = 0;
    while i < items.len() {
        if items[i].race.is_none() {
            i += 1;
            continue;
        }
        let start = i;
        while i < items.len() && items[i].race.is_some() {
            i += 1;
        }
        let run = &mut items[start..i];
        let mut seqs: Vec<Option<u64>> = run.iter().map(|it| it.seq).collect();
        seqs.sort();
        run.sort_by(|a, b| a.race.cmp(&b.race));
        for (item, seq) in run.iter_mut().zip(seqs) {
            item.seq = seq;
        }
    }
}

fn render(lane: &str, items: Vec<Item>) -> Vec<String> {
    items
        .into_iter()
        .map(|it| {
            let mark = if it.race.is_some() { "~" } else { " " };
            let seq = it.seq.map_or("-".to_string(), |s| s.to_string());
            format!("{mark}{lane} seq={seq} {}", it.body)
        })
        .collect()
}

/// A `tool_call_update` of a parallel call: the only updates that race.
fn racing_update(envelope: &UpdateEnvelope) -> Option<String> {
    match &envelope.update {
        SessionUpdate::ToolCallUpdate(u) if u.id.0.starts_with(PARALLEL) => Some(u.id.0.clone()),
        _ => None,
    }
}

/// An envelope as JSON with its sequence written `$SEQ`, so a sorted race
/// window can take its new sequence number in the line's `seq=` column.
fn envelope_json(envelope: &UpdateEnvelope, in_race: bool) -> String {
    let json = serde_json::to_string(envelope).unwrap();
    if in_race {
        json.replacen(
            &format!("\"seq\":{},", envelope.seq),
            "\"seq\":\"$SEQ\",",
            1,
        )
    } else {
        json
    }
}

fn kind_name(kind: &EventKind) -> String {
    let json = serde_json::to_value(kind).unwrap();
    json["kind"].as_str().unwrap_or("unknown").to_string()
}

/// The lines of one Session, before masking.
fn session_lines(
    name: &str,
    key: &SessionKey,
    records: &[LoggedEvent],
    received: &[UpdateEnvelope],
) -> Vec<String> {
    let mut log_items = Vec::new();
    let mut rec_items = Vec::new();
    for record in records
        .iter()
        .filter(|r| r.event.session_key.as_ref() == Some(key))
    {
        let identity = serde_json::to_string(&record.event.identity).unwrap();
        let turn = record.event.turn.as_ref().map_or("-", |t| t.0.as_str());
        match &record.event.kind {
            EventKind::Update(envelope) => {
                let race = racing_update(envelope);
                log_items.push(Item {
                    seq: record.session_seq,
                    body: format!(
                        "turn={turn} who={identity} {}",
                        envelope_json(envelope, race.is_some())
                    ),
                    race,
                });
            }
            kind => {
                let payload = serde_json::to_string(kind).unwrap();
                let race = parallel_id(&payload);
                rec_items.push(Item {
                    seq: record.session_seq,
                    body: format!("{} turn={turn} who={identity} {payload}", kind_name(kind)),
                    race,
                });
            }
        }
    }
    log_items.sort_by_key(|it| it.seq);
    let mut bus_items: Vec<Item> = received
        .iter()
        .filter(|e| &e.session == key)
        .map(|e| {
            let race = racing_update(e);
            Item {
                seq: Some(e.seq),
                body: envelope_json(e, race.is_some()),
                race,
            }
        })
        .collect();
    settle_races(&mut log_items);
    settle_races(&mut rec_items);
    settle_races(&mut bus_items);
    let mut lines = vec![format!("== {name}")];
    lines.extend(render("log", log_items));
    lines.extend(render("rec", rec_items));
    lines.extend(render("bus", bus_items));
    lines
}

/// Replace the number of every JSON field named `<name>_ms`, `<name>_ns`,
/// `timestamp`, `ts`, `dev` or `ino` with `0`.
fn mask_times(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(colon) = rest.find("\":") {
        let (head, tail) = rest.split_at(colon + 2);
        out.push_str(head);
        let name_start = head[..head.len() - 2].rfind('"').map_or(0, |q| q + 1);
        let name = &head[name_start..head.len() - 2];
        let timed = name.ends_with("_ms")
            || name.ends_with("_ns")
            || ["timestamp", "ts", "dev", "ino"].contains(&name);
        let digits = tail
            .find(|c: char| !(c.is_ascii_digit() || c == '-'))
            .unwrap_or(tail.len());
        if timed && digits > 0 {
            out.push('0');
            rest = &tail[digits..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Placeholders for the values that change from run to run.
struct Masks(Vec<(String, String)>);

impl Masks {
    fn apply(&self, line: &str) -> String {
        let mut line = line.to_string();
        for (raw, placeholder) in &self.0 {
            line = line.replace(raw.as_str(), placeholder);
        }
        mask_times(&line)
    }
}

/// Number the values of one class in the order they first appear in `text`.
fn numbered(text: &str, values: Vec<String>, prefix: &str, masks: &mut Vec<(String, String)>) {
    let mut found: Vec<(usize, String)> = values
        .into_iter()
        .filter(|v| !v.is_empty())
        .filter_map(|v| text.find(v.as_str()).map(|at| (at, v)))
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.1 == b.1);
    for (n, (_, value)) in found.into_iter().enumerate() {
        masks.push((value, format!("${prefix}{}", n + 1)));
    }
}

/// The write tool's review folders (`<base>/.loop-file-review-<id>`), as
/// whole paths: the base is the system temp folder of the platform (or of
/// a sandbox), so it is masked with the folder.
fn review_folders(text: &str) -> Vec<String> {
    const PREFIX: &str = ".loop-file-review-";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let start = rest[..at].rfind(['"', ' ']).map_or(0, |q| q + 1);
        let tail = &rest[at..];
        let end = tail.find(['/', '"']).unwrap_or(tail.len());
        out.push(rest[start..at + end].to_string());
        rest = &tail[end..];
    }
    out
}

/// Every string value found under `key` anywhere in `json`.
fn strings_under(json: &serde_json::Value, key: &str, out: &mut Vec<String>) {
    match json {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    if let Some(s) = v.as_str() {
                        out.push(s.to_string());
                    }
                }
                strings_under(v, key, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                strings_under(v, key, out);
            }
        }
        _ => {}
    }
}

fn trace(
    dir: &Path,
    rt: &Runtime,
    root: &SessionRef,
    records: &[LoggedEvent],
    received: &[UpdateEnvelope],
) -> Vec<String> {
    // Sessions by creation order: the root, then each child of a known one.
    let mut sessions = vec![root.key.clone()];
    let mut i = 0;
    while i < sessions.len() {
        let parent = sessions[i].clone();
        for record in records {
            if let EventKind::SessionOpened(p) = &record.event.kind {
                if p["parent"] == parent.0.as_str() {
                    let child = record.event.session_key.clone().unwrap();
                    if !sessions.contains(&child) {
                        sessions.push(child);
                    }
                }
            }
        }
        i += 1;
    }
    let mut raw = Vec::new();
    for (n, key) in sessions.iter().enumerate() {
        raw.extend(session_lines(
            &format!("session {}", n + 1),
            key,
            records,
            received,
        ));
    }
    let text = raw.join("\n");

    let mut masks: Vec<(String, String)> = Vec::new();
    // Whole review-folder paths first, before any folder they are in.
    numbered(&text, review_folders(&text), "REVIEW", &mut masks);
    // Temp folder, longest form first.
    let tmp = dir.to_string_lossy().to_string();
    let canonical = std::fs::canonicalize(dir)
        .map(|p: PathBuf| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| tmp.clone());
    for path in [canonical, tmp.clone(), format!("/private{tmp}")] {
        if !masks.iter().any(|(raw, _)| raw == &path) {
            masks.push((path, "$TMP".into()));
        }
    }
    // The system temp folder (the write tool keeps a copy of the old file
    // there), after `$TMP`, which is inside it.
    let system = std::env::temp_dir().to_string_lossy().to_string();
    let system = system.trim_end_matches('/').to_string();
    for path in [format!("/private{system}"), system] {
        if !masks.iter().any(|(raw, _)| raw == &path) {
            masks.push((path, "$SYSTMP".into()));
        }
    }
    // The home folder, if a path under it shows (none does today).
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/').to_string();
        if home.len() > 1 && !masks.iter().any(|(raw, _)| raw == &home) {
            masks.push((home, "$HOME".into()));
        }
    }
    masks.sort_by_key(|(raw, _)| std::cmp::Reverse(raw.len()));
    // Session keys and native ids, by creation order.
    for (n, key) in sessions.iter().enumerate() {
        masks.push((key.0.clone(), format!("$SESSION{}", n + 1)));
    }
    if root.native_id != root.key.0 {
        masks.push((root.native_id.clone(), "$NATIVE1".into()));
    }
    masks.push((format!("\"{}\"", root.instance.0), "\"$INSTANCE\"".into()));
    masks.push((
        format!("\"generation\":{},", rt.generation()),
        "\"generation\":\"$GEN\",".into(),
    ));
    // Branch views and interaction ids, in first-seen order.
    let mut views = Vec::new();
    let mut interactions = Vec::new();
    let mut decisions = Vec::new();
    for envelope in received {
        if let SessionUpdate::BranchReset { view_id, .. } = &envelope.update {
            views.push(view_id.clone());
        }
    }
    for record in records {
        let payload = serde_json::to_value(&record.event.kind).unwrap();
        if let EventKind::Update(e) = &record.event.kind {
            if let SessionUpdate::BranchReset { view_id, .. } = &e.update {
                views.push(view_id.clone());
            }
        }
        if matches!(
            record.event.kind,
            EventKind::EscalationRaised(_)
                | EventKind::EscalationAnswered(_)
                | EventKind::EscalationWithdrawn(_)
        ) {
            strings_under(&payload, "id", &mut interactions);
        }
        if let EventKind::PolicyDecision(p) = &record.event.kind {
            if let Some(id) = p["id"].as_str() {
                decisions.push(id.to_string());
            }
        }
    }
    numbered(&text, views, "VIEW", &mut masks);
    numbered(&text, interactions, "INTERACTION", &mut masks);
    numbered(&text, decisions, "DECISION", &mut masks);
    let masks = Masks(masks);
    raw.iter().map(|line| masks.apply(line)).collect()
}

/// Lines of each Session (`== session N` starts one).
fn by_session(lines: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current = String::from("(none)");
    for line in lines {
        if let Some(name) = line.strip_prefix("== ") {
            current = name.to_string();
        }
        out.entry(current.clone()).or_default().push(line.clone());
    }
    out
}

/// A readable report of the first differing lines of each Session.
fn diff(expected: &[String], actual: &[String]) -> String {
    let (expected, actual) = (by_session(expected), by_session(actual));
    let mut names: Vec<&String> = expected.keys().chain(actual.keys()).collect();
    names.sort();
    names.dedup();
    let mut report = String::new();
    for name in names {
        let empty = Vec::new();
        let e = expected.get(name).unwrap_or(&empty);
        let a = actual.get(name).unwrap_or(&empty);
        if e == a {
            continue;
        }
        let first = e
            .iter()
            .zip(a.iter())
            .position(|(x, y)| x != y)
            .unwrap_or(e.len().min(a.len()));
        report.push_str(&format!(
            "\n{name}: {} expected lines, {} actual; first difference at line {}\n",
            e.len(),
            a.len(),
            first + 1
        ));
        for k in first..(first + 5) {
            if k >= e.len() && k >= a.len() {
                break;
            }
            report.push_str(&format!(
                "  expected: {}\n  actual:   {}\n",
                e.get(k).map_or("(none)", |s| s.as_str()),
                a.get(k).map_or("(none)", |s| s.as_str())
            ));
        }
    }
    report
}

/// Fail when the script did not reach a thing the trace must cover, so a
/// golden file with a hole cannot be recorded.
fn assert_coverage(lines: &[String]) {
    let count = |needle: &str| lines.iter().filter(|l| l.contains(needle)).count();
    let need = |needle: &str, at_least: usize| {
        let n = count(needle);
        assert!(n >= at_least, "{n} lines with {needle:?}, need {at_least}");
    };
    need("\"type\":\"thought_chunk\"", 3 * 2);
    need("\"type\":\"message_chunk\"", 12 * 2);
    need(PARALLEL, 3 * 2);
    need("~", 2);
    need("escalation_raised", 2);
    need("escalation_answered", 1);
    need("escalation_withdrawn", 1);
    need("{\"kind\":\"cancelled\",\"payload\":{}}", 1);
    need("\"answer\":\"deny\"", 1);
    need("\"pending\":true", 2);
    need("\"model\":\"faux/second\"", 2);
    need("== session 2", 1);
    need("subagent_started", 1);
    need("\"reason\":\"compact\"", 2);
    need("\"stop\":\"error\"", 2);
    need("\"via\":\"token\"", 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pump_trace_matches_the_recorded_one() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, root, records, received) = run(dir.path()).await;
        let lines = trace(dir.path(), &rt, &root, &records, &received);
        assert_coverage(&lines);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        if std::env::var(WRITE_ENV).is_ok_and(|v| v == "1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            eprintln!("wrote {} lines to {}", lines.len(), path.display());
            return;
        }
        let expected: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}; set {WRITE_ENV}=1", path.display()))
            .lines()
            .map(str::to_string)
            .collect();
        if expected != lines {
            panic!(
                "the pump trace differs from {}:{}",
                path.display(),
                diff(&expected, &lines)
            );
        }
    })
    .await
    .unwrap();
}
