use super::*;
use async_trait::async_trait;
use loop_protocol::SessionKey;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use tokio::sync::{mpsc, oneshot};

type Reply<T> = oneshot::Sender<Result<T>>;
enum Command {
    Append(Vec<Event>, Reply<Vec<LoggedEvent>>),
    Read(Option<SessionKey>, u64, Reply<Vec<LoggedEvent>>),
    Latest(SessionKey, Reply<u64>),
    Turns(SessionKey, Reply<Vec<TurnId>>),
    Subagents(Reply<std::collections::HashSet<SessionKey>>),
    Allocate(String, u32, i64, Reply<u64>),
    Finish(u64, i64, Reply<()>),
    Activity(Reply<std::collections::HashMap<SessionKey, i64>>),
    Recovery(u64, Reply<Vec<LoggedEvent>>),
    LastRecovered(Reply<u64>),
    MarkRecovered(u64, i64, Reply<()>),
}

/// SQLite Event Log at `<agent_dir>/events.db` (independent of Session storage).
///
/// One blocking writer task owns the connection, including reads. A bounded
/// channel supplies backpressure. Already queued appends are coalesced up to
/// MAX_BATCH_EVENTS with no intentional batching delay (0 ms). Every request is
/// atomic; a transaction error fails all coalesced requests. Dropping all handles
/// drains queued requests and closes the connection; acknowledged data is durable.
#[derive(Clone)]
pub struct SqliteEventLog {
    sender: mpsc::Sender<Command>,
}

impl SqliteEventLog {
    /// Open under the caller-resolved agent directory. No environment mutation.
    pub async fn open(agent_dir: impl AsRef<Path>) -> Result<Self> {
        let path = agent_dir.as_ref().join("events.db");
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| EventLogError::Storage(e.to_string()))?;
            }
            let mut conn = Connection::open(path)?;
            // These pragmas apply to the actual long-lived writer connection.
            conn.execute_batch(
                "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
            )?;
            migrate(&mut conn)?;
            Ok(conn)
        })
        .await
        .map_err(|e| EventLogError::Storage(e.to_string()))??;
        let (sender, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        tokio::task::spawn_blocking(move || writer(conn, receiver));
        Ok(Self { sender })
    }

    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, receive) = oneshot::channel();
        self.sender
            .send(make(reply))
            .await
            .map_err(|_| EventLogError::WriterStopped)?;
        receive.await.map_err(|_| EventLogError::WriterStopped)?
    }
}

