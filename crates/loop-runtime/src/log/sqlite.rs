use super::*;
use async_trait::async_trait;
use loop_protocol::{SessionKey, TurnId};
use loop_store::{Cursor, Page, PageLimit, SessionSummary, StoreCapabilities, SummaryDelta};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::{mpsc, oneshot, Semaphore};

type Reply<T> = oneshot::Sender<Result<T>>;

/// Read-only connections, and so concurrent blocking reads, per log.
pub const READER_POOL_SIZE: usize = 4;
/// Events applied per transaction when the summary projection is built from
/// an older database, or caught up.
const BACKFILL_CHUNK: usize = 2048;

/// The driver error as a store error. `loop-store` has no `rusqlite`
/// types, so this conversion lives with the SQLite implementation.
fn db(error: rusqlite::Error) -> EventLogError {
    EventLogError::Storage(error.to_string())
}

/// Error of the functions that run on a connection. It exists so that `?`
/// converts `rusqlite` errors; replies convert it back to the store error.
#[derive(Debug)]
enum SqlError {
    Store(EventLogError),
    /// A reader's snapshot did not hold an append that was already
    /// acknowledged. The read is tried again on a new connection.
    Stale {
        seen: u64,
        floor: u64,
    },
}
type SqlResult<T> = std::result::Result<T, SqlError>;

impl From<rusqlite::Error> for SqlError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(db(error))
    }
}
impl From<EventLogError> for SqlError {
    fn from(error: EventLogError) -> Self {
        Self::Store(error)
    }
}
impl From<serde_json::Error> for SqlError {
    fn from(error: serde_json::Error) -> Self {
        Self::Store(error.into())
    }
}
impl From<SqlError> for EventLogError {
    fn from(error: SqlError) -> Self {
        match error {
            SqlError::Store(error) => error,
            SqlError::Stale { seen, floor } => EventLogError::Storage(format!(
                "reader snapshot at global_seq {seen} is older than the acknowledged {floor}"
            )),
        }
    }
}

/// Sequences and generations are stored as signed 64-bit integers.
fn sql_seq(seq: u64) -> Result<i64> {
    i64::try_from(seq)
        .map_err(|_| EventLogError::Invalid("sequence exceeds SQLite integer range".into()))
}
/// A read bound: stored sequences fit `i64`, so a larger bound reads to the end.
fn bound(seq: u64) -> i64 {
    i64::try_from(seq).unwrap_or(i64::MAX)
}
/// The `LIMIT` of a page: one row more than it holds, to know if more exist.
fn page_rows_limit(limit: PageLimit) -> i64 {
    i64::try_from(limit.max_events()).map_or(i64::MAX, |n| n.saturating_add(1))
}

/// Requests that the writer serves. Reads never go through it.
enum Command {
    Append(Vec<Event>, Reply<Vec<LoggedEvent>>),
    Allocate(String, u32, i64, Reply<u64>),
    Finish(u64, i64, Reply<()>),
    MarkRecovered(u64, i64, Reply<()>),
}

/// SQLite Event Log at `<agent_dir>/events.db` (independent of Session storage).
///
/// **Writer.** One blocking writer task owns the read-write connection and
/// serves appends and generation records only. A bounded channel
/// (`WRITER_QUEUE_CAPACITY`) supplies backpressure. Already queued appends
/// are coalesced up to MAX_BATCH_EVENTS with no intentional batching delay.
/// Every request is atomic; a transaction error fails all coalesced requests.
/// Each transaction also updates the `session_summary` projection, so the
/// projection never lags the events.
///
/// **Readers.** Every read runs on a pool of at most [`READER_POOL_SIZE`]
/// read-only connections, on blocking threads, never on the writer. WAL lets
/// them read while the writer commits, so a long read never delays an
/// append. Long reads are paged: each page is one short read transaction.
///
/// **Read consistency.** A read that starts after an append returned sees
/// that append. The writer replies only after COMMIT, and each read is a new
/// read transaction, which in WAL mode sees every commit before it. The log
/// also checks it: the writer records the newest acknowledged `global_seq`
/// before it replies, each read captures that value when it is called, and
/// the read's snapshot must hold it. A pooled connection never keeps a
/// transaction open: one that is not in autocommit mode is dropped.
///
/// Dropping all handles drains queued requests and closes the connections;
/// acknowledged data is durable.
#[derive(Clone)]
pub struct SqliteEventLog {
    sender: mpsc::Sender<Command>,
    readers: Arc<Readers>,
}

/// The read-only connection pool.
struct Readers {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    /// One permit per connection. A permit moves into the blocking task, so
    /// a cancelled caller cannot free it while the read still runs.
    permits: Arc<Semaphore>,
    /// The newest `global_seq` the writer acknowledged to a caller.
    acknowledged: Arc<AtomicU64>,
}

impl Readers {
    fn open_connection(&self) -> SqlResult<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }

    /// An idle connection, or a new one. A connection with an open
    /// transaction would read an old snapshot, so it is never reused.
    fn checkout(&self) -> SqlResult<Connection> {
        loop {
            let pooled = self
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            match pooled {
                Some(conn) if conn.is_autocommit() => return Ok(conn),
                Some(_) => continue,
                None => return self.open_connection(),
            }
        }
    }

    fn checkin(&self, conn: Connection) {
        if conn.is_autocommit() {
            self.idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(conn);
        }
    }

    /// Run `read` in one read transaction whose snapshot holds `floor`.
    fn run<T>(&self, floor: u64, read: &impl Fn(&Connection) -> SqlResult<T>) -> SqlResult<T> {
        let conn = self.checkout()?;
        let result = snapshot(&conn, floor, read);
        let retry = matches!(result, Err(SqlError::Stale { .. })) || !conn.is_autocommit();
        if !retry {
            self.checkin(conn);
            return result;
        }
        // Drop the connection, and try once more on a new one.
        drop(conn);
        let conn = self.open_connection()?;
        let result = snapshot(&conn, floor, read);
        self.checkin(conn);
        result
    }
}

