-- The witness ledger of the lash-facade-failover runbook (FIG-5193), the
-- same as the lash-postgres-workers runbook's (FIG-5199): the
-- outside world the nodes act on, kept in a database of its own
-- (`lash_witness`) on the same server as the lash database.
--
-- No lash store, transaction, serializer or schema lifecycle touches these
-- tables. Nodes write as `lash_witness_writer`, which may only INSERT the
-- columns it supplies and SELECT: every ledger is append-only for its
-- writers, and the database stamps `recorded_at` from its own clock. The
-- laws read only these rows and the store's own durable rows; a node's
-- report lines are diagnostics and timing, never evidence of an effect.
--
-- Apply it to `lash_witness` as a superuser, before any node starts.

CREATE ROLE lash_witness_writer LOGIN;

-- One row per physical body entry and return: `entered` before the body
-- does anything else, `returned` when it answers. A `Once` call holds at
-- most one `entered` row, however many nodes and crashes it saw.
CREATE TABLE witness_effects (
    id BIGSERIAL PRIMARY KEY,
    call_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    node TEXT NOT NULL,
    phase TEXT NOT NULL CONSTRAINT ck_witness_effects_phase CHECK (phase IN ('entered', 'returned')),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

-- One row per model attempt the scripted model received: which call of the
-- turn (1 before the cell's result, 2 after), its attempt number, and the
-- node that sent it.
CREATE TABLE witness_model_attempts (
    id BIGSERIAL PRIMARY KEY,
    call_index INTEGER NOT NULL CONSTRAINT ck_witness_model_call CHECK (call_index IN (1, 2)),
    attempt INTEGER NOT NULL CONSTRAINT ck_witness_model_attempt CHECK (attempt >= 1),
    node TEXT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

-- The faults the test injected, and the `release` marker a held body waits
-- for. Written by the test, which also writes as the writer role.
CREATE TABLE witness_nemesis (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL CONSTRAINT ck_witness_nemesis_kind CHECK (kind IN (
        'kill', 'stop', 'partition', 'heal', 'restart-begin', 'restart-complete', 'release'
    )),
    node TEXT,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

REVOKE ALL ON witness_effects, witness_model_attempts, witness_nemesis FROM PUBLIC;
GRANT SELECT ON witness_effects, witness_model_attempts, witness_nemesis TO lash_witness_writer;
GRANT INSERT (call_id, tool, node, phase) ON witness_effects TO lash_witness_writer;
GRANT INSERT (call_index, attempt, node) ON witness_model_attempts TO lash_witness_writer;
GRANT INSERT (kind, node) ON witness_nemesis TO lash_witness_writer;
GRANT USAGE ON SEQUENCE witness_effects_id_seq, witness_model_attempts_id_seq, witness_nemesis_id_seq
    TO lash_witness_writer;