#[async_trait]
impl EventLog for SqliteEventLog {
    async fn append_batch(&self, events: Vec<Event>) -> Result<Vec<LoggedEvent>> {
        validate(&events)?;
        self.request(|reply| Command::Append(events, reply)).await
    }
    async fn read_after(&self, session: &SessionKey, after_seq: u64) -> Result<Vec<LoggedEvent>> {
        self.request(|reply| Command::Read(Some(session.clone()), after_seq, reply))
            .await
    }
    async fn read_runtime_after(&self, global_seq: u64) -> Result<Vec<LoggedEvent>> {
        self.request(|reply| Command::Read(None, global_seq, reply))
            .await
    }
    async fn latest_seq(&self, session: &SessionKey) -> Result<u64> {
        self.request(|reply| Command::Latest(session.clone(), reply))
            .await
    }
    async fn subagent_sessions(&self) -> Result<std::collections::HashSet<SessionKey>> {
        self.request(Command::Subagents).await
    }
    async fn turn_ids(&self, session: &SessionKey) -> Result<Vec<TurnId>> {
        self.request(|reply| Command::Turns(session.clone(), reply))
            .await
    }
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> Result<u64> {
        self.request(|reply| Command::Allocate(owner.into(), pid, started_ms, reply))
            .await
    }
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> Result<()> {
        self.request(|reply| Command::Finish(generation, stopped_ms, reply))
            .await
    }
    async fn last_activity_ms(&self) -> Result<std::collections::HashMap<SessionKey, i64>> {
        self.request(Command::Activity).await
    }
    async fn recovery_records(&self, from_generation: u64) -> Result<Vec<LoggedEvent>> {
        self.request(|reply| Command::Recovery(from_generation, reply))
            .await
    }
    async fn last_recovered_generation(&self) -> Result<u64> {
        self.request(Command::LastRecovered).await
    }
    async fn mark_recovered(&self, generation: u64, recovered_ms: i64) -> Result<()> {
        self.request(|reply| Command::MarkRecovered(generation, recovered_ms, reply))
            .await
    }
}

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "001_initial.sql",
        include_str!("migrations/001_initial.sql"),
    ),
    (
        "002_recovery.sql",
        include_str!("migrations/002_recovery.sql"),
    ),
];
fn migrate(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
    )?;
    for (id, sql) in MIGRATIONS {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied = tx
            .query_row("SELECT 1 FROM migrations WHERE id=?1", [id], |_| Ok(()))
            .optional()?
            .is_some();
        if !applied {
            tx.execute_batch(sql)?;
            tx.execute("INSERT INTO migrations (id, applied_at) VALUES (?1, CAST(strftime('%s','now') AS INTEGER)*1000)", [id])?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn writer(mut conn: Connection, mut receiver: mpsc::Receiver<Command>) {
    let mut pending = None;
    while let Some(command) = pending.take().or_else(|| receiver.blocking_recv()) {
        match command {
            Command::Append(events, reply) => {
                let mut count = events.len();
                let mut requests = vec![(events, reply)];
                while count < MAX_BATCH_EVENTS && requests.len() < WRITER_QUEUE_CAPACITY {
                    match receiver.try_recv() {
                        Ok(Command::Append(events, reply))
                            if count + events.len() <= MAX_BATCH_EVENTS =>
                        {
                            count += events.len();
                            requests.push((events, reply));
                        }
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let mut all = Vec::with_capacity(count);
                let replies = requests
                    .into_iter()
                    .map(|(events, reply)| {
                        let len = events.len();
                        all.extend(events);
                        (len, reply)
                    })
                    .collect();
                append_group(&mut conn, all, replies);
            }
            Command::Read(session, after, reply) => {
                let _ = reply.send(read(&conn, session.as_ref(), after));
            }
            Command::Latest(session, reply) => {
                let _ = reply.send(latest(&conn, &session));
            }
            Command::Turns(session, reply) => {
                let _ = reply.send(turn_ids(&conn, &session));
            }
            Command::Subagents(reply) => {
                let _ = reply.send(subagents(&conn));
            }
            Command::Allocate(owner, pid, started, reply) => {
                let _ = reply.send(allocate(&mut conn, &owner, pid, started));
            }
            Command::Activity(reply) => {
                let _ = reply.send(activity(&conn));
            }
            Command::Recovery(from, reply) => {
                let _ = reply.send(recovery(&conn, from));
            }
            Command::LastRecovered(reply) => {
                let _ = reply.send(
                    conn.query_row(
                        "SELECT COALESCE(MAX(generation),0) FROM runtime_generations WHERE recovered_ms IS NOT NULL",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(Into::into),
                );
            }
            Command::MarkRecovered(generation, at, reply) => {
                let result = (|| {
                    let changed = conn.execute(
                        "UPDATE runtime_generations SET recovered_ms=?1 WHERE generation=?2",
                        params![at, sql_seq(generation)?],
                    )?;
                    if changed == 0 {
                        return Err(EventLogError::Invalid("unknown Runtime generation".into()));
                    }
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            Command::Finish(generation, stopped, reply) => {
                let result = (|| {
                    let changed = conn.execute(
                        "UPDATE runtime_generations SET stopped_ms=?1 WHERE generation=?2",
                        params![stopped, sql_seq(generation)?],
                    )?;
                    if changed == 0 {
                        return Err(EventLogError::Invalid("unknown Runtime generation".into()));
                    }
                    Ok(())
                })();
                let _ = reply.send(result);
            }
        }
    }
}

fn activity(conn: &Connection) -> Result<std::collections::HashMap<SessionKey, i64>> {
    let mut stmt = conn.prepare(
        "SELECT session_key, MAX(ts_ms) FROM events WHERE session_key IS NOT NULL GROUP BY session_key",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            SessionKey::from(row.get::<_, String>(0)?),
            row.get::<_, i64>(1)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn append_group(
    conn: &mut Connection,
    events: Vec<Event>,
    requests: Vec<(usize, Reply<Vec<LoggedEvent>>)>,
) {
    match append(conn, events) {
        Ok(records) => {
            let mut records = records.into_iter();
            for (len, reply) in requests {
                let _ = reply.send(Ok(records.by_ref().take(len).collect()));
            }
        }
        Err(error) => {
            for (_, reply) in requests {
                let _ = reply.send(Err(error.clone()));
            }
        }
    }
}

fn latest(conn: &Connection, session: &SessionKey) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1",
        [session.as_str()],
        |row| row.get(0),
    )?)
}

fn subagents(conn: &Connection) -> Result<std::collections::HashSet<SessionKey>> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT session_key FROM events WHERE kind='session_opened' \
         AND session_key IS NOT NULL \
         AND json_extract(payload,'$.event.payload.parent') IS NOT NULL",
    )?;
    let keys = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|key| key.map(SessionKey::from))
        .collect::<rusqlite::Result<_>>()?;
    Ok(keys)
}

fn turn_ids(conn: &Connection, session: &SessionKey) -> Result<Vec<TurnId>> {
    let mut statement =
        conn.prepare("SELECT DISTINCT turn FROM events WHERE session_key=?1 AND turn IS NOT NULL")?;
    let turns = statement
        .query_map([session.as_str()], |row| row.get::<_, String>(0))?
        .map(|turn| turn.map(TurnId))
        .collect::<rusqlite::Result<_>>()?;
    Ok(turns)
}

fn append(conn: &mut Connection, events: Vec<Event>) -> Result<Vec<LoggedEvent>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut global: u64 =
        tx.query_row("SELECT COALESCE(MAX(global_seq),0) FROM events", [], |r| {
            r.get(0)
        })?;
    let mut records = Vec::with_capacity(events.len());
    for event in events {
        global = global
            .checked_add(1)
            .ok_or_else(|| EventLogError::Storage("global sequence exhausted".into()))?;
        let seq = if matches!(event.kind, EventKind::Update(_)) {
            Some(latest(&tx, event.session_key.as_ref().expect("validated Update"))? + 1)
        } else {
            None
        };
        let record = stamp(event, global, seq);
        let raw = serde_json::to_value(&record.event.kind)?;
        let kind = raw
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| EventLogError::Invalid("Unknown must retain its kind".into()))?;
        let payload = serde_json::to_string(&serde_json::json!({"v":1,"event":raw}))?;
        tx.execute("INSERT INTO events (global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![sql_seq(global)?, sql_seq(record.event.generation)?, record.event.session_key.as_ref().map(SessionKey::as_str), seq.map(sql_seq).transpose()?, kind, record.event.turn.as_ref().map(TurnId::as_str), record.event.ts_ms, payload])?;
        records.push(record);
    }
    tx.commit()?;
    Ok(records)
}

const COLUMNS: &str = "global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload";

fn read(conn: &Connection, session: Option<&SessionKey>, after: u64) -> Result<Vec<LoggedEvent>> {
    // All stored sequences fit i64; a larger cursor is simply past the end.
    if after > i64::MAX as u64 {
        return Ok(Vec::new());
    }
    let sql = if session.is_some() {
        format!("SELECT {COLUMNS} FROM events WHERE session_key=?1 AND session_seq>?2 ORDER BY session_seq")
    } else {
        format!(
            "SELECT {COLUMNS} FROM events WHERE ?1 IS NULL AND global_seq>?2 ORDER BY global_seq"
        )
    };
    rows(
        conn,
        &sql,
        params![session.map(SessionKey::as_str), after as i64],
    )
}

/// Turn boundaries and Escalation records, filtered in SQL so recovery never
/// loads streamed text.
fn recovery(conn: &Connection, from_generation: u64) -> Result<Vec<LoggedEvent>> {
    if from_generation > i64::MAX as u64 {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM events WHERE generation>=?1 AND (\
         kind IN ('escalation_raised','escalation_answered','escalation_withdrawn') \
         OR (kind='update' AND json_extract(payload,'$.event.payload.update.type') \
             IN ('turn_started','turn_ended'))) ORDER BY global_seq"
    );
    rows(conn, &sql, params![from_generation as i64])
}

fn rows(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Vec<LoggedEvent>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(args)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let payload: String = row.get(7)?;
        let payload: Value = serde_json::from_str(&payload)?;
        let raw = payload
            .get("event")
            .ok_or_else(|| EventLogError::Storage("missing versioned event payload".into()))?
            .clone();
        let kind = if payload.get("v") == Some(&Value::from(1)) {
            serde_json::from_value(raw)?
        } else {
            EventKind::Unknown(raw)
        };
        result.push(LoggedEvent {
            global_seq: row.get(0)?,
            session_seq: row.get(3)?,
            event: Event {
                generation: row.get(1)?,
                session_key: row.get::<_, Option<String>>(2)?.map(Into::into),
                turn: row.get::<_, Option<String>>(5)?.map(Into::into),
                ts_ms: row.get(6)?,
                kind,
            },
        });
    }
    Ok(result)
}

fn allocate(conn: &mut Connection, owner: &str, pid: u32, started: i64) -> Result<u64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let generation: u64 = tx.query_row(
        "SELECT COALESCE(MAX(generation),0)+1 FROM runtime_generations",
        [],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO runtime_generations (generation,owner,pid,started_ms) VALUES (?1,?2,?3,?4)",
        params![sql_seq(generation)?, owner, pid, started],
    )?;
    tx.commit()?;
    Ok(generation)
}