const MAX_GLOBAL_SQL: &str = "SELECT COALESCE(MAX(global_seq),0) FROM events";

fn snapshot<T>(
    conn: &Connection,
    floor: u64,
    read: &impl Fn(&Connection) -> SqlResult<T>,
) -> SqlResult<T> {
    conn.execute_batch("BEGIN DEFERRED")?;
    let result = (|| {
        // The first read starts the snapshot.
        let seen: u64 = conn
            .prepare_cached(MAX_GLOBAL_SQL)?
            .query_row([], |row| row.get(0))?;
        if seen < floor {
            return Err(SqlError::Stale { seen, floor });
        }
        read(conn)
    })();
    match result {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(error) => {
            // A failed rollback leaves the connection out of autocommit,
            // and then it is dropped.
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

impl SqliteEventLog {
    /// Open under the caller-resolved agent directory. No environment mutation.
    ///
    /// Applies the migrations, then builds or catches up the summary
    /// projection from the events (in chunks; an interrupted build continues
    /// at the next open) before the writer serves.
    pub async fn open(agent_dir: impl AsRef<Path>) -> Result<Self> {
        let path = agent_dir.as_ref().join("events.db");
        let writer_path = path.clone();
        let (conn, last) = tokio::task::spawn_blocking(move || -> SqlResult<(Connection, u64)> {
            if let Some(parent) = writer_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| EventLogError::Storage(e.to_string()))?;
            }
            let mut conn = Connection::open(writer_path)?;
            // These pragmas apply to the actual long-lived writer connection.
            conn.execute_batch(
                "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
            )?;
            migrate(&mut conn)?;
            backfill(&mut conn, BACKFILL_CHUNK, None)?;
            let last = conn.query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))?;
            Ok((conn, last))
        })
        .await
        .map_err(|e| EventLogError::Storage(e.to_string()))??;
        let acknowledged = Arc::new(AtomicU64::new(last));
        let (sender, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let ack = Arc::clone(&acknowledged);
        tokio::task::spawn_blocking(move || writer(conn, receiver, &ack));
        Ok(Self {
            sender,
            readers: Arc::new(Readers {
                path,
                idle: Mutex::new(Vec::with_capacity(READER_POOL_SIZE)),
                permits: Arc::new(Semaphore::new(READER_POOL_SIZE)),
                acknowledged,
            }),
        })
    }

    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, receive) = oneshot::channel();
        self.sender
            .send(make(reply))
            .await
            .map_err(|_| EventLogError::Stopped)?;
        receive.await.map_err(|_| EventLogError::Stopped)?
    }

    /// Run `read` on a reader connection, in one read transaction that sees
    /// every append acknowledged before this call.
    ///
    /// Cancellation-safe: before the permit is held nothing is taken; after,
    /// the permit and the connection live in the blocking task, which returns
    /// both when the read ends even if the caller is gone.
    async fn read<T, F>(&self, read: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(&Connection) -> SqlResult<T> + Send + 'static,
    {
        let floor = self.readers.acknowledged.load(Ordering::Acquire);
        let permit = Arc::clone(&self.readers.permits)
            .acquire_owned()
            .await
            .map_err(|_| EventLogError::Stopped)?;
        let readers = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            readers.run(floor, &read).map_err(EventLogError::from)
        })
        .await
        .map_err(|e| EventLogError::Storage(e.to_string()))?
    }

    /// Pages of one Session, `(after_seq, through]`, with `through` pinned by
    /// the first page, so a fast writer cannot keep the loop going.
    async fn read_session_all(
        &self,
        session: &SessionKey,
        after_seq: u64,
    ) -> Result<Vec<LoggedEvent>> {
        let key = session.clone();
        let (through, first) = self
            .read(move |conn| {
                let through = latest(conn, &key)?;
                let page = session_page(conn, &key, after_seq, through, PageLimit::DEFAULT)?;
                Ok((through, page))
            })
            .await?;
        let mut records = first.items;
        let mut next = first.next;
        while let Some(Cursor(after)) = next {
            let key = session.clone();
            let page = self
                .read(move |conn| session_page(conn, &key, after, through, PageLimit::DEFAULT))
                .await?;
            records.extend(page.items);
            next = page.next;
        }
        Ok(records)
    }
}

