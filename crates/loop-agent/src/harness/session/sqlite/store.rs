//! SQLite-backed session store (WAL, migrations, transactional append).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::harness::session::fork::{entries_for_fork_selection, SessionForkSelection};
use crate::harness::session::types::{
    create_session_id, PendingSessionWrite, SessionMetadata, SessionReader, SessionStore,
    SessionTreeEntry,
};
use crate::harness::types::SessionError;
use loop_ai::now_ms;

use super::branch_cache::{
    append_entry_to_branch_cache, is_valid_cached_path, read_cached_branch,
    read_cached_branch_rows, rebuild_cached_branch,
};
use super::materialize::{insert_empty_materialized, update_materialized_on_append};

const MIGRATION_001: &str = include_str!("migrations/001_initial.sql");
const MIGRATION_003: &str = include_str!("migrations/003_session_leases.sql");
const MIGRATION_002: &str = include_str!("migrations/002_branch_tips.sql");

struct Migration {
    id: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        id: "001_initial.sql",
        sql: MIGRATION_001,
    },
    Migration {
        id: "002_branch_tips.sql",
        sql: MIGRATION_002,
    },
    Migration {
        id: "003_session_leases.sql",
        sql: MIGRATION_003,
    },
    Migration {
        id: "004_session_control.sql",
        sql: include_str!("migrations/004_session_control.sql"),
    },
];

/// Open or create a SQLite session store at `path`.
pub fn create_sqlite_session_store(
    path: impl AsRef<Path>,
) -> Result<Arc<dyn SessionStore>, SessionError> {
    Ok(Arc::new(SqliteSessionStore::open(path)?))
}

impl SqliteSessionStore {
    /// Open a store using the system clock and conservative process-liveness probe.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        Self::with_environment(path, Arc::new(super::leases::SystemLeaseEnvironment))
    }

    /// Open with injectable time and process liveness for deterministic recovery tests.
    pub fn with_environment(
        path: impl AsRef<Path>,
        environment: Arc<dyn super::leases::LeaseEnvironment>,
    ) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SessionError::Io(e.to_string()))?;
        }
        let mut conn = open(&path)?;
        apply_migrations(&mut conn)?;
        let gate = WriteGate::for_path(&path);
        Ok(Self {
            path,
            environment,
            identity: None,
            resume_guard: None,
            gate,
        })
    }
}

/// How long a connection waits for another's write lock before SQLite
/// reports BUSY.
pub(super) const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Most time starting one write transaction may take, busy waits and
/// retries together.
/// Serializes this process's write transactions on one database file.
///
/// Many writers in one process (Runtime Sessions writing at once) would
/// otherwise all fight over SQLite's file lock, which on Windows is slow
/// and unfair. Reads never take it.
pub(super) struct WriteGate {
    lock: Arc<tokio::sync::Mutex<()>>,
    busy_ms: std::sync::atomic::AtomicU64,
    busy_seen: std::sync::atomic::AtomicUsize,
}

impl WriteGate {
    /// The gate of the database at `path`, shared by every store on it.
    pub(super) fn for_path(path: &Path) -> Arc<Self> {
        static GATES: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<WriteGate>>>,
        > = std::sync::OnceLock::new();
        let key = std::fs::canonicalize(path)
            .or_else(|_| std::path::absolute(path))
            .unwrap_or_else(|_| path.to_path_buf());
        let mut gates = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&key).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(WriteGate {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            busy_ms: std::sync::atomic::AtomicU64::new(BUSY_TIMEOUT.as_millis() as u64),
            busy_seen: std::sync::atomic::AtomicUsize::new(0),
        });
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }

    /// Wait for this process's turn to write.
    pub(super) async fn enter(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.lock).lock_owned().await
    }

    /// [`Self::enter`], giving up after `limit`.
    pub(super) async fn enter_within(
        &self,
        limit: std::time::Duration,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, SessionError> {
        tokio::time::timeout(limit, self.enter())
            .await
            .map_err(|_| {
                SessionError::Storage(
                    "database is locked: another write in this process is still running".into(),
                )
            })
    }

    /// How long a write waits for another process's lock before BUSY.
    pub(super) fn busy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.busy_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    #[cfg(test)]
    pub(super) fn set_busy_timeout(&self, timeout: std::time::Duration) {
        self.busy_ms.store(
            timeout.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn saw_busy(&self) {
        self.busy_seen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many write starts met BUSY.
    #[cfg(test)]
    pub(super) fn busy_seen(&self) -> usize {
        self.busy_seen.load(std::sync::atomic::Ordering::Relaxed)
    }
}

const BEGIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(6);
/// Most attempts at starting one write transaction.
const BEGIN_ATTEMPTS: u32 = 5;

/// Whether SQLite refused for another connection's lock.
pub(super) fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _)
            if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Start an IMMEDIATE (write) transaction, waiting at most about 6 s in
/// all for another connection's lock. Under heavy concurrent writing (a
/// slow disk) SQLite can report BUSY before that; it is then tried again
/// after a short, jittered pause (20–200 ms), within the same deadline.
///
/// Callers in this process hold the database's [`WriteGate`] around the
/// whole transaction, so the busy wait only meets other processes.
pub(super) fn begin_immediate<'c>(
    conn: &'c Connection,
    gate: &WriteGate,
) -> rusqlite::Result<Transaction<'c>> {
    begin_immediate_within(conn, BEGIN_DEADLINE, gate)
}

