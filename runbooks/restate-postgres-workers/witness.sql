-- The recovery-law witness ledgers (FIG-608).
--
-- They live in their own database under their own account, apart from the
-- `lash` database the workers' store set writes: no Lash transaction, store
-- adapter, serializer or schema lifecycle touches them. The e2e script applies
-- this file with psql as the server's superuser before any service starts.
--
-- The writer account may only INSERT the columns it supplies and SELECT. It
-- holds no UPDATE or DELETE, so every ledger is append-only for the processes
-- that write it, and every `recorded_at_us` comes from this one database clock:
-- a writer cannot supply its own. Digests are computed here, over the bytes the
-- writer sent, not by the writer.

CREATE ROLE lash_witness LOGIN PASSWORD 'lash_witness';
CREATE DATABASE lash_witness OWNER lash;

\connect lash_witness

REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO lash_witness;

CREATE FUNCTION witness_clock_us() RETURNS BIGINT
    LANGUAGE sql VOLATILE
    AS $$ SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000000)::BIGINT $$;
GRANT EXECUTE ON FUNCTION witness_clock_us() TO lash_witness;

-- The client's submission, recorded before the runner sends it.
CREATE TABLE witness_submissions (
    submission_id BIGSERIAL PRIMARY KEY,
    workflow_id TEXT NOT NULL,
    request_bytes BYTEA NOT NULL,
    request_digest TEXT GENERATED ALWAYS AS (encode(sha256(request_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (workflow_id, request_bytes) ON witness_submissions TO lash_witness;

-- The ingress acknowledgement of a submission, recorded once it returns.
CREATE TABLE witness_acknowledgements (
    ack_id BIGSERIAL PRIMARY KEY,
    workflow_id TEXT NOT NULL,
    invocation_id TEXT NOT NULL,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (workflow_id, invocation_id) ON witness_acknowledgements TO lash_witness;

-- The exact terminal bytes a client read from the workflow's Restate output:
-- `observed` when the runner first saw the terminal, `reattached` when it read
-- the identical address again after the restart.
CREATE TABLE witness_client_terminals (
    observation_id BIGSERIAL PRIMARY KEY,
    workflow_id TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('observed', 'reattached')),
    output_bytes BYTEA NOT NULL,
    output_digest TEXT GENERATED ALWAYS AS (encode(sha256(output_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (workflow_id, phase, output_bytes) ON witness_client_terminals TO lash_witness;

-- Faults the harness injected: the control node's cluster restart, a worker's
-- exit from `crash_once`, and the loss after a witness commit.
CREATE TABLE witness_nemesis (
    nemesis_id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (
        kind IN ('restart-begin', 'restart-complete', 'worker-exit', 'loss-after-commit')
    ),
    subject TEXT NOT NULL,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (kind, subject) ON witness_nemesis TO lash_witness;

-- Every physical attempt at a witnessed effect, retries included.
CREATE TABLE witness_effect_attempts (
    attempt_id TEXT PRIMARY KEY,
    logical_key TEXT NOT NULL,
    parent_workflow_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    request_bytes BYTEA NOT NULL,
    request_digest TEXT GENERATED ALWAYS AS (encode(sha256(request_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (attempt_id, logical_key, parent_workflow_id, call_id, worker_id, request_bytes)
    ON witness_effect_attempts TO lash_witness;

-- The idempotent receiver's accepted commit: the first attempt to arrive wins
-- the logical key, and every later attempt is answered with this response.
CREATE TABLE witness_effect_commits (
    logical_key TEXT PRIMARY KEY,
    parent_workflow_id TEXT NOT NULL,
    first_attempt_id TEXT NOT NULL,
    request_bytes BYTEA NOT NULL,
    request_digest TEXT GENERATED ALWAYS AS (encode(sha256(request_bytes), 'hex')) STORED,
    response_bytes BYTEA NOT NULL,
    response_digest TEXT GENERATED ALWAYS AS (encode(sha256(response_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (logical_key, parent_workflow_id, first_attempt_id, request_bytes, response_bytes)
    ON witness_effect_commits TO lash_witness;

-- The receiver's answer to one attempt. An attempt lost before its answer was
-- recorded has no row here.
CREATE TABLE witness_effect_replies (
    attempt_id TEXT PRIMARY KEY,
    logical_key TEXT NOT NULL,
    accepted BOOLEAN NOT NULL,
    response_bytes BYTEA NOT NULL,
    response_digest TEXT GENERATED ALWAYS AS (encode(sha256(response_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (attempt_id, logical_key, accepted, response_bytes)
    ON witness_effect_replies TO lash_witness;

-- Every completion the mock provider served, written by the provider process.
CREATE TABLE witness_provider_receipts (
    receipt_id BIGSERIAL PRIMARY KEY,
    request_id TEXT NOT NULL,
    scenario TEXT NOT NULL,
    workflow_id TEXT NOT NULL,
    model TEXT NOT NULL,
    request_bytes BYTEA NOT NULL,
    request_digest TEXT GENERATED ALWAYS AS (encode(sha256(request_bytes), 'hex')) STORED,
    response_bytes BYTEA NOT NULL,
    response_digest TEXT GENERATED ALWAYS AS (encode(sha256(response_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us()
);
GRANT INSERT (request_id, scenario, workflow_id, model, request_bytes, response_bytes)
    ON witness_provider_receipts TO lash_witness;

GRANT SELECT ON ALL TABLES IN SCHEMA public TO lash_witness;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO lash_witness;
