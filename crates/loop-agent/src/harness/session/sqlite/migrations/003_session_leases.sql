CREATE TABLE session_leases (
  session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  owner TEXT NOT NULL,
  pid INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  heartbeat_ms INTEGER NOT NULL
);
