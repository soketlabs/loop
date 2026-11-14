use super::*;
use async_trait::async_trait;
use loop_protocol::{SessionKey, TurnId};
use loop_store::{
    Cursor, Identity, Page, PageLimit, Principal, PrincipalKind, Scope, SessionSummary,
    StoreCapabilities, SummaryDelta, SystemScope, Tenant, TenantSession, Via,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, Semaphore};

type Reply<T> = oneshot::Sender<Result<T>>;

/// Read-only connections, and so concurrent blocking reads, per log.
pub const READER_POOL_SIZE: usize = 4;
/// Events applied per transaction when the summary projection is built from
/// an older database, or caught up.
const BACKFILL_CHUNK: usize = 2048;

/// The backfill chunk of `open`. Tests may set `LOOP_B2_BACKFILL_CHUNK`.
fn backfill_chunk() -> usize {
    #[cfg(test)]
    if let Some(chunk) = std::env::var("LOOP_B2_BACKFILL_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return chunk;
    }
    BACKFILL_CHUNK
}

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
    /// Resolves after every request queued before it has committed or failed.
    Sync(Reply<()>),
    /// Test hook: the writer waits until the sender is dropped or sends.
    #[cfg(test)]
    Pause(std::sync::mpsc::Receiver<()>),
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
/// **WAL size.** Readers that overlap without a gap keep the automatic
/// PASSIVE checkpoint from ever resetting the WAL, so it would grow without
/// bound. Every 64 commits the writer reads the WAL size. Above 64 MiB it
/// waits up to 50 ms for all reader permits (a reader holds one for one
/// page; readers that come later queue behind the writer). With all of them
/// held, no reader is in a transaction, and a TRUNCATE checkpoint resets
/// the WAL at once. If the wait times out, it runs a PASSIVE checkpoint and
/// tries again at the next check; it never waits on a busy reader inside
/// SQLite. `journal_size_limit` is 64 MiB as well.
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
                "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; \
                 PRAGMA journal_size_limit=67108864;",
            )?;
            migrate(&mut conn)?;
            backfill(&mut conn, backfill_chunk(), None)?;
            let last = conn.query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))?;
            Ok((conn, last))
        })
        .await
        .map_err(|e| EventLogError::Storage(e.to_string()))??;
        let acknowledged = Arc::new(AtomicU64::new(last));
        let (sender, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let ack = Arc::clone(&acknowledged);
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let permits = Arc::new(Semaphore::new(READER_POOL_SIZE));
        let writer_permits = Arc::clone(&permits);
        let runtime = Handle::current();
        tokio::task::spawn_blocking(move || {
            writer(conn, receiver, &ack, &wal, &writer_permits, &runtime)
        });
        Ok(Self {
            sender,
            readers: Arc::new(Readers {
                path,
                idle: Mutex::new(Vec::with_capacity(READER_POOL_SIZE)),
                permits,
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
            readers.run(floor, &read).map_err(|error| {
                if let SqlError::Stale { seen, floor } = error {
                    // A new read transaction in WAL mode sees every commit, so
                    // this is a broken invariant, not a condition to handle.
                    debug_assert!(false, "stale reader snapshot {seen} < {floor}");
                    return EventLogError::Storage(format!(
                        "invariant violated: a new reader snapshot at global_seq {seen} \
                         misses the acknowledged {floor}"
                    ));
                }
                EventLogError::from(error)
            })
        })
        .await
        .map_err(|e| EventLogError::Storage(e.to_string()))?
    }

    /// Pages of one Session, `(after_seq, through]`, with `through` pinned by
    /// the first page, so a fast writer cannot keep the loop going.
    async fn read_session_all(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
    ) -> Result<Vec<LoggedEvent>> {
        let (first_tenant, key) = (tenant.clone(), session.clone());
        let (through, first) = self
            .read(move |conn| {
                let through = latest(conn, &first_tenant, &key)?;
                let page = session_page(
                    conn,
                    &first_tenant,
                    &key,
                    after_seq,
                    through,
                    PageLimit::DEFAULT,
                )?;
                Ok((through, page))
            })
            .await?;
        let mut records = first.items;
        let mut next = first.next;
        while let Some(Cursor(after)) = next {
            let (tenant, key) = (tenant.clone(), session.clone());
            let page = self
                .read(move |conn| {
                    session_page(conn, &tenant, &key, after, through, PageLimit::DEFAULT)
                })
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
    async fn read_after(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
    ) -> Result<Vec<LoggedEvent>> {
        self.read_session_all(tenant, session, after_seq).await
    }
    async fn read_runtime_after(
        &self,
        scope: Scope<'_>,
        global_seq: u64,
    ) -> Result<Vec<LoggedEvent>> {
        let only = scope_tenant(scope);
        let first_only = only.clone();
        let (through, first) = self
            .read(move |conn| {
                let through: u64 = conn.query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))?;
                let page = global_page(
                    conn,
                    first_only.as_deref(),
                    global_seq,
                    through,
                    PageLimit::DEFAULT,
                )?;
                Ok((through, page))
            })
            .await?;
        let mut records = first.items;
        let mut next = first.next;
        while let Some(Cursor(after)) = next {
            let only = only.clone();
            let page = self
                .read(move |conn| {
                    global_page(conn, only.as_deref(), after, through, PageLimit::DEFAULT)
                })
                .await?;
            records.extend(page.items);
            next = page.next;
        }
        Ok(records)
    }
    async fn latest_seq(&self, tenant: &Tenant, session: &SessionKey) -> Result<u64> {
        let (tenant, key) = (tenant.clone(), session.clone());
        self.read(move |conn| latest(conn, &tenant, &key)).await
    }
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            paged_reads: true,
            reader_pool: true,
            tenant_scoped: true,
            ..StoreCapabilities::EVENT_LOG_V1
        }
    }
    async fn read_session_page(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
        after_seq: u64,
        through_seq: Option<u64>,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        let (tenant, key) = (tenant.clone(), session.clone());
        let through = through_seq.unwrap_or(u64::MAX);
        self.read(move |conn| session_page(conn, &tenant, &key, after_seq, through, limit))
            .await
    }
    async fn session_summary(
        &self,
        tenant: &Tenant,
        session: &SessionKey,
    ) -> Result<Option<SessionSummary>> {
        let (tenant, key) = (tenant.clone(), session.clone());
        self.read(move |conn| summary(conn, &tenant, &key)).await
    }
    async fn recovery_page(
        &self,
        scope: Scope<'_>,
        from_generation: u64,
        after_global: u64,
        limit: PageLimit,
    ) -> Result<Page<LoggedEvent>> {
        let only = scope_tenant(scope);
        self.read(move |conn| {
            recovery_page(conn, only.as_deref(), from_generation, after_global, limit)
        })
        .await
    }
    async fn subagent_sessions(&self, scope: Scope<'_>) -> Result<HashSet<TenantSession>> {
        let only = scope_tenant(scope);
        self.read(move |conn| subagents(conn, only.as_deref()))
            .await
    }
    async fn prompts_sent(&self, tenant: &Tenant, session: &SessionKey) -> Result<u64> {
        let (tenant, key) = (tenant.clone(), session.clone());
        self.read(move |conn| {
            Ok(conn
                .prepare_cached(PROMPTS_SQL)?
                .query_row([tenant.as_str(), key.as_str()], |row| row.get(0))
                .optional()?
                .unwrap_or(0))
        })
        .await
    }
    async fn turn_ids(&self, tenant: &Tenant, session: &SessionKey) -> Result<Vec<TurnId>> {
        let (tenant, key) = (tenant.clone(), session.clone());
        self.read(move |conn| turn_ids(conn, &tenant, &key)).await
    }
    async fn allocate_generation(&self, owner: &str, pid: u32, started_ms: i64) -> Result<u64> {
        self.request(|reply| Command::Allocate(owner.into(), pid, started_ms, reply))
            .await
    }
    async fn finish_generation(&self, generation: u64, stopped_ms: i64) -> Result<()> {
        self.request(|reply| Command::Finish(generation, stopped_ms, reply))
            .await
    }
    async fn last_activity_ms(&self, scope: Scope<'_>) -> Result<HashMap<TenantSession, i64>> {
        let only = scope_tenant(scope);
        self.read(move |conn| activity(conn, only.as_deref())).await
    }
    async fn recovery_records(
        &self,
        scope: Scope<'_>,
        from_generation: u64,
    ) -> Result<Vec<LoggedEvent>> {
        let mut records = Vec::new();
        let mut after = 0;
        loop {
            let page = self
                .recovery_page(scope, from_generation, after, PageLimit::DEFAULT)
                .await?;
            records.extend(page.items);
            match page.next {
                Some(Cursor(next)) => after = next,
                None => return Ok(records),
            }
        }
    }
    async fn last_recovered_generation(&self, _scope: &SystemScope) -> Result<u64> {
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
    async fn sync(&self) -> Result<()> {
        self.request(Command::Sync).await
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
    (
        "005_identity.sql",
        include_str!("migrations/005_identity.sql"),
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

fn writer(
    mut conn: Connection,
    mut receiver: mpsc::Receiver<Command>,
    acknowledged: &AtomicU64,
    wal: &Path,
    permits: &Arc<Semaphore>,
    runtime: &Handle,
) {
    let mut pending = None;
    let mut commits = 0u32;
    while let Some(command) = pending.take().or_else(|| receiver.blocking_recv()) {
        match command {
            Command::Sync(reply) => {
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            Command::Pause(release) => {
                let _ = release.recv();
            }
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
                commits += 1;
                if commits.is_multiple_of(WAL_CHECK_COMMITS) {
                    bound_wal(&conn, wal, permits, runtime);
                }
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

/// Last Update sequence of one tenant's Session. `session_seq IS NOT NULL`
/// changes no result (`MAX` skips NULL), but it lets the planner use the
/// partial index `events_tenant_session_seq`. Without it each call scans the
/// whole table, once per appended Update.
const LATEST_SEQ_SQL: &str = "SELECT COALESCE(MAX(session_seq),0) FROM events \
    WHERE session_key=?1 AND tenant=?2 AND session_seq IS NOT NULL";

fn latest(conn: &Connection, tenant: &Tenant, session: &SessionKey) -> SqlResult<u64> {
    Ok(conn
        .prepare_cached(LATEST_SEQ_SQL)?
        .query_row([session.as_str(), tenant.as_str()], |row| row.get(0))?)
}

/// The tenant of a scope as owned text, for a read closure; `None` for a
/// system scope.
fn scope_tenant(scope: Scope<'_>) -> Option<String> {
    scope.tenant().map(|tenant| tenant.as_str().to_owned())
}

const INSERT_SQL: &str = "INSERT INTO events \
    (global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload,\
    tenant,principal_kind,principal_issuer,principal_id,via,actor) \
    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)";

fn append(conn: &mut Connection, events: Vec<Event>) -> SqlResult<Vec<LoggedEvent>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut global: u64 = tx
        .prepare_cached(MAX_GLOBAL_SQL)?
        .query_row([], |r| r.get(0))?;
    let mut records = Vec::with_capacity(events.len());
    let mut rows = Vec::with_capacity(events.len());
    // Session sequences of this transaction: one index read per Session.
    // A batch holds few Sessions, so a short list beats hashing the keys.
    let mut sessions: Vec<(&Tenant, &SessionKey, u64)> = Vec::new();
    let mut assigned = Vec::with_capacity(events.len());
    for event in &events {
        assigned.push(match (&event.kind, &event.session_key) {
            (EventKind::Update(_), Some(key)) => {
                let tenant = event.tenant();
                let at = sessions
                    .iter()
                    .position(|(t, k, _)| *k == key && *t == tenant);
                let seq = match at {
                    Some(at) => {
                        sessions[at].2 += 1;
                        sessions[at].2
                    }
                    None => {
                        let seq = latest(&tx, tenant, key)? + 1;
                        sessions.push((tenant, key, seq));
                        seq
                    }
                };
                Some(seq)
            }
            (EventKind::Update(_), None) => {
                return Err(EventLogError::Invalid("Update without a Session".into()).into())
            }
            _ => None,
        });
    }
    drop(sessions);
    {
        let mut insert = tx.prepare_cached(INSERT_SQL)?;
        for (event, seq) in events.into_iter().zip(assigned) {
            global = global
                .checked_add(1)
                .ok_or_else(|| EventLogError::Storage("global sequence exhausted".into()))?;
            let record = stamp(event, global, seq);
            let raw = serde_json::to_value(&record.event.kind)?;
            let kind = raw
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| EventLogError::Invalid("Unknown must retain its kind".into()))?;
            let payload = serde_json::to_string(&serde_json::json!({"v":1,"event":raw}))?;
            let identity = &record.event.identity;
            insert.execute(params![
                sql_seq(global)?,
                sql_seq(record.event.generation)?,
                record.event.session_key.as_ref().map(SessionKey::as_str),
                seq.map(sql_seq).transpose()?,
                kind,
                record.event.turn.as_ref().map(TurnId::as_str),
                record.event.ts_ms,
                payload,
                identity.tenant.as_str(),
                identity.principal.kind().as_str(),
                identity.principal.issuer(),
                identity.principal.id(),
                identity.via.as_ref().map(Via::as_str),
                identity.actor.as_deref(),
            ])?;
            rows.push(ProjectionRow::of(&record, kind));
            records.push(record);
        }
    }
    project(&tx, &rows)?;
    tx.commit()?;
    Ok(records)
}

// ---------------------------------------------------------------- projection

/// The `projection_state` row of the tenant-keyed projection (migration
/// 005). The 004 row `session_summary` belongs to older binaries.
const PROJECTION: &str = "tenant_session_summary";
const MARK_SQL: &str = "SELECT applied_global_seq FROM projection_state WHERE name=?1";
const SET_MARK_SQL: &str = "UPDATE projection_state SET applied_global_seq=?2 WHERE name=?1";
const SUMMARY_SQL: &str = "SELECT tenant,session_key,last_seq,last_revision,last_activity_ms,\
    prompts_sent,turn_count,parent,owner_seq,owner_kind,owner_issuer,owner_id,owner_via,owner_actor \
    FROM tenant_session_summary WHERE tenant=?1 AND session_key=?2";
const PROMPTS_SQL: &str =
    "SELECT prompts_sent FROM tenant_session_summary WHERE tenant=?1 AND session_key=?2";
const TURNS_SQL: &str = "SELECT turn FROM tenant_session_turns \
    WHERE tenant=?1 AND session_key=?2 ORDER BY first_global_seq";
const SUBAGENTS_SQL: &str =
    "SELECT tenant,session_key FROM tenant_session_summary WHERE parent IS NOT NULL";
/// `INDEXED BY`: without statistics the planner would walk all of the
/// tenant's Sessions by primary key; the partial index holds Subagents only.
const SUBAGENTS_TENANT_SQL: &str = "SELECT tenant,session_key FROM tenant_session_summary \
    INDEXED BY tenant_session_summary_subagents WHERE tenant=?1 AND parent IS NOT NULL";
const ACTIVITY_SQL: &str = "SELECT tenant,session_key,last_activity_ms FROM tenant_session_summary";
const ACTIVITY_TENANT_SQL: &str =
    "SELECT tenant,session_key,last_activity_ms FROM tenant_session_summary WHERE tenant=?1";
const INSERT_TURN_SQL: &str = "INSERT OR IGNORE INTO tenant_session_turns \
    (tenant,session_key,turn,first_global_seq) VALUES (?1,?2,?3,?4)";
/// Merge one commit group's fold of a Session into its row. The rules equal
/// `SessionSummary::apply` in log order: sequences and activity take the
/// maximum, the revision of the group's last revision record wins, counts
/// add, and the first parent and the first owner stay. In `DO UPDATE`,
/// a bare column is the row's value before the update.
const UPSERT_SUMMARY_SQL: &str = "INSERT INTO tenant_session_summary \
    (tenant,session_key,last_seq,last_revision,last_activity_ms,prompts_sent,turn_count,parent) \
    VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
    ON CONFLICT(tenant,session_key) DO UPDATE SET \
    last_seq=MAX(last_seq,excluded.last_seq), \
    last_revision=CASE WHEN ?9 THEN excluded.last_revision ELSE last_revision END, \
    last_activity_ms=MAX(last_activity_ms,excluded.last_activity_ms), \
    prompts_sent=prompts_sent+excluded.prompts_sent, \
    turn_count=turn_count+excluded.turn_count, \
    parent=COALESCE(parent,excluded.parent)";
/// Set the owner of record once: only a group with a creating
/// `session_opened` runs it, after the upsert, so the common commit keeps
/// the upsert of step B2.
const SET_OWNER_SQL: &str = "UPDATE tenant_session_summary SET owner_seq=?3, owner_kind=?4, \
    owner_issuer=?5, owner_id=?6, owner_via=?7, owner_actor=?8 \
    WHERE tenant=?1 AND session_key=?2 AND owner_seq IS NULL";

/// What the projection needs of stored rows, read from the columns and three
/// JSON paths; the payload is never decoded. The text test `instr` keeps
/// JSON parsing to the few rows that can hold a revision. It relies on the
/// compact form that `serde_json` writes (`"type":"config_changed"`, no
/// spaces). An importer that writes other JSON (ADR-0007 step B4) must
/// write it compact, or this query must drop the `instr` test. A row whose
/// needed JSON is not valid gives NULL for that field and 1 in `bad`.
/// The rules equal the queries before step B2: prompts by the `kind`
/// column, a parent by `json_extract` of `session_opened`, turns and
/// activity by their columns. Step B3 adds the identity columns and the
/// `created` mark of a `session_opened` (the owner of record).
const PROJECTION_ROWS_SQL: &str = "SELECT global_seq, session_key, session_seq, kind, turn, ts_ms, \
    CASE WHEN kind='update' AND (instr(payload,'\"type\":\"config_changed\"')>0 \
        OR instr(payload,'\"type\":\"branch_reset\"')>0) AND json_valid(payload) \
        AND json_extract(payload,'$.event.payload.update.type') IN ('config_changed','branch_reset') \
        THEN json_extract(payload,'$.event.payload.update.revision') END, \
    CASE WHEN kind='session_opened' AND json_valid(payload) \
        THEN payload -> '$.event.payload.parent' END, \
    CASE WHEN kind='session_opened' OR (kind='update' AND (instr(payload,'\"type\":\"config_changed\"')>0 \
        OR instr(payload,'\"type\":\"branch_reset\"')>0)) \
        THEN NOT COALESCE(json_valid(payload),0) ELSE 0 END, \
    tenant, principal_kind, principal_issuer, principal_id, via, actor, \
    CASE WHEN kind='session_opened' AND json_valid(payload) \
        THEN json_extract(payload,'$.event.payload.created') IS 1 ELSE 0 END \
    FROM events WHERE global_seq>?1 AND global_seq<=?2 ORDER BY global_seq LIMIT ?3";

/// The identity of a stored row. A row written before step B3 has NULL
/// principal columns and reads as [`Identity::unrecorded`] in tenant
/// `tenant` (the column default `local`). Text this binary did not write
/// is checked by the identity types; a value they refuse is an error.
fn identity_of(
    tenant: String,
    kind: Option<String>,
    issuer: Option<String>,
    id: Option<String>,
    via: Option<String>,
    actor: Option<String>,
) -> SqlResult<Identity> {
    let bad = |what: &str, error: &dyn std::fmt::Display| {
        SqlError::Store(EventLogError::Storage(format!(
            "stored {what} is not valid: {error}"
        )))
    };
    let tenant: Tenant =
        serde_json::from_value(Value::String(tenant)).map_err(|e| bad("tenant", &e))?;
    let principal = match (kind, issuer, id) {
        (Some(kind), Some(issuer), Some(id)) => {
            let kind = PrincipalKind::parse(&kind).map_err(|e| bad("principal kind", &e))?;
            Principal::new(kind, issuer, id).map_err(|e| bad("principal", &e))?
        }
        _ => Identity::unrecorded().principal,
    };
    let via = via
        .map(|via| serde_json::from_value::<Via>(Value::String(via)))
        .transpose()
        .map_err(|e| bad("via", &e))?;
    Ok(Identity {
        tenant,
        principal,
        via,
        actor,
    })
}

/// One record as the projection sees it.
#[derive(Debug, Clone, PartialEq)]
struct ProjectionRow {
    global_seq: u64,
    tenant: Tenant,
    session: Option<SessionKey>,
    seq: Option<u64>,
    kind: String,
    turn: Option<TurnId>,
    ts_ms: i64,
    revision: Option<u64>,
    parent: Option<SessionKey>,
    /// A JSON field the projection needs could not be read.
    malformed: bool,
    /// The row's identity, when it is the `session_opened` that created the
    /// Session. Only this binary writes the `created` mark, so such a row
    /// always has its identity columns.
    owner: Option<Identity>,
}

impl ProjectionRow {
    /// The live path: from the record in memory and its `kind` column. It
    /// gives the same values as the column path for any row this binary
    /// writes (a test compares both).
    fn of(record: &LoggedEvent, kind: &str) -> Self {
        let delta = SummaryDelta::of(record);
        Self {
            global_seq: record.global_seq,
            tenant: record.event.identity.tenant.clone(),
            session: record.event.session_key.clone(),
            seq: record.session_seq,
            kind: kind.to_owned(),
            turn: record.event.turn.clone(),
            ts_ms: record.event.ts_ms,
            revision: delta.as_ref().and_then(|d| d.revision),
            owner: delta.as_ref().and_then(|d| d.owner.clone()),
            parent: delta.and_then(|d| d.parent),
            malformed: false,
        }
    }

    /// The column path. Never fails on the content of a payload. An
    /// identity that does not read leaves the row without an owner and
    /// marks it malformed.
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        use rusqlite::types::Value as Sql;
        let mut malformed = row.get::<_, Option<bool>>(8)?.unwrap_or(true);
        let revision = match row.get::<_, Sql>(6)? {
            Sql::Null => None,
            Sql::Integer(n) => match u64::try_from(n) {
                Ok(n) => Some(n),
                Err(_) => {
                    malformed = true;
                    None
                }
            },
            _ => {
                malformed = true;
                None
            }
        };
        // `->` gives the JSON text of the parent; read it as `SummaryDelta`
        // reads the payload: a string is a key, null is none, any other
        // value is kept as its JSON text.
        let parent = match row.get::<_, Option<String>>(7)? {
            None => None,
            Some(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Null) => None,
                Ok(Value::String(key)) => Some(SessionKey::from(key)),
                Ok(other) => Some(SessionKey::from(other.to_string())),
                Err(_) => {
                    malformed = true;
                    None
                }
            },
        };
        let tenant_text: String = row.get(9)?;
        let created = row.get::<_, Option<bool>>(15)?.unwrap_or(false);
        let identity = identity_of(
            tenant_text.clone(),
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
            row.get(14)?,
        );
        let (tenant, owner) = match identity {
            Ok(identity) => (identity.tenant.clone(), created.then_some(identity)),
            Err(_) => {
                malformed = true;
                // The key still groups the row with its tenant's Session.
                let tenant = serde_json::from_value(Value::String(tenant_text))
                    .unwrap_or_else(|_| Tenant::local());
                (tenant, None)
            }
        };
        Ok(Self {
            global_seq: row.get(0)?,
            tenant,
            session: row.get::<_, Option<String>>(1)?.map(SessionKey::from),
            seq: row.get(2)?,
            kind: row.get(3)?,
            turn: row.get::<_, Option<String>>(4)?.map(TurnId::from),
            ts_ms: row.get(5)?,
            revision,
            parent,
            malformed,
            owner,
        })
    }
}

/// Rows `(after, through]` for the projection, at most `limit`.
fn projection_rows(
    conn: &Connection,
    after: u64,
    through: u64,
    limit: usize,
) -> SqlResult<Vec<ProjectionRow>> {
    if after >= through {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(PROJECTION_ROWS_SQL)?;
    let rows = stmt
        .query_map(
            params![bound(after), bound(through), bound(limit as u64)],
            ProjectionRow::read,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Rows whose JSON the projection could not read: how many, and the first.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Malformed {
    count: u64,
    first: Option<u64>,
}

impl Malformed {
    fn add(&mut self, rows: &[ProjectionRow]) {
        for row in rows.iter().filter(|r| r.malformed) {
            self.count += 1;
            self.first.get_or_insert(row.global_seq);
        }
    }
    /// One warning line for all the rows found by one pass.
    fn warn(&self, pass: &str) {
        if let Some(first) = self.first {
            eprintln!(
                "Event Log: {pass}: {} rows have a payload the Session summary cannot read \
                 (first global_seq {first}); their revision or parent is not counted",
                self.count
            );
        }
    }
}

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
/// projection), the missing rows are applied first, from their columns.
fn project(conn: &Connection, rows: &[ProjectionRow]) -> SqlResult<()> {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return Ok(());
    };
    let mut mark = projection_mark(conn)?;
    if mark + 1 < first.global_seq {
        let malformed = catch_up(conn, mark, first.global_seq - 1)?;
        malformed.warn("catch-up of rows written without the summary");
        mark = first.global_seq - 1;
    }
    apply(conn, rows.iter().filter(|r| r.global_seq > mark))?;
    set_projection_mark(conn, last.global_seq.max(mark))
}

/// Apply stored rows `(after, through]` in chunks.
fn catch_up(conn: &Connection, mut after: u64, through: u64) -> SqlResult<Malformed> {
    let mut malformed = Malformed::default();
    while after < through {
        let rows = projection_rows(conn, after, through, BACKFILL_CHUNK)?;
        let Some(last) = rows.last().map(|r| r.global_seq) else {
            break;
        };
        malformed.add(&rows);
        apply(conn, rows.iter())?;
        after = last;
    }
    Ok(malformed)
}

/// One commit group's change to one Session.
struct GroupFold {
    summary: SessionSummary,
    revised: bool,
    turns: Vec<(TurnId, u64)>,
    /// The `global_seq` of the group's first creating `session_opened`.
    owner_seq: Option<u64>,
}

/// Fold the rows per `(tenant, Session)`, then write one upsert per Session.
fn apply<'a>(conn: &Connection, rows: impl Iterator<Item = &'a ProjectionRow>) -> SqlResult<()> {
    let mut groups: BTreeMap<TenantSession, GroupFold> = BTreeMap::new();
    for row in rows {
        let Some(session) = &row.session else {
            continue;
        };
        let delta = SummaryDelta {
            tenant: row.tenant.clone(),
            session: session.clone(),
            seq: row.seq,
            ts_ms: row.ts_ms,
            revision: row.revision,
            prompt: row.kind == "prompt_sent",
            turn: row.turn.clone(),
            parent: row.parent.clone(),
            owner: row.owner.clone(),
        };
        let group = groups
            .entry((row.tenant.clone(), session.clone()))
            .or_insert_with(|| GroupFold {
                summary: SessionSummary::empty(row.tenant.clone(), session.clone()),
                revised: false,
                turns: Vec::new(),
                owner_seq: None,
            });
        group.revised |= delta.revision.is_some();
        if delta.owner.is_some() && group.owner_seq.is_none() {
            group.owner_seq = Some(row.global_seq);
        }
        if let Some(turn) = &delta.turn {
            if !group.turns.iter().any(|(seen, _)| seen == turn) {
                group.turns.push((turn.clone(), row.global_seq));
            }
        }
        // Turns are counted against the table below, not in the group.
        group.summary.apply(&delta, false);
    }
    let mut insert_turn = conn.prepare_cached(INSERT_TURN_SQL)?;
    let mut upsert = conn.prepare_cached(UPSERT_SUMMARY_SQL)?;
    for ((tenant, session), group) in groups {
        let mut new_turns: u64 = 0;
        for (turn, global) in &group.turns {
            new_turns += insert_turn.execute(params![
                tenant.as_str(),
                session.as_str(),
                turn.as_str(),
                sql_seq(*global)?
            ])? as u64;
        }
        let summary = group.summary;
        upsert.execute(params![
            tenant.as_str(),
            session.as_str(),
            sql_seq(summary.last_seq)?,
            sql_seq(summary.last_revision)?,
            summary.last_activity_ms,
            sql_seq(summary.prompts_sent)?,
            sql_seq(new_turns)?,
            summary.parent.as_ref().map(SessionKey::as_str),
            group.revised,
        ])?;
        if let (Some(owner_seq), Some(owner)) = (group.owner_seq, summary.owner.as_ref()) {
            conn.prepare_cached(SET_OWNER_SQL)?.execute(params![
                tenant.as_str(),
                session.as_str(),
                sql_seq(owner_seq)?,
                owner.principal.kind().as_str(),
                owner.principal.issuer(),
                owner.principal.id(),
                owner.via.as_ref().map(Via::as_str),
                owner.actor.as_deref(),
            ])?;
        }
    }
    Ok(())
}

/// Build or catch up the projection from the stored rows, one transaction
/// per chunk; each moves the mark with its rows, so an interrupted backfill
/// continues where it stopped. Returns whether it reached the end; with
/// `max_chunks` it stops early (tests use it to interrupt).
///
/// It reads columns and two JSON paths only, so the content of a payload
/// never makes it fail. A backfill that runs over a second prints a start
/// line, a progress line every few seconds and a done line on stderr.
fn backfill(conn: &mut Connection, chunk: usize, max_chunks: Option<usize>) -> SqlResult<bool> {
    let start = std::time::Instant::now();
    let mut announced: Option<std::time::Instant> = None;
    let mut chunks = 0;
    let mut applied = 0u64;
    let mut malformed = Malformed::default();
    let result = loop {
        if max_chunks.is_some_and(|max| chunks >= max) {
            break Ok(false);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mark = projection_mark(&tx)?;
        let through: u64 = tx.query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))?;
        if mark >= through {
            tx.commit()?;
            break Ok(true);
        }
        let rows = projection_rows(&tx, mark, through, chunk)?;
        let last = rows.last().map_or(through, |r| r.global_seq);
        malformed.add(&rows);
        apply(&tx, rows.iter())?;
        set_projection_mark(&tx, last)?;
        tx.commit()?;
        chunks += 1;
        applied += rows.len() as u64;
        let left = through - last;
        match announced {
            None if start.elapsed() >= std::time::Duration::from_secs(1) => {
                eprintln!(
                    "Event Log: building the Session summary: {applied} events done, \
                     {left} to go (global_seq {last} of {through})"
                );
                announced = Some(std::time::Instant::now());
            }
            Some(at) if at.elapsed() >= std::time::Duration::from_secs(5) => {
                eprintln!(
                    "Event Log: building the Session summary: {applied} events done, {left} to go"
                );
                announced = Some(std::time::Instant::now());
            }
            _ => {}
        }
    };
    if announced.is_some() {
        eprintln!(
            "Event Log: Session summary built from {applied} events in {:.1} s",
            start.elapsed().as_secs_f64()
        );
    }
    malformed.warn("Session summary backfill");
    result
}

/// WAL size above which the writer resets the WAL (see the type docs).
const WAL_RESET_BYTES: u64 = 64 * 1024 * 1024;
/// The writer checks the WAL size after this many commits.
const WAL_CHECK_COMMITS: u32 = 64;
/// How long the writer waits for the reader permits before it gives up.
/// A reader holds its permit for one page.
const WAL_RESET_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Keep the WAL bounded while readers overlap (the rule is in the docs of
/// [`SqliteEventLog`]). Reading the file size costs one `stat`.
fn bound_wal(conn: &Connection, wal: &Path, permits: &Arc<Semaphore>, runtime: &Handle) {
    if !std::fs::metadata(wal).is_ok_and(|m| m.len() >= WAL_RESET_BYTES) {
        return;
    }
    // A waiter for all permits takes them as readers finish their page;
    // readers that come later queue behind it, so it cannot starve.
    let all = u32::try_from(READER_POOL_SIZE).unwrap_or(u32::MAX);
    let held = runtime.block_on(async {
        tokio::time::timeout(WAL_RESET_WAIT, Arc::clone(permits).acquire_many_owned(all)).await
    });
    match held {
        Ok(Ok(_permits)) => {
            // No reader of this log is in a transaction: the reset does not wait.
            let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        _ => {
            let _ = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        }
    }
}

fn summary(
    conn: &Connection,
    tenant: &Tenant,
    session: &SessionKey,
) -> SqlResult<Option<SessionSummary>> {
    let row = conn
        .prepare_cached(SUMMARY_SQL)?
        .query_row([tenant.as_str(), session.as_str()], |row| {
            Ok((
                SummaryRow {
                    tenant: row.get(0)?,
                    session: row.get(1)?,
                    last_seq: row.get(2)?,
                    last_revision: row.get(3)?,
                    last_activity_ms: row.get(4)?,
                    prompts_sent: row.get(5)?,
                    turn_count: row.get(6)?,
                    parent: row.get(7)?,
                },
                row.get::<_, Option<i64>>(8)?,
                (
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ),
            ))
        })
        .optional()?;
    let Some((row, owner_seq, (kind, issuer, id, via, actor))) = row else {
        return Ok(None);
    };
    // An owner of record needs its creating row.
    let owner = match owner_seq {
        Some(_) => Some(identity_of(
            row.tenant.clone(),
            kind,
            issuer,
            id,
            via,
            actor,
        )?),
        None => None,
    };
    let tenant = identity_of(row.tenant, None, None, None, None, None)?.tenant;
    Ok(Some(SessionSummary {
        tenant,
        session: SessionKey::from(row.session),
        last_seq: row.last_seq,
        last_revision: row.last_revision,
        last_activity_ms: row.last_activity_ms,
        prompts_sent: row.prompts_sent,
        turn_count: row.turn_count,
        parent: row.parent.map(SessionKey::from),
        owner,
    }))
}

/// The plain columns of a summary row.
struct SummaryRow {
    tenant: String,
    session: String,
    last_seq: u64,
    last_revision: u64,
    last_activity_ms: i64,
    prompts_sent: u64,
    turn_count: u64,
    parent: Option<String>,
}

/// A `(tenant, session_key)` pair from two text columns.
fn tenant_session(tenant: String, session: String) -> SqlResult<TenantSession> {
    let tenant = identity_of(tenant, None, None, None, None, None)?.tenant;
    Ok((tenant, SessionKey::from(session)))
}

fn activity(conn: &Connection, only: Option<&str>) -> SqlResult<HashMap<TenantSession, i64>> {
    let mut stmt = match only {
        Some(_) => conn.prepare_cached(ACTIVITY_TENANT_SQL)?,
        None => conn.prepare_cached(ACTIVITY_SQL)?,
    };
    let read = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    };
    let rows = match only {
        Some(tenant) => stmt
            .query_map([tenant], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => stmt
            .query_map([], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    rows.into_iter()
        .map(|(tenant, session, ts)| Ok((tenant_session(tenant, session)?, ts)))
        .collect()
}

fn subagents(conn: &Connection, only: Option<&str>) -> SqlResult<HashSet<TenantSession>> {
    let mut statement = match only {
        Some(_) => conn.prepare_cached(SUBAGENTS_TENANT_SQL)?,
        None => conn.prepare_cached(SUBAGENTS_SQL)?,
    };
    let read = |row: &rusqlite::Row<'_>| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
    let rows = match only {
        Some(tenant) => statement
            .query_map([tenant], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => statement
            .query_map([], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    rows.into_iter()
        .map(|(tenant, session)| tenant_session(tenant, session))
        .collect()
}

fn turn_ids(conn: &Connection, tenant: &Tenant, session: &SessionKey) -> SqlResult<Vec<TurnId>> {
    let mut statement = conn.prepare_cached(TURNS_SQL)?;
    let turns = statement
        .query_map([tenant.as_str(), session.as_str()], |row| {
            row.get::<_, String>(0)
        })?
        .map(|turn| turn.map(TurnId))
        .collect::<rusqlite::Result<_>>()?;
    Ok(turns)
}

// ---------------------------------------------------------------- pages

/// The columns of a record, in the order [`decode`] reads them.
macro_rules! record_columns {
    () => {
        "global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload,\
         tenant,principal_kind,principal_issuer,principal_id,via,actor"
    };
}
const SESSION_PAGE_SQL: &str = concat!(
    "SELECT ",
    record_columns!(),
    " FROM events WHERE session_key=?1 AND tenant=?2 AND session_seq>?3 AND session_seq<=?4 \
     ORDER BY session_seq LIMIT ?5"
);
const GLOBAL_PAGE_SQL: &str = concat!(
    "SELECT ",
    record_columns!(),
    " FROM events WHERE global_seq>?1 AND global_seq<=?2 ORDER BY global_seq LIMIT ?3"
);
/// One tenant's rows: the primary key walk, filtered by tenant.
const GLOBAL_TENANT_PAGE_SQL: &str = concat!(
    "SELECT ",
    record_columns!(),
    " FROM events WHERE global_seq>?1 AND global_seq<=?2 AND tenant=?4 \
     ORDER BY global_seq LIMIT ?3"
);

fn session_page(
    conn: &Connection,
    tenant: &Tenant,
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
            tenant.as_str(),
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
    only: Option<&str>,
    after: u64,
    through: u64,
    limit: PageLimit,
) -> SqlResult<Page<LoggedEvent>> {
    if after >= through || after > i64::MAX as u64 {
        return Ok(Page::last(Vec::new()));
    }
    match only {
        None => page(
            conn,
            GLOBAL_PAGE_SQL,
            params![after as i64, bound(through), page_rows_limit(limit)],
            limit,
            |r| r.global_seq,
        ),
        Some(tenant) => page(
            conn,
            GLOBAL_TENANT_PAGE_SQL,
            params![after as i64, bound(through), page_rows_limit(limit), tenant],
            limit,
            |r| r.global_seq,
        ),
    }
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
///
/// `?4` is a tenant, or NULL for every tenant (a system scope).
const RECOVERY_PAGE_SQL: &str =
    "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,\
     CASE WHEN kind='prompt_sent' \
     THEN '{\"v\":1,\"event\":{\"kind\":\"prompt_sent\",\"payload\":{}}}' \
     ELSE payload END,\
     tenant,principal_kind,principal_issuer,principal_id,via,actor \
     FROM events WHERE global_seq>?2 AND +generation>=?1 AND (?4 IS NULL OR tenant=?4) AND (\
     kind IN ('prompt_sent','prompt_settled','runtime_stopped',\
     'escalation_raised','escalation_answered','escalation_withdrawn') \
     OR (kind='update' AND json_extract(payload,'$.event.payload.update.type') \
         IN ('turn_started','turn_ended'))) ORDER BY global_seq LIMIT ?3";

fn recovery_page(
    conn: &Connection,
    only: Option<&str>,
    from_generation: u64,
    after_global: u64,
    limit: PageLimit,
) -> SqlResult<Page<LoggedEvent>> {
    if from_generation > i64::MAX as u64 || after_global > i64::MAX as u64 {
        return Ok(Page::last(Vec::new()));
    }
    // The floor matters only for the first page: later pages start after a
    // record that is already past it.
    let after = if after_global == 0 {
        match recovery_floor(conn, from_generation as i64)? {
            Some(floor) => floor - 1,
            None => return Ok(Page::last(Vec::new())),
        }
    } else {
        after_global as i64
    };
    page(
        conn,
        RECOVERY_PAGE_SQL,
        params![from_generation as i64, after, page_rows_limit(limit), only],
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
    // Rows from before step B3 have NULL principal columns: see `identity_of`.
    let identity = identity_of(
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    )?;
    Ok(LoggedEvent {
        global_seq: row.get(0)?,
        session_seq: row.get(3)?,
        event: Event {
            generation: row.get(1)?,
            session_key: row.get::<_, Option<String>>(2)?.map(Into::into),
            turn: row.get::<_, Option<String>>(5)?.map(Into::into),
            ts_ms: row.get(6)?,
            kind,
            identity,
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
    use std::collections::BTreeSet;

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
    /// The text before step B2 (no `IS NOT NULL`) shows why: the planner
    /// cannot prove the index predicate, so it scans `events`. The query of
    /// a binary from before step B3 (no tenant) still reads the new index,
    /// so an older binary that opens a new database does not scan per append.
    #[test]
    fn latest_seq_uses_the_session_seq_index() {
        let conn = migrated();
        let old = "SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1";
        let before = plan(&conn, old);
        let after = plan(&conn, LATEST_SEQ_SQL);
        let pre_b3 = plan(&conn, OLD_LATEST_SEQ_SQL);
        println!("old plan: {before:?}");
        println!("new plan: {after:?}");
        println!("pre-B3 plan: {pre_b3:?}");
        assert!(
            !before
                .iter()
                .any(|row| row.contains("events_tenant_session_seq")),
            "old query should not use the partial index: {before:?}"
        );
        for rows in [&after, &pre_b3] {
            assert!(
                rows.iter()
                    .any(|row| row.contains("events_tenant_session_seq")),
                "latest() must use events_tenant_session_seq: {rows:?}"
            );
        }
        assert!(
            after
                .iter()
                .any(|row| row.contains("session_key=? AND tenant=?")),
            "the new query seeks key and tenant: {after:?}"
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
                "USING INDEX events_tenant_session_seq (session_key=? AND tenant=? AND session_seq>? AND session_seq<?)",
            ),
            ("global page", GLOBAL_PAGE_SQL, "USING INTEGER PRIMARY KEY"),
            (
                "global page of one tenant",
                GLOBAL_TENANT_PAGE_SQL,
                "USING INTEGER PRIMARY KEY",
            ),
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
            // The summary tables are WITHOUT ROWID: a primary key search is
            // index-only, on the full key (tenant, session_key).
            (
                "summary",
                SUMMARY_SQL,
                "SEARCH tenant_session_summary USING PRIMARY KEY (tenant=? AND session_key=?)",
            ),
            (
                "prompts sent",
                PROMPTS_SQL,
                "SEARCH tenant_session_summary USING PRIMARY KEY (tenant=? AND session_key=?)",
            ),
            (
                "turn ids",
                TURNS_SQL,
                "SEARCH tenant_session_turns USING PRIMARY KEY (tenant=? AND session_key=?)",
            ),
            (
                // The partial index holds only Subagents: O(Subagents).
                "subagents of all tenants",
                SUBAGENTS_SQL,
                "USING INDEX tenant_session_summary_subagents",
            ),
            (
                "subagents of one tenant",
                SUBAGENTS_TENANT_SQL,
                "USING INDEX tenant_session_summary_subagents (tenant=?)",
            ),
            (
                "activity of one tenant",
                ACTIVITY_TENANT_SQL,
                "SEARCH tenant_session_summary USING PRIMARY KEY (tenant=?)",
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
            rows.iter()
                .all(|row| row.contains("tenant_session_summary")),
            "{rows:?}"
        );
        // The queries of a binary from before step B3 on the new schema:
        // the per-append and page queries still read an index.
        for (name, old) in [
            ("pre-B3 latest seq", OLD_LATEST_SEQ_SQL),
            ("pre-B3 session page", OLD_SESSION_PAGE_SQL),
        ] {
            let rows = plan(&conn, old);
            println!("{name}: {rows:?}");
            assert!(
                rows.iter()
                    .any(|row| row.contains("events_tenant_session_seq")),
                "{name}: {rows:?}"
            );
            assert!(!rows.iter().any(|row| row.starts_with("SCAN events")));
        }
        for (name, old) in [
            ("old last activity", "SELECT session_key, MAX(ts_ms) FROM events WHERE session_key IS NOT NULL GROUP BY session_key"),
            ("old subagents", "SELECT DISTINCT session_key FROM events WHERE kind='session_opened' AND session_key IS NOT NULL AND json_extract(payload,'$.event.payload.parent') IS NOT NULL"),
            ("old turn ids", "SELECT DISTINCT turn FROM events WHERE session_key=?1 AND turn IS NOT NULL"),
            ("old prompts sent", "SELECT COUNT(*) FROM events WHERE session_key=?1 AND kind='prompt_sent'"),
        ] {
            println!("{name}: {:?}", plan(&conn, old));
        }
    }

    /// The insert of the code before step B3: no identity columns.
    const OLD_INSERT_SQL: &str = "INSERT INTO events (global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)";
    /// The per-append sequence query of the code before step B3 (one key
    /// space for all tenants).
    const OLD_LATEST_SEQ_SQL: &str =
        "SELECT COALESCE(MAX(session_seq),0) FROM events WHERE session_key=?1 AND session_seq IS NOT NULL";

    fn old_latest(conn: &Connection, session: &str) -> u64 {
        conn.query_row(OLD_LATEST_SEQ_SQL, [session], |row| row.get(0))
            .expect("latest")
    }

    /// Insert events the way the code before step B2 did: no projection
    /// and (before B3) no identity.
    fn insert_old(conn: &Connection, events: Vec<Event>) {
        let mut global: u64 = conn
            .query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))
            .expect("max");
        for event in events {
            global += 1;
            let seq = match (&event.kind, &event.session_key) {
                (EventKind::Update(_), Some(key)) => Some(old_latest(conn, key.as_str()) + 1),
                _ => None,
            };
            let record = stamp(event, global, seq);
            let mut raw = serde_json::to_value(&record.event.kind).expect("json");
            // Older binaries never wrote the `created` mark.
            if let Some(payload) = raw.get_mut("payload").and_then(Value::as_object_mut) {
                payload.remove(loop_store::SESSION_CREATED_KEY);
            }
            let kind = raw["kind"].as_str().expect("kind").to_owned();
            let payload = serde_json::json!({"v":1,"event":raw}).to_string();
            conn.execute(
                OLD_INSERT_SQL,
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

    /// The projection rows (summaries with their tenant and owner, and
    /// turns as `(tenant, session, turn, first_global_seq)`), in key order.
    #[allow(clippy::type_complexity)]
    fn projection_rows_table(
        conn: &Connection,
    ) -> (Vec<SessionSummary>, Vec<(String, String, String, i64)>) {
        let keys = conn
            .prepare(
                "SELECT tenant,session_key FROM tenant_session_summary ORDER BY tenant,session_key",
            )
            .expect("prepare")
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        let summaries = keys
            .into_iter()
            .map(|(tenant, key)| {
                let (tenant, key) = tenant_session(tenant, key).expect("key");
                summary(conn, &tenant, &key).expect("summary").expect("row")
            })
            .collect();
        let turns = conn
            .prepare("SELECT tenant,session_key,turn,first_global_seq FROM tenant_session_turns ORDER BY tenant,session_key,turn")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        (summaries, turns)
    }

    /// Delete the projection and build it again from the events.
    fn rebuild(conn: &mut Connection) {
        conn.execute_batch(
            "DELETE FROM tenant_session_summary; DELETE FROM tenant_session_turns; \
             UPDATE projection_state SET applied_global_seq=0 WHERE name='tenant_session_summary';",
        )
        .expect("clear");
        assert!(backfill(conn, 7, None).expect("rebuild"));
    }

    async fn assert_matches_fold(log: &SqliteEventLog) {
        let records = log.read_runtime_after(kit::all(), 0).await.expect("read");
        let expected = summarize(&records);
        assert!(!expected.is_empty());
        for ((tenant, session), (summary, turns)) in &expected {
            assert_eq!(
                log.session_summary(tenant, session)
                    .await
                    .expect("summary")
                    .as_ref(),
                Some(summary)
            );
            assert_eq!(&log.turn_ids(tenant, session).await.expect("turns"), turns);
            assert_eq!(
                log.session_owner(tenant, session).await.expect("owner"),
                summary.owner
            );
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
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(
            projection_rows_table(&conn),
            live,
            "projection equals a rebuild"
        );
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
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    /// Applying the same records twice changes nothing: the mark skips them.
    #[test]
    fn projection_apply_is_idempotent() {
        let mut conn = migrated();
        for batch in kit::workload() {
            append(&mut conn, batch).expect("append");
        }
        let before = projection_rows_table(&conn);
        let rows = projection_rows(&conn, 0, u64::MAX, 10_000).expect("rows");
        let tx = conn.transaction().expect("tx");
        project(&tx, &rows).expect("project again");
        tx.commit().expect("commit");
        assert_eq!(projection_rows_table(&conn), before);
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
        assert_eq!(
            log.latest_seq(&loop_store::Tenant::local(), &"s".into())
                .await
                .expect("latest"),
            2
        );
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
            let local = loop_store::Tenant::local();
            let read = log.read_after(&local, &big, 0);
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
            log.read_after(&loop_store::Tenant::local(), &"big".into(), 0)
                .await
                .expect("read")
                .len(),
            2048
        );
    }

    /// The queries of the code before step B2, as the reference for the
    /// projection. Rows whose JSON is not valid are left out of the parent
    /// and revision queries (the old `json_extract` failed on them).
    struct OldQueries {
        prompts: BTreeMap<String, u64>,
        activity: BTreeMap<String, i64>,
        turns: BTreeMap<String, Vec<String>>,
        subagents: BTreeSet<String>,
        revision: BTreeMap<String, u64>,
        last_seq: BTreeMap<String, u64>,
    }

    fn old_queries(conn: &Connection) -> OldQueries {
        let pairs = |sql: &str| -> Vec<(String, i64)> {
            conn.prepare(sql)
                .expect("prepare")
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        let mut turns: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (session, turn) in conn
            .prepare("SELECT session_key, turn FROM events WHERE session_key IS NOT NULL AND turn IS NOT NULL GROUP BY session_key, turn ORDER BY MIN(global_seq)")
            .expect("prepare")
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
        {
            turns.entry(session).or_default().push(turn);
        }
        let subagents = conn
            .prepare("SELECT DISTINCT session_key FROM events WHERE kind='session_opened' AND session_key IS NOT NULL AND json_valid(payload) AND json_extract(payload,'$.event.payload.parent') IS NOT NULL")
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        let mut revision = BTreeMap::new();
        for (session, rev) in pairs(
            "SELECT session_key, json_extract(payload,'$.event.payload.update.revision') FROM events \
             WHERE kind='update' AND json_valid(payload) \
             AND json_extract(payload,'$.event.payload.update.type') IN ('config_changed','branch_reset') \
             AND json_type(payload,'$.event.payload.update.revision')='integer' \
             ORDER BY global_seq",
        ) {
            revision.insert(session, rev as u64);
        }
        OldQueries {
            prompts: pairs("SELECT session_key, COUNT(*) FROM events WHERE session_key IS NOT NULL AND kind='prompt_sent' GROUP BY session_key")
                .into_iter()
                .map(|(k, n)| (k, n as u64))
                .collect(),
            activity: pairs("SELECT session_key, MAX(ts_ms) FROM events WHERE session_key IS NOT NULL GROUP BY session_key")
                .into_iter()
                .collect(),
            turns,
            subagents,
            revision,
            last_seq: pairs("SELECT session_key, MAX(session_seq) FROM events WHERE session_key IS NOT NULL AND session_seq IS NOT NULL GROUP BY session_key")
                .into_iter()
                .map(|(k, n)| (k, n as u64))
                .collect(),
        }
    }

    /// The projection equals the old queries for every Session.
    fn assert_matches_old_queries(conn: &Connection) {
        let old = old_queries(conn);
        let (summaries, turn_rows) = projection_rows_table(conn);
        assert_eq!(
            summaries.len(),
            old.activity.len(),
            "one summary per Session"
        );
        for summary in &summaries {
            let key = summary.session.as_str().to_owned();
            assert_eq!(
                summary.prompts_sent,
                old.prompts.get(&key).copied().unwrap_or(0),
                "{key} prompts"
            );
            assert_eq!(
                summary.last_activity_ms, old.activity[&key],
                "{key} activity"
            );
            assert_eq!(
                summary.last_seq,
                old.last_seq.get(&key).copied().unwrap_or(0),
                "{key} seq"
            );
            assert_eq!(
                summary.last_revision,
                old.revision.get(&key).copied().unwrap_or(0),
                "{key} revision"
            );
            assert_eq!(
                summary.parent.is_some(),
                old.subagents.contains(&key),
                "{key} subagent"
            );
            let turns: Vec<String> = turn_rows
                .iter()
                .filter(|(_, s, _, _)| *s == key)
                .map(|(_, _, t, g)| (g, t.clone()))
                .collect::<BTreeMap<_, _>>()
                .into_values()
                .collect();
            assert_eq!(
                turns,
                old.turns.get(&key).cloned().unwrap_or_default(),
                "{key} turns"
            );
            assert_eq!(summary.turn_count, turns.len() as u64);
        }
    }

    /// Rows that the current binary cannot decode, or whose JSON is broken,
    /// as an older or newer writer could leave them.
    fn insert_bad_rows(conn: &Connection) {
        let mut global: i64 = conn
            .query_row(MAX_GLOBAL_SQL, [], |row| row.get(0))
            .expect("max");
        let mut raw = |session: &str,
                       seq: Option<i64>,
                       kind: &str,
                       turn: Option<&str>,
                       ts: i64,
                       payload: &str| {
            global += 1;
            conn.execute(
                OLD_INSERT_SQL,
                params![global, 1, session, seq, kind, turn, ts, payload],
            )
            .expect("insert");
        };
        let next = |session: &str| -> i64 { old_latest(conn, session) as i64 + 1 };
        // Broken JSON in each kind that the summary reads JSON from.
        let s = next("bad");
        raw(
            "bad",
            Some(s),
            "update",
            Some("turn-9"),
            900,
            "{not json \"type\":\"config_changed\"",
        );
        raw("bad", None, "session_opened", None, 901, "{not json");
        // A prompt of a newer payload version: the kind column still counts.
        raw(
            "bad",
            None,
            "prompt_sent",
            Some("turn-10"),
            902,
            "{\"v\":2,\"event\":{\"kind\":\"prompt_sent\",\"payload\":{}}}",
        );
        // No `event` key, and a kind this binary does not know.
        raw("bad", None, "prompt_sent", None, 903, "{\"v\":1}");
        raw(
            "a",
            None,
            "future_kind",
            Some("turn-11"),
            904,
            "{\"v\":1,\"event\":{\"kind\":\"future_kind\",\"payload\":{}}}",
        );
        // A valid revision record of a newer version, and a non-integer revision.
        let s = next("a");
        raw("a", Some(s), "update", None, 905, "{\"v\":2,\"event\":{\"kind\":\"update\",\"payload\":{\"update\":{\"type\":\"config_changed\",\"revision\":77}}}}");
        let s = next("a");
        raw("a", Some(s), "update", None, 906, "{\"v\":1,\"event\":{\"kind\":\"update\",\"payload\":{\"update\":{\"type\":\"branch_reset\",\"revision\":\"x\"}}}}");
        // A parent that is not a string.
        raw("odd", None, "session_opened", None, 907, "{\"v\":1,\"event\":{\"kind\":\"session_opened\",\"payload\":{\"parent\":{\"id\":\"a\"}}}}");
    }

    /// H1: a pre-B2 database with rows the binary cannot read still opens.
    /// The summary equals the old queries, Unknown rows included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_rows_do_not_stop_open_and_the_summary_equals_the_old_queries() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        old_database(&path);
        {
            let conn = Connection::open(&path).expect("open");
            insert_bad_rows(&conn);
            for batch in kit::workload() {
                insert_old(&conn, batch);
            }
        }
        let log = SqliteEventLog::open(dir.path())
            .await
            .expect("open with bad rows");
        let summary = log
            .session_summary(&loop_store::Tenant::local(), &"bad".into())
            .await
            .expect("summary")
            .expect("bad");
        assert_eq!(summary.prompts_sent, 2, "prompts by the kind column");
        assert_eq!(summary.turn_count, 2);
        // Appends still work.
        log.append_batch(vec![kit::update("bad")])
            .await
            .expect("append");
        drop(log);
        let mut conn = Connection::open(&path).expect("open");
        assert_matches_old_queries(&conn);
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    /// H1: bad rows written after open (by an older writer) do not stop the
    /// next append, which catches them up from their columns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_rows_during_catch_up_do_not_stop_appends() {
        let dir = tempfile::tempdir().expect("dir");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        for batch in kit::workload() {
            log.append_batch(batch).await.expect("append");
        }
        {
            let conn = Connection::open(dir.path().join("events.db")).expect("open");
            insert_bad_rows(&conn);
        }
        log.append_batch(vec![kit::update("a")])
            .await
            .expect("append after bad rows");
        let summary = log
            .session_summary(&loop_store::Tenant::local(), &"a".into())
            .await
            .expect("summary")
            .expect("a");
        assert_eq!(
            summary.last_revision, 77,
            "a newer version's revision counts"
        );
        drop(log);
        let mut conn = Connection::open(dir.path().join("events.db")).expect("open");
        assert_matches_old_queries(&conn);
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    /// The live path (records in memory) and the column path (stored rows)
    /// give the same projection row for every record this binary writes,
    /// Unknown kinds and non-string parents included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_and_column_paths_agree() {
        let dir = tempfile::tempdir().expect("dir");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        for batch in kit::workload() {
            log.append_batch(batch).await.expect("append");
        }
        let opened = |parent: serde_json::Value| Event {
            generation: 1,
            session_key: Some("p".into()),
            turn: Some("turn-1".into()),
            ts_ms: 5,
            kind: EventKind::SessionOpened(serde_json::json!({"parent": parent, "z": 1, "a": [1]})),
            identity: loop_store::Identity::unrecorded(),
        };
        log.append_batch(vec![
            opened(serde_json::json!({"b": 2, "a": "x"})),
            opened(serde_json::json!(7)),
            opened(serde_json::json!(true)),
            opened(serde_json::json!(1.5)),
            opened(serde_json::json!(["k"])),
        ])
        .await
        .expect("append");
        let conn = Connection::open(dir.path().join("events.db")).expect("open");
        let column = projection_rows(&conn, 0, u64::MAX, 100_000).expect("rows");
        let stored =
            global_page(&conn, None, 0, u64::MAX, PageLimit::events(100_000)).expect("records");
        assert_eq!(column.len(), stored.items.len());
        for (row, record) in column.iter().zip(&stored.items) {
            assert_eq!(
                &ProjectionRow::of(record, &row.kind),
                row,
                "global_seq {}",
                row.global_seq
            );
        }
        assert_matches_old_queries(&conn);
    }

    /// M1: a backfill that fails in the middle of a chunk through the real
    /// `open` (a trigger aborts the chunk's commit) rolls that chunk back,
    /// and the next open finishes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_open_that_fails_mid_chunk_resumes_at_the_next_open() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        old_database_rows(&path, 3 * BACKFILL_CHUNK as u64 + 100);
        {
            let mut conn = Connection::open(&path).expect("open");
            migrate(&mut conn).expect("migrate");
            conn.execute_batch(&format!(
                "CREATE TRIGGER stop_backfill BEFORE UPDATE ON projection_state \
                 WHEN NEW.applied_global_seq > {} BEGIN SELECT RAISE(ABORT, 'interrupted'); END;",
                BACKFILL_CHUNK
            ))
            .expect("trigger");
        }
        let error = SqliteEventLog::open(dir.path())
            .await
            .err()
            .expect("open fails");
        assert!(error.to_string().contains("interrupted"), "{error}");
        {
            let conn = Connection::open(&path).expect("open");
            assert_eq!(projection_mark(&conn).expect("mark"), BACKFILL_CHUNK as u64);
            conn.execute_batch("DROP TRIGGER stop_backfill")
                .expect("drop");
        }
        let log = SqliteEventLog::open(dir.path()).await.expect("reopen");
        assert_matches_fold(&log).await;
        drop(log);
        let mut conn = Connection::open(&path).expect("open");
        assert_matches_old_queries(&conn);
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    /// The child side of the kill test: open (and backfill) the database
    /// named by `LOOP_B2_KILL_DB`, with small chunks, then wait to be killed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "run by backfill_killed_by_the_os_resumes"]
    async fn backfill_child_process() {
        let Ok(dir) = std::env::var("LOOP_B2_KILL_DB") else {
            return;
        };
        let _log = SqliteEventLog::open(&dir).await.expect("child open");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }

    /// M1: a process killed (SIGKILL) while `open` backfills leaves the mark
    /// at a chunk boundary; the next open finishes, equal to a rebuild.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_killed_by_the_os_resumes() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        old_database_rows(&path, 60_000);
        let max: u64 = Connection::open(&path)
            .expect("open")
            .query_row(MAX_GLOBAL_SQL, [], |r| r.get(0))
            .expect("max");
        let mut child = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args([
                "--exact",
                "log::sqlite::tests::backfill_child_process",
                "--ignored",
                "--test-threads=1",
            ])
            .env("LOOP_B2_KILL_DB", dir.path())
            .env("LOOP_B2_BACKFILL_CHUNK", "16")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mark = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never backfilled"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
            let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            else {
                continue;
            };
            if let Ok(mark) = projection_mark(&conn) {
                if mark > 0 {
                    break mark;
                }
            }
        };
        child.kill().expect("kill");
        child.wait().expect("wait");
        let after_kill = projection_mark(&Connection::open(&path).expect("open")).expect("mark");
        assert!(after_kill >= mark);
        assert!(
            after_kill < max,
            "killed before the end ({after_kill} of {max})"
        );
        assert_eq!(after_kill % 16, 0, "the mark stops at a chunk boundary");
        let log = SqliteEventLog::open(dir.path()).await.expect("reopen");
        drop(log);
        let mut conn = Connection::open(&path).expect("open");
        assert_eq!(projection_mark(&conn).expect("mark"), max);
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    /// M2: an append whose caller stopped waiting while it was still queued
    /// commits later. Reads may miss it until then; `sync` waits for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_waits_for_an_abandoned_queued_append() {
        let dir = tempfile::tempdir().expect("dir");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        let session = SessionKey::from("s");
        let started = |turn: &str| {
            kit::update_with(
                "s",
                turn,
                1,
                loop_protocol::SessionUpdate::TurnStarted {
                    turn_id: turn.into(),
                },
            )
        };
        log.append_batch(vec![started("turn-1")])
            .await
            .expect("append");
        // Hold the writer, queue `turn-2`, then drop the caller.
        let (release, paused) = std::sync::mpsc::channel();
        log.sender
            .send(Command::Pause(paused))
            .await
            .expect("pause");
        let queued = {
            let log = log.clone();
            tokio::spawn(async move { log.append_batch(vec![started("turn-2")]).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        queued.abort();
        let _ = queued.await;
        // A reader does not see the queued append yet.
        assert_eq!(
            log.turn_ids(&loop_store::Tenant::local(), &session)
                .await
                .expect("turns"),
            vec![TurnId::from("turn-1")]
        );
        let sync = {
            let log = log.clone();
            tokio::spawn(async move { log.sync().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!sync.is_finished(), "sync waits for the queued append");
        release.send(()).expect("release");
        sync.await.expect("join").expect("sync");
        // After sync, the abandoned append is visible: no turn id is reused.
        assert_eq!(
            log.turn_ids(&loop_store::Tenant::local(), &session)
                .await
                .expect("turns"),
            vec![TurnId::from("turn-1"), TurnId::from("turn-2")]
        );
    }

    /// Time of the backfill of a pre-B2 database (review item M1). Ignored.
    /// `LOOP_B2_BACKFILL_EVENTS` [200000] sets the event count. The rows are
    /// a Runtime-like mix: per turn a prompt, a turn start and 100 text
    /// chunks, over 1,000 Sessions.
    #[test]
    #[ignore = "measurement; run by hand"]
    fn backfill_cost() {
        let total: u64 = std::env::var("LOOP_B2_BACKFILL_EVENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200_000);
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        old_database_rows(&path, total);
        let mut conn = Connection::open(&path).expect("open");
        let start = std::time::Instant::now();
        migrate(&mut conn).expect("migrate");
        assert!(backfill(&mut conn, BACKFILL_CHUNK, None).expect("backfill"));
        println!(
            "BACKFILL| {total} events: {:.2} s",
            start.elapsed().as_secs_f64()
        );
    }

    /// An old-schema database with `total` Runtime-like rows, written fast.
    fn old_database_rows(path: &Path, total: u64) {
        let mut conn = Connection::open(path).expect("open");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;")
            .expect("pragmas");
        conn.execute_batch(
            "CREATE TABLE migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )
        .expect("migrations");
        for (id, sql) in &MIGRATIONS[..3] {
            conn.execute_batch(sql).expect("old migration");
            conn.execute(
                "INSERT INTO migrations (id, applied_at) VALUES (?1, 0)",
                [id],
            )
            .expect("record");
        }
        let tx = conn.transaction().expect("tx");
        {
            let mut insert = tx.prepare(OLD_INSERT_SQL).expect("prepare");
            let mut seqs: HashMap<String, u64> = HashMap::new();
            let mut global = 0u64;
            let mut turn = 0u64;
            while global < total {
                turn += 1;
                let session = format!("s{}", turn % 1000);
                let turn_id = format!("turn-{}", turn / 1000 + 1);
                let mut events = vec![kit::update_with(
                    &session,
                    &turn_id,
                    turn as i64,
                    loop_protocol::SessionUpdate::TurnStarted {
                        turn_id: turn_id.as_str().into(),
                    },
                )];
                events.insert(
                    0,
                    Event {
                        generation: 1,
                        session_key: Some(session.as_str().into()),
                        turn: None,
                        ts_ms: turn as i64,
                        kind: EventKind::PromptSent(serde_json::json!({"content": []})),
                        identity: loop_store::Identity::unrecorded(),
                    },
                );
                for _ in 0..100 {
                    let mut chunk = kit::chunk(&session, 100);
                    chunk.turn = Some(turn_id.as_str().into());
                    if let EventKind::Update(envelope) = &mut chunk.kind {
                        envelope.turn = chunk.turn.clone();
                    }
                    events.push(chunk);
                }
                for event in events {
                    if global >= total {
                        break;
                    }
                    global += 1;
                    let seq = match &event.kind {
                        EventKind::Update(_) => {
                            let seq = seqs.entry(session.clone()).or_default();
                            *seq += 1;
                            Some(*seq)
                        }
                        _ => None,
                    };
                    let record = stamp(event, global, seq);
                    let raw = serde_json::to_value(&record.event.kind).expect("json");
                    let kind = raw["kind"].as_str().expect("kind").to_owned();
                    let payload = serde_json::json!({"v":1,"event":raw}).to_string();
                    insert
                        .execute(params![
                            global as i64,
                            1,
                            session,
                            seq.map(|s| s as i64),
                            kind,
                            record.event.turn.as_ref().map(TurnId::as_str),
                            record.event.ts_ms,
                            payload
                        ])
                        .expect("insert");
                }
            }
        }
        tx.commit().expect("commit");
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
        let records = log
            .read_runtime_after(
                loop_store::Scope::System(&loop_store::SystemScope::for_runtime_startup()),
                0,
            )
            .await
            .expect("read");
        println!("events: {}", records.len());
        assert_matches_fold(&log).await;
        drop(log);
        let mut conn = Connection::open(dir.path().join("events.db")).expect("open");
        let live = projection_rows_table(&conn);
        println!("sessions: {}", live.0.len());
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    // ------------------------------------------------------------ step B3

    /// The statements of the binary before step B3 (staging 344dee4) that
    /// touch the events and the 004 projection. An older binary that opens
    /// a database of this version runs exactly these.
    const OLD_SESSION_PAGE_SQL: &str = "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload FROM events WHERE session_key=?1 AND session_seq>?2 AND session_seq<=?3 ORDER BY session_seq LIMIT ?4";
    const OLD_GLOBAL_PAGE_SQL: &str = "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload FROM events WHERE global_seq>?1 AND global_seq<=?2 ORDER BY global_seq LIMIT ?3";
    const OLD_UPSERT_SUMMARY_SQL: &str = "INSERT INTO session_summary \
        (session_key,last_seq,last_revision,last_activity_ms,prompts_sent,turn_count,parent) \
        VALUES (?1,?2,?3,?4,?5,?6,?7) \
        ON CONFLICT(session_key) DO UPDATE SET \
        last_seq=MAX(last_seq,excluded.last_seq), \
        last_revision=CASE WHEN ?8 THEN excluded.last_revision ELSE last_revision END, \
        last_activity_ms=MAX(last_activity_ms,excluded.last_activity_ms), \
        prompts_sent=prompts_sent+excluded.prompts_sent, \
        turn_count=turn_count+excluded.turn_count, \
        parent=COALESCE(parent,excluded.parent)";
    const OLD_INSERT_TURN_SQL: &str =
        "INSERT OR IGNORE INTO session_turns (session_key,turn,first_global_seq) VALUES (?1,?2,?3)";
    const OLD_PROJECTION_ROWS_SQL: &str = "SELECT global_seq, session_key, session_seq, kind, turn, ts_ms, \
        CASE WHEN kind='update' AND (instr(payload,'\"type\":\"config_changed\"')>0 \
            OR instr(payload,'\"type\":\"branch_reset\"')>0) AND json_valid(payload) \
            AND json_extract(payload,'$.event.payload.update.type') IN ('config_changed','branch_reset') \
            THEN json_extract(payload,'$.event.payload.update.revision') END, \
        CASE WHEN kind='session_opened' AND json_valid(payload) \
            THEN payload -> '$.event.payload.parent' END, \
        CASE WHEN kind='session_opened' OR (kind='update' AND (instr(payload,'\"type\":\"config_changed\"')>0 \
            OR instr(payload,'\"type\":\"branch_reset\"')>0)) \
            THEN NOT COALESCE(json_valid(payload),0) ELSE 0 END \
        FROM events WHERE global_seq>?1 AND global_seq<=?2 ORDER BY global_seq LIMIT ?3";

    /// Decode a row the way the binary before step B3 did: eight columns,
    /// the kind from the payload.
    fn old_decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<(u64, Option<u64>, EventKind)> {
        let payload: String = row.get(7)?;
        let payload: Value = serde_json::from_str(&payload).expect("payload json");
        let kind: EventKind = serde_json::from_value(payload["event"].clone()).expect("kind");
        Ok((row.get(0)?, row.get(3)?, kind))
    }

    /// The 004 projection as the binary before step B3 kept it, from the
    /// events (the old queries are its reference).
    fn fill_004_projection(conn: &Connection) {
        let old = old_queries(conn);
        for (key, activity) in &old.activity {
            conn.execute(
                "INSERT INTO session_summary (session_key,last_seq,last_revision,last_activity_ms,prompts_sent,turn_count,parent) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    key,
                    old.last_seq.get(key).copied().unwrap_or(0) as i64,
                    old.revision.get(key).copied().unwrap_or(0) as i64,
                    activity,
                    old.prompts.get(key).copied().unwrap_or(0) as i64,
                    old.turns.get(key).map_or(0, Vec::len) as i64,
                    first_parent(conn, key),
                ],
            )
            .expect("summary row");
        }
        conn.execute_batch(
            "INSERT INTO session_turns (session_key,turn,first_global_seq) \
             SELECT session_key, turn, MIN(global_seq) FROM events \
             WHERE session_key IS NOT NULL AND turn IS NOT NULL GROUP BY session_key, turn; \
             UPDATE projection_state SET applied_global_seq=(SELECT MAX(global_seq) FROM events) \
             WHERE name='session_summary';",
        )
        .expect("turns and mark");
    }

    /// The first parent a `session_opened` of `key` recorded.
    fn first_parent(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT json_extract(payload,'$.event.payload.parent') FROM events \
             WHERE session_key=?1 AND kind='session_opened' AND json_valid(payload) \
             AND json_extract(payload,'$.event.payload.parent') IS NOT NULL \
             ORDER BY global_seq LIMIT 1",
            [key],
            |r| r.get(0),
        )
        .optional()
        .expect("parent")
    }

    fn table_rows(conn: &Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut statement = conn.prepare(sql).expect("prepare");
        let width = statement.column_count();
        statement
            .query_map([], |row| {
                (0..width)
                    .map(|i| row.get(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows")
    }

    /// A database of the binary before step B3 (migrations 001 to 004, its
    /// projection built, then more rows past its mark) opens with this
    /// binary: no row is rewritten; old rows read as tenant `local` and the
    /// Runtime principal; the 004 projection is copied in tenant `local`
    /// and caught up; the 004 tables are left as they were; new appends of
    /// the same key in another tenant get their own sequence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_database_from_before_b3_upgrades_without_a_rewrite() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; \
                 CREATE TABLE migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
            )
            .expect("schema");
            for (id, sql) in &MIGRATIONS[..4] {
                conn.execute_batch(sql).expect("old migration");
                conn.execute(
                    "INSERT INTO migrations (id, applied_at) VALUES (?1, 0)",
                    [id],
                )
                .expect("record");
            }
            for _ in 0..5 {
                for batch in kit::workload() {
                    insert_old(&conn, batch);
                }
            }
            fill_004_projection(&conn);
            // Rows an older binary wrote after its projection, past the mark.
            insert_old(&conn, kit::workload().remove(0));
        }
        let (old_summary, old_turns, old_rows) =
            {
                let conn = Connection::open(&path).expect("open");
                (
                table_rows(&conn, "SELECT * FROM session_summary ORDER BY session_key"),
                table_rows(&conn, "SELECT * FROM session_turns ORDER BY session_key, turn"),
                table_rows(
                    &conn,
                    "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload \
                     FROM events ORDER BY global_seq",
                ),
            )
            };
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        let records = log.read_runtime_after(kit::all(), 0).await.expect("read");
        assert_eq!(records.len(), old_rows.len());
        for record in &records {
            assert_eq!(
                record.event.identity,
                loop_store::Identity::unrecorded(),
                "an old row reads as tenant local and the Runtime principal"
            );
        }
        assert_matches_fold(&log).await;
        let a = SessionKey::from("a");
        let summary = log
            .session_summary(&kit::local(), &a)
            .await
            .expect("summary")
            .expect("row");
        assert_eq!(summary.owner, None, "no owner of record before B3");
        let local_seq = summary.last_seq;
        // The same key in another tenant: its own sequence from 1.
        let mut update = kit::update("a");
        update.identity = kit::edge_user(kit::acme());
        let committed = log.append_batch(vec![update]).await.expect("append");
        assert_eq!(committed[0].session_seq, Some(1));
        assert_eq!(
            log.latest_seq(&kit::local(), &a).await.expect("latest"),
            local_seq
        );
        drop(log);
        let mut conn = Connection::open(&path).expect("open");
        // Stored rows are unchanged; new columns hold defaults or NULL.
        assert_eq!(
            table_rows(
                &conn,
                "SELECT global_seq,generation,session_key,session_seq,kind,turn,ts_ms,payload \
                 FROM events WHERE global_seq<=(SELECT MAX(global_seq)-1 FROM events) \
                 ORDER BY global_seq",
            ),
            old_rows
        );
        let unset: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE tenant='local' AND principal_kind IS NULL \
                 AND principal_issuer IS NULL AND principal_id IS NULL AND via IS NULL AND actor IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(unset as usize, old_rows.len());
        // The 004 tables are as the older binary left them.
        assert_eq!(
            table_rows(&conn, "SELECT * FROM session_summary ORDER BY session_key"),
            old_summary
        );
        assert_eq!(
            table_rows(
                &conn,
                "SELECT * FROM session_turns ORDER BY session_key, turn"
            ),
            old_turns
        );
        let indexes = table_rows(
            &conn,
            "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='events' ORDER BY name",
        );
        let names: Vec<String> = indexes
            .into_iter()
            .map(|row| match &row[0] {
                rusqlite::types::Value::Text(name) => name.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(names.contains(&"events_tenant_session_seq".to_owned()));
        assert!(!names.contains(&"events_session_seq".to_owned()));
        // The copied and caught-up projection equals a rebuild from events.
        let live = projection_rows_table(&conn);
        rebuild(&mut conn);
        assert_eq!(projection_rows_table(&conn), live);
    }

    /// An older binary (before step B3) still works on a database of this
    /// version, as far as it did: its migrations are all applied, its reads,
    /// appends and 004 projection statements run, it reads every row of
    /// this version, and its appends read back here as tenant `local`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_binary_works_on_a_new_database() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        for batch in kit::workload() {
            log.append_batch(batch).await.expect("append");
        }
        let mut acme = kit::update("only-acme");
        acme.identity = kit::edge_user(kit::acme());
        log.append_batch(vec![acme]).await.expect("append");
        drop(log);
        {
            let conn = Connection::open(&path).expect("open");
            // Its migrations 001 to 004 are recorded as applied.
            for (id, _) in &MIGRATIONS[..4] {
                let applied: i64 = conn
                    .query_row("SELECT COUNT(*) FROM migrations WHERE id=?1", [id], |r| {
                        r.get(0)
                    })
                    .expect("migration");
                assert_eq!(applied, 1, "{id}");
            }
            // Its reads: every row decodes; a Session page uses its query.
            let all: Vec<_> = conn
                .prepare(OLD_GLOBAL_PAGE_SQL)
                .expect("prepare")
                .query_map(params![0, i64::MAX, i64::MAX], old_decode)
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows");
            assert_eq!(all.len(), 23);
            let page: Vec<_> = conn
                .prepare(OLD_SESSION_PAGE_SQL)
                .expect("prepare")
                .query_map(params!["a", 0, i64::MAX, 100], old_decode)
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows");
            assert_eq!(
                page.iter()
                    .map(|(_, seq, _)| seq.expect("seq"))
                    .collect::<Vec<_>>(),
                (1..=page.len() as u64).collect::<Vec<_>>()
            );
            // Its projection catch-up and writes.
            let rows: i64 = conn
                .prepare(OLD_PROJECTION_ROWS_SQL)
                .expect("prepare")
                .query_map(params![0, i64::MAX, 10_000], |_| Ok(()))
                .expect("query")
                .count() as i64;
            assert_eq!(rows, 23);
            conn.execute(
                OLD_UPSERT_SUMMARY_SQL,
                params!["a", 9, 0, 1, 1, 1, None::<String>, false],
            )
            .expect("old upsert");
            conn.execute(OLD_INSERT_TURN_SQL, params!["a", "turn-old", 99])
                .expect("old turn");
        }
        // An append of the older binary, read by this one.
        {
            let conn = Connection::open(&path).expect("open");
            insert_old(&conn, vec![kit::update("a")]);
        }
        let log = SqliteEventLog::open(dir.path()).await.expect("open");
        let a = SessionKey::from("a");
        let records = log.read_after(&kit::local(), &a, 0).await.expect("read");
        let last = records.last().expect("records");
        assert_eq!(last.event.identity, loop_store::Identity::unrecorded());
        assert_eq!(
            records
                .iter()
                .map(|r| r.session_seq.expect("seq"))
                .collect::<Vec<_>>(),
            (1..=records.len() as u64).collect::<Vec<_>>(),
            "contiguous in tenant local"
        );
        assert_matches_fold(&log).await;
        assert_eq!(
            log.latest_seq(&kit::acme(), &"only-acme".into())
                .await
                .expect("latest"),
            1
        );
    }

    /// The cost of migration 005 on a large database of the code before
    /// step B3 whose 004 projection is current: the time of `migrate` and
    /// the file size before and after. `LOOP_B3_EVENTS` rows (1,000,000).
    #[test]
    #[ignore = "measurement; run by hand"]
    fn identity_migration_cost() {
        let total: u64 = std::env::var("LOOP_B3_EVENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000);
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("events.db");
        old_database_rows(&path, total);
        let mut conn = Connection::open(&path).expect("open");
        conn.execute_batch(MIGRATIONS[3].1).expect("004");
        conn.execute(
            "INSERT INTO migrations (id, applied_at) VALUES (?1, 0)",
            [MIGRATIONS[3].0],
        )
        .expect("record");
        fill_004_projection(&conn);
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
        let size = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT page_count*page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |r| r.get(0),
            )
            .expect("size")
        };
        let before = size(&conn);
        let start = std::time::Instant::now();
        migrate(&mut conn).expect("migrate");
        let migrated = start.elapsed();
        let start = std::time::Instant::now();
        assert!(backfill(&mut conn, BACKFILL_CHUNK, None).expect("backfill"));
        let caught_up = start.elapsed();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
        let after = size(&conn);
        println!(
            "B3MIGRATION| {total} events: migrate {:.2} s, backfill {:.3} s, \
             file {before} -> {after} bytes ({:+.1} bytes per event)",
            migrated.as_secs_f64(),
            caught_up.as_secs_f64(),
            (after - before) as f64 / total as f64
        );
    }
}