#[async_trait]
impl EventLog for SqliteEventLog {
    async fn append_batch(&self, events: Vec<Event>) -> Result<Vec<LoggedEvent>> {
        validate(&events)?;
        self.request(|reply| Command::Append(events, reply)).await
    }
    async fn read_after(&self, session: &SessionKey, after_seq: u64) -> Result<Vec<LoggedEvent>> {
        self.read_session_all(session, after_seq).await
    }
    async fn read_runtime_after(&self, global_seq: u64) -> Result<Vec<LoggedEvent>> {
        let (through, first) = self
            .read(move |conn| {
                let through: u64 = conn.query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))?;
                let page = global_page(conn, global_seq, through, PageLimit::DEFAULT)?;
                Ok((through, page))
            })
            .await?;
        let mut records = first.items;
        let mut next = first.next;
        while let Some(Cursor(after)) = next {
            let page = self
                .read(move |conn| global_page(conn, after, through, PageLimit::DEFAULT))
                .await?;
            records.extend(page.items);
            next = page.next;
        }
        Ok(records)
    }
    async fn latest_seq(&self, session: &SessionKey) -> Result<u64> {
        let key = session.clone();
        self.read(move |conn| latest(conn, &key)).await
    }
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            paged_reads: true,
            reader_pool: true,
            ..StoreCapabilities::EVENT_LOG_V1
        }
    }
    async fn read_session_page(
        &self,
        session: &SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        let key = session.clone();
        let through = through_seq.unwrap_or(u64::MAX);
        self.read(move |conn| session_page(conn, &key, after_seq, through, limit))
            .await
    }
    async fn session_summary(&self, session: &SessionKey) -> Result<Option<SessionSummary>> {
        let key = session.clone();
        self.read(move |conn| summary(conn, &key)).await
    }
    async fn recovery_page(
        &self,
        from_generation: u64,
        after_global: u64,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        self.read(move |conn| recovery_page(conn, from_generation, after_global, limit))
            .await
    }
    async fn subagent_sessions(&self) -> Result<HashSet<SessionKey>> {
        self.read(subagents).await
    }
    async fn prompts_sent(&self, session: &SessionKey) -> Result<u64> {
        let key = session.clone();
        self.read(move |conn| {
            Ok(conn
                .prepare_cached(PROMPTS_SQL)?
                .query_row([key.as_str()], |row| row.get(0))
                .optional()?
                .unwrap_or(0))
        })
        .await
    }
    async fn turn_ids(&self, session: &SessionKey) -> Result<Vec<TurnId>> {
        let key = session.clone();
        self.read(move |conn| turn_ids(conn, &key)).await
    }
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> Result<u64> {
        self.request(|reply| Command::Allocate(owner.into(), pid, started_ms, reply))
            .await
    }
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> Result<()> {
        self.request(|reply| Command::Finish(generation, stopped_ms, reply))
            .await
    }
    async fn last_activity_ms(&self) -> Result<HashMap<SessionKey, i64>> {
        self.read(activity).await
    }
    async fn recovery_records(&self, from_generation: u64) -> Result<Vec<LoggedEvent>> {
        let mut records = Vec::new();
        let mut after = 0;
        loop {
            let page = self
                .recovery_page(from_generation, after, PageLimit::DEFAULT)
                .await?;
            records.extend(page.items);
            match page.next {
                Some(Cursor(next)) => after = next,
                None => return Ok(records),
            }
        }
    }
    async fn last_recovered_generation(&self) -> Result<u64> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT COALESCE(MAX(generation),0) FROM runtime_generations WHERE recovered_ms IS NOT NULL",
                )?
                .query_row([], |row| row.get(0))?)
        })
        .await
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
    (
        "003_recovery_kinds.sql",
        include_str!("migrations/003_recovery_kinds.sql"),
    ),
    (
        "004_session_summary.sql",
        include_str!("migrations/004_session_summary.sql"),
    ),
];
fn migrate(conn: &mut Connection) -> SqlResult<()> {
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

fn writer(mut conn: Connection, mut receiver: mpsc::Receiver<Command>, acknowledged: &AtomicU64) {
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
                append_group(&mut conn, all, replies, acknowledged);
            }
            Command::Allocate(owner, pid, started, reply) => {
                let _ = reply.send(allocate(&mut conn, &owner, pid, started).map_err(Into::into));
            }
            Command::MarkRecovered(generation, at, reply) => {
                let result = (|| -> SqlResult<()> {
                    let changed = conn.execute(
                        "UPDATE runtime_generations SET recovered_ms=?1 WHERE generation=?2",
                        params![at, sql_seq(generation)?],
                    )?;
                    if changed == 0 {
                        return Err(
                            EventLogError::Invalid("unknown Runtime generation".into()).into()
                        );
                    }
                    Ok(())
                })();
                let _ = reply.send(result.map_err(Into::into));
            }
            Command::Finish(generation, stopped, reply) => {
                let result = (|| -> SqlResult<()> {
                    let changed = conn.execute(
                        "UPDATE runtime_generations SET stopped_ms=?1 WHERE generation=?2",
                        params![stopped, sql_seq(generation)?],
                    )?;
                    if changed == 0 {
                        return Err(
                            EventLogError::Invalid("unknown Runtime generation".into()).into()
                        );
                    }
                    Ok(())
                })();
                let _ = reply.send(result.map_err(Into::into));
            }
        }
    }
}

