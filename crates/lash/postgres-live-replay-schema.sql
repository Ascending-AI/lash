-- lash PostgreSQL live replay store schema (`lash::postgres::PostgresLiveReplayStore`).
--
-- Published artifact. These bytes are exactly the DDL the store executes in
-- its `install` schema mode; `PostgresLiveReplayStore::schema_ddl()` returns
-- this file verbatim. A host that provisions its schemas itself copies this
-- file byte-for-byte into its own migration tooling rather than transcribing
-- it, applies it into the schema it configures as `schema`, and runs the store
-- in `verify_only` mode: the store then runs no DDL and refuses to start when
-- the tables differ from `postgres-live-replay-schema-shape.txt`, the
-- structure this file produces.
--
-- Every statement is creation-only and idempotent, so applying the file twice
-- is a no-op, and nothing here is schema-qualified, so the file provisions
-- into whichever schema the session's `search_path` resolves. It seeds no
-- rows: the store mints its incarnation row itself.

-- The one row naming the history the unlogged tables hold, and the position
-- watermark of forgotten sessions. Logged, so it outlives a crash that
-- truncates the other two.
CREATE TABLE IF NOT EXISTS live_replay_incarnation (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    incarnation_id text NOT NULL,
    watermark bigint NOT NULL,
    rotations bigint NOT NULL,
    rotated_at timestamptz NOT NULL
);

-- One row per session: its sequencer and its retention counters.
CREATE UNLOGGED TABLE IF NOT EXISTS live_replay_head (
    session_id text PRIMARY KEY,
    tail_position bigint NOT NULL,
    floor_position bigint NOT NULL,
    first_retained bigint NOT NULL,
    retained_events bigint NOT NULL,
    retained_bytes bigint NOT NULL,
    touched_at timestamptz NOT NULL
);

-- The events, keyed by (session, position). The row ('', 0) is the sentinel
-- naming the incarnation.
CREATE UNLOGGED TABLE IF NOT EXISTS live_replay_log (
    session_id text NOT NULL,
    position bigint NOT NULL,
    revision bigint NOT NULL,
    turn_id text,
    activity_id text,
    activity_key text,
    activity_first bigint,
    activity_last bigint,
    payload bytea NOT NULL,
    bytes bigint NOT NULL,
    published_at timestamptz NOT NULL,
    PRIMARY KEY (session_id, position)
);

-- An activity is unique within the window by its identity: a keyed activity
-- by its first ordinal, an unkeyed one by its id.
CREATE UNIQUE INDEX IF NOT EXISTS live_replay_log_activity
    ON live_replay_log (session_id, activity_id)
    WHERE activity_key IS NULL AND activity_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS live_replay_log_span
    ON live_replay_log (session_id, activity_key, activity_first)
    WHERE activity_key IS NOT NULL;
