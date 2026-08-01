//! SQLite FTS5 session search.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use crate::harness::session::search::{create_scanning_session_search, SessionSearch, SessionSearchHit};
use crate::harness::session::types::{SessionMetadata, SessionStore};
use crate::harness::types::SessionError;

const ENSURE_FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS session_search_fts USING fts5(
  payload,
  content = 'session_entries',
  content_rowid = 'rowid',
  tokenize = 'trigram remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS session_search_fts_ai AFTER INSERT ON session_entries BEGIN
  INSERT INTO session_search_fts(rowid, payload) VALUES (new.rowid, new.payload);
END;
CREATE TRIGGER IF NOT EXISTS session_search_fts_ad AFTER DELETE ON session_entries BEGIN
  INSERT INTO session_search_fts(session_search_fts, rowid, payload) VALUES('delete', old.rowid, old.payload);
END;
CREATE TRIGGER IF NOT EXISTS session_search_fts_au AFTER UPDATE OF payload ON session_entries BEGIN
  INSERT INTO session_search_fts(session_search_fts, rowid, payload) VALUES('delete', old.rowid, old.payload);
  INSERT INTO session_search_fts(rowid, payload) VALUES (new.rowid, new.payload);
END;
"#;

fn open(path: &Path) -> Result<Connection, SessionError> {
    let conn = Connection::open(path).map_err(|e| SessionError::Storage(e.to_string()))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, SessionError> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
        params![name],
        |_| Ok(true),
    )
    .optional()
    .map(|opt| opt.is_some())
    .map_err(|e| SessionError::Storage(e.to_string()))
}

fn ensure_search_schema(conn: &Connection) -> Result<bool, SessionError> {
    let fts_exists = table_exists(conn, "session_search_fts")?;
    conn.execute_batch(ENSURE_FTS_SCHEMA)
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    if !fts_exists {
        conn.execute("INSERT INTO session_search_fts(session_search_fts) VALUES('rebuild')", [])
            .map_err(|e| SessionError::Storage(e.to_string()))?;
    }
    Ok(true)
}

fn fts_available(conn: &Connection) -> bool {
    ensure_search_schema(conn).is_ok()
}

fn escape_fts_query(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// FTS-backed search over a SQLite session database.
pub struct SqliteSessionSearch {
    path: PathBuf,
    fallback: std::sync::Arc<dyn SessionSearch>,
}

/// Create SQLite FTS search; falls back to scanning when FTS5 is unavailable.
pub fn create_sqlite_session_search(
    path: impl AsRef<Path>,
    store: std::sync::Arc<dyn SessionStore>,
) -> std::sync::Arc<dyn SessionSearch> {
    std::sync::Arc::new(SqliteSessionSearch {
        path: path.as_ref().to_path_buf(),
        fallback: create_scanning_session_search(store),
    })
}

#[async_trait]
impl SessionSearch for SqliteSessionSearch {
    async fn search(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>, SessionError> {
        let text = query.trim().to_string();
        if text.is_empty() {
            return Ok(vec![]);
        }
        let path = self.path.clone();
        let cwd_owned = cwd.map(|s| s.to_string());
        let fts_hits = tokio::task::spawn_blocking(move || {
            let conn = open(&path)?;
            if !fts_available(&conn) {
                return Ok::<_, SessionError>(None);
            }
            let fts_query = escape_fts_query(&text);
            let mut stmt = conn
                .prepare(
                    "SELECT s.id, s.cwd, s.name, s.parent_session_id, s.created_at,
                            se.entry_id, se.payload,
                            bm25(session_search_fts) AS score
                     FROM session_search_fts
                     JOIN session_entries se ON se.rowid = session_search_fts.rowid
                     JOIN sessions s ON s.id = se.session_id
                     WHERE session_search_fts MATCH ?1
                       AND (?2 IS NULL OR s.cwd = ?2)
                     ORDER BY score
                     LIMIT ?3",
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![fts_query, cwd_owned, limit as i64],
                    |row| {
                        Ok(SessionSearchHit {
                            session: SessionMetadata {
                                id: row.get(0)?,
                                cwd: row.get(1)?,
                                name: row.get(2)?,
                                parent_session_id: row.get(3)?,
                                created_at: row.get(4)?,
                                path: None,
                            },
                            entry_id: row.get(5)?,
                            snippet: row
                                .get::<_, String>(6)?
                                .chars()
                                .take(200)
                                .collect(),
                        })
                    },
                )
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            let mut hits = Vec::new();
            for row in rows {
                hits.push(row.map_err(|e| SessionError::Storage(e.to_string()))?);
            }
            Ok(Some(hits))
        })
        .await
        .map_err(|e| SessionError::Storage(e.to_string()))??;
        if let Some(hits) = fts_hits {
            return Ok(hits);
        }
        self.fallback.search(query, cwd, limit).await
    }
}