fn append_group(
    conn: &mut Connection,
    events: Vec<Event>,
    requests: Vec<(usize, Reply<Vec<LoggedEvent>>)>,
    acknowledged: &AtomicU64,
) {
    match append(conn, events).map_err(EventLogError::from) {
        Ok(records) => {
            // Before any reply: a read that starts after a reply must see it.
            if let Some(last) = records.last() {
                acknowledged.fetch_max(last.global_seq, Ordering::Release);
            }
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

/// Last Update sequence of one Session. `session_seq IS NOT NULL` changes no
/// result (`MAX` skips NULL), but it lets the planner use the partial index
/// `events_session_seq`. Without it each call scans the whole table, once per
/// appended Update.
const LATEST_SEQ_SQL: &str =
    "SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1 AND session_seq IS NOT NULL";

fn latest(conn: &Connection, session: &SessionKey) -> SqlResult<u64> {
    Ok(conn
        .prepare_cached(LATEST_SEQ_SQL)?
        .query_row([session.as_str()], |row| row.get(0))?)
}

const INSERT_SQL: &str = "INSERT INTO events (global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)";

fn append(conn: &mut Connection, events: Vec<Event>) -> SqlResult<Vec<LoggedEvent>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut global: u64 = tx
        .prepare_cached(MAX_GLOBAL_SQL)?
        .query_row([], |r| r.get(0))?;
    let mut records = Vec::with_capacity(events.len());
    {
        // Session sequences of this transaction: one index read per Session.
        let mut seqs: HashMap<SessionKey, u64> = HashMap::new();
        let mut insert = tx.prepare_cached(INSERT_SQL)?;
        for event in events {
            global = global
                .checked_add(1)
                .ok_or_else(|| EventLogError::Storage("global sequence exhausted".into()))?;
            let seq = if matches!(event.kind, EventKind::Update(_)) {
                let key = event
                    .session_key
                    .as_ref()
                    .ok_or_else(|| EventLogError::Invalid("Update without a Session".into()))?;
                let next = match seqs.get_mut(key) {
                    Some(seq) => {
                        *seq += 1;
                        *seq
                    }
                    None => {
                        let seq = latest(&tx, key)? + 1;
                        seqs.insert(key.clone(), seq);
                        seq
                    }
                };
                Some(next)
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
            insert.execute(params![
                sql_seq(global)?,
                sql_seq(record.event.generation)?,
                record.event.session_key.as_ref().map(SessionKey::as_str),
                seq.map(sql_seq).transpose()?,
                kind,
                record.event.turn.as_ref().map(TurnId::as_str),
                record.event.ts_ms,
                payload
            ])?;
            records.push(record);
        }
    }
    project(&tx, &records)?;
    tx.commit()?;
    Ok(records)
}

// ---------------------------------------------------------------- projection

const PROJECTION: &str = "session_summary";
const MARK_SQL: &str = "SELECT applied_global_seq FROM projection_state WHERE name=?1";
const SET_MARK_SQL: &str = "UPDATE projection_state SET applied_global_seq=?2 WHERE name=?1";
const SUMMARY_SQL: &str = "SELECT session_key,last_seq,last_revision,last_activity_ms,prompts_sent,turn_count,parent FROM session_summary WHERE session_key=?1";
const PROMPTS_SQL: &str = "SELECT prompts_sent FROM session_summary WHERE session_key=?1";
const TURNS_SQL: &str =
    "SELECT turn FROM session_turns WHERE session_key=?1 ORDER BY first_global_seq";
const SUBAGENTS_SQL: &str = "SELECT session_key FROM session_summary WHERE parent IS NOT NULL";
const ACTIVITY_SQL: &str = "SELECT session_key,last_activity_ms FROM session_summary";
const INSERT_TURN_SQL: &str =
    "INSERT OR IGNORE INTO session_turns (session_key,turn,first_global_seq) VALUES (?1,?2,?3)";
/// Merge one commit group's fold of a Session into its row. The rules equal
/// `SessionSummary::apply` in log order: sequences and activity take the
/// maximum, the revision of the group's last revision record wins, counts
/// add, and the first parent stays.
const UPSERT_SUMMARY_SQL: &str = "INSERT INTO session_summary \
    (session_key,last_seq,last_revision,last_activity_ms,prompts_sent,turn_count,parent) \
    VALUES (?1,?2,?3,?4,?5,?6,?7) \
    ON CONFLICT(session_key) DO UPDATE SET \
    last_seq=MAX(last_seq,excluded.last_seq), \
    last_revision=CASE WHEN ?8 THEN excluded.last_revision ELSE last_revision END, \
    last_activity_ms=MAX(last_activity_ms,excluded.last_activity_ms), \
    prompts_sent=prompts_sent+excluded.prompts_sent, \
    turn_count=turn_count+excluded.turn_count, \
    parent=COALESCE(parent,excluded.parent)";

fn projection_mark(conn: &Connection) -> SqlResult<u64> {
    Ok(conn
        .prepare_cached(MARK_SQL)?
        .query_row([PROJECTION], |row| row.get(0))?)
}

fn set_projection_mark(conn: &Connection, global_seq: u64) -> SqlResult<()> {
    conn.prepare_cached(SET_MARK_SQL)?
        .execute(params![PROJECTION, sql_seq(global_seq)?])?;
    Ok(())
}

/// Apply committed records to the projection, in the caller's transaction.
/// Records at or below the mark are skipped (idempotent). If the mark is
/// behind the first record (an older writer appended without the
/// projection), the missing events are applied first.
fn project(conn: &Connection, records: &[LoggedEvent]) -> SqlResult<()> {
    let (Some(first), Some(last)) = (records.first(), records.last()) else {
        return Ok(());
    };
    let mut mark = projection_mark(conn)?;
    if mark + 1 < first.global_seq {
        catch_up(conn, mark, first.global_seq - 1)?;
        mark = first.global_seq - 1;
    }
    apply(conn, records.iter().filter(|r| r.global_seq > mark))?;
    set_projection_mark(conn, last.global_seq.max(mark))
}

/// Apply stored events `(after, through]` in chunks.
fn catch_up(conn: &Connection, mut after: u64, through: u64) -> SqlResult<()> {
    while after < through {
        let page = global_page(
            conn,
            after,
            through,
            PageLimit::events(BACKFILL_CHUNK as u32),
        )?;
        let Some(last) = page.items.last().map(|r| r.global_seq) else {
            break;
        };
        apply(conn, page.items.iter())?;
        after = last;
    }
    Ok(())
}

/// One commit group's change to one Session.
struct GroupFold {
    summary: SessionSummary,
    revised: bool,
    turns: Vec<(TurnId, u64)>,
}

/// Fold the records per Session, then write one upsert per Session.
fn apply<'a>(conn: &Connection, records: impl Iterator<Item = &'a LoggedEvent>) -> SqlResult<()> {
    let mut groups: BTreeMap<SessionKey, GroupFold> = BTreeMap::new();
    for record in records {
        let Some(delta) = SummaryDelta::of(record) else {
            continue;
        };
        let group = groups
            .entry(delta.session.clone())
            .or_insert_with(|| GroupFold {
                summary: SessionSummary::empty(delta.session.clone()),
                revised: false,
                turns: Vec::new(),
            });
        group.revised |= delta.revision.is_some();
        if let Some(turn) = &delta.turn {
            if !group.turns.iter().any(|(seen, _)| seen == turn) {
                group.turns.push((turn.clone(), record.global_seq));
            }
        }
        // Turns are counted against the table below, not in the group.
        group.summary.apply(&delta, false);
    }
    let mut insert_turn = conn.prepare_cached(INSERT_TURN_SQL)?;
    let mut upsert = conn.prepare_cached(UPSERT_SUMMARY_SQL)?;
    for (session, group) in groups {
        let mut new_turns: u64 = 0;
        for (turn, global) in &group.turns {
            new_turns +=
                insert_turn.execute(params![session.as_str(), turn.as_str(), sql_seq(*global)?])?
                    as u64;
        }
        let summary = group.summary;
        upsert.execute(params![
            session.as_str(),
            sql_seq(summary.last_seq)?,
            sql_seq(summary.last_revision)?,
            summary.last_activity_ms,
            sql_seq(summary.prompts_sent)?,
            sql_seq(new_turns)?,
            summary.parent.as_ref().map(SessionKey::as_str),
            group.revised,
        ])?;
    }
    Ok(())
}

/// Build or catch up the projection from the events, one transaction per
/// chunk; each moves the mark with its rows, so an interrupted backfill
/// continues where it stopped. Returns whether it reached the end; with
/// `max_chunks` it stops early (tests use it to interrupt).
fn backfill(conn: &mut Connection, chunk: usize, max_chunks: Option<usize>) -> SqlResult<bool> {
    let mut chunks = 0;
    loop {
        if max_chunks.is_some_and(|max| chunks >= max) {
            return Ok(false);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mark = projection_mark(&tx)?;
        let through: u64 = tx.query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))?;
        if mark >= through {
            tx.commit()?;
            return Ok(true);
        }
        let limit = PageLimit::events(u32::try_from(chunk).unwrap_or(u32::MAX));
        let page = global_page(&tx, mark, through, limit)?;
        let last = page.items.last().map_or(through, |r| r.global_seq);
        apply(&tx, page.items.iter())?;
        set_projection_mark(&tx, last)?;
        tx.commit()?;
        chunks += 1;
    }
}

fn summary(conn: &Connection, session: &SessionKey) -> SqlResult<Option<SessionSummary>> {
    Ok(conn
        .prepare_cached(SUMMARY_SQL)?
        .query_row([session.as_str()], summary_row)
        .optional()?)
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        session: SessionKey::from(row.get::<_, String>(0)?),
        last_seq: row.get(1)?,
        last_revision: row.get(2)?,
        last_activity_ms: row.get(3)?,
        prompts_sent: row.get(4)?,
        turn_count: row.get(5)?,
        parent: row.get::<_, Option<String>>(6)?.map(SessionKey::from),
    })
}

