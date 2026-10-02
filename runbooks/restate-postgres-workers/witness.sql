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

-- The FIG-3790 durable load workload's evidence (FIG-4168). The driver
-- appends every operation it sends and the typed terminal it read back; the
-- attachment tool and the workers' read endpoint append the exact blob bytes
-- they put and read. With the provider receipts and the effect ledgers above,
-- the load verifier reconciles these rows against the regenerated plan.
CREATE TABLE witness_load_events (
    event_id BIGSERIAL PRIMARY KEY,
    run_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    operation TEXT NOT NULL,
    phase TEXT NOT NULL,
    observer TEXT NOT NULL,
    detail_json TEXT NOT NULL,
    content_bytes BYTEA,
    content_digest TEXT GENERATED ALWAYS AS (encode(sha256(content_bytes), 'hex')) STORED,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us(),
    CONSTRAINT load_operation_pair CHECK (
        (operation = 'behaviors' AND phase = 'sent') OR
        (operation = 'behaviors' AND phase = 'terminal') OR
        (operation = 'turn' AND phase = 'sent') OR
        (operation = 'turn' AND phase = 'terminal') OR
        (operation = 'delete-session' AND phase = 'sent') OR
        (operation = 'delete-session' AND phase = 'terminal') OR
        (operation = 'cron-setup' AND phase = 'sent') OR
        (operation = 'cron-setup' AND phase = 'terminal') OR
        (operation = 'cron-tick' AND phase = 'sent') OR
        (operation = 'cron-tick' AND phase = 'terminal') OR
        (operation = 'attachment' AND phase = 'put') OR
        (operation = 'attachment' AND phase = 'read')
    )
);
CREATE INDEX witness_load_events_by_run ON witness_load_events (run_id, operation, phase);
GRANT INSERT (run_id, subject, operation, phase, observer, detail_json, content_bytes)
    ON witness_load_events TO lash_witness;

-- The FIG-3790 fault controller's ledger (FIG-4169). The controller appends
-- its intent before each fault, the injection with the busy work it hit,
-- and the recovery it observed, all on this database's clock, so the load
-- verifier can place every load event before, during or after each fault.
-- `campaign` rows start and end the fault phase the driver runs under. The
-- rolling-upgrade campaign (FIG-3805 phase B) appends its steps to the same
-- ledger: the half roll, the rollback, the roll, finalize and the fence.
CREATE TABLE witness_load_faults (
    fault_event_id BIGSERIAL PRIMARY KEY,
    run_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    phase TEXT NOT NULL,
    target TEXT NOT NULL,
    detail_json TEXT NOT NULL,
    recorded_at_us BIGINT NOT NULL DEFAULT witness_clock_us(),
    CONSTRAINT load_kind_pair CHECK (
        (kind = 'campaign' AND phase = 'started') OR
        (kind = 'campaign' AND phase = 'complete') OR
        (kind = 'campaign' AND phase = 'failed') OR
        (kind = 'worker-kill' AND phase = 'intent') OR
        (kind = 'worker-kill' AND phase = 'injected') OR
        (kind = 'worker-kill' AND phase = 'recovered') OR
        (kind = 'worker-kill' AND phase = 'failed') OR
        (kind = 'restate-restart' AND phase = 'intent') OR
        (kind = 'restate-restart' AND phase = 'injected') OR
        (kind = 'restate-restart' AND phase = 'recovered') OR
        (kind = 'restate-restart' AND phase = 'failed') OR
        (kind = 'rolling-deploy' AND phase = 'intent') OR
        (kind = 'rolling-deploy' AND phase = 'injected') OR
        (kind = 'rolling-deploy' AND phase = 'recovered') OR
        (kind = 'rolling-deploy' AND phase = 'failed') OR
        (kind = 'half-roll' AND phase = 'intent') OR
        (kind = 'half-roll' AND phase = 'injected') OR
        (kind = 'half-roll' AND phase = 'recovered') OR
        (kind = 'half-roll' AND phase = 'failed') OR
        (kind = 'rollback' AND phase = 'intent') OR
        (kind = 'rollback' AND phase = 'injected') OR
        (kind = 'rollback' AND phase = 'recovered') OR
        (kind = 'rollback' AND phase = 'failed') OR
        (kind = 'roll' AND phase = 'intent') OR
        (kind = 'roll' AND phase = 'injected') OR
        (kind = 'roll' AND phase = 'recovered') OR
        (kind = 'roll' AND phase = 'failed') OR
        (kind = 'finalize' AND phase = 'intent') OR
        (kind = 'finalize' AND phase = 'injected') OR
        (kind = 'finalize' AND phase = 'recovered') OR
        (kind = 'finalize' AND phase = 'failed') OR
        (kind = 'fence' AND phase = 'intent') OR
        (kind = 'fence' AND phase = 'injected') OR
        (kind = 'fence' AND phase = 'recovered') OR
        (kind = 'fence' AND phase = 'failed')
    )
);
CREATE INDEX witness_load_faults_by_run ON witness_load_faults (run_id, fault_event_id);
GRANT INSERT (run_id, kind, phase, target, detail_json)
    ON witness_load_faults TO lash_witness;

GRANT SELECT ON ALL TABLES IN SCHEMA public TO lash_witness;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO lash_witness;
