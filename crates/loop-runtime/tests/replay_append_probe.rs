//! Append latency while a large replay runs (task B2). Ignored by default.
//!
//! The probe opens a SQLite Event Log, writes one large Session history
//! (`PROBE_MB` MiB of `message_chunk` Updates), and then measures single-event
//! appends to another Session two times:
//!
//! * `idle`: no read runs;
//! * `replay`: `PROBE_READERS` tasks read the whole large Session with
//!   `read_after(key, 0)` again and again, as an attach with an old cursor does.
//!
//! It uses only `append_batch` and `read_after`, so the same file runs on the
//! code before and after the reader/writer split.
//!
//! Parameters (environment variables, defaults in brackets):
//!
//! ```text
//! PROBE_MB [8]          size of the large Session's payload, in MiB
//! PROBE_EVENT_BYTES [1024] text bytes per Update
//! PROBE_READERS [2]     replay tasks that run at the same time
//! PROBE_APPENDS [300]   appends per phase
//! PROBE_GAP_MS [2]      pause between two appends
//! ```
//!
//! ```text
//! cargo test --locked -p loop-runtime --test replay_append_probe -- --ignored --nocapture
//! ```
//!
//! The result is one line that starts with `PROBE|RESULT|` and holds JSON.

use loop_protocol::{ContentBlock, Role, SessionKey, SessionUpdate, UpdateEnvelope};
use loop_runtime::log::{Event, EventLog, SqliteEventLog};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn chunk(session: &str, text: String) -> Event {
    Event::update(
        1,
        1,
        UpdateEnvelope::live(
            "loop".into(),
            0,
            SessionKey::from(session),
            Some("turn-1".into()),
            0,
            SessionUpdate::MessageChunk {
                message_id: "msg-1".into(),
                role: Role::Agent,
                content: ContentBlock::Text { text },
            },
        ),
    )
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

async fn appends(log: &Arc<SqliteEventLog>, count: u64, gap: Duration) -> Vec<f64> {
    let mut latencies = Vec::with_capacity(count as usize);
    for i in 0..count {
        let start = Instant::now();
        log.append_batch(vec![chunk("small", format!("tick {i}"))])
            .await
            .expect("append");
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
        tokio::time::sleep(gap).await;
    }
    latencies.sort_by(f64::total_cmp);
    latencies
}

fn summary(sorted: &[f64]) -> serde_json::Value {
    json!({
        "p50_ms": percentile(sorted, 0.50),
        "p99_ms": percentile(sorted, 0.99),
        "max_ms": sorted.last().copied().unwrap_or(0.0),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; run by hand"]
async fn append_latency_during_large_replay() {
    let mb = env_u64("PROBE_MB", 8);
    let event_bytes = env_u64("PROBE_EVENT_BYTES", 1024) as usize;
    let readers = env_u64("PROBE_READERS", 2);
    let count = env_u64("PROBE_APPENDS", 300);
    let gap = Duration::from_millis(env_u64("PROBE_GAP_MS", 2));

    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(SqliteEventLog::open(dir.path()).await.unwrap());
    let events = (mb * 1024 * 1024) as usize / event_bytes;
    let text = "x".repeat(event_bytes);
    let seed_start = Instant::now();
    let mut written = 0;
    while written < events {
        let n = (events - written).min(loop_runtime::log::MAX_BATCH_EVENTS);
        log.append_batch((0..n).map(|_| chunk("big", text.clone())).collect())
            .await
            .unwrap();
        written += n;
    }
    let seed_s = seed_start.elapsed().as_secs_f64();

    // One replay alone, for scale.
    let replay_start = Instant::now();
    let replayed = log.read_after(&"big".into(), 0).await.unwrap().len();
    let replay_ms = replay_start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(replayed, events);

    let idle = appends(&log, count, gap).await;

    let stop = Arc::new(AtomicBool::new(false));
    let replays = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for _ in 0..readers {
        let log = log.clone();
        let stop = stop.clone();
        let replays = replays.clone();
        tasks.push(tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let records = log.read_after(&"big".into(), 0).await.unwrap();
                assert!(records.len() >= events);
                replays.fetch_add(1, Ordering::AcqRel);
            }
        }));
    }
    // Let the readers start.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let busy = appends(&log, count, gap).await;
    stop.store(true, Ordering::Release);
    for task in tasks {
        task.await.unwrap();
    }

    let result = json!({
        "events": events,
        "event_bytes": event_bytes,
        "payload_mib": mb,
        "readers": readers,
        "appends_per_phase": count,
        "seed_s": seed_s,
        "replay_alone_ms": replay_ms,
        "replays_during_phase": replays.load(Ordering::Acquire),
        "idle": summary(&idle),
        "replay": summary(&busy),
    });
    println!(
        "PROBE| idle append p50 {:.2} ms p99 {:.2} ms | during replay p50 {:.2} ms p99 {:.2} ms max {:.2} ms | one replay {:.0} ms",
        percentile(&idle, 0.5),
        percentile(&idle, 0.99),
        percentile(&busy, 0.5),
        percentile(&busy, 0.99),
        busy.last().copied().unwrap_or(0.0),
        replay_ms
    );
    println!("PROBE|RESULT|{result}");
}