fn activity(conn: &Connection) -> SqlResult<HashMap<SessionKey, i64>> {
    let mut stmt = conn.prepare_cached(ACTIVITY_SQL)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            SessionKey::from(row.get::<_, String>(0)?),
            row.get::<_, i64>(1)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn subagents(conn: &Connection) -> SqlResult<HashSet<SessionKey>> {
    let mut statement = conn.prepare_cached(SUBAGENTS_SQL)?;
    let keys = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|key| key.map(SessionKey::from))
        .collect::<rusqlite::Result<_>>()?;
    Ok(keys)
}

fn turn_ids(conn: &Connection, session: &SessionKey) -> SqlResult<Vec<TurnId>> {
    let mut statement = conn.prepare_cached(TURNS_SQL)?;
    let turns = statement
        .query_map([session.as_str()], |row| row.get::<_, String>(0))?
        .map(|turn| turn.map(TurnId))
        .collect::<rusqlite::Result<_>>()?;
    Ok(turns)
}

// ---------------------------------------------------------------- pages

const SESSION_PAGE_SQL: &str = "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload FROM events WHERE session_key=?1 AND session_seq>?2 AND session_seq<=?3 ORDER BY session_seq LIMIT ?4";
const GLOBAL_PAGE_SQL: &str = "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload FROM events WHERE global_seq>?1 AND global_seq<=?2 ORDER BY global_seq LIMIT ?3";

fn session_page(
    conn: &Connection,
    session: &SessionKey,
    after: u64,
    through: u64,
    limit: PageLimit,
) -> SqlResult<Page<LoggedEvent>> {
    // All stored sequences fit i64; a larger cursor is simply past the end.
    if after >= through || after > i64::MAX as u64 {
        return Ok(Page::last(Vec::new()));
    }
    page(
        conn,
        SESSION_PAGE_SQL,
        params![
            session.as_str(),
            after as i64,
            bound(through),
            page_rows_limit(limit)
        ],
        limit,
        |r| r.session_seq.unwrap_or(0),
    )
}

fn global_page(
    conn: &Connection,
    after: u64,
    through: u64,
    limit: PageLimit,
) -> SqlResult<Page<LoggedEvent>> {
    if after >= through || after > i64::MAX as u64 {
        return Ok(Page::last(Vec::new()));
    }
    page(
        conn,
        GLOBAL_PAGE_SQL,
        params![after as i64, bound(through), page_rows_limit(limit)],
        limit,
        |r| r.global_seq,
    )
}

/// The first generation at or after `from` and its first `global_seq`, then
/// the next generation, and so on: a few index reads per generation. The
/// smallest is where recovery records can start.
fn recovery_floor(conn: &Connection, from: i64) -> SqlResult<Option<i64>> {
    let mut floor: Option<i64> = None;
    let mut generation: Option<i64> = conn
        .prepare_cached("SELECT MIN(generation) FROM events WHERE generation>=?1")?
        .query_row([from], |row| row.get(0))?;
    while let Some(g) = generation {
        let first: Option<i64> = conn
            .prepare_cached("SELECT MIN(global_seq) FROM events WHERE generation=?1")?
            .query_row([g], |row| row.get(0))?;
        if let Some(first) = first {
            floor = Some(floor.map_or(first, |f: i64| f.min(first)));
        }
        generation = conn
            .prepare_cached("SELECT MIN(generation) FROM events WHERE generation>?1")?
            .query_row([g], |row| row.get(0))?;
    }
    Ok(floor)
}

/// Turn boundaries, prompts sent and settled, clean stops and Escalation
/// records, filtered in SQL so recovery never loads streamed text. Recovery
/// needs only that a prompt was sent, so its content is not loaded either.
/// The read walks the primary key from the page's cursor (`+generation`
/// keeps the planner off the generation index, whose order is not
/// `global_seq` order), so a page stops after its limit.
const RECOVERY_PAGE_SQL: &str =
    "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,\
     CASE WHEN kind='prompt_sent' \
     THEN '{\"v\":1,\"event\":{\"kind\":\"prompt_sent\",\"payload\":{}}}' \
     ELSE payload END \
     FROM events WHERE global_seq>?2 AND +generation>=?1 AND (\
     kind IN ('prompt_sent','prompt_settled','runtime_stopped',\
     'escalation_raised','escalation_answered','escalation_withdrawn') \
     OR (kind='update' AND json_extract(payload,'$.event.payload.update.type') \
         IN ('turn_started','turn_ended'))) ORDER BY global_seq LIMIT ?3";

