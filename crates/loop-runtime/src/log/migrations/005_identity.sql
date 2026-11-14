-- Step B3 (ADR-0007 §7, ADR-0009): who acted, and tenant-scoped reads.
--
-- Cost on upgrade. ADD COLUMN with a constant default changes only the
-- schema; SQLite does not rewrite a row. Old rows read `tenant` as 'local'
-- (the column default) and NULL principal columns, which a reader of this
-- version decodes as the Runtime's own principal {service, local, runtime}
-- (`Identity::unrecorded`). The one O(rows) step is building the new unique
-- index: one pass over the Updates and a sort, with no table rewrite.
ALTER TABLE events ADD COLUMN tenant TEXT NOT NULL DEFAULT 'local';
ALTER TABLE events ADD COLUMN principal_kind TEXT;
ALTER TABLE events ADD COLUMN principal_issuer TEXT;
ALTER TABLE events ADD COLUMN principal_id TEXT;
ALTER TABLE events ADD COLUMN via TEXT;
ALTER TABLE events ADD COLUMN actor TEXT;

-- The Session key space is per tenant: each tenant has its own
-- `session_seq` for a key. The old index made (session_key, session_seq)
-- unique across tenants, so it goes. The new one leads with session_key,
-- so the per-append query of an older binary (`session_key=?` only) still
-- reads an index.
DROP INDEX events_session_seq;
CREATE UNIQUE INDEX events_tenant_session_seq
    ON events(session_key, tenant, session_seq) WHERE session_seq IS NOT NULL;

-- The per-Session projection keyed by (tenant, session_key). New tables:
-- the 004 tables stay as they are, so an older binary that opens this
-- database keeps its own projection (its writes name `session_key` alone in
-- ON CONFLICT). This binary no longer writes them; an older binary catches
-- them up from its own mark when it opens the database.
CREATE TABLE tenant_session_summary (
    tenant TEXT NOT NULL,
    session_key TEXT NOT NULL,
    last_seq INTEGER NOT NULL DEFAULT 0,
    last_revision INTEGER NOT NULL DEFAULT 0,
    last_activity_ms INTEGER NOT NULL,
    prompts_sent INTEGER NOT NULL DEFAULT 0,
    turn_count INTEGER NOT NULL DEFAULT 0,
    parent TEXT,
    -- Owner of record: the identity of the session_opened that created the
    -- Session (payload `created: true`), set once. owner_seq is that
    -- record's global_seq; NULL means no creating record was seen.
    owner_seq INTEGER,
    owner_kind TEXT,
    owner_issuer TEXT,
    owner_id TEXT,
    owner_via TEXT,
    owner_actor TEXT,
    PRIMARY KEY (tenant, session_key)
) WITHOUT ROWID;
CREATE INDEX tenant_session_summary_subagents
    ON tenant_session_summary(tenant, session_key) WHERE parent IS NOT NULL;
CREATE TABLE tenant_session_turns (
    tenant TEXT NOT NULL,
    session_key TEXT NOT NULL,
    turn TEXT NOT NULL,
    first_global_seq INTEGER NOT NULL,
    PRIMARY KEY (tenant, session_key, turn)
) WITHOUT ROWID;

-- Every row so far is in tenant 'local'. Copy the 004 projection (small:
-- one row per Session, one per turn) and its mark; the open-time backfill
-- then applies any rows after the mark, in resumable chunks, as for 004.
-- Copied Sessions have no owner of record: their creating record, if any,
-- had no identity.
INSERT INTO tenant_session_summary
    (tenant, session_key, last_seq, last_revision, last_activity_ms, prompts_sent, turn_count, parent)
    SELECT 'local', session_key, last_seq, last_revision, last_activity_ms, prompts_sent, turn_count, parent
    FROM session_summary;
INSERT INTO tenant_session_turns (tenant, session_key, turn, first_global_seq)
    SELECT 'local', session_key, turn, first_global_seq FROM session_turns;
INSERT INTO projection_state (name, applied_global_seq)
    SELECT 'tenant_session_summary', applied_global_seq
    FROM projection_state WHERE name = 'session_summary';
