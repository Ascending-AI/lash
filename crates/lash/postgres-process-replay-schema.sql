-- lash PostgreSQL process replay store schema (`lash::postgres::PostgresProcessReplayStore`).
--
-- Published artifact. These bytes are exactly the DDL the store executes in
-- its `install` schema mode; `PostgresProcessReplayStore::schema_ddl()` returns
-- this file verbatim. A host that provisions its schemas itself copies this
-- file byte-for-byte into its own migration tooling rather than transcribing
-- it, applies it into the schema it configures as `schema`, and runs the store
-- in `verify_only` mode: the store then runs no DDL and refuses to start when
-- the tables differ from `postgres-process-replay-schema-shape.txt`, the
-- structure this file produces.
--
-- Every statement is creation-only and idempotent, so applying the file twice
-- is a no-op, and nothing here is schema-qualified, so the file provisions
-- into whichever schema the session's `search_path` resolves. It seeds no
-- rows: the store mints its incarnation and sentinel rows itself. The tables
-- share no object with the live replay store's (`postgres-live-replay-schema.sql`).

-- The one row naming the history the unlogged tables hold. Logged, so it
-- outlives a crash that truncates the other four.
CREATE TABLE IF NOT EXISTS process_replay_incarnation (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    incarnation_id text NOT NULL,
    rotations bigint NOT NULL,
    rotated_at timestamptz NOT NULL
);

-- The one row proving the unlogged tables still hold the incarnation's
-- history, with what is only true of that history: the position watermark of
-- evicted and forgotten processes, and the aggregate budget the heads hold.
CREATE UNLOGGED TABLE IF NOT EXISTS process_replay_sentinel (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    incarnation_id text NOT NULL,
    watermark bigint NOT NULL,
    resident_processes bigint NOT NULL,
    reserved_bytes bigint NOT NULL
);

-- One row per process: its sequencer, its retention counters and its share
-- of the aggregate byte budget.
CREATE UNLOGGED TABLE IF NOT EXISTS process_replay_head (
    process_id text PRIMARY KEY,
    tail_position bigint NOT NULL,
    floor_position bigint NOT NULL,
    first_retained bigint NOT NULL,
    retained_events bigint NOT NULL,
    retained_bytes bigint NOT NULL,
    reserved_bytes bigint NOT NULL,
    touched_at timestamptz NOT NULL
);

-- The events, keyed by (process, position). `sequence` is the durable
-- process sequence the event was published at.
CREATE UNLOGGED TABLE IF NOT EXISTS process_replay_log (
    process_id text NOT NULL,
    position bigint NOT NULL,
    sequence bigint NOT NULL,
    payload bytea NOT NULL,
    bytes bigint NOT NULL,
    published_at timestamptz NOT NULL,
    PRIMARY KEY (process_id, position)
);

-- The observation identity of each retained event: a redelivery finds the
-- position that already holds it.
CREATE UNLOGGED TABLE IF NOT EXISTS process_replay_dedupe (
    process_id text NOT NULL,
    identity text NOT NULL,
    position bigint NOT NULL,
    PRIMARY KEY (process_id, identity)
);