fn recovery_page(
    conn: &Connection,
    from_generation: u64,
    after_global: u64,
    limit: PageLimit,
) -> SqlResult<Page<LoggedEvent>> {
    if from_generation > i64::MAX as u64 || after_global > i64::MAX as u64 {
        return Ok(Page::last(Vec::new()));
    }
    let Some(floor) = recovery_floor(conn, from_generation as i64)? else {
        return Ok(Page::last(Vec::new()));
    };
    let after = (after_global as i64).max(floor - 1);
    page(
        conn,
        RECOVERY_PAGE_SQL,
        params![from_generation as i64, after, page_rows_limit(limit)],
        limit,
        |r| r.global_seq,
    )
}

/// Read one page: at most `limit` records and, past the first, at most
/// `limit.bytes` of stored payload. The query asks for one row more, so
/// `next` is set only when another record exists.
fn page(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
    limit: PageLimit,
    position: impl Fn(&LoggedEvent) -> u64,
) -> SqlResult<Page<LoggedEvent>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(args)?;
    let mut items = Vec::new();
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        let payload: String = row.get(7)?;
        let full = items.len() >= limit.max_events()
            || (!items.is_empty() && bytes.saturating_add(payload.len()) > limit.bytes);
        if full {
            let next = items.last().map(|r| Cursor(position(r)));
            return Ok(Page { items, next });
        }
        bytes = bytes.saturating_add(payload.len());
        items.push(decode(row, &payload)?);
    }
    Ok(Page::last(items))
}

fn decode(row: &rusqlite::Row<'_>, payload: &str) -> SqlResult<LoggedEvent> {
    let payload: Value = serde_json::from_str(payload)?;
    let raw = payload
        .get("event")
        .ok_or_else(|| EventLogError::Storage("missing versioned event payload".into()))?
        .clone();
    let kind = if payload.get("v") == Some(&Value::from(1)) {
        serde_json::from_value(raw)?
    } else {
        EventKind::Unknown(raw)
    };
    Ok(LoggedEvent {
        global_seq: row.get(0)?,
        session_seq: row.get(3)?,
        event: Event {
            generation: row.get(1)?,
            session_key: row.get::<_, Option<String>>(2)?.map(Into::into),
            turn: row.get::<_, Option<String>>(5)?.map(Into::into),
            ts_ms: row.get(6)?,
            kind,
        },
    })
}