/// [`begin_immediate`] with an explicit overall deadline.
pub(super) fn begin_immediate_within<'c>(
    conn: &'c Connection,
    limit: std::time::Duration,
    gate: &WriteGate,
) -> rusqlite::Result<Transaction<'c>> {
    let deadline = std::time::Instant::now() + limit;
    let left = || deadline.saturating_duration_since(std::time::Instant::now());
    let mut attempt = 1;
    let result = loop {
        // Every wait, SQLite's busy wait and our pause alike, is capped to
        // what is left of the deadline, so it is overshot by at most the
        // scheduler's slack.
        conn.busy_timeout(left().min(gate.busy_timeout()))?;
        let begun = Transaction::new_unchecked(conn, TransactionBehavior::Immediate);
        if begun.as_ref().err().is_some_and(is_busy) {
            gate.saw_busy();
        }
        match begun {
            Err(error) if is_busy(&error) && attempt < BEGIN_ATTEMPTS && !left().is_zero() => {
                let base = (20u64 << (attempt - 1)).min(200);
                let jitter = u64::from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos(),
                ) % (base / 2 + 1);
                let pause = std::time::Duration::from_millis((base / 2 + jitter).clamp(20, 200));
                std::thread::sleep(pause.min(left()));
                attempt += 1;
            }
            result => break result,
        }
    };
    conn.busy_timeout(gate.busy_timeout())?;
    result
}

pub(super) fn open(path: &Path) -> Result<Connection, SessionError> {
    let conn = Connection::open(path).map_err(|e| SessionError::Storage(e.to_string()))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    Ok(conn)
}

fn migration_applied(conn: &Connection, id: &str) -> Result<bool, SessionError> {
    conn.query_row(
        "SELECT 1 FROM migrations WHERE id = ?1",
        params![id],
        |_| Ok(true),
    )
    .optional()
    .map(|opt| opt.is_some())
    .map_err(|e| SessionError::Storage(e.to_string()))
}

fn apply_migrations(conn: &mut Connection) -> Result<(), SessionError> {
    // Switching a fresh file to WAL reports BUSY at once, without the busy
    // timeout, while another process opens it too. Try again for a while.
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;") {
            Ok(()) => break,
            Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(SessionError::Storage(e.to_string())),
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    for migration in MIGRATIONS {
        if migration_applied(conn, migration.id)? {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        let result = (|| {
            // Another process may have applied it since the check above.
            if migration_applied(conn, migration.id)? {
                return Ok(());
            }
            conn.execute_batch(migration.sql)
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            conn.execute(
                "INSERT INTO migrations (id, applied_at) VALUES (?1, ?2)",
                params![migration.id, now_ms()],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
            Ok::<(), SessionError>(())
        })();
        if let Err(error) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error);
        }
        conn.execute_batch("COMMIT")
            .map_err(|e| SessionError::Storage(e.to_string()))?;
    }
    Ok(())
}

/// SQLite Session store with optional lease fencing for writers.
#[derive(Clone)]
pub struct SqliteSessionStore {
    pub(super) path: PathBuf,
    pub(super) environment: Arc<dyn super::leases::LeaseEnvironment>,
    pub(super) identity: Option<super::leases::LeaseIdentity>,
    pub(super) resume_guard: Option<Arc<super::leases::ResumeLeaseGuard>>,
    /// This process's write gate for the database file.
    pub(super) gate: Arc<WriteGate>,
}

struct SqliteReader {
    meta: SessionMetadata,
    path: PathBuf,
}

fn pending_to_entry(
    pending: PendingSessionWrite,
    id: String,
    parent_id: Option<String>,
    timestamp: i64,
) -> SessionTreeEntry {
    match pending {
        PendingSessionWrite::Message { message } => SessionTreeEntry::Message {
            id,
            parent_id,
            timestamp,
            message,
        },
        PendingSessionWrite::ThinkingLevelChange { thinking_level } => {
            SessionTreeEntry::ThinkingLevelChange {
                id,
                parent_id,
                timestamp,
                thinking_level,
            }
        }
        PendingSessionWrite::ModelChange { provider, model_id } => SessionTreeEntry::ModelChange {
            id,
            parent_id,
            timestamp,
            provider,
            model_id,
        },
        PendingSessionWrite::ActiveToolsChange { tool_names } => {
            SessionTreeEntry::ActiveToolsChange {
                id,
                parent_id,
                timestamp,
                tool_names,
            }
        }
        PendingSessionWrite::Compaction {
            summary,
            first_kept_entry_id,
            details,
        } => SessionTreeEntry::Compaction {
            id,
            parent_id,
            timestamp,
            summary,
            first_kept_entry_id,
            details,
        },
        PendingSessionWrite::BranchSummary { summary } => SessionTreeEntry::BranchSummary {
            id,
            parent_id,
            timestamp,
            summary,
        },
        PendingSessionWrite::Leaf { target_id } => SessionTreeEntry::Leaf {
            id,
            parent_id,
            timestamp,
            target_id,
        },
        PendingSessionWrite::Label { label } => SessionTreeEntry::Label {
            id,
            parent_id,
            timestamp,
            label,
        },
        PendingSessionWrite::SessionInfo { name } => SessionTreeEntry::SessionInfo {
            id,
            parent_id,
            timestamp,
            name,
        },
        PendingSessionWrite::Custom { custom_type, data } => SessionTreeEntry::Custom {
            id,
            parent_id,
            timestamp,
            custom_type,
            data,
        },
    }
}

