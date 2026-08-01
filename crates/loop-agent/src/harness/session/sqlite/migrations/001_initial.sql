CREATE TABLE IF NOT EXISTS migrations (
  id TEXT PRIMARY KEY,
  applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  cwd TEXT,
  name TEXT,
  parent_session_id TEXT,
  active_leaf_id TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS session_entries (
  session_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  parent_id TEXT,
  entry_seq INTEGER NOT NULL,
  payload TEXT NOT NULL,
  PRIMARY KEY (session_id, entry_id),
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_entries_seq
  ON session_entries(session_id, entry_seq);

CREATE TABLE IF NOT EXISTS session_sequences (
  session_id TEXT PRIMARY KEY,
  next_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS branch_entries (
  session_id TEXT NOT NULL,
  branch_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  entry_seq INTEGER NOT NULL,
  PRIMARY KEY (session_id, branch_id, entry_id)
);

CREATE INDEX IF NOT EXISTS idx_branch_entries_session_branch
  ON branch_entries(session_id, branch_id);
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
