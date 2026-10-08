-- lash-postgres-store schema, component version 1.
--
-- Generated artifact. These bytes are exactly the DDL `lash migrate`
-- executes to provision a database; `PostgresStorage::schema_ddl()` returns
-- this file verbatim. A host that provisions the database itself must copy
-- this file byte-for-byte into its own migration tooling rather than
-- transcribe it: lash verifies the resulting structure at open and rejects a
-- mismatch with a per-object diff. Workers never execute it: an open runs no
-- DDL at all.
--
-- The component schema is a reject-and-recreate boundary except for explicit
-- migrations implemented by the owning build. Every statement in this artifact
-- is creation-only and idempotent, so applying the file twice is a no-op, and
-- nothing here is schema-qualified, so the file provisions into whichever schema
-- the session's `search_path` resolves.

CREATE TABLE IF NOT EXISTS lash_schema_versions (
    component TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    min_reader INTEGER NOT NULL,
    CONSTRAINT ck_lash_schema_versions_stamp
        CHECK (version >= 1 AND min_reader >= 1 AND min_reader <= version)
);

-- The migration ledger (FIG-3816, FIG-3817): `lashctl migrate` and
-- `lashctl finalize` record each step here so a rerun is a no-op and an
-- interrupted run resumes from the rows that committed. Runtime code never
-- reads or writes it — it is the operational record, and the migrate runner
-- is its only writer. Expand and contract steps commit atomically, so their
-- rows are always `applied`. A backfill runs in batches after finalize: its
-- row is `running` from its first batch, carries the key its last committed
-- batch ended at and the rows the backfill has rewritten, and becomes
-- `applied` with the batch that finds nothing left. Contract waits for every
-- backfill of its release to read `applied`.
CREATE TABLE IF NOT EXISTS lash_migrations (
    phase TEXT NOT NULL
        CONSTRAINT ck_lash_migrations_phase
        CHECK (phase IN ('expand', 'backfill', 'contract')),
    migration TEXT NOT NULL,
    release TEXT NOT NULL,
    state TEXT NOT NULL
        CONSTRAINT ck_lash_migrations_state
        CHECK (state IN ('running', 'applied')),
    from_version INTEGER,
    to_version INTEGER NOT NULL,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    backfill_cursor TEXT,
    backfill_rows BIGINT,
    PRIMARY KEY (phase, migration),
    CONSTRAINT ck_lash_migrations_backfill_progress
        CHECK ((phase = 'backfill' AND backfill_rows IS NOT NULL AND backfill_rows >= 0)
            OR (phase <> 'backfill' AND backfill_cursor IS NULL AND backfill_rows IS NULL))
);

-- The durable-format generation every writer in the fleet emits (ADR 0106
-- §1 `F`). The installer seeds the row below; opens read the recorded
-- generation, never record one, and keep writing it until a move of `F`
-- changes it. One row, like the other deployment-scoped singletons in this
-- schema.
CREATE TABLE IF NOT EXISTS lash_fleet_format (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    format_version INTEGER NOT NULL,
    CONSTRAINT ck_fleet_format_singleton CHECK (singleton)
);

-- The fleet record's per-plugin writer ranges (FIG-4746), beside `F`: the
-- format versions the fleet permits each plugin's state and config namespaces
-- to be published in. Its rows are read and written only under the
-- `lash_fleet_format` row's lock. A publishing transaction holds that row
-- `FOR SHARE` and admits every namespace it writes against its plugin's row;
-- finalize holds it `FOR UPDATE` and moves the ranges in the transaction that
-- moves `F`. Bounds are validated when read, so a malformed row refuses typed.
CREATE TABLE IF NOT EXISTS lash_fleet_plugin_writers (
    plugin_id TEXT PRIMARY KEY,
    min_format INTEGER NOT NULL,
    max_format INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_blobs (
    hash TEXT PRIMARY KEY,
    content BYTEA NOT NULL
);

-- One row per published head revision a session still retains (FIG-4731).
-- The name of a state is (session_id, head_revision). Membership is the
-- retained-revisions relation every reclaimer roots what it keeps in. Each
-- row carries the head document its revision was published with: the
-- configuration a fork of it copies.
CREATE TABLE IF NOT EXISTS lash_session_revisions (
    session_id TEXT NOT NULL,
    head_revision BIGINT NOT NULL,
    leaf_node_id TEXT,
    checkpoint_ref TEXT,
    head_json TEXT NOT NULL,
    PRIMARY KEY (session_id, head_revision)
);
CREATE INDEX IF NOT EXISTS idx_lash_session_revisions_leaf
    ON lash_session_revisions(leaf_node_id);
CREATE INDEX IF NOT EXISTS idx_lash_session_revisions_checkpoint_ref
    ON lash_session_revisions(checkpoint_ref);

-- The current revision is one pointer; per-revision facts live in revisions.
CREATE TABLE IF NOT EXISTS lash_session_head (
    session_id TEXT PRIMARY KEY,
    head_revision BIGINT NOT NULL,
    FOREIGN KEY (session_id, head_revision)
        REFERENCES lash_session_revisions(session_id, head_revision)
        DEFERRABLE INITIALLY DEFERRED
);

-- One row per target a host asked a session to retain: an input, a turn or a
-- head revision. A pin names a target, never a state, so it can be written
-- before the target exists. Pins are deleted with their session.
CREATE TABLE IF NOT EXISTS lash_pins (
    session_id TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    PRIMARY KEY (session_id, target_kind, target_id),
    CONSTRAINT ck_pins_target_kind CHECK (target_kind IN ('input', 'turn', 'revision'))
);

-- Indexed projection of exact checkpoint-manifest component edges. Each row is
-- owned by the session whose retained revision owns the checkpoint root named by
-- checkpoint_ref. Session deletion removes an unreferenced root and cascades
-- its edges in the same transaction. The
-- component foreign key only prevents dangling edges; it is not a second
-- reclaim trigger. This is reference data, never a cached reference count.
CREATE TABLE IF NOT EXISTS lash_checkpoint_blob_refs (
    checkpoint_ref TEXT NOT NULL REFERENCES lash_blobs(hash) ON DELETE CASCADE,
    blob_ref TEXT NOT NULL REFERENCES lash_blobs(hash),
    PRIMARY KEY (checkpoint_ref, blob_ref)
);
CREATE INDEX IF NOT EXISTS idx_lash_checkpoint_blob_refs_blob_ref
    ON lash_checkpoint_blob_refs(blob_ref, checkpoint_ref);

-- The recovery leader lease (ADR 0109 §1.6): one row per engine authority
-- naming the deployment that runs the leader-only recovery duties. Load
-- control, never a fence; every comparison runs on the database clock.
CREATE TABLE IF NOT EXISTS lash_recovery_leader (
    name TEXT PRIMARY KEY,
    holder_id TEXT NOT NULL,
    generation_rank BIGINT NOT NULL,
    term BIGINT NOT NULL,
    elected_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_deleted_sessions (
    session_id TEXT PRIMARY KEY,
    created_at_ms BIGINT NOT NULL,
    last_commit_at_ms BIGINT,
    head_revision BIGINT NOT NULL,
    relation_kind TEXT NOT NULL,
    parent_session_id TEXT
);

CREATE TABLE IF NOT EXISTS lash_graph_nodes (
    session_id TEXT NOT NULL,
    node_id TEXT PRIMARY KEY,
    parent_node_id TEXT,
    generation BIGINT NOT NULL CONSTRAINT ck_graph_nodes_generation CHECK (generation >= 0),
    frame_node_id TEXT NOT NULL,
    node_json TEXT NOT NULL,
    body_bytes BIGINT NOT NULL CONSTRAINT ck_graph_nodes_body_bytes CHECK (body_bytes >= 0),
    tombstoned BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (session_id, generation)
);
CREATE INDEX IF NOT EXISTS idx_lash_graph_nodes_parent
    ON lash_graph_nodes(parent_node_id);

CREATE TABLE IF NOT EXISTS lash_fork_lineage (
    session_id TEXT NOT NULL,
    ancestor_session_id TEXT NOT NULL,
    fork_node_id TEXT NOT NULL,
    fork_generation BIGINT NOT NULL CONSTRAINT ck_fork_lineage_fork_generation CHECK (fork_generation >= 0),
    PRIMARY KEY (session_id, ancestor_session_id)
);

CREATE TABLE IF NOT EXISTS lash_session_meta (
    session_id TEXT PRIMARY KEY,
    session_state_version INTEGER,
    created_at_ms BIGINT NOT NULL DEFAULT 0,
    last_commit_at_ms BIGINT,
    relation_kind TEXT NOT NULL,
    parent_session_id TEXT,
    caused_by_kind TEXT,
    caused_by_session_id TEXT,
    caused_by_turn_id TEXT,
    caused_by_effect_id TEXT,
    caused_by_call_id TEXT,
    caused_by_process_id TEXT,
    -- caused_by_process_event_sequence is u64 in `CausalRef`; the full range
    -- does not fit signed BIGINT, so it is decimal TEXT and parsed on read.
    caused_by_process_event_sequence TEXT,
    caused_by_node_id TEXT,
    source_session_id TEXT,
    source_node_id TEXT,
    admission_base_checkpoint_ref TEXT,
    closing_intent BIGINT,
    owning_process_id TEXT,
    retention_kind TEXT NOT NULL,
    retention_last_turns BIGINT,
    -- The session's standing fault (ADR 0109 §9) and when it was recorded.
    fault_json TEXT,
    fault_at_ms BIGINT CONSTRAINT ck_session_meta_fault CHECK ((fault_json IS NULL) = (fault_at_ms IS NULL)),
    CONSTRAINT ck_session_meta_retention CHECK ((retention_kind IN ('until_gc', 'head_only') AND retention_last_turns IS NULL) OR (retention_kind = 'last_turns' AND retention_last_turns > 0)),
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'session_node')),
    CONSTRAINT ck_session_meta_relation_family CHECK ((relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))),
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'session_node'))))
);

