//! The pump's sequencer inside a Runtime (step A0): who caused a queued
//! update, and the in-progress snapshot under concurrent publishing.
use super::*;
use crate::log::{LoggedEvent, MemoryEventLog};
use crate::sequencer::{Sequencer, Snapshot};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, StopReason as AiStopReason,
    TextContent, ThinkingContent,
};
use loop_store::Scope;
use std::collections::BTreeMap;
use std::pin::Pin;

const LIMIT: Duration = Duration::from_secs(120);

/// An Event Log whose appends wait while it is closed.
struct GatedLog {
    inner: MemoryEventLog,
    open: watch::Sender<bool>,
    waiting: std::sync::atomic::AtomicUsize,
}
impl GatedLog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryEventLog::new(),
            open: watch::channel(true).0,
            waiting: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    fn set_open(&self, open: bool) {
        self.open.send_replace(open);
    }
    fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Acquire)
    }
}
#[async_trait]
impl EventLog for GatedLog {
    async fn append_batch(&self, events: Vec<Event>) -> crate::log::Result<Vec<LoggedEvent>> {
        let mut open = self.open.subscribe();
        if !*open.borrow() {
            self.waiting.fetch_add(1, Ordering::AcqRel);
            let _ = open.wait_for(|open| *open).await;
            self.waiting.fetch_sub(1, Ordering::AcqRel);
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
    ) -> crate::log::Result<Vec<LoggedEvent>> {
        self.inner.read_after(tenant, session, after_seq).await
    }
    async fn read_runtime_after(
        &self,
        scope: Scope<'_>,
        global_seq: u64,
    ) -> crate::log::Result<Vec<LoggedEvent>> {
        self.inner.read_runtime_after(scope, global_seq).await
    }
    async fn latest_seq(&self, tenant: &Tenant, session: &SessionKey) -> crate::log::Result<u64> {
        self.inner.latest_seq(tenant, session).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        started_ms: i64,
    ) -> crate::log::Result<u64> {
        self.inner.allocate_generation(owner, pid, started_ms).await
    }
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> crate::log::Result<()> {
        self.inner.finish_generation(generation, stopped_ms).await
    }
}

type Responder =
    Arc<dyn Fn(&Context) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> + Send + Sync>;

async fn runtime(
    dir: &std::path::Path,
    log: Arc<dyn EventLog>,
    responder: Responder,
) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(responder));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let opts = BootstrapOpts {
        cwd: dir.into(),
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
    };
    let store = globals.open_runtime_session_store().unwrap();
    Runtime::new(
        globals,
        opts,
        log,
        store,
        RuntimeLimits {
            max_sessions: 32,
            max_active_turns: 16,
        },
    )
    .await
    .unwrap()
    .into_shared()
}

fn host(uid: u32) -> Caller {
    Caller::host(Principal::local_uid(uid))
}

fn text_responder() -> Responder {
    Arc::new(|_| Box::pin(std::future::ready(FauxResponse::Text("done".into()))))
}

/// The two limits that the old `Acting` cells documented, with a real
/// unclaimed change: the tool gate records a mode on its own (no setter
/// claims its revision, and it has no turn id). It waits in the pump's
/// queue while another caller's prompt is admitted. It stays the caller's
/// whose turn or command the backend made it in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_unclaimed_change_keeps_its_caller_when_another_caller_s_turn_is_admitted() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let log = GatedLog::new();
        let rt = runtime(dir.path(), log.clone(), text_responder()).await;
        let (me, alice) = (host(501), Caller::local_token(Principal::local_uid(502)));
        let session = me
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await
            .unwrap();
        let turn = me
            .clone()
            .scope(rt.prompt(&session, vec![ContentBlock::text("mine")]))
            .await
            .unwrap();
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        let native = Arc::clone(&rt.route(&session).unwrap().opened.native);
        let before = native.revision();
        log.set_open(false);
        // Two changes the gate makes on its own, after my turn: the first
        // blocks the pump in its commit, the second waits in its queue.
        native.record_gate_mode("ask").await.unwrap();
        while log.waiting() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        native.record_gate_mode("full-access").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Alice's prompt is admitted meanwhile.
        let turn = alice
            .clone()
            .scope(rt.prompt(&session, vec![ContentBlock::text("hers")]))
            .await
            .unwrap();
        log.set_open(true);
        assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
        let records = log
            .inner
            .read_after(&Tenant::local(), &session.key, 0)
            .await
            .unwrap();
        let mut changes = 0;
        let mut hers = 0;
        for record in &records {
            let EventKind::Update(envelope) = &record.event.kind else {
                continue;
            };
            match &envelope.update {
                SessionUpdate::ConfigChanged(c) if c.revision > before => {
                    changes += 1;
                    assert_eq!(
                        record.event.identity,
                        Identity::acting(&me, Tenant::local()),
                        "an unclaimed change queued before Alice's turn is not hers"
                    );
                }
                _ if envelope.turn.is_some()
                    && record.event.identity == Identity::acting(&alice, Tenant::local()) =>
                {
                    hers += 1
                }
                _ => {}
            }
        }
        assert_eq!(changes, 2);
        assert!(hers > 0, "Alice's turn is hers");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