fn load_all(conn: &Connection, sid: &str) -> Result<Vec<SessionTreeEntry>, SessionError> {
    let mut stmt = conn
        .prepare("SELECT payload FROM session_entries WHERE session_id=?1 ORDER BY entry_seq")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let rows = stmt
        .query_map(params![sid], |row| row.get::<_, String>(0))
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let mut out = Vec::new();
    for row in rows {
        let payload = row.map_err(|e| SessionError::Storage(e.to_string()))?;
        out.push(serde_json::from_str(&payload).map_err(|e| SessionError::Storage(e.to_string()))?);
    }
    Ok(out)
}

fn trim_path_to_root_or_compaction(entries: &[SessionTreeEntry]) -> Vec<SessionTreeEntry> {
    let mut path = Vec::new();
    let mut iter = entries.iter().rev();
    while let Some(entry) = iter.next() {
        path.push(entry.clone());
        if let SessionTreeEntry::Compaction {
            first_kept_entry_id,
            ..
        } = entry
        {
            // Include the retained tail (compaction parent back to the first
            // kept entry) so recent context survives.
            if let Some(first_kept) = first_kept_entry_id {
                for kept_entry in iter.by_ref() {
                    path.push(kept_entry.clone());
                    if kept_entry.id() == first_kept {
                        break;
                    }
                }
            }
            break;
        }
    }
    path.reverse();
    path
}

fn read_canonical_path_to_root(
    conn: &Connection,
    session_id: &str,
    leaf_id: &str,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    let mut path = Vec::new();
    let mut current = Some(leaf_id.to_string());
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id.clone()) {
            return Err(SessionError::Invalid(format!("cycle at entry {id}")));
        }
        let payload: String = conn
            .query_row(
                "SELECT payload FROM session_entries WHERE session_id=?1 AND entry_id=?2",
                params![session_id, id],
                |row| row.get(0),
            )
            .map_err(|_| SessionError::NotFound(id.clone()))?;
        let entry: SessionTreeEntry =
            serde_json::from_str(&payload).map_err(|e| SessionError::Storage(e.to_string()))?;
        current = entry.parent_id().map(|s| s.to_string());
        path.push(entry);
    }
    path.reverse();
    Ok(path)
}

