//! Derived branch path cache (root-to-tip) mirroring pi-storage-sqlite-node.

use rusqlite::{params, Connection, OptionalExtension};

use crate::harness::session::types::SessionTreeEntry;
use crate::harness::types::SessionError;
use loop_ai::new_id;

/// Cached branch membership for a leaf entry.
#[derive(Debug, Clone)]
pub struct CachedBranch {
    /// Branch id.
    pub branch_id: String,
    /// Leaf sequence in branch.
    pub leaf_seq: i64,
}

/// Read cached branch for a leaf entry, if present.
pub fn read_cached_branch(
    conn: &Connection,
    session_id: &str,
    leaf_id: &str,
) -> Result<Option<CachedBranch>, SessionError> {
    conn.query_row(
        "SELECT branch_id, entry_seq FROM branch_entries
         WHERE session_id = ?1 AND entry_id = ?2
         ORDER BY branch_id LIMIT 1",
        params![session_id, leaf_id],
        |row| {
            Ok(CachedBranch {
                branch_id: row.get(0)?,
                leaf_seq: row.get(1)?,
            })
        },
    )
    .optional()
    .map_err(|e| SessionError::Storage(e.to_string()))
}

/// Read cached branch rows from `start_seq` through the branch tip.
pub fn read_cached_branch_rows(
    conn: &Connection,
    session_id: &str,
    branch: &CachedBranch,
    start_seq: i64,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    let mut stmt = conn
        .prepare(
            "SELECT e.payload
             FROM branch_entries AS b
             JOIN session_entries AS e ON e.session_id = b.session_id AND e.entry_id = b.entry_id
             WHERE b.session_id = ?1 AND b.branch_id = ?2 AND b.entry_seq BETWEEN ?3 AND ?4
             ORDER BY b.entry_seq",
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let rows = stmt
        .query_map(
            params![session_id, branch.branch_id, start_seq, branch.leaf_seq],
            |row| row.get::<_, String>(0),
        )
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

fn extend_branch(
    conn: &Connection,
    session_id: &str,
    branch_id: &str,
    parent_id: &str,
    entry_id: &str,
    entry_seq: i64,
) -> Result<(), SessionError> {
    conn.execute(
        "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq) VALUES (?1, ?2, ?3, ?4)",
        params![session_id, branch_id, entry_id, entry_seq],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    let changed = conn
        .execute(
            "UPDATE branch_tips SET tip_id = ?1 WHERE session_id = ?2 AND branch_id = ?3 AND tip_id = ?4",
            params![entry_id, session_id, branch_id, parent_id],
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    if changed != 1 {
        return Err(SessionError::Invalid(format!(
            "branch tip {parent_id} changed during append"
        )));
    }
    Ok(())
}

/// Rebuild branch cache for a leaf using a recursive parent walk.
pub fn rebuild_cached_branch(
    conn: &Connection,
    session_id: &str,
    leaf_id: &str,
    branch_id_to_replace: Option<&str>,
) -> Result<(), SessionError> {
    conn.execute_batch("SAVEPOINT rebuild_branch_cache")
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    let result = (|| {
        let tip_branch_id: Option<String> = conn
            .query_row(
                "SELECT branch_id FROM branch_tips WHERE session_id = ?1 AND tip_id = ?2",
                params![session_id, leaf_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        let mut branch_ids: Vec<String> = Vec::new();
        if let Some(id) = branch_id_to_replace {
            branch_ids.push(id.to_string());
        }
        if let Some(id) = tip_branch_id {
            if !branch_ids.iter().any(|b| b == &id) {
                branch_ids.push(id);
            }
        }
        for branch_id in &branch_ids {
            conn.execute(
                "DELETE FROM branch_tips WHERE session_id = ?1 AND branch_id = ?2",
                params![session_id, branch_id],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
            conn.execute(
                "DELETE FROM branch_entries WHERE session_id = ?1 AND branch_id = ?2",
                params![session_id, branch_id],
            )
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        }
        let branch_id = new_id();
        conn.execute(
            "WITH RECURSIVE path(id, entry_seq, parent_id) AS (
                SELECT entry_id, entry_seq, parent_id
                FROM session_entries
                WHERE session_id = ?1 AND entry_id = ?2
                UNION ALL
                SELECT parent.entry_id, parent.entry_seq, parent.parent_id
                FROM session_entries AS parent
                JOIN path AS child ON child.parent_id = parent.entry_id
                WHERE parent.session_id = ?3
            )
            INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq)
            SELECT ?4, ?5, id, entry_seq FROM path",
            params![session_id, leaf_id, session_id, session_id, branch_id],
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT INTO branch_tips (session_id, tip_id, branch_id) VALUES (?1, ?2, ?3)",
            params![session_id, leaf_id, branch_id],
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
        conn.execute_batch("RELEASE SAVEPOINT rebuild_branch_cache")
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        Ok::<(), SessionError>(())
    })();
    if let Err(error) = result {
        let _ = conn.execute_batch(
            "ROLLBACK TO SAVEPOINT rebuild_branch_cache; RELEASE SAVEPOINT rebuild_branch_cache;",
        );
        return Err(error);
    }
    Ok(())
}

/// Extend or fork branch cache after appending an entry.
pub fn append_entry_to_branch_cache(
    conn: &Connection,
    session_id: &str,
    entry_id: &str,
    entry_seq: i64,
    parent_id: Option<&str>,
) -> Result<(), SessionError> {
    let Some(parent_id) = parent_id else {
        let branch_id = new_id();
        conn.execute(
            "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, branch_id, entry_id, entry_seq],
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT INTO branch_tips (session_id, tip_id, branch_id) VALUES (?1, ?2, ?3)",
            params![session_id, entry_id, branch_id],
        )
        .map_err(|e| SessionError::Storage(e.to_string()))?;
        return Ok(());
    };

    let tip_branch_id: Option<String> = conn
        .query_row(
            "SELECT branch_id FROM branch_tips WHERE session_id = ?1 AND tip_id = ?2",
            params![session_id, parent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| SessionError::Storage(e.to_string()))?;
    if let Some(branch_id) = tip_branch_id {
        return extend_branch(conn, session_id, &branch_id, parent_id, entry_id, entry_seq);
    }

    let source: Option<(String, i64)> = conn
        .query_row(
            "SELECT branch_id, entry_seq
             FROM branch_entries
             WHERE session_id = ?1 AND entry_id = ?2
             ORDER BY branch_id
             LIMIT 1",
            params![session_id, parent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| SessionError::Storage(e.to_string()))?;

    if source.is_none() {
        rebuild_cached_branch(conn, session_id, parent_id, None)?;
        let tip_branch_id: Option<String> = conn
            .query_row(
                "SELECT branch_id FROM branch_tips WHERE session_id = ?1 AND tip_id = ?2",
                params![session_id, parent_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        let Some(branch_id) = tip_branch_id else {
            return Err(SessionError::Invalid(format!(
                "branch cache repair did not create tip {parent_id}"
            )));
        };
        return extend_branch(conn, session_id, &branch_id, parent_id, entry_id, entry_seq);
    }

    let (source_branch_id, source_seq) = source.unwrap_or_default();
    let branch_id = new_id();
    conn.execute(
        "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq)
         SELECT session_id, ?1, entry_id, entry_seq
         FROM branch_entries
         WHERE session_id = ?2 AND branch_id = ?3 AND entry_seq <= ?4",
        params![branch_id, session_id, source_branch_id, source_seq],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    conn.execute(
        "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq) VALUES (?1, ?2, ?3, ?4)",
        params![session_id, branch_id, entry_id, entry_seq],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    conn.execute(
        "INSERT INTO branch_tips (session_id, tip_id, branch_id) VALUES (?1, ?2, ?3)",
        params![session_id, entry_id, branch_id],
    )
    .map_err(|e| SessionError::Storage(e.to_string()))?;
    Ok(())
}

/// Validate cached path ordering and parent links.
pub fn is_valid_cached_path(entries: &[SessionTreeEntry], leaf_id: &str) -> bool {
    if entries.is_empty() || entries.last().map(|e| e.id()) != Some(leaf_id) {
        return false;
    }
    if entries[0].parent_id().is_some() {
        return false;
    }
    for index in 1..entries.len() {
        if entries[index].parent_id() != Some(entries[index - 1].id()) {
            return false;
        }
    }
    true
}