fn faux_model() -> loop_ai::Model {
    loop_ai::Model {
        id: "faux-model".into(),
        name: "Faux Model".into(),
        api: loop_ai::API_OPENAI_COMPLETIONS.to_string(),
        provider: "faux".into(),
        base_url: "http://faux.local".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![loop_ai::InputModality::Text],
        cost: loop_ai::ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

/// Thinking then text, `chunks` deltas each, numbered so that a repeat or a
/// gap shows in the joined text.
fn streaming(chunks: usize) -> Responder {
    Arc::new(move |_| {
        Box::pin(async move {
            let model = faux_model();
            let mut partial = AssistantMessage::pending(&model);
            let mut events = vec![AssistantMessageEvent::Start {
                partial: partial.clone(),
            }];
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
            let mut thought = String::new();
            for i in 0..chunks {
                let delta = format!("t{i};");
                thought.push_str(&delta);
                events.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta,
                    partial: partial.clone(),
                });
            }
            events.push(AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: thought.clone(),
                partial: partial.clone(),
            });
            if let AssistantContent::Thinking(t) = &mut partial.content[0] {
                t.thinking = thought;
            }
            partial.content.push(AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }));
            events.push(AssistantMessageEvent::TextStart {
                content_index: 1,
                partial: partial.clone(),
            });
            let mut text = String::new();
            for i in 0..chunks {
                let delta = format!("x{i};");
                text.push_str(&delta);
                events.push(AssistantMessageEvent::TextDelta {
                    content_index: 1,
                    delta,
                    partial: partial.clone(),
                });
            }
            events.push(AssistantMessageEvent::TextEnd {
                content_index: 1,
                content: text.clone(),
                partial: partial.clone(),
            });
            if let AssistantContent::Text(t) = &mut partial.content[1] {
                t.text = text;
            }
            partial.usage.output = 1;
            partial.usage.total_tokens = 1;
            partial.stop_reason = AiStopReason::Stop;
            events.push(AssistantMessageEvent::Done {
                reason: AiStopReason::Stop,
                message: partial,
            });
            FauxResponse::Events(events)
        })
    })
}

/// Text and thought of each message, by message id.
type Texts = BTreeMap<String, (String, String)>;

fn add(texts: &mut Texts, update: &SessionUpdate) {
    match update {
        SessionUpdate::MessageChunk {
            message_id,
            content,
            ..
        } => texts
            .entry(message_id.0.clone())
            .or_default()
            .0
            .push_str(content.as_text().unwrap_or_default()),
        SessionUpdate::ThoughtChunk {
            message_id,
            content,
        } => texts
            .entry(message_id.0.clone())
            .or_default()
            .1
            .push_str(content.as_text().unwrap_or_default()),
        _ => {}
    }
}

fn updates_of(records: &[LoggedEvent]) -> Vec<UpdateEnvelope> {
    records
        .iter()
        .filter_map(|r| match &r.event.kind {
            EventKind::Update(envelope) => Some(envelope.clone()),
            _ => None,
        })
        .collect()
}

/// The future attach of ADR-0007 §2.3, as a reader: subscribe, take `H`
/// and the snapshot, read the log up to `H`, then follow the live tail.
/// Returns what it rebuilt, once the Session's `turns`-th turn has ended.
async fn attach_and_follow(
    rt: Arc<Runtime>,
    log: Arc<GatedLog>,
    session: SessionRef,
    turns: usize,
) -> (Texts, u64) {
    let mut live = rt.updates.subscribe();
    let snapshot: Snapshot = rt.in_progress(&session).unwrap();
    let h = snapshot.through_seq;
    let rows = log
        .inner
        .read_after(&Tenant::local(), &session.key, 0)
        .await
        .unwrap();
    let durable: Vec<UpdateEnvelope> = updates_of(&rows)
        .into_iter()
        .filter(|e| e.seq <= h)
        .collect();
    assert_eq!(
        durable.len() as u64,
        h,
        "the log holds every update up to H"
    );
    // The snapshot is exactly the state of the updates up to H.
    let mut again = Sequencer::new(0, Identity::runtime_in(Tenant::local()));
    for envelope in &durable {
        again.committed(envelope.seq, &envelope.update);
    }
    assert_eq!(again.snapshot(), snapshot, "snapshot at H = {h}");
    // Durable replay (old-format rule: chunk rows of a message still open
    // at H are in the snapshot, not replayed), then the snapshot.
    let open: Vec<String> = snapshot.messages.iter().map(|m| m.id.0.clone()).collect();
    let mut texts = Texts::new();
    let mut ended = 0;
    for envelope in &durable {
        match &envelope.update {
            SessionUpdate::MessageChunk { message_id, .. }
            | SessionUpdate::ThoughtChunk { message_id, .. }
                if open.contains(&message_id.0) => {}
            SessionUpdate::TurnEnded { .. } => ended += 1,
            update => add(&mut texts, update),
        }
    }
    for m in &snapshot.messages {
        let slot = texts.entry(m.id.0.clone()).or_default();
        slot.0 = m.content.iter().filter_map(ContentBlock::as_text).collect();
        slot.1 = m.thought.iter().filter_map(ContentBlock::as_text).collect();
    }
    // The live tail: drop what is at or below H; the rest has no gap.
    let mut last = h;
    while ended < turns {
        let envelope = match live.recv().await {
            Ok(envelope) => envelope,
            Err(error) => panic!("the reader lost the live tail: {error}"),
        };
        if envelope.session != session.key || envelope.seq <= h {
            continue;
        }
        assert_eq!(envelope.seq, last + 1, "a gap or a repeat after H = {h}");
        last = envelope.seq;
        if matches!(envelope.update, SessionUpdate::TurnEnded { .. }) {
            ended += 1;
        }
        add(&mut texts, &envelope.update);
    }
    (texts, h)
}

