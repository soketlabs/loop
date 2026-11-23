-- Step B2 (ADR-0007 §8): the per-Session summary projection.
-- The writer updates it in the transaction of each append. Rows are a fold
-- of the Session's events in global_seq order (loop_store::summarize).
-- projection_state holds the last global_seq applied; a backfill after this
-- migration applies the events of an older database from that mark, in
-- chunks, so an interrupted backfill continues at the next open.
CREATE TABLE session_summary (
    session_key TEXT PRIMARY KEY,
    last_seq INTEGER NOT NULL DEFAULT 0,
    last_revision INTEGER NOT NULL DEFAULT 0,
    last_activity_ms INTEGER NOT NULL,
    prompts_sent INTEGER NOT NULL DEFAULT 0,
    turn_count INTEGER NOT NULL DEFAULT 0,
    parent TEXT
) WITHOUT ROWID;
-- Subagent lookup reads only the Sessions with a parent.
CREATE INDEX session_summary_subagents
    ON session_summary(session_key) WHERE parent IS NOT NULL;
-- Distinct turns of each Session, for turn_ids.
CREATE TABLE session_turns (
    session_key TEXT NOT NULL,
    turn TEXT NOT NULL,
    first_global_seq INTEGER NOT NULL,
    PRIMARY KEY (session_key, turn)
) WITHOUT ROWID;
CREATE TABLE projection_state (
    name TEXT PRIMARY KEY,
    applied_global_seq INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO projection_state (name, applied_global_seq) VALUES ('session_summary', 0);