fn read_path_to_root_or_compaction_conn(
    conn: &mut Connection,
    session_id: &str,
    leaf_id: Option<&str>,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    let leaf = match leaf_id {
        Some(l) => Some(l.to_string()),
        None => conn
            .query_row(
                "SELECT active_leaf_id FROM sessions WHERE id=?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?,
    };
    let Some(leaf) = leaf else {
        return Ok(vec![]);
    };

    let cached = read_cached_branch(conn, session_id, &leaf)?;
    if let Some(ref branch) = cached {
        let entries = read_cached_branch_rows(conn, session_id, branch, 0)?;
        if is_valid_cached_path(&entries, &leaf) {
            return Ok(trim_path_to_root_or_compaction(&entries));
        }
    }

    let entries = read_canonical_path_to_root(conn, session_id, &leaf)?;
    conn.execute_batch("SAVEPOINT rebuild_branch_cache_read")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let rebuild = rebuild_cached_branch(
        conn,
        session_id,
        &leaf,
        cached.as_ref().map(|b| b.branch_id.as_str()),
    );
    if rebuild.is_err() {
        let _ = conn.execute_batch(
            "ROLLBACK TO SAVEPOINT rebuild_branch_cache_read; RELEASE SAVEPOINT rebuild_branch_cache_read;",
        );
        return rebuild.map(|_| trim_path_to_root_or_compaction(&entries));
    }
    conn.execute_batch("RELEASE SAVEPOINT rebuild_branch_cache_read")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    Ok(trim_path_to_root_or_compaction(&entries))
}

fn append_entry_tx(
    tx: &Transaction<'_>,
    session_id: &str,
    pending: PendingSessionWrite,
) -> Result<SessionTreeEntry, SessionError> {
    let leaf: Option<String> = tx
        .query_row(
            "SELECT active_leaf_id FROM sessions WHERE id=?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|_| SessionError::NotFound(session_id.to_string()))?;
    let seq: i64 = tx
        .query_row(
            "SELECT next_seq FROM session_sequences WHERE session_id=?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let id = loop_ai::new_id();
    let ts = now_ms();
    let entry = pending_to_entry(pending, id.clone(), leaf.clone(), ts);
    let new_leaf = if let SessionTreeEntry::Leaf { target_id, .. } = &entry {
        target_id.clone()
    } else {
        Some(entry.id().to_string())
    };
    let payload =
        serde_json::to_string(&entry).map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
                "INSERT INTO session_entries(session_id, entry_id, parent_id, entry_seq, payload) VALUES (?1,?2,?3,?4,?5)",
                params![session_id, entry.id(), entry.parent_id(), seq, payload],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "UPDATE session_sequences SET next_seq=?1 WHERE session_id=?2",
        params![seq + 1, session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "UPDATE sessions SET active_leaf_id=?1 WHERE id=?2",
        params![new_leaf, session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    append_entry_to_branch_cache(tx, session_id, entry.id(), seq, entry.parent_id())?;
    update_materialized_on_append(tx, session_id, seq, &entry)?;
    Ok(entry)
}

fn delete_session_rows(tx: &Transaction<'_>, session_id: &str) -> Result<(), SessionError> {
    tx.execute(
        "DELETE FROM entry_materialized WHERE session_id=?1",
        params![session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "DELETE FROM session_materialized WHERE session_id=?1",
        params![session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "DELETE FROM branch_entries WHERE session_id=?1",
        params![session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "DELETE FROM branch_tips WHERE session_id=?1",
        params![session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "DELETE FROM session_entries WHERE session_id=?1",
        params![session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute(
        "DELETE FROM session_sequences WHERE session_id=?1",
        params![session_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    tx.execute("DELETE FROM sessions WHERE id=?1", params![session_id])
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    Ok(())
}

#[async_trait]
impl SessionReader for SqliteReader {
    fn metadata(&self) -> &SessionMetadata {
        &self.meta
    }

    async fn read_head(&self) -> Result<Option<String>, SessionError> {
        let path = self.path.clone();
        let id = self.meta.id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            conn.query_row(
                "SELECT active_leaf_id FROM sessions WHERE id=?1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| SessionError::Storage(e.to_string()))
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn read_entry(&self, entry_id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
        let path = self.path.clone();
        let sid = self.meta.id.clone();
        let eid = entry_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            let payload: Option<String> = conn
                .query_row(
                    "SELECT payload FROM session_entries WHERE session_id=?1 AND entry_id=?2",
                    params![sid, eid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            payload
                .map(|p| serde_json::from_str(&p).map_err(|e| SessionError::Storage(e.to_string())))
                .transpose()
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn read_entries(
        &self,
        _after_seq: Option<u64>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let path = self.path.clone();
        let sid = self.meta.id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            load_all(&conn, &sid)
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn read_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let path = self.path.clone();
        let sid = self.meta.id.clone();
        let leaf = leaf_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let mut conn = open(&path)?;
            read_path_to_root_or_compaction_conn(&mut conn, &sid, leaf.as_deref())
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }
}

impl SqliteSessionStore {
    /// Create a Session whose parent is recorded in the same transaction, as
    /// for a Subagent. Unlike `fork`, no parent entries are copied.
    pub async fn create_child(
        &self,
        cwd: Option<String>,
        name: Option<String>,
        parent: &str,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.create_row(cwd, name, Some(parent.to_owned())).await
    }

    async fn create_row(
        &self,
        cwd: Option<String>,
        name: Option<String>,
        parent: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let path = self.path.clone();
        let id = create_session_id();
        let created_at = now_ms();
        let meta = SessionMetadata {
            id: id.clone(),
            cwd: cwd.clone(),
            name: name.clone(),
            parent_session_id: parent.clone(),
            created_at,
            path: None,
        };
        let guard = self.resume_guard.clone();
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        tokio::task::spawn_blocking({
            let path = path.clone();
            let id = id.clone();
            move || {
                let _permit = permit;
                let conn = open(&path)?;
                let tx = begin_immediate(&conn, &gate)
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                tx.execute(
                    "INSERT INTO sessions(id, cwd, name, parent_session_id, active_leaf_id, created_at) VALUES (?1,?2,?3,?4,NULL,?5)",
                    params![id, cwd, name, parent, created_at],
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
                tx.execute(
                    "INSERT INTO session_sequences(session_id, next_seq) VALUES (?1, 1)",
                    params![id],
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
                insert_empty_materialized(&tx, &id)?;
                if let Some(guard) = &guard { guard.claim_created(&tx, &id)?; }
                tx.commit()
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                if let Some(guard) = &guard { guard.track(&id); }
                Ok::<(), SessionError>(())
            }
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))??;
        Ok(Arc::new(SqliteReader { meta, path }))
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    fn write_cancellation(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.resume_guard
            .as_ref()
            .map(|guard| guard.cancellation.clone())
    }

    async fn release_leases(&self) -> Result<(), SessionError> {
        match &self.resume_guard {
            Some(guard) => guard.release().await,
            None => Ok(()),
        }
    }

    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.create_row(cwd, name, None).await
    }

    async fn open_writer(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        if let Some(guard) = &self.resume_guard {
            if guard.failed() {
                return Err(guard.refusal());
            }
            self.takeover_lease(
                id,
                self.identity.as_ref().expect("standalone identity"),
                false,
            )
            .await?;
            guard.track(id);
        }
        self.load(id).await
    }

    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        let path = self.path.clone();
        let id = id.to_string();
        let meta = tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            conn.query_row(
                "SELECT id, cwd, name, parent_session_id, created_at FROM sessions WHERE id=?1",
                params![id],
                |row| {
                    Ok(SessionMetadata {
                        id: row.get(0)?,
                        cwd: row.get(1)?,
                        name: row.get(2)?,
                        parent_session_id: row.get(3)?,
                        created_at: row.get(4)?,
                        path: None,
                    })
                },
            )
            .map_err(|_| SessionError::NotFound(id))
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))??;
        Ok(Arc::new(SqliteReader {
            meta,
            path: self.path.clone(),
        }))
    }

    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        let path = self.path.clone();
        let cwd = cwd.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            let mut out = Vec::new();
            if let Some(cwd) = cwd {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, cwd, name, parent_session_id, created_at FROM sessions WHERE cwd=?1 ORDER BY created_at",
                    )
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map(params![cwd], |row| {
                        Ok(SessionMetadata {
                            id: row.get(0)?,
                            cwd: row.get(1)?,
                            name: row.get(2)?,
                            parent_session_id: row.get(3)?,
                            created_at: row.get(4)?,
                            path: None,
                        })
                    })
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                for row in rows {
                    out.push(row.map_err(|e| SessionError::Storage(e.to_string()))?);
                }
            } else {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, cwd, name, parent_session_id, created_at FROM sessions ORDER BY created_at",
                    )
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(SessionMetadata {
                            id: row.get(0)?,
                            cwd: row.get(1)?,
                            name: row.get(2)?,
                            parent_session_id: row.get(3)?,
                            created_at: row.get(4)?,
                            path: None,
                        })
                    })
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                for row in rows {
                    out.push(row.map_err(|e| SessionError::Storage(e.to_string()))?);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        let path = self.path.clone();
        let session_id = session_id.to_string();
        let identity = self.identity.clone();
        let guard = self.resume_guard.clone();
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx =
                begin_immediate(&conn, &gate).map_err(|e| SessionError::Storage(e.to_string()))?;
            if let Some(guard) = guard.as_ref().filter(|guard| guard.failed()) {
                return Err(guard.refusal());
            }
            super::leases::check_write(&tx, &session_id, identity.as_ref())?;
            let entry = append_entry_tx(&tx, &session_id, pending)?;
            tx.commit()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            Ok(entry)
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        let path = self.path.clone();
        let id = id.to_string();
        let identity = self.identity.clone();
        let guard = self.resume_guard.clone();
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx =
                begin_immediate(&conn, &gate).map_err(|e| SessionError::Storage(e.to_string()))?;
            if let Some(guard) = guard.as_ref().filter(|guard| guard.failed()) {
                return Err(guard.refusal());
            }
            super::leases::check_write(&tx, &id, identity.as_ref())?;
            delete_session_rows(&tx, &id)?;
            tx.commit()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            if let Some(guard) = &guard {
                guard.forget(&id);
            }
            Ok(())
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn create_surface_destination(
        &self,
        file: super::super::SessionFile,
        cwd: String,
        parent: Option<String>,
        request_key: String,
        fingerprint: String,
        receipt: String,
    ) -> Result<String, SessionError> {
        let guard = self.resume_guard.as_ref().ok_or_else(|| {
            SessionError::Invalid("Surface destinations require a standalone writer".into())
        })?;
        if guard.failed() {
            return Err(guard.refusal());
        }
        let identity = self.identity.clone().ok_or_else(|| {
            SessionError::Invalid("Surface destinations require a writer identity".into())
        })?;
        let mut created = self
            .create_destination(
                file,
                cwd,
                parent,
                request_key,
                fingerprint,
                receipt,
                identity,
            )
            .await?;
        if guard.failed() {
            created.rollback().await?;
            return Err(guard.refusal());
        }
        guard.track(&created.id);
        created.admitted();
        Ok(created.id.clone())
    }

    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let path = self.path.clone();
        let source_id = source_id.to_string();
        let through_entry_id = through_entry_id.map(str::to_owned);
        let guard = self.resume_guard.clone();
        let gate = Arc::clone(&self.gate);
        // One writer of this process at a time; other processes meet
        // SQLite's busy timeout and the bounded retry.
        let permit = gate.enter().await;
        let new_id = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx = begin_immediate(&conn, &gate)
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            let (cwd, leaf): (Option<String>, Option<String>) = tx.query_row(
                "SELECT cwd, active_leaf_id FROM sessions WHERE id=?1", [&source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(|e| SessionError::Storage(e.to_string()))?
                .ok_or_else(|| SessionError::NotFound(source_id.clone()))?;
            let entries = load_all(&tx, &source_id)?;
            let selected = entries_for_fork_selection(&entries, leaf.as_deref(), selection, through_entry_id.as_deref())?;
            let new_id = create_session_id();
            tx.execute(
                "INSERT INTO sessions(id, cwd, name, parent_session_id, active_leaf_id, created_at) VALUES (?1,?2,?3,?4,NULL,?5)",
                params![new_id, cwd, name, source_id, now_ms()],
            ).map_err(|e| SessionError::Storage(e.to_string()))?;
            tx.execute("INSERT INTO session_sequences(session_id, next_seq) VALUES (?1, 1)", [&new_id])
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            insert_empty_materialized(&tx, &new_id)?;
            for entry in selected {
            let pending = match entry {
                SessionTreeEntry::Message { message, .. } => {
                    PendingSessionWrite::Message { message }
                }
                SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                    PendingSessionWrite::ThinkingLevelChange { thinking_level }
                }
                SessionTreeEntry::ModelChange {
                    provider, model_id, ..
                } => PendingSessionWrite::ModelChange { provider, model_id },
                SessionTreeEntry::ActiveToolsChange { tool_names, .. } => {
                    PendingSessionWrite::ActiveToolsChange { tool_names }
                }
                SessionTreeEntry::Compaction {
                    summary,
                    first_kept_entry_id,
                    details,
                    ..
                } => PendingSessionWrite::Compaction {
                    summary,
                    first_kept_entry_id,
                    details,
                },
                SessionTreeEntry::Label { label, .. } => PendingSessionWrite::Label { label },
                SessionTreeEntry::SessionInfo { name, .. } => {
                    PendingSessionWrite::SessionInfo { name }
                }
                SessionTreeEntry::Leaf { target_id, .. } => PendingSessionWrite::Leaf { target_id },
                SessionTreeEntry::BranchSummary { summary, .. } => {
                    PendingSessionWrite::BranchSummary { summary }
                }
                SessionTreeEntry::Custom { .. } => continue,
            };
                append_entry_tx(&tx, &new_id, pending)?;
            }
            if let Some(guard) = &guard { guard.claim_created(&tx, &new_id)?; }
            tx.commit().map_err(|e| SessionError::Storage(e.to_string()))?;
            if let Some(guard) = &guard { guard.track(&new_id); }
            Ok::<_, SessionError>(new_id)
        }).await.map_err(|e| SessionError::Storage(e.to_string()))??;
        self.load(&new_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hold the write lock on another connection for `hold`.
    fn hold_write_lock(path: &Path, hold: std::time::Duration) -> std::thread::JoinHandle<()> {
        let holder = Connection::open(path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(hold);
            holder.execute_batch("COMMIT").unwrap();
        })
    }

    #[test]
    fn a_busy_write_transaction_is_retried_until_the_lock_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        SqliteSessionStore::open(&path).unwrap();
        // No busy timeout: SQLite reports BUSY at once, so only the retry waits.
        let conn = Connection::open(&path).unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let held = hold_write_lock(&path, std::time::Duration::from_millis(100));
        let plain = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate);
        assert!(
            matches!(&plain, Err(e) if is_busy(e)),
            "the lock is held: {plain:?}"
        );
        drop(plain);
        let gate = WriteGate::for_path(&path);
        let tx = begin_immediate(&conn, &gate).unwrap();
        tx.execute("UPDATE session_sequences SET next_seq = next_seq", [])
            .unwrap();
        tx.commit().unwrap();
        held.join().unwrap();
    }

    #[test]
    fn a_write_that_stays_busy_gives_up_at_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        SqliteSessionStore::open(&path).unwrap();
        let conn = open(&path).unwrap();
        let held = hold_write_lock(&path, std::time::Duration::from_secs(6));
        let started = std::time::Instant::now();
        let result = begin_immediate_within(
            &conn,
            std::time::Duration::from_millis(400),
            &WriteGate::for_path(&path),
        );
        let took = started.elapsed();
        assert!(matches!(&result, Err(e) if is_busy(e)), "{result:?}");
        // The busy waits and the retries share one deadline: it is used up,
        // and the 5 s busy timeout is not waited out. The upper bound sits
        // well between the two, with room for a loaded machine.
        let deadline = std::time::Duration::from_millis(400);
        assert!(took >= deadline, "gave up early, after {took:?}");
        assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
        drop(result);
        held.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_writers_in_one_process_never_see_busy() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("sessions.db");
            let store = Arc::new(SqliteSessionStore::open(&path).unwrap());
            // No busy waiting at all: any contention would surface as BUSY.
            store.gate.set_busy_timeout(std::time::Duration::ZERO);
            let id = store
                .create(None, None)
                .await
                .unwrap()
                .metadata()
                .id
                .clone();
            let mut writers = tokio::task::JoinSet::new();
            for n in 0..64 {
                let (store, id) = (store.clone(), id.clone());
                writers.spawn(async move {
                    match n % 4 {
                        0 => store.create(None, None).await.map(|_| ()),
                        _ => store
                            .append_entry(
                                &id,
                                PendingSessionWrite::Label {
                                    label: format!("w{n}"),
                                },
                            )
                            .await
                            .map(|_| ()),
                    }
                });
            }
            while let Some(done) = writers.join_next().await {
                done.unwrap().unwrap();
            }
            assert_eq!(
                store.gate.busy_seen(),
                0,
                "a writer in this process met BUSY"
            );
            // Reads do not wait for the gate.
            let held = store.gate.enter().await;
            tokio::time::timeout(std::time::Duration::from_secs(20), store.list(None))
                .await
                .expect("a read waited for the write gate")
                .unwrap();
            drop(held);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn release_waits_only_briefly_for_a_write_in_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let base = SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap();
        let writer = base.standalone_writer();
        let id = writer
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let held = base.gate.enter().await;
        let started = std::time::Instant::now();
        let error = writer.release_leases().await.unwrap_err().to_string();
        assert!(
            // 200 ms for the gate; the bound only needs to stay below the
            // 5 s busy timeout, with room for a loaded machine.
            started.elapsed() < std::time::Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(error.contains("locked"), "{error}");
        drop(held);
        writer.release_leases().await.unwrap();
        assert!(base.lease(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_append_waits_out_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let id = store
            .create(None, None)
            .await
            .unwrap()
            .metadata()
            .id
            .clone();
        let held = hold_write_lock(&path, std::time::Duration::from_millis(300));
        store
            .append_entry(
                &id,
                PendingSessionWrite::Label {
                    label: "after the lock".into(),
                },
            )
            .await
            .unwrap();
        held.join().unwrap();
    }

    #[test]
    fn migrations_apply_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migrations.sqlite");
        let mut conn = open(&path).unwrap();
        apply_migrations(&mut conn).unwrap();
        assert!(migration_applied(&conn, "001_initial.sql").unwrap());
        assert!(migration_applied(&conn, "002_branch_tips.sql").unwrap());
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='branch_tips'",
            [],
            |_| Ok(()),
        )
        .expect("branch_tips table exists");
    }
}

/// Owns an unadmitted destination; cancellation removes its row and lease.
pub struct DestinationCreation {
    /// The destination Session id.
    pub id: String,
    /// The retry receipt committed with it.
    pub receipt: String,
    store: SqliteSessionStore,
    identity: super::leases::LeaseIdentity,
    armed: bool,
}
impl DestinationCreation {
    /// Transfer responsibility to the admitted Runtime Session.
    pub fn admitted(&mut self) {
        self.armed = false;
    }
    /// Remove an unadmitted destination, whether admission released its lease or not.
    pub async fn rollback(&mut self) -> Result<(), SessionError> {
        if !self.armed {
            return Ok(());
        }
        rollback_destination(&self.store, &self.id, &self.identity).await?;
        self.armed = false;
        Ok(())
    }
}
async fn rollback_destination(
    store: &SqliteSessionStore,
    id: &str,
    identity: &super::leases::LeaseIdentity,
) -> Result<(), SessionError> {
    match store.lease(id).await? {
        Some(lease) if &lease.identity == identity => store.release_lease(id, identity).await?,
        Some(_) => {
            return Err(SessionError::Invalid(
                "destination has another writer; refusing rollback".into(),
            ))
        }
        None => {}
    }
    store.delete(id).await
}
impl Drop for DestinationCreation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let store = self.store.clone();
        let identity = self.identity.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            if let Err(error) = rollback_destination(&store, &id, &identity).await {
                eprintln!("could not remove unadmitted Session {id}: {error}");
            }
        });
    }
}

/// Persisted branch revision and an unannounced mutation. The Session
/// revision is counted from the tree itself.
#[derive(Debug, Clone, Default)]
pub struct SessionControlState {
    /// Committed active-branch revision.
    pub branch_revision: u64,
    /// Marker of a committed mutation not yet announced in the Event Log.
    pub pending: Option<String>,
}

impl SqliteSessionStore {
    /// Read revisions without acquiring a writer lease.
    pub async fn control_state(&self, id: &str) -> Result<SessionControlState, SessionError> {
        let path = self.path.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            open(&path)?
                .query_row(
                    "SELECT branch_revision, pending FROM session_control WHERE session_id=?1",
                    [&id],
                    |row| {
                        Ok(SessionControlState {
                            branch_revision: row.get(0)?,
                            pending: row.get(1)?,
                        })
                    },
                )
                .optional()
                .map(|state| state.unwrap_or_default())
                .map_err(|e| SessionError::Storage(e.to_string()))
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    /// Commit a tree/metadata write and its recovery marker in one transaction.
    /// The caller has checked the Session revision under its own lock.
    pub async fn control_mutation(
        &self,
        id: &str,
        identity: super::leases::LeaseIdentity,
        branch: bool,
        write: PendingSessionWrite,
        marker: String,
    ) -> Result<SessionControlState, SessionError> {
        let path = self.path.clone();
        let id = id.to_owned();
        let gate = self.gate.clone();
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx =
                begin_immediate(&conn, &gate).map_err(|e| SessionError::Storage(e.to_string()))?;
            super::leases::check_write(&tx, &id, Some(&identity))?;
            tx.execute(
                "INSERT OR IGNORE INTO session_control(session_id) VALUES (?1)",
                [&id],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
            let (branch_revision, pending): (u64, Option<String>) = tx
                .query_row(
                    "SELECT branch_revision, pending FROM session_control WHERE session_id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            if pending.is_some() {
                return Err(SessionError::Invalid(
                    "Session has an unannounced mutation; reopen it".into(),
                ));
            }
            if let PendingSessionWrite::Leaf {
                target_id: Some(target),
            } = &write
            {
                if !load_all(&tx, &id)?.iter().any(|entry| {
                    entry.id() == target && !matches!(entry, SessionTreeEntry::Leaf { .. })
                }) {
                    return Err(SessionError::Invalid("unknown branch target".into()));
                }
            }
            let marker_value: serde_json::Value = serde_json::from_str(&marker)
                .map_err(|error| SessionError::Invalid(error.to_string()))?;
            if let Some(summary) = marker_value["summary"].as_str() {
                append_entry_tx(
                    &tx,
                    &id,
                    PendingSessionWrite::BranchSummary {
                        summary: summary.into(),
                    },
                )?;
            }
            append_entry_tx(&tx, &id, write)?;
            let branch_revision = branch_revision + u64::from(branch);
            tx.execute(
                "UPDATE session_control SET branch_revision=?2, pending=?3 WHERE session_id=?1",
                params![id, branch_revision, marker],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
            tx.commit()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            Ok(SessionControlState {
                branch_revision,
                pending: Some(marker),
            })
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    /// Clear only the marker that was durably announced in the Event Log.
    pub async fn acknowledge_control(&self, id: &str, marker: &str) -> Result<(), SessionError> {
        let path = self.path.clone();
        let id = id.to_owned();
        let marker = marker.to_owned();
        tokio::task::spawn_blocking(move || {
            open(&path)?
                .execute(
                    "UPDATE session_control SET pending=NULL WHERE session_id=?1 AND pending=?2",
                    params![id, marker],
                )
                .map(|_| ())
                .map_err(|e| SessionError::Storage(e.to_string()))
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    /// Read a destination retry receipt without changing ownership or capacity.
    pub async fn destination_receipt(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<(String, String)>, SessionError> {
        let path = self.path.clone();
        let key = key.to_owned();
        let fingerprint = fingerprint.to_owned();
        tokio::task::spawn_blocking(move || {
            let previous: Option<(String,String,String)> = open(&path)?.query_row(
                "SELECT fingerprint,session_id,result FROM session_destinations WHERE request_key=?1", [&key],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional().map_err(|error| SessionError::Storage(error.to_string()))?;
            previous.map(|(old,id,result)| {
                if old != fingerprint { Err(SessionError::Invalid("request id already used with different parameters".into())) }
                else { Ok((id,result)) }
            }).transpose()
        }).await.map_err(|error| SessionError::Storage(error.to_string()))?
    }

    /// Atomically create a validated branch and its retry receipt, with its own lease.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_destination(
        &self,
        file: super::super::SessionFile,
        cwd: String,
        parent: Option<String>,
        request_key: String,
        fingerprint: String,
        result: String,
        identity: super::leases::LeaseIdentity,
    ) -> Result<DestinationCreation, SessionError> {
        let store = self.clone();
        let path = self.path.clone();
        let gate = self.gate.clone();
        let permit = gate.enter().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = open(&path)?;
            let tx = begin_immediate(&conn, &gate).map_err(|e| SessionError::Storage(e.to_string()))?;
            let previous: Option<(String,String,String)> = tx.query_row(
                "SELECT fingerprint,session_id,result FROM session_destinations WHERE request_key=?1", [&request_key],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
            ).optional().map_err(|e| SessionError::Storage(e.to_string()))?;
            if let Some((old,id,result)) = previous {
                if old != fingerprint { return Err(SessionError::Invalid("request id already used with different parameters".into())); }
                return Ok(DestinationCreation { id,receipt:result,store,identity,armed:false });
            }
            let id = create_session_id();
            tx.execute("INSERT INTO sessions(id,cwd,name,parent_session_id,active_leaf_id,created_at) VALUES (?1,?2,?3,?4,NULL,?5)",
                params![id,cwd,file.name,parent,now_ms()]).map_err(|e| SessionError::Storage(e.to_string()))?;
            tx.execute("INSERT INTO session_sequences(session_id,next_seq) VALUES (?1,1)",[&id]).map_err(|e| SessionError::Storage(e.to_string()))?;
            insert_empty_materialized(&tx, &id)?;
            let mut remap = std::collections::HashMap::new();
            for entry in file.entries {
                let old_id = entry.id().to_owned();
                let pending = match entry {
                    SessionTreeEntry::Message { message, .. } => PendingSessionWrite::Message { message },
                    SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => PendingSessionWrite::ThinkingLevelChange { thinking_level },
                    SessionTreeEntry::ModelChange { provider, model_id, .. } => PendingSessionWrite::ModelChange { provider, model_id },
                    SessionTreeEntry::ActiveToolsChange { tool_names, .. } => PendingSessionWrite::ActiveToolsChange { tool_names },
                    // Approval grants belong to the source Session, never to a copy.
                    SessionTreeEntry::Label { label, .. }
                        if label.starts_with(super::super::TOOL_APPROVAL_LABEL_NAMESPACE) => continue,
                    SessionTreeEntry::Label { label, .. } => PendingSessionWrite::Label { label },
                    SessionTreeEntry::SessionInfo { .. } => continue,
                    SessionTreeEntry::Compaction { summary, first_kept_entry_id, details, .. } => PendingSessionWrite::Compaction {
                        summary, first_kept_entry_id: first_kept_entry_id.and_then(|id| remap.get(&id).cloned()),
                        // Only the shortened tool outputs (by new entry id), so the copy builds the same context.
                        details: crate::harness::compaction::remap_details(
                            details.map(|d| serde_json::json!({"shortened": d.get("shortened").cloned().unwrap_or_default()})),
                            |old| remap.get(old).cloned(),
                        ),
                    },
                    SessionTreeEntry::BranchSummary { summary, .. } => PendingSessionWrite::BranchSummary { summary },
                    // Imported paths, custom data and grants never confer authority.
                    _ => continue,
                };
                let written = append_entry_tx(&tx, &id, pending)?;
                remap.insert(old_id, written.id().to_owned());
            }
            tx.execute("INSERT INTO session_leases(session_id,owner,pid,generation,heartbeat_ms) VALUES (?1,?2,?3,?4,?5)",
                params![id,identity.owner,identity.pid,identity.generation,self::now_ms()]).map_err(|e| SessionError::Storage(e.to_string()))?;
            tx.execute("INSERT INTO session_destinations(request_key,fingerprint,session_id,result) VALUES (?1,?2,?3,?4)",
                params![request_key,fingerprint,id,result]).map_err(|e| SessionError::Storage(e.to_string()))?;
            tx.commit().map_err(|e| SessionError::Storage(e.to_string()))?;
            Ok(DestinationCreation { id,receipt:result,store,identity,armed:true })
        }).await.map_err(|e| SessionError::Storage(e.to_string()))?
    }
}