/// Task 5: many Sessions publish at once while readers attach at random
/// points. Each snapshot equals the state of the log up to its `H`, and
/// each reader gets every character exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_snapshot_and_h_give_every_character_once_under_concurrent_publishing() {
    tokio::time::timeout(LIMIT, async {
        const SESSIONS: usize = 12;
        const TURNS: usize = 3;
        const READERS: usize = 6;
        let dir = tempfile::tempdir().unwrap();
        let log = GatedLog::new();
        let rt = runtime(dir.path(), log.clone(), streaming(120)).await;
        let mut sessions = Vec::new();
        for _ in 0..SESSIONS {
            sessions.push(rt.new_session(SessionSpec::default()).await.unwrap());
        }
        let mut drivers = tokio::task::JoinSet::new();
        let mut readers = tokio::task::JoinSet::new();
        for (n, session) in sessions.iter().enumerate() {
            let (rt2, session2) = (rt.clone(), session.clone());
            drivers.spawn(async move {
                for t in 0..TURNS {
                    let turn = rt2
                        .prompt(&session2, vec![ContentBlock::text(format!("turn {t}"))])
                        .await
                        .unwrap();
                    assert!(matches!(turn.outcome().await, TurnOutcome::Completed(_)));
                }
            });
            for r in 0..READERS {
                let (rt2, log2, session2) = (rt.clone(), log.clone(), session.clone());
                let mut seen = rt.updates.subscribe();
                readers.spawn(async move {
                    // Attach at different points of the stream: after this
                    // many updates of the Session (a turn has about 250).
                    let mut wait = (n * 37 + r * 101) % 700;
                    while wait > 0 {
                        match seen.recv().await {
                            Ok(e) if e.session == session2.key => wait -= 1,
                            Ok(_) => {}
                            Err(error) => panic!("{error}"),
                        }
                    }
                    drop(seen);
                    let (texts, h) = attach_and_follow(rt2, log2, session2.clone(), TURNS).await;
                    (session2, texts, h)
                });
            }
        }
        while let Some(done) = drivers.join_next().await {
            done.unwrap();
        }
        let mut mid_message = 0;
        while let Some(done) = readers.join_next().await {
            let (session, texts, h) = done.unwrap();
            let rows = log
                .inner
                .read_after(&Tenant::local(), &session.key, 0)
                .await
                .unwrap();
            let mut want = Texts::new();
            for envelope in updates_of(&rows) {
                add(&mut want, &envelope.update);
            }
            // The prompt and the answer of each turn.
            assert_eq!(want.len(), 2 * TURNS);
            let answers = want.values().filter(|(_, thought)| !thought.is_empty());
            assert_eq!(answers.count(), TURNS);
            for (id, (text, thought)) in &want {
                assert!(
                    thought.is_empty() || (text.starts_with("x0;") && thought.starts_with("t0;"))
                );
                assert_eq!(
                    texts.get(id),
                    Some(&(text.clone(), thought.clone())),
                    "{id}"
                );
            }
            let partial = updates_of(&rows).iter().any(|e| {
                e.seq == h
                    && matches!(
                        e.update,
                        SessionUpdate::MessageChunk { .. } | SessionUpdate::ThoughtChunk { .. }
                    )
            });
            mid_message += usize::from(partial);
        }
        eprintln!(
            "{mid_message} of {} readers attached in the middle of a message",
            SESSIONS * READERS
        );
        assert!(
            mid_message > 0,
            "no reader attached in the middle of a message"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
