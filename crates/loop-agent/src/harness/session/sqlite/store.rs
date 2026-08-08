//! SQLite-backed session store (WAL, migrations, transactional append).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

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
];

/// Open or create a SQLite session store at `path`.
pub fn create_sqlite_session_store(
    path: impl AsRef<Path>,
) -> Result<Arc<dyn SessionStore>, SessionError> {
    let path = path.as_ref().to_path_buf();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SessionError::Io(e.to_string()))?;
    }
    let mut conn = open(&path)?;
    apply_migrations(&mut conn)?;
    Ok(Arc::new(SqliteSessionStore { path }))
}

fn open(path: &Path) -> Result<Connection, SessionError> {
    let conn = Connection::open(path).map_err(|e| SessionError::Storage(e.to_string()))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
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
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
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

struct SqliteSessionStore {
    path: PathBuf,
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
        out.push(
            serde_json::from_str(&payload).map_err(|e| SessionError::Storage(e.to_string()))?,
        );
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
                .map(|p| {
                    serde_json::from_str(&p).map_err(|e| SessionError::Storage(e.to_string()))
                })
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

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let path = self.path.clone();
        let id = create_session_id();
        let created_at = now_ms();
        let meta = SessionMetadata {
            id: id.clone(),
            cwd: cwd.clone(),
            name: name.clone(),
            parent_session_id: None,
            created_at,
            path: None,
        };
        tokio::task::spawn_blocking({
            let path = path.clone();
            let id = id.clone();
            move || {
                let mut conn = open(&path)?;
                let tx = conn
                    .transaction()
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                tx.execute(
                    "INSERT INTO sessions(id, cwd, name, parent_session_id, active_leaf_id, created_at) VALUES (?1,?2,?3,NULL,NULL,?4)",
                    params![id, cwd, name, created_at],
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
                tx.execute(
                    "INSERT INTO session_sequences(session_id, next_seq) VALUES (?1, 1)",
                    params![id],
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
                insert_empty_materialized(&tx, &id)?;
                tx.commit()
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
                Ok::<(), SessionError>(())
            }
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))??;
        Ok(Arc::new(SqliteReader { meta, path }))
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
        tokio::task::spawn_blocking(move || {
            let mut conn = open(&path)?;
            let tx = conn
                .transaction()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            let leaf: Option<String> = tx
                .query_row(
                    "SELECT active_leaf_id FROM sessions WHERE id=?1",
                    params![session_id],
                    |row| row.get(0),
                )
                .map_err(|_| SessionError::NotFound(session_id.clone()))?;
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
            append_entry_to_branch_cache(
                &tx,
                &session_id,
                entry.id(),
                seq,
                entry.parent_id(),
            )?;
            update_materialized_on_append(&tx, &session_id, seq, &entry)?;
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
        tokio::task::spawn_blocking(move || {
            let mut conn = open(&path)?;
            let tx = conn
                .transaction()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            delete_session_rows(&tx, &id)?;
            tx.commit()
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))?
    }

    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        through_entry_id: Option<&str>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        let reader = self.load(source_id).await?;
        let leaf = reader.read_head().await?;
        let entries = reader.read_entries(None).await?;
        let selected = entries_for_fork_selection(
            &entries,
            leaf.as_deref(),
            selection,
            through_entry_id,
        )?;
        let meta = reader.metadata().clone();
        let new_reader = self.create(meta.cwd.clone(), name).await?;
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
                } => PendingSessionWrite::ModelChange {
                    provider,
                    model_id,
                },
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
            self.append_entry(&new_reader.metadata().id, pending)
                .await?;
        }
        let path = self.path.clone();
        let new_id = new_reader.metadata().id.clone();
        let source_id = source_id.to_string();
        let update_id = new_id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            conn.execute(
                "UPDATE sessions SET parent_session_id=?1 WHERE id=?2",
                params![source_id, update_id],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
            Ok::<(), SessionError>(())
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))??;
        self.load(&new_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
