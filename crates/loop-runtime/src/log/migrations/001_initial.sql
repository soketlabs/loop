CREATE TABLE events (
    global_seq INTEGER PRIMARY KEY,
    generation INTEGER NOT NULL,
    session_key TEXT,
    session_seq INTEGER,
    kind TEXT NOT NULL,
    turn TEXT,
    ts_ms INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE UNIQUE INDEX events_session_seq
    ON events(session_key, session_seq) WHERE session_seq IS NOT NULL;
CREATE TABLE runtime_generations (
    generation INTEGER PRIMARY KEY,
    owner TEXT NOT NULL,
    pid INTEGER NOT NULL,
    started_ms INTEGER NOT NULL,
    stopped_ms INTEGER
);