fn allocate(conn: &mut Connection, owner: &str, pid: u32, started: i64) -> SqlResult<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use loop_store::summarize;
    use loop_store_testkit::event_log as kit;

    /// Rows of `EXPLAIN QUERY PLAN`, with every parameter left NULL.
    fn plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut statement = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare plan");
        let mut rows = statement.raw_query();
        let mut plan = Vec::new();
        while let Some(row) = rows.next().expect("plan row") {
            plan.push(row.get::<_, String>(3).expect("plan detail"));
        }
        plan
    }

    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn).expect("migrate");
        conn
    }

    /// The per-append sequence query must read the partial index, not scan.
    /// The old text (no `IS NOT NULL`) shows why: the planner cannot prove
    /// the index predicate, so it scans `events`.
    #[test]
    fn latest_seq_uses_the_session_seq_index() {
        let conn = migrated();
        let old = "SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1";
        let before = plan(&conn, old);
        let after = plan(&conn, LATEST_SEQ_SQL);
        println!("old plan: {before:?}");
        println!("new plan: {after:?}");
        assert!(
            !before.iter().any(|row| row.contains("events_session_seq")),
            "old query should not use the partial index: {before:?}"
        );
        assert!(
            after.iter().any(|row| row.contains("events_session_seq")),
            "latest() must use events_session_seq: {after:?}"
        );
    }

    /// Time of one `latest()` query, old and new text, on 51,200 rows
    /// (200 Sessions x 256 Updates). Ignored: it only prints numbers.
    #[test]
    #[ignore]
    fn latest_seq_cost() {
        let conn = migrated();
        let tx = conn.unchecked_transaction().expect("tx");
        for global in 1..=51_200_i64 {
            let session = format!("s{}", global % 200);
            tx.execute(
                "INSERT INTO events (global_seq,generation,session_key,session_seq,kind,ts_ms,payload) VALUES (?1,1,?2,?3,'update',0,'{}')",
                params![global, session, global / 200 + 1],
            )
            .expect("insert");
        }
        tx.commit().expect("commit");
        let old = "SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1";
        for (name, sql) in [("old", old), ("new", LATEST_SEQ_SQL)] {
            let start = std::time::Instant::now();
            let calls = 200;
            for i in 0..calls {
                let _: u64 = conn
                    .query_row(sql, [format!("s{i}")], |row| row.get(0))
                    .expect("query");
            }
            let per_call = start.elapsed().as_secs_f64() * 1000.0 / f64::from(calls);
            println!("{name}: {per_call:.4} ms per call");
        }
    }

    /// Each query on a hot or per-Session path reads an index or a primary
    /// key; none scans `events`. The old full-scan queries are shown next to
    /// them for the record.
    #[test]
    fn hot_and_per_session_queries_use_indexes() {
        let conn = migrated();
        let cases: &[(&str, &str, &str)] = &[
            (
                "session page",
                SESSION_PAGE_SQL,
                "USING INDEX events_session_seq",
            ),
            ("global page", GLOBAL_PAGE_SQL, "USING INTEGER PRIMARY KEY"),
            (
                "recovery page",
                RECOVERY_PAGE_SQL,
                "USING INTEGER PRIMARY KEY",
            ),
            (
                "recovery floor: next generation",
                "SELECT MIN(generation) FROM events WHERE generation>?1",
                "USING COVERING INDEX events_generation",
            ),
            (
                "recovery floor: first record",
                "SELECT MIN(global_seq) FROM events WHERE generation=?1",
                "USING COVERING INDEX events_generation",
            ),
            ("max global_seq", MAX_GLOBAL_SQL, "SEARCH events"),
            (
                "summary",
                SUMMARY_SQL,
                "SEARCH session_summary USING PRIMARY KEY",
            ),
            (
                "prompts sent",
                PROMPTS_SQL,
                "SEARCH session_summary USING PRIMARY KEY",
            ),
            (
                "turn ids",
                TURNS_SQL,
                "SEARCH session_turns USING PRIMARY KEY",
            ),
            (
                // The partial index holds only Subagents: O(Subagents).
                "subagents",
                SUBAGENTS_SQL,
                "USING INDEX session_summary_subagents",
            ),
            (
                "projection mark",
                MARK_SQL,
                "SEARCH projection_state USING PRIMARY KEY",
            ),
        ];
        for (name, sql, expected) in cases {
            let rows = plan(&conn, sql);
            println!("{name}: {rows:?}");
            assert!(
                rows.iter().any(|row| row.contains(expected)),
                "{name} must use `{expected}`: {rows:?}"
            );
            assert!(
                !rows.iter().any(|row| row.starts_with("SCAN events")),
                "{name} scans events: {rows:?}"
            );
        }
        // Once at Runtime start: one row per Session, never the events.
        let rows = plan(&conn, ACTIVITY_SQL);
        println!("last activity: {rows:?}");
        assert!(
            rows.iter().all(|row| row.contains("session_summary")),
            "{rows:?}"
        );
        for (name, old) in [
            ("old last activity", "SELECT session_key, MAX(ts_ms) FROM events WHERE session_key IS NOT NULL GROUP BY session_key"),
            ("old subagents", "SELECT DISTINCT session_key FROM events WHERE kind='session_opened' AND session_key IS NOT NULL AND json_extract(payload,'$.event.payload.parent') IS NOT NULL"),
            ("old turn ids", "SELECT DISTINCT turn FROM events WHERE session_key=?1 AND turn IS NOT NULL"),
            ("old prompts sent", "SELECT COUNT(*) FROM events WHERE session_key=?1 AND kind='prompt_sent'"),
        ] {
            println!("{name}: {:?}", plan(&conn, old));
        }
    }

    /// Insert events the way the code before step B2 did: no projection.
    fn insert_old(conn: &Connection, events: Vec<Event>) {
        let mut global: u64 = conn
            .query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))
            .expect("max");
        for event in events {
            global += 1;
            let seq = match (&event.kind, &event.session_key) {
                (EventKind::Update(_), Some(key)) => Some(latest(conn, key).expect("latest") + 1),
                _ => None,
            };
            let record = stamp(event, global, seq);
            let raw = serde_json::to_value(&record.event.kind).expect("json");
            let kind = raw["kind"].as_str().expect("kind").to_owned();
            let payload = serde_json::json!({"v":1,"event":raw}).to_string();
            conn.execute(
                INSERT_SQL,
                params![
                    global as i64,
                    record.event.generation as i64,
                    record.event.session_key.as_ref().map(SessionKey::as_str),
                    seq.map(|s| s as i64),
                    kind,
                    record.event.turn.as_ref().map(TurnId::as_str),
                    record.event.ts_ms,
                    payload
                ],
            )
            .expect("insert");
        }
    }

    /// A database with the schema and rows of the code before step B2:
    /// migrations 001 to 003 only, and no projection tables.
    fn old_database(path: &Path) {
        let mut conn = Connection::open(path).expect("open");
        conn.execute_batch("PRAGMA journal_mode=WAL;").expect("wal");
        conn.execute_batch(
            "CREATE TABLE migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )
        .expect("migrations");
        for (id, sql) in &MIGRATIONS[..3] {
            let tx = conn.transaction().expect("tx");
            tx.execute_batch(sql).expect("old migration");
            tx.execute(
                "INSERT INTO migrations (id, applied_at) VALUES (?1, 0)",
                [id],
            )
            .expect("record");
            tx.commit().expect("commit");
        }
        for _ in 0..40 {
            for batch in kit::workload() {
                insert_old(&conn, batch);
            }
        }
    }

    /// The projection rows, in key order.
    fn projection_rows(conn: &Connection) -> (Vec<SessionSummary>, Vec<(String, String, i64)>) {
        let summaries = conn
            .prepare("SELECT session_key,last_seq,last_revision,last_activity_ms,prompts_sent,turn_count,parent FROM session_summary ORDER BY session_key")
            .expect("prepare")
            .query_map([], summary_row)
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        let turns = conn
            .prepare("SELECT session_key,turn,first_global_seq FROM session_turns ORDER BY session_key,turn")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        (summaries, turns)
    }

    /// Delete the projection and build it again from the events.
    fn rebuild(conn: &mut Connection) {
        conn.execute_batch(
            "DELETE FROM session_summary; DELETE FROM session_turns; \
             UPDATE projection_state SET applied_global_seq=0;",
        )
        .expect("clear");
        assert!(backfill(conn, 7, None).expect("rebuild"));
    }

    async fn assert_matches_fold(log: &SqliteEventLog) {
        let records = log.read_runtime_after(0).await.expect("read");
        let expected = summarize(&records);
        assert!(!expected.is_empty());
        for (session, (summary, turns)) in &expected {
            assert_eq!(
                log.session_summary(session)
                    .await
                    .expect("summary")
                    .as_ref(),
                Some(summary)
            );
            assert_eq!(&log.turn_ids(session).await.expect("turns"), turns);
        }
    }

    /// Migration 004 on a database of the old code: the backfill is cut
    /// after two chunks (as by a crash), the next open finishes it, and the
    /// result equals a fold of the events and a rebuild.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migration_backfills_an_old_database_and_resumes_after_an_interrupt() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        old_database(&path);
        {
            let mut conn = Connection::open(&path).expect("open");
            migrate(&mut conn).expect("migrate");
            assert!(!backfill(&mut conn, 25, Some(2)).expect("partial backfill"));
            assert_eq!(
                projection_mark(&conn).expect("mark"),
                50,
                "two chunks applied"
            );
            let max: u64 = conn
                .query_row(MAX_GLOBAL_SQL, [], |r| r.get(0))
                .expect("max");
            assert!(max > 50);
        }
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        assert_matches_fold(&log).await;
        // New appends after the backfill continue the projection.
        for batch in kit::workload() {
            log.append_batch(batch).await.expect("append");
        }
        assert_matches_fold(&log).await;
        drop(log);
        let mut conn = Connection::open(&path).expect("open");
        let live = projection_rows(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows(&conn), live, "projection equals a rebuild");
    }

    /// An older binary can append after the migration (a downgrade): the
    /// mark falls behind. The next append catches up in its transaction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_append_catches_up_events_written_without_the_projection() {
        let dir = tempfile::tempdir().expect("dir");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        log.append_batch(kit::workload().remove(0))
            .await
            .expect("append");
        {
            let conn = Connection::open(dir.path().join("events.db")).expect("open");
            insert_old(&conn, kit::workload().remove(1));
        }
        log.append_batch(kit::workload().remove(2))
            .await
            .expect("append");
        assert_matches_fold(&log).await;
        let mut conn = Connection::open(dir.path().join("events.db")).expect("open");
        let live = projection_rows(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows(&conn), live);
    }

    /// Applying the same records twice changes nothing: the mark skips them.
    #[test]
    fn projection_apply_is_idempotent() {
        let mut conn = migrated();
        for batch in kit::workload() {
            append(&mut conn, batch).expect("append");
        }
        let before = projection_rows(&conn);
        let records = {
            let tx = conn.transaction().expect("tx");
            let page = global_page(&tx, 0, u64::MAX, PageLimit::events(10_000)).expect("read");
            tx.commit().expect("commit");
            page.items
        };
        let tx = conn.transaction().expect("tx");
        project(&tx, &records).expect("project again");
        tx.commit().expect("commit");
        assert_eq!(projection_rows(&conn), before);
    }

    /// A pooled connection left inside a read transaction would keep an old
    /// snapshot. The pool drops it, and the read sees the append.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_never_reads_an_old_snapshot() {
        let dir = tempfile::tempdir().expect("dir");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        log.append_batch(vec![kit::update("s")])
            .await
            .expect("append");
        let leaked = log.readers.open_connection().expect("reader");
        leaked.execute_batch("BEGIN").expect("begin");
        let seen: u64 = leaked
            .query_row(MAX_GLOBAL_SQL, [], |r| r.get(0))
            .expect("snapshot");
        assert_eq!(seen, 1);
        log.readers.idle.lock().expect("pool").push(leaked);
        log.append_batch(vec![kit::update("s")])
            .await
            .expect("append");
        assert_eq!(log.latest_seq(&"s".into()).await.expect("latest"), 2);
        assert!(log
            .readers
            .idle
            .lock()
            .expect("pool")
            .iter()
            .all(Connection::is_autocommit));
        // The check itself: a snapshot below the acknowledged floor fails.
        let conn = log.readers.open_connection().expect("reader");
        let stale = snapshot(&conn, 99, &|_| Ok(()));
        assert!(
            matches!(stale, Err(SqlError::Stale { seen: 2, floor: 99 })),
            "{stale:?}"
        );
        assert!(conn.is_autocommit());
        let error = log.readers.run(99, &|_| Ok(())).expect_err("stale twice");
        assert!(EventLogError::from(error)
            .to_string()
            .contains("older than"));
    }

    /// A cancelled read keeps its permit until its blocking work ends, then
    /// returns it; the pool stays bounded and usable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_reads_return_their_permits() {
        let dir = tempfile::tempdir().expect("dir");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        for _ in 0..8 {
            log.append_batch(vec![kit::chunk("big", 4096); MAX_BATCH_EVENTS])
                .await
                .expect("append");
        }
        let big = SessionKey::from("big");
        for _ in 0..16 {
            let read = log.read_after(&big, 0);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(1), read).await;
        }
        assert!(log.readers.permits.available_permits() <= READER_POOL_SIZE);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while log.readers.permits.available_permits() < READER_POOL_SIZE {
            assert!(std::time::Instant::now() < deadline, "permits not returned");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(log.readers.idle.lock().expect("pool").len() <= READER_POOL_SIZE);
        assert_eq!(
            log.read_after(&"big".into(), 0).await.expect("read").len(),
            2048
        );
    }

    /// A database made by the code before step B2 (for example by the load
    /// tool at the parent commit). Set `LOOP_B2_OLD_EVENTS_DB` to its path;
    /// the test copies it, opens the copy and checks the backfill.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs LOOP_B2_OLD_EVENTS_DB"]
    async fn backfills_a_database_from_the_parent_commit() {
        let Ok(source) = std::env::var("LOOP_B2_OLD_EVENTS_DB") else {
            panic!("set LOOP_B2_OLD_EVENTS_DB");
        };
        let dir = tempfile::tempdir().expect("dir");
        std::fs::copy(&source, dir.path().join("events.db")).expect("copy");
        {
            let conn = Connection::open(dir.path().join("events.db")).expect("open");
            let tables: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='session_summary'",
                    [],
                    |r| r.get(0),
                )
                .expect("tables");
            assert_eq!(tables, 0, "the source must be a database of the old code");
        }
        let start = std::time::Instant::now();
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        println!("open with backfill: {:?}", start.elapsed());
        let records = log.read_runtime_after(0).await.expect("read");
        println!("events: {}", records.len());
        assert_matches_fold(&log).await;
        drop(log);
        let mut conn = Connection::open(dir.path().join("events.db")).expect("open");
        let live = projection_rows(&conn);
        println!("sessions: {}", live.0.len());
        rebuild(&mut conn);
        assert_eq!(projection_rows(&conn), live);
    }
}