-- The fault listing (ADR 0109 §9).
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_fault
    ON lash_session_meta(session_id)
    WHERE fault_json IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_catalog
    ON lash_session_meta(created_at_ms, session_id);
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_state_version
    ON lash_session_meta(session_state_version, session_id);

CREATE TABLE IF NOT EXISTS lash_session_meta_pending_observer_intents (
    session_id TEXT NOT NULL,
    process_index BIGINT NOT NULL,
    process_id TEXT NOT NULL,
    PRIMARY KEY (session_id, process_id),
    UNIQUE (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES lash_session_meta(session_id) ON DELETE CASCADE
);

-- The turn feed's staging order: a change takes the next value when its
-- transaction writes it, without a row lock. Its feed sequence (`change_seq`)
-- is assigned after it commits (FIG-5276).
CREATE SEQUENCE IF NOT EXISTS lash_turn_change_staging;

CREATE TABLE IF NOT EXISTS lash_runtime_turn_commits (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    turn_commit_hash TEXT NOT NULL,
    result_json TEXT NOT NULL,
    outcome_code TEXT CONSTRAINT ck_runtime_turn_commits_outcome CHECK (outcome_code IN ('completed', 'frame_switch', 'cancelled', 'failed_incomplete', 'failed_invalid_input', 'failed_max_turns', 'failed_tool_failure', 'failed_provider_error', 'failed_context_overflow', 'failed_plugin_abort', 'failed_runtime_error', 'failed_agent_frame_switch_limit', 'failed_submitted_error', 'failed_tool_error')),
    change_seq BIGINT UNIQUE CONSTRAINT ck_runtime_turn_commits_change_seq CHECK (change_seq > 0),
    staged_seq BIGINT NOT NULL DEFAULT nextval('lash_turn_change_staging'),
    committed_at_ms BIGINT NOT NULL,
    failure_evidence BOOLEAN NOT NULL,
    -- The head revision the commit published: the session's commit order,
    -- which its committed-turn read pages by (FIG-5297).
    head_revision BIGINT NOT NULL CONSTRAINT ck_runtime_turn_commits_head_revision CHECK (head_revision > 0),
    request_identity_hash TEXT,
    requested_node_count BIGINT,
    identity_encoding_version INTEGER,
    PRIMARY KEY (session_id, turn_id),
    -- Identity families: all-NULL is a plain commit; hash+version+count is an
    -- append identity; hash+version without a count is a semantic-boundary
    -- identity (FIG-2480). A count without a hash is representable nowhere.
    CONSTRAINT ck_runtime_turn_commits_identity CHECK ((request_identity_hash IS NULL) = (identity_encoding_version IS NULL) AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_turn_commits_failure_evidence
    ON lash_runtime_turn_commits(session_id, committed_at_ms, turn_id)
    WHERE failure_evidence;


CREATE INDEX IF NOT EXISTS idx_lash_runtime_turn_commits_change_seq
    ON lash_runtime_turn_commits(change_seq) WHERE outcome_code IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lash_runtime_turn_commits_unsequenced
    ON lash_runtime_turn_commits(staged_seq) WHERE change_seq IS NULL;
-- A session's committed turns in commit order (FIG-5297). One commit
-- publishes each revision, so no two receipts of a session share one.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_runtime_turn_commits_session_revision
    ON lash_runtime_turn_commits(session_id, head_revision);

-- The turn feed's clock: the last sequence a sequencing transaction assigned
-- to committed changes. Writers never take it.
CREATE TABLE IF NOT EXISTS lash_turn_change_clock (
    singleton INTEGER PRIMARY KEY CONSTRAINT ck_turn_change_clock_singleton CHECK (singleton = 1),
    current_seq BIGINT NOT NULL CONSTRAINT ck_turn_change_clock_current_seq CHECK (current_seq >= 0),
    retention_horizon BIGINT NOT NULL CONSTRAINT ck_turn_change_clock_retention_horizon CHECK (retention_horizon >= 0 AND retention_horizon <= current_seq)
);
INSERT INTO lash_turn_change_clock VALUES (1, 0, 0) ON CONFLICT (singleton) DO NOTHING;

-- Session faults outlive their standing state and the session's physical delete.
CREATE TABLE IF NOT EXISTS lash_session_terminal_changes (
    staged_seq BIGINT PRIMARY KEY DEFAULT nextval('lash_turn_change_staging'),
    change_seq BIGINT UNIQUE CONSTRAINT ck_session_terminal_changes_change_seq CHECK (change_seq > 0),
    session_id TEXT NOT NULL,
    fault_json TEXT,
    recorded_at_ms BIGINT NOT NULL CONSTRAINT ck_session_terminal_changes_recorded_at_ms CHECK (recorded_at_ms >= 0)
);
CREATE INDEX IF NOT EXISTS idx_lash_session_terminal_changes_session
    ON lash_session_terminal_changes(session_id, change_seq);
CREATE INDEX IF NOT EXISTS idx_lash_session_terminal_changes_unsequenced
    ON lash_session_terminal_changes(staged_seq) WHERE change_seq IS NULL;

CREATE TABLE IF NOT EXISTS lash_turn_cancel_requests (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    origin TEXT,
    reason TEXT,
    disposition TEXT NOT NULL CONSTRAINT ck_turn_cancel_requests_disposition CHECK (disposition IN ('defer', 'drop')),
    mode TEXT NOT NULL CONSTRAINT ck_turn_cancel_requests_mode CHECK (mode IN ('immediate', 'after_step')),
    intent_revision BIGINT NOT NULL CONSTRAINT ck_turn_cancel_requests_intent_revision CHECK (intent_revision >= 1),
    PRIMARY KEY (session_id, turn_id)
);


CREATE TABLE IF NOT EXISTS lash_queued_work_batches (
    enqueue_seq BIGINT NOT NULL,
    batch_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    delivery_policy TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    authority_json TEXT NOT NULL,
    merge_key TEXT,
    enqueued_at_ms BIGINT NOT NULL,
    submission_digest TEXT NOT NULL, -- Written once at admission (ADR 0101 §8).
    settled_operation_key TEXT, -- The original applying commit receipt; no separate completion marker.
    admitted_run TEXT, -- The run whose fenced admission holds the batch; NULL while open.
    admitted_by TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    terminal_cause TEXT, -- NULL while open or admitted; the tombstone's cause after.
    terminal_at_ms BIGINT,
    trace_cause_json TEXT, -- The batch's trace cause, written once at enqueue; NULL is a root cause.
    CONSTRAINT ck_queued_work_batches_payload CHECK ((payload_json::jsonb ->> 'type' = 'session_command') IS TRUE),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK (delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')),
    CONSTRAINT ck_queued_work_batches_admission_all_or_none CHECK ((admitted_run IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_queued_work_batches_terminal CHECK ((terminal_cause IS NULL AND terminal_at_ms IS NULL) OR (terminal_cause IN ('delivered', 'applied', 'cancelled', 'stale_config_revision') AND terminal_at_ms IS NOT NULL AND admitted_run IS NULL)),
    UNIQUE (session_id, source_key),
    PRIMARY KEY (session_id, enqueue_seq)
);
-- Both scans range over live batches only: a tombstone never lengthens an
-- open-work scan (ADR 0101 §8).
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_admission_order
    ON lash_queued_work_batches(session_id, admitted_run, enqueue_seq)
    WHERE terminal_cause IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_session_command_order
    ON lash_queued_work_batches(session_id, enqueued_at_ms, enqueue_seq)
    WHERE terminal_cause IS NULL;

CREATE TABLE IF NOT EXISTS lash_pending_turn_inputs (
    enqueue_seq BIGINT NOT NULL,
    input_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    ingress_json TEXT NOT NULL, -- The submitted delivery, written once (ADR 0101 §5.1).
    state TEXT NOT NULL,
    input_json TEXT NOT NULL,
    submission_digest TEXT NOT NULL,
    enqueued_at_ms BIGINT NOT NULL,
    admitted_run TEXT, -- The run whose fenced admission holds the input; NULL while open.
    admitted_by TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    run_spec_hash TEXT,
    terminal_at_ms BIGINT, -- When the input's tombstone was written; NULL until then.
    trace_cause_json TEXT, -- The submission's trace cause, written once at acceptance; NULL is a root cause.
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK (((ingress_json::jsonb ->> 'scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR ((ingress_json::jsonb ->> 'scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_admission_all_or_none CHECK ((admitted_run IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_pending_turn_inputs_settled_unadmitted CHECK (admitted_run IS NULL OR state NOT IN ('cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_terminal_at CHECK ((state IN ('cancelled', 'completed')) = (terminal_at_ms IS NOT NULL)),
    UNIQUE (session_id, source_key),
    PRIMARY KEY (session_id, enqueue_seq)
);
-- All undelivered inputs, including ones a run already holds. Settled rows
-- cannot lengthen an open-input scan, and the key retains enqueue order.
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_open_state
    ON lash_pending_turn_inputs(session_id, enqueue_seq)
    WHERE state IN ('pending_active', 'deferred_next_turn');
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_accepted_state
    ON lash_pending_turn_inputs(session_id, enqueue_seq)
    WHERE state IN ('accepted');
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_bound_run
    ON lash_pending_turn_inputs(session_id, admitted_run)
    WHERE admitted_run IS NOT NULL;

-- One row per run spec a session's inputs carry (FIG-3838), interned once per
-- hash in the transaction that admits the input naming it, immutable, and
-- owned by the session: reclaimed when the session is deleted. The default
-- spec is never interned; its inputs carry a NULL `run_spec_hash`.
CREATE TABLE IF NOT EXISTS lash_session_run_specs (
    session_id TEXT NOT NULL,
    spec_hash TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    PRIMARY KEY (session_id, spec_hash)
);

-- The session ingress's one per-session order (ADR 0101 §5, amended): both
-- admission tables, `pending_turn_inputs` and `queued_work_batches`, draw
-- their `enqueue_seq` from this counter under the session history lock.
CREATE TABLE IF NOT EXISTS lash_session_ingress_sequence (
    session_id TEXT NOT NULL PRIMARY KEY,
    enqueue_seq BIGINT NOT NULL,
    CONSTRAINT ck_session_ingress_sequence_positive CHECK (enqueue_seq > 0)
);

-- The logical-run family (FIG-3600 S7). `lash_session_runs` holds one row
-- per (session, run) admitted work ran under, with the exact result of
-- the run's admission, committed in the admission's own transaction, and the
-- run's terminal evidence once it has one: the `terminal_*` columns are set
-- together, exactly once, and `terminal_kind` is the kind the cause derives,
-- kept for the indexes and checked against it. `lash_session_run_inputs` binds each accepted input to the
-- run that executes it. `lash_control_intents` records a session's close; a
-- `close_session` row outlives its session as the deletion tombstone.
CREATE TABLE IF NOT EXISTS lash_session_runs (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    admission_json TEXT,
    terminal_kind TEXT,
    terminal_cause_json TEXT,
    terminal_head_revision BIGINT,
    terminal_window_json TEXT,
    terminal_at_ms BIGINT,
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_session_runs_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL)),
    CONSTRAINT ck_session_runs_terminal_kind CHECK (terminal_kind IS NULL OR terminal_kind = CASE (terminal_cause_json::jsonb ->> 'cause')
        WHEN 'committed' THEN CASE
            WHEN (terminal_cause_json::jsonb #> '{outcome,finished}') IS NOT NULL THEN 'answered'
            WHEN (terminal_cause_json::jsonb #> '{outcome,agent_frame_switch}') IS NOT NULL THEN 'answered'
            WHEN (terminal_cause_json::jsonb #> '{outcome,stopped,cancelled}') IS NOT NULL THEN 'cancelled'
            ELSE 'failed' END
        WHEN 'refused' THEN 'failed'
        WHEN 'commands_applied' THEN 'answered'
        WHEN 'cancelled' THEN 'cancelled'
        WHEN 'operator_cancelled' THEN 'cancelled'
        WHEN 'forked' THEN 'cancelled'
        WHEN 'session_deleted' THEN 'cancelled'
        ELSE '' END)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_lash_session_runs_unfinished
    ON lash_session_runs(session_id)
    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_session_runs_open
    ON lash_session_runs(session_id, run)
    WHERE terminal_kind IS NULL;

CREATE TABLE IF NOT EXISTS lash_session_run_inputs (
    session_id TEXT NOT NULL,
    input_id TEXT NOT NULL,
    run TEXT NOT NULL,
    PRIMARY KEY (session_id, input_id)
);

CREATE TABLE IF NOT EXISTS lash_control_intents (
    intent_id BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    format BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_control_intents_kind CHECK (kind IN ('close_session')),
    kind_json TEXT NOT NULL,
    state TEXT NOT NULL CONSTRAINT ck_control_intents_state CHECK (state IN ('pending', 'acknowledged')),
    state_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_lash_control_intents_session
    ON lash_control_intents(session_id, kind);

CREATE TABLE IF NOT EXISTS lash_attachment_referrer_edges (
    attachment_id TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_attachment CHECK (char_length(attachment_id) > 0),
    referrer_kind TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_kind CHECK (referrer_kind IN ('session', 'upload', 'execution', 'start_input', 'process_record', 'source')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_id CHECK (char_length(referrer_id) > 0),
    PRIMARY KEY (attachment_id, referrer_kind, referrer_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_referrer_edges_referrer
    ON lash_attachment_referrer_edges(referrer_kind, referrer_id);

CREATE TABLE IF NOT EXISTS lash_attachment_pending_writes (
    write_id      TEXT PRIMARY KEY CONSTRAINT ck_attachment_pending_writes_write_id CHECK (char_length(write_id) = 32),
    attachment_id TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_attachment CHECK (char_length(attachment_id) > 0),
    referrer_kind TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_kind CHECK (referrer_kind IN ('session', 'upload', 'execution', 'start_input', 'process_record', 'source')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_id CHECK (char_length(referrer_id) > 0),
    begun_at_ms   BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_pending_writes_attachment
    ON lash_attachment_pending_writes(attachment_id);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_pending_writes_referrer
    ON lash_attachment_pending_writes(referrer_kind, referrer_id);

CREATE TABLE IF NOT EXISTS lash_attachment_uploads (
    attachment_id TEXT PRIMARY KEY CONSTRAINT ck_attachment_uploads_attachment CHECK (char_length(attachment_id) > 0),
    written_at_ms BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_attachment_condemnations (
    attachment_id     TEXT PRIMARY KEY,
    phase             TEXT NOT NULL CONSTRAINT ck_attachment_condemnations_phase CHECK (phase IN ('condemned', 'deleting')),
    write_token       TEXT REFERENCES lash_attachment_pending_writes(write_id) ON DELETE SET NULL,
    next_delete_at_ms BIGINT NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_next_delete CHECK (next_delete_at_ms >= 0),
    sweep_generation  BIGINT NOT NULL,
    delete_attempts   INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_delete_attempts CHECK (delete_attempts >= 0),
    last_delete_error TEXT,
    stall_reason      TEXT CONSTRAINT ck_attachment_condemnations_stall_reason CHECK (stall_reason IN ('attempts_exhausted', 'refused')),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (write_token IS NULL OR phase = 'condemned'),
    CONSTRAINT ck_attachment_condemnations_failure_pairing CHECK ((delete_attempts = 0) = (last_delete_error IS NULL)),
    CONSTRAINT ck_attachment_condemnations_stall_attempts CHECK (stall_reason IS NULL OR delete_attempts > 0)
);
CREATE TABLE IF NOT EXISTS lash_attachment_sweep_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    generation BIGINT NOT NULL,
    CONSTRAINT ck_attachment_sweep_clock_singleton CHECK (singleton)
);

-- The process feed's staging order and clock, as the turn feed's (FIG-5276).
CREATE SEQUENCE IF NOT EXISTS lash_process_change_staging;

CREATE TABLE IF NOT EXISTS lash_process_change_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    current_seq BIGINT NOT NULL,
    tombstone_compaction_horizon BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT ck_process_change_clock_singleton CHECK (singleton)
);
-- Opaque process identifiers use byte order on every host locale. The primary
-- key, live-worklist index, MAX, and keyset bounds inherit this collation.
--
-- `record_json` is the process record and the row's one authority for the
-- process's lifecycle. `status`, `last_event_sequence` and
-- `cancel_requested_at_ms` are the database's own projections of it, kept
-- for the indexes below: no statement can write them, so none can make them
-- disagree with the record. A record whose lifecycle does not name a status
-- generates NULL and is refused by the column's NOT NULL.
--
-- PostgreSQL's JSON functions refuse a document holding the escape of a NUL
-- character, which a record's strings may hold. Each projection therefore
-- reads the record with that escape (a backslash, chr(92), then `u0000`)
-- respelled as the escape of U+0001. The sequence occurs only inside a
-- string and the respelling has its length, so the document's structure and
-- every key and label a projection reads are unchanged; the stored
-- `record_json` is never rewritten.
CREATE TABLE IF NOT EXISTS lash_processes (
    process_id TEXT COLLATE "C" PRIMARY KEY,
    start_key TEXT COLLATE "C",
    originator_id TEXT NOT NULL,
    identity_kind TEXT NOT NULL,
    identity_label TEXT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    last_event_sequence BIGINT NOT NULL GENERATED ALWAYS AS ((replace(record_json, chr(92) || 'u0000', chr(92) || 'u0001')::json ->> 'last_event_sequence')::bigint) STORED,
    change_seq BIGINT,
    staged_seq BIGINT NOT NULL DEFAULT nextval('lash_process_change_staging'),
    -- Saves since the row was last sequenced: the feed's clock moves once
    -- per save, and the row takes the position of its last.
    unsequenced_saves INTEGER NOT NULL DEFAULT 1 CONSTRAINT ck_processes_unsequenced_saves CHECK (unsequenced_saves >= 1),
    status TEXT NOT NULL GENERATED ALWAYS AS (
        CASE replace(record_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{lifecycle,state}'
            WHEN 'running' THEN 'running'
            WHEN 'waiting' THEN 'waiting'
            WHEN 'terminal' THEN CASE replace(record_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{lifecycle,outcome,type}'
                WHEN 'abandoned' THEN 'abandoned'
                WHEN 'settled' THEN CASE replace(record_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{lifecycle,outcome,output,outcome,status}'
                    WHEN 'success' THEN 'completed'
                    WHEN 'failure' THEN 'failed'
                    WHEN 'cancelled' THEN 'cancelled'
                END
            END
        END
    ) STORED,
    lifetime TEXT NOT NULL,
    lifetime_scope_kind TEXT,
    lifetime_scope_id TEXT COLLATE "C",
    cancel_requested_at_ms BIGINT GENERATED ALWAYS AS ((replace(record_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{cancel_request,requested_at_ms}')::bigint) STORED,
    state_rev BIGINT NOT NULL DEFAULT 0,
    driver_json TEXT,
    cascade_cursor TEXT,
    written_epoch BIGINT,
    published_event_sequence BIGINT NOT NULL DEFAULT 0,
    record_json TEXT NOT NULL,
    consumer_hold_key TEXT,
    consumer_hold_scope_kind TEXT,
    consumer_hold_scope_id TEXT COLLATE "C",
    consumer_hold_cancels BOOLEAN,
    CONSTRAINT ck_processes_consumer_hold CHECK ((consumer_hold_key IS NULL) = (consumer_hold_scope_kind IS NULL) AND (consumer_hold_key IS NULL) = (consumer_hold_scope_id IS NULL)),
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned')),
    CONSTRAINT ck_processes_cancel_requested_at CHECK ((cancel_requested_at_ms IS NULL) = ((replace(record_json, chr(92) || 'u0000', chr(92) || 'u0001')::json -> 'cancel_request') IS NULL)),
    CONSTRAINT ck_processes_lifetime CHECK (lifetime IN ('until', 'detached')),
    CONSTRAINT ck_processes_lifetime_scope CHECK ((lifetime = 'detached' AND lifetime_scope_kind IS NULL AND lifetime_scope_id IS NULL) OR (lifetime = 'until' AND lifetime_scope_kind IN ('turn', 'session_operation', 'process', 'session') AND lifetime_scope_id IS NOT NULL))
);

-- A held row names the scope whose close releases it (ADR 0116 §3.6).
CREATE INDEX IF NOT EXISTS idx_lash_processes_consumer_hold_owner
    ON lash_processes(consumer_hold_scope_kind, consumer_hold_scope_id)
    WHERE consumer_hold_key IS NOT NULL;

-- The consumer holds whose call was abandoned before it consumed its child
-- (ADR 0116 §3.4): a registration under a marked key is refused, and the
-- owning scope's close forgets the marks.
CREATE TABLE IF NOT EXISTS lash_abandoned_consumer_holds (
    hold_key TEXT COLLATE "C" PRIMARY KEY,
    owner_scope_kind TEXT NOT NULL,
    owner_scope_id TEXT COLLATE "C" NOT NULL,
    abandoned_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_abandoned_consumer_holds_owner
    ON lash_abandoned_consumer_holds(owner_scope_kind, owner_scope_id);

-- A start key maps to the one retained process minted for it (ADR 0107).
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_processes_start_key
    ON lash_processes(start_key) WHERE start_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lash_processes_status
    ON lash_processes(status);
CREATE INDEX IF NOT EXISTS idx_lash_processes_non_terminal
    ON lash_processes(process_id) WHERE status IN ('running', 'waiting');
CREATE INDEX IF NOT EXISTS idx_lash_processes_change_seq
    ON lash_processes(change_seq);
CREATE INDEX IF NOT EXISTS idx_lash_processes_unsequenced
    ON lash_processes(staged_seq) WHERE change_seq IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_processes_originator
    ON lash_processes(originator_id);
CREATE INDEX IF NOT EXISTS idx_lash_processes_identity
    ON lash_processes(identity_kind, identity_label);
CREATE INDEX IF NOT EXISTS idx_lash_processes_created
    ON lash_processes(created_at_ms);
CREATE INDEX IF NOT EXISTS idx_lash_processes_updated
    ON lash_processes(updated_at_ms);
-- The pending-cancel sweep's scan: rows whose cancel request is older than a
-- horizon and whose outcome is still open. The predicate is the negation of
-- the terminal statuses. It must stay
-- byte-identical to `nonterminal_process_status("status")`, or the planner
-- refuses the partial index.
CREATE INDEX IF NOT EXISTS idx_lash_processes_pending_cancel
    ON lash_processes(cancel_requested_at_ms, process_id)
    WHERE cancel_requested_at_ms IS NOT NULL
      AND status NOT IN ('completed', 'failed', 'cancelled', 'abandoned');
CREATE INDEX IF NOT EXISTS idx_lash_processes_lifetime_scope
    ON lash_processes(lifetime_scope_kind, lifetime_scope_id, process_id);
-- The scope-close sweep's only scan. The predicate names the live statuses
-- rather than a NOT IN so a status added later cannot silently widen the
-- index; it is exactly LIVE_PROCESS_STATUS_LABELS.
CREATE INDEX IF NOT EXISTS idx_lash_processes_lifetime_pending
    ON lash_processes(lifetime_scope_kind, lifetime_scope_id, process_id)
    WHERE lifetime = 'until'
      AND cancel_requested_at_ms IS NULL
      AND status IN ('running', 'waiting');

CREATE TABLE IF NOT EXISTS lash_process_events (
    process_id TEXT COLLATE "C" NOT NULL,
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    idempotency_key TEXT,
    event_json TEXT NOT NULL,
    released_payload_digest TEXT,
    PRIMARY KEY (process_id, sequence),
    FOREIGN KEY (process_id) REFERENCES lash_processes(process_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_process_events_key
    ON lash_process_events(process_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- The event prefix a host released of a still-retained process (FIG-3482):
-- events at or below `released_through` keep their row without a payload,
-- and reads below it are refused typed.
CREATE TABLE IF NOT EXISTS lash_process_event_horizons (
    process_id TEXT COLLATE "C" PRIMARY KEY,
    released_through BIGINT NOT NULL CONSTRAINT ck_process_event_horizons_positive CHECK (released_through > 0),
    FOREIGN KEY (process_id) REFERENCES lash_processes(process_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS lash_process_observers (
    session_id TEXT NOT NULL,
    process_id TEXT COLLATE "C" NOT NULL,
    PRIMARY KEY (session_id, process_id),
    FOREIGN KEY (process_id) REFERENCES lash_processes(process_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lash_process_observers_process
    ON lash_process_observers(process_id, session_id);

CREATE TABLE IF NOT EXISTS lash_process_tombstones (
    process_id TEXT COLLATE "C" PRIMARY KEY,
    terminal_label TEXT NOT NULL,
    pruned_at_ms BIGINT NOT NULL,
    pruned_change_seq BIGINT,
    staged_seq BIGINT NOT NULL DEFAULT nextval('lash_process_change_staging'),
    CONSTRAINT ck_process_tombstones_terminal_label CHECK (terminal_label IN ('completed', 'failed', 'cancelled', 'abandoned'))
);
CREATE INDEX IF NOT EXISTS idx_lash_process_tombstones_change
    ON lash_process_tombstones(pruned_change_seq);
CREATE INDEX IF NOT EXISTS idx_lash_process_tombstones_unsequenced
    ON lash_process_tombstones(staged_seq) WHERE pruned_change_seq IS NULL;

-- One row per ended parent scope, keyed by the scope itself rather than by a
-- process row: a turn-scoped parent has no process row at all, and a
-- process-scoped parent's row may be pruned before its children settle.
CREATE TABLE IF NOT EXISTS lash_parent_end_plans (
    parent_kind TEXT NOT NULL,
    parent_id TEXT COLLATE "C" NOT NULL,
    parent_payload TEXT NOT NULL,
    ended_at_ms BIGINT NOT NULL,
    PRIMARY KEY (parent_kind, parent_id),
    CONSTRAINT ck_parent_end_plans_kind CHECK (parent_kind IN ('turn', 'session_operation', 'process', 'session'))
);


CREATE TABLE IF NOT EXISTS lash_tool_intent_submissions (
    replay_key TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    execution_scope_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    intent_index BIGINT NOT NULL,
    payload_hash TEXT NOT NULL,
    submission_json TEXT NOT NULL,
    admitted_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_tool_intent_submissions_scope
    ON lash_tool_intent_submissions(owner, execution_scope_id, intent_index);

-- The owners whose submission ledger the retained-evidence lever reclaimed
-- (FIG-1509): each is a durably deleted session, and the fence refuses every
-- later submission under it so a reclaimed identity cannot realize again.
CREATE TABLE IF NOT EXISTS lash_tool_intent_retired_owners (
    owner TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS lash_lashlang_artifacts (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    artifact_bytes BYTEA NOT NULL,
    PRIMARY KEY (namespace, artifact_ref)
);
CREATE TABLE IF NOT EXISTS lash_artifact_referrer_edges (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'start', 'execution', 'host_pin', 'source')),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_id CHECK (char_length(referrer_id) > 0),
    PRIMARY KEY (namespace, artifact_ref, referrer_kind, referrer_id),
    FOREIGN KEY (namespace, artifact_ref) REFERENCES lash_lashlang_artifacts(namespace, artifact_ref) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lash_artifact_referrer_edges_referrer
    ON lash_artifact_referrer_edges(referrer_kind, referrer_id);
CREATE TABLE IF NOT EXISTS lash_referrer_fences (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_referrer_fences_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'start', 'start_input', 'execution', 'host_pin', 'session', 'upload', 'source')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_referrer_fences_id CHECK (char_length(referrer_id) > 0),
    ended_at_ms   BIGINT NOT NULL,
    PRIMARY KEY (referrer_kind, referrer_id)
);
CREATE TABLE IF NOT EXISTS lash_artifact_cleanup_obligations (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_cleanup_obligations_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'start', 'start_input', 'execution', 'host_pin', 'session', 'upload', 'source')),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_cleanup_obligations_id CHECK (char_length(referrer_id) > 0),
    cleanup_json TEXT NOT NULL,
    obligation_id TEXT NOT NULL,
    obligation_state TEXT NOT NULL DEFAULT 'due',
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_artifact_cleanup_obligations_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_artifact_cleanup_obligations_obligation CHECK (((obligation_state = 'due' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'stalled' AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    PRIMARY KEY (referrer_kind, referrer_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_artifact_cleanup_obligations_id
    ON lash_artifact_cleanup_obligations(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_artifact_cleanup_obligations_due
    ON lash_artifact_cleanup_obligations(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_artifact_cleanup_obligations_stalled
    ON lash_artifact_cleanup_obligations(obligation_id)
    WHERE obligation_state = 'stalled';

-- Which lash release wrote this database, recorded so a host running store
-- preflight can answer "which release reopens this store" before wiring a
-- runtime, and so a refusal at open can name a release beside the schema
-- integers. Written on the first open of an unstamped database and advanced
-- only by a strictly newer release; never downgraded. One row, like the other
-- deployment-scoped singletons above.
CREATE TABLE IF NOT EXISTS lash_release_stamp (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    release_version TEXT NOT NULL,
    schema_versions TEXT NOT NULL,
    written_at_epoch_ms BIGINT NOT NULL,
    CONSTRAINT ck_release_stamp_singleton CHECK (singleton)
);

-- The identity of this catalog: what a session catalog registers under with
-- its turn-cancel-closure owner. Random per install, so two installations
-- that share a database and schema name are still two catalogs. One row, like
-- the other deployment-scoped singletons above.
CREATE TABLE IF NOT EXISTS lash_catalog_identity (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    catalog_id TEXT NOT NULL,
    CONSTRAINT ck_catalog_identity_singleton CHECK (singleton)
);

-- Seed rows. Every opened catalog requires them: the component version stamp,
-- the fleet epoch, the transactional clock rows, and the catalog identity.
-- `gen_random_uuid()` is core PostgreSQL, so the identity needs no extension.
INSERT INTO lash_schema_versions (component, version, min_reader)
VALUES ('lash-postgres-store', 1, 1)
ON CONFLICT (component) DO NOTHING;

-- The fleet epoch starts at the floor of the installing build's writable
-- range, so a newer build never opens a fresh store outside the rollback
-- window (ADR 0115 §2.1).
INSERT INTO lash_fleet_format (singleton, format_version)
VALUES (TRUE, 1)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_process_change_clock (
    singleton, current_seq, tombstone_compaction_horizon
) VALUES (TRUE, 0, 0)
ON CONFLICT (singleton) DO NOTHING;


INSERT INTO lash_attachment_sweep_clock (singleton, generation)
VALUES (TRUE, 0)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_catalog_identity (singleton, catalog_id)
VALUES (TRUE, gen_random_uuid()::text)
ON CONFLICT (singleton) DO NOTHING;


-- The durability engine (ruling #74; crates/lash-durable). A serving node's
-- lease: one row per node, renewed by its heartbeat, deleted by a reap.
CREATE TABLE IF NOT EXISTS lash_nodes (
    node_id TEXT PRIMARY KEY,
    boot_id TEXT NOT NULL,
    formats_json TEXT NOT NULL,
    registered_at_ms BIGINT NOT NULL,
    heartbeat_expires_at_ms BIGINT NOT NULL,
    draining BOOLEAN NOT NULL DEFAULT FALSE
);

-- One scheduling row per session and per process: the only row a claim, a
-- wake or a fence touches. `epoch` changes only by claim, reap and release,
-- and every owner write is fenced on it; an actor has mail exactly when
-- `mail_seq > acked_seq`.
CREATE TABLE IF NOT EXISTS lash_actors (
    actor_key TEXT COLLATE "C" PRIMARY KEY,
    kind TEXT NOT NULL CONSTRAINT ck_lash_actors_kind CHECK (kind IN ('session', 'process')),
    state TEXT NOT NULL CONSTRAINT ck_lash_actors_state
        CHECK (state IN ('idle', 'ready', 'owned', 'waiting', 'parked', 'terminal')),
    epoch BIGINT NOT NULL,
    owner_node TEXT,
    owner_boot TEXT,
    ready_at_ms BIGINT,
    next_due_ms BIGINT,
    formats TEXT NOT NULL,
    state_revision BIGINT NOT NULL,
    mail_seq BIGINT NOT NULL,
    acked_seq BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    park_json TEXT,
    failed_activations BIGINT NOT NULL DEFAULT 0,
    claimed_revision BIGINT,
    CONSTRAINT ck_lash_actors_parked CHECK (state <> 'parked' OR park_json IS NOT NULL),
    CONSTRAINT ck_lash_actors_owned CHECK ((state = 'owned') = (owner_node IS NOT NULL)),
    CONSTRAINT ck_lash_actors_owner_boot CHECK ((owner_node IS NULL) = (owner_boot IS NULL)),
    CONSTRAINT ck_lash_actors_ready CHECK ((state = 'ready') = (ready_at_ms IS NOT NULL)),
    CONSTRAINT ck_lash_actors_due CHECK (next_due_ms IS NULL OR state = 'waiting'),
    CONSTRAINT ck_lash_actors_mail CHECK (acked_seq <= mail_seq),
    CONSTRAINT ck_lash_actors_key_kind CHECK (
        (kind = 'session' AND substr(actor_key, 1, 2) = 's/') OR
        (kind = 'process' AND substr(actor_key, 1, 2) = 'p/'))
);
CREATE INDEX IF NOT EXISTS ix_lash_actors_ready ON lash_actors (ready_at_ms) WHERE state = 'ready';
CREATE INDEX IF NOT EXISTS ix_lash_actors_due ON lash_actors (next_due_ms)
    WHERE state = 'waiting' AND next_due_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS ix_lash_actors_owner ON lash_actors (owner_node, owner_boot)
    WHERE state = 'owned';

-- An actor's mailbox: rows only non-owners append and only the owner
-- acknowledges, each at the position the append took from `mail_seq`.
CREATE TABLE IF NOT EXISTS lash_actor_mail (
    actor_key TEXT COLLATE "C" NOT NULL REFERENCES lash_actors (actor_key),
    seq BIGINT NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    appended_at_ms BIGINT NOT NULL,
    PRIMARY KEY (actor_key, seq)
);

-- Run records (I0, FIG-5194; statements: V0, then L4): the RunLedger
-- records as rows, keyed by (owner, run, ordinal), the second fence. An
-- execution's start commits before its body runs; one outcome per call.
CREATE TABLE IF NOT EXISTS lash_run_records (
    owner_key TEXT COLLATE "C" NOT NULL,
    run_seq BIGINT NOT NULL,
    ordinal BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_run_records_kind
        CHECK (kind IN ('admit', 'x_start', 'x_outcome', 'x_wait', 'decide', 'present', 'retry')),
    call_id TEXT,
    record_json TEXT NOT NULL,
    written_epoch BIGINT NOT NULL,
    PRIMARY KEY (owner_key, run_seq, ordinal)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_lash_run_records_outcome
    ON lash_run_records (owner_key, run_seq, call_id)
    WHERE kind = 'x_outcome' AND call_id IS NOT NULL;

-- Session close (L6b, FIG-5176): a closing session's last step done, each
-- step its own fenced transaction (ADR 0132 §12). After the 'tombstone'
-- step the row is the session's tombstone.
CREATE TABLE IF NOT EXISTS lash_session_close (
    session_id TEXT COLLATE "C" PRIMARY KEY,
    done_step TEXT CONSTRAINT ck_session_close_step
        CHECK (done_step IN ('cancel', 'revoke', 'end_scope', 'artifacts', 'tombstone')),
    begun_at_ms BIGINT NOT NULL,
    written_epoch BIGINT NOT NULL
);

-- A session's scopes whose cascade still has Until children to mark (L6b,
-- FIG-5176; ADR 0132 §11): written in the transaction that ended the turn,
-- deleted in the one that marks its last batch.
CREATE TABLE IF NOT EXISTS lash_session_scope_ends (
    session_id TEXT COLLATE "C" NOT NULL,
    scope_key TEXT COLLATE "C" NOT NULL,
    begun_at_ms BIGINT NOT NULL,
    written_epoch BIGINT NOT NULL,
    PRIMARY KEY (session_id, scope_key)
);

-- VM snapshots (I0, FIG-5194; statements: V0, then L7): the latest snapshot
-- of each code cell ('c/...') and lashlang process ('p/...'),
-- compare-and-set on rev.
-- The operator's park feed (L6, FIG-5175): one entry per park of an actor,
-- per redrive and per end of a parked actor.
CREATE TABLE IF NOT EXISTS lash_park_events (
    seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    actor_key TEXT COLLATE "C" NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_lash_park_events_kind
        CHECK (kind IN ('parked', 'redriven', 'ended')),
    reason_json TEXT NOT NULL,
    at_ms BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_exec_snapshots (
    exec_key TEXT COLLATE "C" PRIMARY KEY,
    rev BIGINT NOT NULL CONSTRAINT ck_exec_snapshots_rev CHECK (rev >= 1),
    snapshot_ref TEXT NOT NULL,
    executable_identity TEXT NOT NULL,
    format_version BIGINT NOT NULL CONSTRAINT ck_exec_snapshots_format_version
        CHECK (format_version BETWEEN 0 AND 4294967295),
    written_epoch BIGINT NOT NULL
);

-- Prompt snapshots (P2, FIG-5256; ADR 0133 §5, §6): one audit root per
-- admitted model call, keyed by its owner (`turn:<run>` or `owned:<scope>`)
-- and its call there (FIG-5259), holding its prompt snapshot and exact
-- provider body; section text and body chunks stored once by content
-- address, and an edge from each root to every text it references. Ending a turn leaves its roots; only
-- an explicit release removes them, and a text goes with its last edge.
CREATE TABLE IF NOT EXISTS lash_prompt_snapshots (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    written_epoch BIGINT NOT NULL,
    PRIMARY KEY (session_id, owner, call)
);

CREATE TABLE IF NOT EXISTS lash_prompt_texts (
    hash TEXT PRIMARY KEY,
    text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_prompt_snapshot_texts (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    hash TEXT NOT NULL REFERENCES lash_prompt_texts(hash),
    PRIMARY KEY (session_id, owner, call, hash),
    FOREIGN KEY (session_id, owner, call)
        REFERENCES lash_prompt_snapshots(session_id, owner, call)
);

CREATE INDEX IF NOT EXISTS idx_lash_prompt_snapshot_texts_hash
    ON lash_prompt_snapshot_texts (hash);

-- Turn phase state (V0, FIG-5170; then L3): a 1:1 side table of
-- lash_session_runs while the turn is unfinished. The run row stays the one
-- admission authority (ux_lash_session_runs_unfinished); this row carries the
-- phase with what a restore resumes it from: no checkpoint while admitted and
-- one after, and the pinned model call exactly in the model phase, whose
-- attempt is phase_arg and whose call is model_calls, the count of model
-- calls the turn admitted. trace_exported records that the admission's trace
-- export is discharged (FIG-5457).
CREATE TABLE IF NOT EXISTS lash_turn_phases (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    phase TEXT NOT NULL CONSTRAINT ck_turn_phases_phase
        CHECK (phase IN ('admitted', 'model', 'tools')),
    phase_arg BIGINT,
    iteration BIGINT NOT NULL,
    checkpoint_ref TEXT,
    model_request_ref TEXT,
    model_deadline_ms BIGINT,
    model_stream_from TEXT,
    turn_deadline_ms BIGINT,
    written_epoch BIGINT NOT NULL,
    model_calls BIGINT NOT NULL DEFAULT 0,
    trace_exported BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_turn_phases_arg CHECK ((phase IN ('model', 'tools')) = (phase_arg IS NOT NULL)),
    CONSTRAINT ck_turn_phases_checkpoint CHECK ((phase = 'admitted') = (checkpoint_ref IS NULL)),
    CONSTRAINT ck_turn_phases_model CHECK (
        (phase = 'model') = (model_request_ref IS NOT NULL)
        AND (phase = 'model') = (model_deadline_ms IS NOT NULL)
        AND (phase = 'model') = (model_stream_from IS NOT NULL))
);

-- The plugin namespaces an unfinished turn's run changed (FIG-5301): the
-- entry (values address and metadata, JSON) and its matching values body,
-- NULL exactly for base values, dropped with the phase row when the turn ends.
CREATE TABLE IF NOT EXISTS lash_turn_namespaces (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    plugin TEXT NOT NULL,
    entry TEXT NOT NULL,
    body BYTEA,
    PRIMARY KEY (session_id, run, plugin)
);

-- Waits, keyed promises and timers (L5, FIG-5173; ADR 0132 §6): one row per
-- wait from its minting. The deadline is written once; a resolution is a
-- conditional update from 'pending', so the first one wins. Only
-- tool_completion and custom waits are host resolvable: their completion key
-- is the wait id, 128 random bits.
CREATE TABLE IF NOT EXISTS lash_waits (
    wait_id TEXT COLLATE "C" PRIMARY KEY,
    owner_actor TEXT COLLATE "C" NOT NULL,
    owner_scope TEXT COLLATE "C" NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_waits_kind CHECK (kind IN
        ('tool_completion', 'engine_key', 'process_terminal', 'timer', 'child_session')),
    host_resolvable BOOLEAN NOT NULL,
    target_process TEXT COLLATE "C",
    state TEXT NOT NULL CONSTRAINT ck_waits_state
        CHECK (state IN ('pending', 'resolved', 'timed_out', 'revoked')),
    deadline_ms BIGINT,
    resolution_digest TEXT,
    resolution_ref TEXT,
    resolved_at_ms BIGINT,
    created_epoch BIGINT NOT NULL,
    call_id TEXT COLLATE "C",
    tool_id TEXT COLLATE "C",
    key_name TEXT COLLATE "C",
    CONSTRAINT ck_waits_host CHECK (host_resolvable = (kind IN ('tool_completion', 'engine_key'))),
    CONSTRAINT ck_waits_target
        CHECK ((target_process IS NOT NULL) = (kind IN ('process_terminal', 'child_session'))),
    CONSTRAINT ck_waits_resolved CHECK ((state = 'resolved') = (resolution_digest IS NOT NULL)),
    CONSTRAINT ck_waits_timer_deadline CHECK (kind <> 'timer' OR deadline_ms IS NOT NULL),
    CONSTRAINT ck_waits_settled_at CHECK ((state <> 'pending') = (resolved_at_ms IS NOT NULL)),
    CONSTRAINT ck_waits_resolution_ref
        CHECK ((state = 'resolved' AND kind <> 'timer') = (resolution_ref IS NOT NULL)),
    CONSTRAINT ck_waits_call CHECK ((call_id IS NOT NULL) = (kind = 'tool_completion')
        AND (tool_id IS NOT NULL) = (kind = 'tool_completion')),
    CONSTRAINT ck_waits_key_name CHECK ((key_name IS NOT NULL) = (kind = 'engine_key'))
);

CREATE INDEX IF NOT EXISTS ix_lash_waits_owner ON lash_waits (owner_actor)
    WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS ix_lash_waits_scope ON lash_waits (owner_scope);
CREATE INDEX IF NOT EXISTS ix_lash_waits_target ON lash_waits (target_process)
    WHERE state = 'pending' AND kind = 'process_terminal';
CREATE INDEX IF NOT EXISTS ix_lash_waits_child_target ON lash_waits (target_process)
    WHERE state = 'pending' AND kind = 'child_session';
