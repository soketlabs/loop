CREATE TABLE IF NOT EXISTS branch_tips (
  session_id TEXT NOT NULL,
  tip_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  PRIMARY KEY (session_id, tip_id),
  UNIQUE (session_id, branch_id)
);

DELETE FROM branch_tips;
DELETE FROM branch_entries;

DROP INDEX IF EXISTS idx_branch_entries_session_branch;

-- Tables added after initial release (no-op when already present from 001).
CREATE TABLE IF NOT EXISTS branch_entries (
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  entry_seq INTEGER NOT NULL,
  PRIMARY KEY (session_id, branch_id, entry_id)
);

CREATE INDEX IF NOT EXISTS idx_branch_entries_session_branch_seq
  ON branch_entries(session_id, branch_id, entry_seq);
CREATE INDEX IF NOT EXISTS idx_branch_entries_session_entry
  ON branch_entries(session_id, entry_id);

CREATE TABLE IF NOT EXISTS session_materialized (
  session_id TEXT PRIMARY KEY,
  payload TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS entry_materialized (
  session_id TEXT NOT NULL,
  entry_seq INTEGER NOT NULL,
  type TEXT NOT NULL,
  payload TEXT NOT NULL,
  PRIMARY KEY (session_id, entry_seq, type)
);

CREATE INDEX IF NOT EXISTS idx_entry_materialized_session_type_seq
  ON entry_materialized(session_id, type, entry_seq);
