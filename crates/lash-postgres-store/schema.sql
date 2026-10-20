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

-- A blob's encoding is stored, never sniffed from its bytes (FIG-5887 §7):
-- 0 is opaque bytes an adapter's codec owns, with no edges; 1 is a canonical
-- checkpoint graph node, whose edges are exactly its child table.
CREATE TABLE IF NOT EXISTS lash_blobs (
    hash BYTEA PRIMARY KEY,
    content BYTEA NOT NULL,
    encoding SMALLINT NOT NULL CONSTRAINT ck_blobs_encoding CHECK (encoding IN (0, 1))
);

-- One row per published head revision a session still retains (FIG-4731).
-- The name of a state is (session_id, head_revision). Membership is the
-- retained-revisions relation every reclaimer roots what it keeps in. Each
-- row names the content-addressed config its revision was published with:
-- the configuration a fork of it copies.
CREATE TABLE IF NOT EXISTS lash_session_revisions (
    session_id TEXT NOT NULL,
    head_revision BIGINT NOT NULL,
    leaf_node_id TEXT,
    checkpoint_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    config_ref BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    terminal_ordinal BIGINT CONSTRAINT ck_session_revisions_terminal_ordinal CHECK (terminal_ordinal > 0),
    PRIMARY KEY (session_id, head_revision)
);
CREATE INDEX IF NOT EXISTS idx_lash_session_revisions_leaf
    ON lash_session_revisions(leaf_node_id);
CREATE INDEX IF NOT EXISTS idx_lash_session_revisions_checkpoint_ref
    ON lash_session_revisions(checkpoint_ref);

CREATE INDEX IF NOT EXISTS idx_lash_session_revisions_config_ref
    ON lash_session_revisions(config_ref);
CREATE INDEX IF NOT EXISTS idx_lash_session_revisions_terminal_ordinal
    ON lash_session_revisions(session_id, terminal_ordinal);

-- The current revision is one pointer; per-revision facts live in revisions.
CREATE TABLE IF NOT EXISTS lash_session_head (
    session_id TEXT PRIMARY KEY,
    head_revision BIGINT NOT NULL,
    terminal_ordinal BIGINT NOT NULL DEFAULT 0 CONSTRAINT ck_session_head_terminal_ordinal CHECK (terminal_ordinal >= 0),
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
    admission_base_checkpoint_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
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
    payload_json TEXT, -- NULL once the revision its applying commit published is released (FIG-6607).
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
    entered_at_ms BIGINT, -- When a plugin task's operation run entered, before its handler ran; NULL while it never ran.
    cancel_request_json TEXT, -- The first host cancel of the entered operation run; NULL while none.
    CONSTRAINT ck_queued_work_batches_cancel_after_entry CHECK (cancel_request_json IS NULL OR entered_at_ms IS NOT NULL),
    CONSTRAINT ck_queued_work_batches_payload CHECK (((payload_json IS NULL AND terminal_cause IS NOT NULL) OR payload_json::jsonb ->> 'type' = 'session_command') IS TRUE),
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
-- The applied commands whose payload a revision release may clear
-- (FIG-6607): a released or still-open row never lengthens the scan.
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_settled_payload
    ON lash_queued_work_batches(session_id, enqueue_seq)
    WHERE payload_json IS NOT NULL AND terminal_cause IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_pending_turn_inputs (
    enqueue_seq BIGINT NOT NULL,
    input_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    ingress_json TEXT NOT NULL, -- The submitted delivery, written once (ADR 0101 §5.1).
    state TEXT NOT NULL,
    input_json TEXT, -- NULL once the revision its run ended on is released (FIG-6607).
    submission_digest TEXT NOT NULL,
    enqueued_at_ms BIGINT NOT NULL,
    admitted_run TEXT, -- The run that took the input, retained after settlement; NULL while unbound.
    admitted_by TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    run_spec_hash TEXT,
    terminal_at_ms BIGINT, -- When the input's tombstone was written; NULL until then.
    trace_cause_json TEXT, -- The submission's trace cause, written once at acceptance; NULL is a root cause.
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK (((ingress_json::jsonb ->> 'scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR ((ingress_json::jsonb ->> 'scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_admission_all_or_none CHECK (state IN ('cancelled', 'completed') OR (admitted_run IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_pending_turn_inputs_settled_unadmitted CHECK (admitted_by IS NULL OR state NOT IN ('cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_terminal_at CHECK ((state IN ('cancelled', 'completed')) = (terminal_at_ms IS NOT NULL)),
    CONSTRAINT ck_pending_turn_inputs_released_settled CHECK (input_json IS NOT NULL OR state IN ('cancelled', 'completed')),
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

-- Settled inputs retain admitted_run for observation, but cannot keep a run
-- active. Match the session-mail probe's nonterminal vocabulary predicate so
-- finding the active run never scans those retained bindings (FIG-6909).
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_open_bound_run
    ON lash_pending_turn_inputs(session_id, admitted_run)
    WHERE admitted_run IS NOT NULL AND state NOT IN ('cancelled', 'completed');
-- The settled inputs whose body a revision release may clear (FIG-6607):
-- a released or live row never lengthens the scan.
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_settled_body
    ON lash_pending_turn_inputs(session_id, enqueue_seq)
    WHERE input_json IS NOT NULL AND state IN ('cancelled', 'completed');

-- The source keys a host forsook before any input was filed under them: no
-- input is ever admitted under one. A forsake and an admission of one key
-- are serialized by the session's write authority, so a key holds either a
-- `lash_pending_turn_inputs` row or a tombstone, never both. `input_id` is the
-- id a send under the key would carry, for the follower's status read. A
-- tombstone is deleted with its session; no vacuum reclaims it.
CREATE TABLE IF NOT EXISTS lash_forsaken_turn_inputs (
    session_id TEXT NOT NULL,
    source_key TEXT NOT NULL,
    input_id TEXT NOT NULL UNIQUE,
    forsaken_at_ms BIGINT NOT NULL,
    PRIMARY KEY (session_id, source_key)
);

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
-- kept for the indexes and checked against it. A terminal names the window it
-- ended on by the head revision it ended on (FIG-5931, FIG-6606): the window
-- is that revision's row, retained and released with it, and the terminal
-- roots nothing of its own but the transcript a turn's terminal publishes,
-- the calls it answered (FIG-6601). Each input row retains the run that
-- executes it.
-- `lash_control_intents` records a session's close; a
-- `close_session` row outlives its session as the deletion tombstone.
CREATE TABLE IF NOT EXISTS lash_session_runs (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    admission_json TEXT,
    scope_has_holds BOOLEAN NOT NULL DEFAULT FALSE,
    terminal_kind TEXT,
    terminal_cause_json TEXT,
    terminal_head_revision BIGINT,
    terminal_at_ms BIGINT,
    terminal_window_revision BIGINT,
    terminal_transcript_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_session_runs_transcript CHECK (terminal_transcript_ref IS NULL OR terminal_kind IS NOT NULL),
    CONSTRAINT ck_session_runs_terminal_window CHECK (terminal_window_revision IS NULL OR terminal_kind IS NOT NULL),
    CONSTRAINT ck_session_runs_terminal CHECK (((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IS NOT NULL AND terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL)) IS TRUE),
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

CREATE INDEX IF NOT EXISTS idx_lash_session_runs_terminal_head_revision
    ON lash_session_runs(session_id, terminal_head_revision);
CREATE INDEX IF NOT EXISTS idx_lash_session_runs_terminal_transcript_ref
    ON lash_session_runs(terminal_transcript_ref);
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
    attachment_id BYTEA NOT NULL CONSTRAINT ck_attachment_referrer_edges_attachment CHECK (octet_length(attachment_id) = 32),
    referrer_kind TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_kind CHECK (referrer_kind IN ('session', 'upload', 'execution', 'start_input', 'process_record', 'source')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_id CHECK (char_length(referrer_id) > 0),
    PRIMARY KEY (attachment_id, referrer_kind, referrer_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_referrer_edges_referrer
    ON lash_attachment_referrer_edges(referrer_kind, referrer_id);

-- Why a session's edge holds an attachment (FIG-6611): its committed history
-- (holder id empty), a pending input or a queued batch. The edge lasts while
-- one hold does.
CREATE TABLE IF NOT EXISTS lash_attachment_session_holds (
    session_id    TEXT NOT NULL CONSTRAINT ck_attachment_session_holds_session CHECK (char_length(session_id) > 0),
    attachment_id BYTEA NOT NULL CONSTRAINT ck_attachment_session_holds_attachment CHECK (octet_length(attachment_id) = 32),
    holder_kind   TEXT NOT NULL CONSTRAINT ck_attachment_session_holds_kind CHECK (holder_kind IN ('history', 'input', 'batch')),
    holder_id     TEXT NOT NULL,
    CONSTRAINT ck_attachment_session_holds_holder CHECK ((holder_kind = 'history') = (holder_id = '')),
    PRIMARY KEY (session_id, attachment_id, holder_kind, holder_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_session_holds_holder
    ON lash_attachment_session_holds(session_id, holder_kind, holder_id);

-- Attachments whose ingress hold ended: the release boundary drops the
-- session edge of each one no hold names any more.
CREATE TABLE IF NOT EXISTS lash_attachment_session_releases (
    session_id    TEXT NOT NULL,
    attachment_id BYTEA NOT NULL,
    PRIMARY KEY (session_id, attachment_id)
);

CREATE TABLE IF NOT EXISTS lash_attachment_pending_writes (
    write_id      TEXT PRIMARY KEY CONSTRAINT ck_attachment_pending_writes_write_id CHECK (char_length(write_id) = 32),
    attachment_id BYTEA NOT NULL CONSTRAINT ck_attachment_pending_writes_attachment CHECK (octet_length(attachment_id) = 32),
    referrer_kind TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_kind CHECK (referrer_kind IN ('session', 'upload', 'execution', 'start_input', 'process_record', 'source')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_id CHECK (char_length(referrer_id) > 0),
    begun_at_ms   BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_pending_writes_attachment
    ON lash_attachment_pending_writes(attachment_id);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_pending_writes_referrer
    ON lash_attachment_pending_writes(referrer_kind, referrer_id);

CREATE TABLE IF NOT EXISTS lash_attachment_uploads (
    attachment_id BYTEA PRIMARY KEY CONSTRAINT ck_attachment_uploads_attachment CHECK (octet_length(attachment_id) = 32),
    written_at_ms BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_attachment_condemnations (
    attachment_id     BYTEA PRIMARY KEY,
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
-- `start_json` holds immutable creation metadata. `execution_json` holds the
-- current input/definition pair, replaced only by a committed successor.
-- `lifecycle_json` holds mutable facts and is the row's one lifecycle authority.
-- `status`, `last_event_sequence` and
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
-- The projection never respells the stored `lifecycle_json`.
CREATE TABLE IF NOT EXISTS lash_processes (
    process_id TEXT COLLATE "C" PRIMARY KEY,
    start_key TEXT COLLATE "C",
    originator_id TEXT NOT NULL,
    identity_kind TEXT NOT NULL,
    identity_label TEXT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    last_event_sequence BIGINT NOT NULL GENERATED ALWAYS AS ((replace(lifecycle_json, chr(92) || 'u0000', chr(92) || 'u0001')::json ->> 'last_event_sequence')::bigint) STORED,
    change_seq BIGINT,
    staged_seq BIGINT NOT NULL DEFAULT nextval('lash_process_change_staging'),
    -- Saves since the row was last sequenced: the feed's clock moves once
    -- per save, and the row takes the position of its last.
    unsequenced_saves INTEGER NOT NULL DEFAULT 1 CONSTRAINT ck_processes_unsequenced_saves CHECK (unsequenced_saves >= 1),
    status TEXT NOT NULL GENERATED ALWAYS AS (
        CASE replace(lifecycle_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{lifecycle,state}'
            WHEN 'running' THEN 'running'
            WHEN 'waiting' THEN 'waiting'
            WHEN 'terminal' THEN CASE replace(lifecycle_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{lifecycle,outcome,type}'
                WHEN 'abandoned' THEN 'abandoned'
                WHEN 'settled' THEN CASE replace(lifecycle_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{lifecycle,outcome,output,outcome,status}'
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
    cancel_requested_at_ms BIGINT GENERATED ALWAYS AS ((replace(lifecycle_json, chr(92) || 'u0000', chr(92) || 'u0001')::json #>> '{cancel_request,requested_at_ms}')::bigint) STORED,
    state_rev BIGINT NOT NULL DEFAULT 0,
    driver_json TEXT,
    cascade_cursor TEXT,
    written_epoch BIGINT,
    published_event_sequence BIGINT NOT NULL DEFAULT 0,
    start_json TEXT NOT NULL,
    execution_json TEXT NOT NULL,
    lifecycle_json TEXT NOT NULL,
    consumer_hold_key TEXT,
    consumer_hold_scope_kind TEXT,
    consumer_hold_scope_id TEXT COLLATE "C",
    consumer_hold_cancels BOOLEAN,
    CONSTRAINT ck_processes_consumer_hold CHECK ((consumer_hold_key IS NULL) = (consumer_hold_scope_kind IS NULL) AND (consumer_hold_key IS NULL) = (consumer_hold_scope_id IS NULL)),
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned')),
    CONSTRAINT ck_processes_cancel_requested_at CHECK ((cancel_requested_at_ms IS NULL) = ((replace(lifecycle_json, chr(92) || 'u0000', chr(92) || 'u0001')::json -> 'cancel_request') IS NULL)),
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


-- A submission keeps its replay envelope and first settlement while the row
-- lives; its first payload follows the revision its owning run published
-- (FIG-6612) and is cleared only once the submission settled.
CREATE TABLE IF NOT EXISTS lash_tool_intent_submissions (
    replay_key TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    execution_scope_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    intent_index BIGINT NOT NULL,
    payload_hash TEXT NOT NULL,
    submission_json TEXT NOT NULL,
    intent_json TEXT,
    settlement_json TEXT,
    owner_session_id TEXT,
    owner_run TEXT,
    admitted_at_ms BIGINT NOT NULL,
    CONSTRAINT ck_lash_tool_intent_submissions_payload CHECK (intent_json IS NOT NULL OR settlement_json IS NOT NULL),
    CONSTRAINT ck_lash_tool_intent_submissions_owning_run CHECK ((owner_session_id IS NULL) = (owner_run IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_lash_tool_intent_submissions_scope
    ON lash_tool_intent_submissions(owner, execution_scope_id, intent_index);
CREATE INDEX IF NOT EXISTS idx_lash_tool_intent_submissions_settled_payloads
    ON lash_tool_intent_submissions(owner_session_id, owner_run)
    WHERE intent_json IS NOT NULL AND settlement_json IS NOT NULL;

-- The owners whose submission ledger the retained-evidence lever reclaimed
-- (FIG-1509): each is a durably deleted session, and the fence refuses every
-- later submission under it so a reclaimed identity cannot realize again.
CREATE TABLE IF NOT EXISTS lash_tool_intent_retired_owners (
    owner TEXT PRIMARY KEY
);

-- An artifact is its bytes (storage_kind 0) or, through the graph bridge, a
-- graph node its namespace codec validates (storage_kind 1), never both
-- (FIG-5887 §7). Only the bridge's graph_ref roots graph material.
CREATE TABLE IF NOT EXISTS lash_lash_vm_artifacts (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    storage_kind SMALLINT NOT NULL CONSTRAINT ck_lash_vm_artifacts_storage_kind
        CHECK (storage_kind IN (0, 1)),
    artifact_bytes BYTEA,
    graph_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY (namespace, artifact_ref),
    CONSTRAINT ck_lash_vm_artifacts_storage CHECK (
        (storage_kind = 0 AND artifact_bytes IS NOT NULL AND graph_ref IS NULL)
        OR (storage_kind = 1 AND artifact_bytes IS NULL AND graph_ref IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_lash_lash_vm_artifacts_graph_ref
    ON lash_lash_vm_artifacts (graph_ref) WHERE graph_ref IS NOT NULL;
CREATE TABLE IF NOT EXISTS lash_artifact_referrer_edges (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'start', 'execution', 'host_pin', 'source')),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_id CHECK (char_length(referrer_id) > 0),
    PRIMARY KEY (namespace, artifact_ref, referrer_kind, referrer_id),
    FOREIGN KEY (namespace, artifact_ref) REFERENCES lash_lash_vm_artifacts(namespace, artifact_ref) ON DELETE CASCADE
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
    draining BOOLEAN NOT NULL DEFAULT FALSE,
    -- What the node offers running code, its own package set among it
    -- (FIG-6991), as JSON: gone with the row.
    capabilities_json TEXT NOT NULL
);

-- An installed package artifact (FIG-6918), keyed by its digest, with the
-- ordinal the store assigned it on its package line and its header.
CREATE TABLE IF NOT EXISTS lash_package_artifacts (
    artifact TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    ordinal BIGINT NOT NULL,
    header TEXT NOT NULL,
    CONSTRAINT uq_lash_package_artifacts_line UNIQUE (path, ordinal)
);

-- An installed package set, keyed by its digest, at the store's install
-- sequence: the only key behind "newest first". Its surface snapshot and the
-- JSON of what running its code requires are written once (FIG-6991).
CREATE TABLE IF NOT EXISTS lash_package_sets (
    digest TEXT PRIMARY KEY,
    sequence BIGINT NOT NULL CONSTRAINT uq_lash_package_sets_sequence UNIQUE,
    surface BYTEA NOT NULL,
    requirements_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_package_set_members (
    set_digest TEXT NOT NULL REFERENCES lash_package_sets (digest),
    artifact TEXT NOT NULL REFERENCES lash_package_artifacts (artifact),
    PRIMARY KEY (set_digest, artifact)
);

-- Every definition of a stored package, helpers included, by identity and
-- path (FIG-6991): written only by an install, never deleted. Membership is
-- the authority to perform the package's internal operations.
CREATE TABLE IF NOT EXISTS lash_package_definitions (
    function TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (function, path)
);

-- Each distinct labels an install wrote for an artifact, in install order:
-- append-only, so releases of identical content keep their labels.
CREATE TABLE IF NOT EXISTS lash_package_labels (
    artifact TEXT NOT NULL,
    labels TEXT NOT NULL,
    labels_digest BYTEA NOT NULL CONSTRAINT ck_lash_package_labels_labels_digest CHECK (octet_length(labels_digest) = 32),
    sequence BIGINT NOT NULL CONSTRAINT uq_lash_package_labels_sequence UNIQUE,
    PRIMARY KEY (artifact, labels_digest)
);

-- Every operation a stored package declares internal, by effect name and
-- the package's path: written only by an install, never deleted.
CREATE TABLE IF NOT EXISTS lash_package_internal_operations (
    operation TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (operation, path)
);

-- The artifacts a live node holds, gone with its row.
CREATE TABLE IF NOT EXISTS lash_node_artifacts (
    node_id TEXT NOT NULL REFERENCES lash_nodes (node_id) ON DELETE CASCADE,
    artifact TEXT NOT NULL REFERENCES lash_package_artifacts (artifact),
    path TEXT NOT NULL,
    PRIMARY KEY (node_id, artifact)
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
    requires TEXT,
    packages TEXT,
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

CREATE TABLE IF NOT EXISTS lash_model_call_pins (
    session_id TEXT NOT NULL, run TEXT NOT NULL, call TEXT NOT NULL, call_id TEXT NOT NULL,
    attempt BIGINT NOT NULL CONSTRAINT lash_model_call_pins_attempt_check CHECK (attempt > 0), answered BOOLEAN NOT NULL,
    enclosing_owner TEXT, enclosing_run BIGINT, enclosing_call TEXT,
    PRIMARY KEY (session_id, run, call), UNIQUE (session_id, run, call_id)
);
CREATE TABLE IF NOT EXISTS lash_model_call_records (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    call TEXT NOT NULL,
    call_id TEXT NOT NULL,
    record_ref BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    ended_at_ms BIGINT NOT NULL,
    written_epoch BIGINT NOT NULL,
    PRIMARY KEY (session_id, run, call),
    UNIQUE (session_id, run, call_id)
);
CREATE INDEX IF NOT EXISTS idx_model_call_records_record_ref ON lash_model_call_records(record_ref);

CREATE TABLE IF NOT EXISTS lash_run_records (
    owner_key TEXT COLLATE "C" NOT NULL,
    run_seq BIGINT NOT NULL,
    ordinal BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_run_records_kind
        CHECK (kind IN ('admit', 'x_start', 'x_outcome', 'x_wait', 'present', 'retry',
                        'cell_report')),
    call_id TEXT,
    -- The fact's RoundFact node (FIG-5887 §7).
    record_ref BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    written_epoch BIGINT NOT NULL,
    PRIMARY KEY (owner_key, run_seq, ordinal)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_lash_run_records_outcome
    ON lash_run_records (owner_key, run_seq, call_id)
    WHERE kind = 'x_outcome' AND call_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lash_run_records_record_ref ON lash_run_records (record_ref);

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

-- Execution roots (I0, FIG-5194; statements: V0, then L7): the root of each
-- code cell ('c/...') and process ('p/...'), compare-and-set on rev.
CREATE TABLE IF NOT EXISTS lash_exec_snapshots (
    exec_key TEXT COLLATE "C" PRIMARY KEY,
    rev BIGINT NOT NULL CONSTRAINT ck_exec_snapshots_rev CHECK (rev >= 1),
    -- The execution's ExecRoot node (FIG-5887 §7) and the commit that wrote it.
    root_ref BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    commit_id BYTEA NOT NULL CONSTRAINT ck_exec_snapshots_commit_id
        CHECK (octet_length(commit_id) = 32),
    written_epoch BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_exec_snapshots_root_ref ON lash_exec_snapshots (root_ref);

-- Prompt snapshots (P2, FIG-5256; ADR 0133 §5, §6): one root per admitted
-- model call, keyed by its owner (`turn:<run>` or `owned:<scope>`) and its
-- call there (FIG-5259), holding its prompt snapshot and, until the call
-- settles, its request: the exact provider body (FIG-5946). Section text and
-- body chunks are stored once by content address, with an edge from each
-- root part to every text it references. Settling a call drops its request
-- and the request's edges; only an explicit release removes a root, and a
-- text goes with its last edge.
CREATE TABLE IF NOT EXISTS lash_prompt_snapshots (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    snapshot TEXT,
    request TEXT,
    written_epoch BIGINT NOT NULL,
    -- Both parts NULL is a compact expired-call identity.
    PRIMARY KEY (session_id, owner, call)
);

-- Reclaimed call identities stay out of the release scan.
CREATE INDEX IF NOT EXISTS idx_lash_prompt_snapshots_settled
    ON lash_prompt_snapshots (session_id, owner, call)
    WHERE request IS NULL AND snapshot IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_prompt_texts (
    hash BYTEA PRIMARY KEY,
    text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_prompt_snapshot_texts (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    part TEXT NOT NULL CONSTRAINT ck_lash_prompt_snapshot_texts_part
        CHECK (part IN ('snapshot', 'request')),
    hash BYTEA NOT NULL REFERENCES lash_prompt_texts(hash),
    PRIMARY KEY (session_id, owner, call, part, hash),
    FOREIGN KEY (session_id, owner, call)
        REFERENCES lash_prompt_snapshots(session_id, owner, call)
);

CREATE INDEX IF NOT EXISTS idx_lash_prompt_snapshot_texts_hash
    ON lash_prompt_snapshot_texts (hash);

CREATE TABLE IF NOT EXISTS lash_prompt_text_edges (
    parent BYTEA NOT NULL REFERENCES lash_prompt_texts(hash) ON DELETE CASCADE,
    child BYTEA NOT NULL REFERENCES lash_prompt_texts(hash),
    PRIMARY KEY (parent, child)
);
CREATE INDEX IF NOT EXISTS idx_lash_prompt_text_edges_child ON lash_prompt_text_edges(child);
CREATE TABLE IF NOT EXISTS lash_prompt_prefixes (
    session_id TEXT PRIMARY KEY,
    hash BYTEA NOT NULL REFERENCES lash_prompt_texts(hash)
);


-- Turn phase state (V0, FIG-5170; then L3): a 1:1 side table of
-- lash_session_runs while the turn is unfinished. The run row stays the one
-- admission authority (ux_lash_session_runs_unfinished); this row carries the
-- phase with what a restore resumes it from: no checkpoint while admitted and
-- one after, and the pinned model call exactly in the model phase, whose
-- attempt is phase_arg and whose call is model_calls, the count of model
-- calls the turn admitted, and the pinned call's receipt only in the model
-- phase.
CREATE TABLE IF NOT EXISTS lash_turn_phases (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    phase TEXT NOT NULL CONSTRAINT ck_turn_phases_phase
        CHECK (phase IN ('admitted', 'model', 'tools')),
    phase_arg BIGINT,
    iteration BIGINT NOT NULL,
    -- The phase's PhaseRoot node and the post-cell working SessionExecution
    -- (FIG-5887 §6, §7), and the pinned call's receipt: its placed text,
    -- from model.answer until the next phase.
    checkpoint_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    working_execution_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    answer_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    model_request_ref TEXT,
    model_deadline_ms BIGINT,
    model_stream_from TEXT,
    turn_deadline_ms BIGINT,
    written_epoch BIGINT NOT NULL,
    model_calls BIGINT NOT NULL DEFAULT 0,
    tool_calls BIGINT NOT NULL DEFAULT 0 CONSTRAINT lash_turn_phases_tool_calls_check
        CHECK (tool_calls >= 0),
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_turn_phases_arg CHECK ((phase IN ('model', 'tools')) = (phase_arg IS NOT NULL)),
    CONSTRAINT ck_turn_phases_checkpoint CHECK ((phase = 'admitted') = (checkpoint_ref IS NULL)),
    CONSTRAINT ck_turn_phases_answer CHECK (answer_ref IS NULL OR phase = 'model'),
    CONSTRAINT ck_turn_phases_model CHECK (
        (phase = 'model') = (model_request_ref IS NOT NULL)
        AND (phase = 'model') = (model_deadline_ms IS NOT NULL)
        AND (phase = 'model') = (model_stream_from IS NOT NULL))
);

-- The plugin namespaces an unfinished turn's run changed (FIG-5301): the
-- entry (values address and metadata, JSON) and its values, dropped with the
-- phase row when the turn ends.
CREATE TABLE IF NOT EXISTS lash_turn_namespaces (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    plugin TEXT NOT NULL,
    entry TEXT NOT NULL,
    -- The namespace's NamespaceValue node; NULL exactly for base values
    -- (FIG-5887 §7).
    graph_root_ref BYTEA REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY (session_id, run, plugin)
);
CREATE INDEX IF NOT EXISTS idx_lash_turn_phases_checkpoint_ref ON lash_turn_phases (checkpoint_ref);
CREATE INDEX IF NOT EXISTS idx_lash_turn_phases_working_execution_ref
    ON lash_turn_phases (working_execution_ref);
CREATE INDEX IF NOT EXISTS idx_lash_turn_phases_answer_ref ON lash_turn_phases (answer_ref);
CREATE INDEX IF NOT EXISTS idx_lash_turn_namespaces_graph_root_ref
    ON lash_turn_namespaces (graph_root_ref);

-- Waits, keyed promises and timers (L5, FIG-5173; ADR 0132 §6): one row per
-- wait from its minting. The deadline is written once; a resolution is a
-- conditional update from 'pending', so the first one wins. Only
-- tool_completion and custom waits are host resolvable: their completion key
-- is the wait id, 128 random bits.
CREATE TABLE IF NOT EXISTS lash_waits (
    wait_id TEXT COLLATE "C" PRIMARY KEY,
    -- NULL exactly on a tombstone: its turn ended, and it wakes nobody.
    owner_actor TEXT COLLATE "C",
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
    -- A tombstone: the id, kind, state, digest and settlement time a late
    -- resolution is answered from, once its turn ended. Never pending.
    compacted BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT ck_waits_compacted CHECK (NOT compacted OR (state <> 'pending'
        AND owner_actor IS NULL AND target_process IS NULL AND deadline_ms IS NULL
        AND resolution_ref IS NULL AND call_id IS NULL AND tool_id IS NULL
        AND key_name IS NULL)),
    CONSTRAINT ck_waits_owner CHECK (compacted OR owner_actor IS NOT NULL),
    CONSTRAINT ck_waits_host CHECK (host_resolvable = (kind IN ('tool_completion', 'engine_key'))),
    CONSTRAINT ck_waits_target CHECK (compacted
        OR (target_process IS NOT NULL) = (kind IN ('process_terminal', 'child_session'))),
    CONSTRAINT ck_waits_resolved CHECK ((state = 'resolved') = (resolution_digest IS NOT NULL)),
    CONSTRAINT ck_waits_timer_deadline
        CHECK (compacted OR kind <> 'timer' OR deadline_ms IS NOT NULL),
    CONSTRAINT ck_waits_settled_at CHECK ((state <> 'pending') = (resolved_at_ms IS NOT NULL)),
    CONSTRAINT ck_waits_resolution_ref CHECK (compacted
        OR (state = 'resolved' AND kind <> 'timer') = (resolution_ref IS NOT NULL)),
    CONSTRAINT ck_waits_call CHECK (compacted OR (call_id IS NOT NULL) = (kind = 'tool_completion')
        AND (tool_id IS NOT NULL) = (kind = 'tool_completion')),
    CONSTRAINT ck_waits_key_name
        CHECK (compacted OR (key_name IS NOT NULL) = (kind = 'engine_key'))
);

CREATE INDEX IF NOT EXISTS ix_lash_waits_owner ON lash_waits (owner_actor)
    WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS ix_lash_waits_scope ON lash_waits (owner_scope);
CREATE INDEX IF NOT EXISTS ix_lash_waits_target ON lash_waits (target_process)
    WHERE state = 'pending' AND kind = 'process_terminal';
CREATE INDEX IF NOT EXISTS ix_lash_waits_child_target ON lash_waits (target_process)
    WHERE state = 'pending' AND kind = 'child_session';

-- The checkpoint graph (FIG-5887 §7, FIG-5890). One row per child of an
-- installed graph node: exactly its envelope's child table. A parent's edges
-- go with it; a child cannot go while an edge names it.
CREATE TABLE IF NOT EXISTS lash_blob_edges (
    parent_hash BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE CASCADE,
    child_hash BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY (parent_hash, child_hash)
);
CREATE INDEX IF NOT EXISTS idx_lash_blob_edges_child ON lash_blob_edges (child_hash, parent_hash);

-- An execution's admitted effect, by its canonical binary identity, to its
-- Admission node, while a live kernel or a released member needs the lookup.
CREATE TABLE IF NOT EXISTS lash_effect_admissions (
    exec_key TEXT COLLATE "C" NOT NULL,
    effect_id BYTEA NOT NULL,
    owner_key TEXT COLLATE "C",
    run_seq BIGINT,
    member_ordinal BIGINT,
    admission_ref BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    written_epoch BIGINT NOT NULL,
    PRIMARY KEY (exec_key, effect_id),
    UNIQUE (owner_key, run_seq, member_ordinal),
    CONSTRAINT ck_effect_admissions_member CHECK (
        (owner_key IS NULL AND run_seq IS NULL AND member_ordinal IS NULL) OR
        (owner_key IS NOT NULL AND run_seq IS NOT NULL AND member_ordinal IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_lash_effect_admissions_admission_ref
    ON lash_effect_admissions (admission_ref);

-- An activation's or a reader's lease on the exact committed root it reads on demand
-- (FIG-6191, D-LEASE): a root of the catalog until released or reaped,
-- though reads and renewals past its expiry are refused.
CREATE TABLE IF NOT EXISTS lash_root_leases (
    lease_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_key BYTEA NOT NULL,
    root_ref BYTEA NOT NULL REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    actor_key TEXT COLLATE "C",
    epoch BIGINT,
    ttl_ms BIGINT NOT NULL CONSTRAINT ck_root_leases_ttl CHECK (ttl_ms > 0),
    expires_at_ms BIGINT NOT NULL,
    -- An actor's lease names its actor and epoch; a reader's names
    -- neither.
    CONSTRAINT ck_root_leases_fence CHECK ((actor_key IS NULL) = (epoch IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_lash_root_leases_root_ref ON lash_root_leases (root_ref);
CREATE INDEX IF NOT EXISTS idx_lash_root_leases_expiry ON lash_root_leases (expires_at_ms, lease_id);

-- A retirement hold (FIG-6191, D-RET): a root a publication retired or a
-- lease released. A root of the catalog until a step takes it; a hold
-- without `closure` peels the node alone.
CREATE TABLE IF NOT EXISTS lash_retirement_holds (
    root_ref BYTEA PRIMARY KEY REFERENCES lash_blobs(hash) ON DELETE RESTRICT,
    closure BOOLEAN NOT NULL,
    queued_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_retirement_holds_queue
    ON lash_retirement_holds (queued_at_ms, root_ref);

-- A retirement step's unfinished frontier (FIG-6193): a node under a
-- retired root, or a reaped lease's root, that a bounded step has not peeled
-- yet. Only retirement steps, one at a time, write or take it, so a step
-- never waits on another transaction for it: no root of the catalog and no
-- foreign key; a node gone meanwhile is passed over.
CREATE TABLE IF NOT EXISTS lash_retirement_frontier (
    node_ref BYTEA PRIMARY KEY,
    closure BOOLEAN NOT NULL,
    queued_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_retirement_frontier_queue
    ON lash_retirement_frontier (queued_at_ms, node_ref);


-- Every root of every source: a view of the owning columns, never a cache.
CREATE OR REPLACE VIEW lash_graph_root_catalog AS
    SELECT 'exec' AS source, root_ref FROM lash_exec_snapshots WHERE root_ref IS NOT NULL
    UNION ALL SELECT 'run_fact', record_ref FROM lash_run_records WHERE record_ref IS NOT NULL
    UNION ALL SELECT 'model_call', record_ref FROM lash_model_call_records
    UNION ALL SELECT 'admission', admission_ref FROM lash_effect_admissions
    UNION ALL SELECT 'phase', checkpoint_ref FROM lash_turn_phases WHERE checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'working', working_execution_ref FROM lash_turn_phases
        WHERE working_execution_ref IS NOT NULL
    UNION ALL SELECT 'namespace', graph_root_ref FROM lash_turn_namespaces
        WHERE graph_root_ref IS NOT NULL
    UNION ALL SELECT 'revision', checkpoint_ref FROM lash_session_revisions
        WHERE checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'admission_base', admission_base_checkpoint_ref FROM lash_session_meta
        WHERE admission_base_checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'artifact', graph_ref FROM lash_lash_vm_artifacts WHERE storage_kind = 1
    UNION ALL SELECT 'revision_config', config_ref FROM lash_session_revisions
    UNION ALL SELECT 'answer', answer_ref FROM lash_turn_phases WHERE answer_ref IS NOT NULL
    UNION ALL SELECT 'lease', root_ref FROM lash_root_leases
    UNION ALL SELECT 'retirement', root_ref FROM lash_retirement_holds
    UNION ALL SELECT 'terminal_transcript', terminal_transcript_ref FROM lash_session_runs
        WHERE terminal_transcript_ref IS NOT NULL;

-- One-call owner commits (FIG-5936): each call's answer, recorded under its
-- token as the call's last write. A call whose answer was lost is reconciled
-- by its token: a row with an answer is a call that committed, and the
-- reconciler's own row, with none, refuses a call that never reached its
-- record. Each call sweeps a few rows older than fifteen minutes.
CREATE TABLE IF NOT EXISTS lash_owner_commit_latches (
    token TEXT COLLATE "C" PRIMARY KEY,
    created_ms BIGINT NOT NULL,
    answer JSONB
);
CREATE INDEX IF NOT EXISTS idx_lash_owner_commit_latches_created ON lash_owner_commit_latches (created_ms);

-- The one-call owner commit (FIG-5936), generated from this build's
-- statements by `durable/executor.rs`. Regenerate it with the executor's
-- LASH_REGENERATE=1 test, never by hand.
-- BEGIN GENERATED lash_owner_commit
CREATE OR REPLACE FUNCTION lash_owner_commit(
    p_registry TEXT, p_actor TEXT, p_epoch BIGINT, p_session TEXT,
    p_fleet_low INTEGER, p_fleet_high INTEGER, p_token TEXT,
    p_lock_timeout TEXT, p_statement_ms BIGINT,
    p_body_hashes BYTEA[], p_bodies BYTEA[],
    p_named BYTEA[], p_roots BYTEA[], p_digests BYTEA[], p_contents BYTEA[],
    p_parents BYTEA[], p_children BYTEA[], p_steps JSONB,
    p_formats TEXT, p_ack BIGINT, p_release TEXT, p_due BIGINT, p_now BIGINT)
RETURNS TABLE (o_revision BIGINT, o_now BIGINT, o_fleet INTEGER, o_state TEXT,
    o_installed BIGINT, o_cancel_turn TEXT, o_cancel_request TEXT, o_cancel_origin TEXT,
    o_cancel_reason TEXT, o_cancel_disposition TEXT, o_cancel_mode TEXT)
LANGUAGE plpgsql
AS $lash_owner_commit$
#variable_conflict use_column
DECLARE
    v_fleet INTEGER;
    v_now BIGINT;
    v_xact TEXT;
    v_rev BIGINT;
    v_cancel_turn TEXT;
    v_cancel_request TEXT;
    v_cancel_origin TEXT;
    v_cancel_reason TEXT;
    v_cancel_disposition TEXT;
    v_cancel_mode TEXT;
    v_clock BIGINT;
    v_inserted BYTEA[] := '{}';
    v_parents BYTEA[];
    v_children BYTEA[];
    v_installed BIGINT := 0;
    v_rows BIGINT;
    v_index INTEGER := 0;
    v_step JSONB;
    v_p JSONB;
    v_state TEXT := 'owned';
    v_retired BYTEA[] := '{}';
    v_phase_ends BYTEA[] := '{}';
    v_held BYTEA[];
    v_row RECORD;
    v_first JSONB;
    v_slots JSONB := '{}';
    v_when JSONB;
    v_leaves TEXT[] := '{}';
    v_node TEXT;
    v_parent TEXT;
    v_reached BOOLEAN;
    v_wakes TEXT[] := '{}';
    v_owner TEXT;
    v_woken BIGINT;
    v_texts BYTEA[] := '{}';
    v_protect BYTEA[] := '{}';
    v_tfront BYTEA[];
    v_closure BYTEA[];
    v_tparents BYTEA[];
    v_tchildren BYTEA[];
    v_answer JSONB;
    v_frame TEXT;
BEGIN
    -- Generated from this build's statements (registry e696efecaabe0542); a call
    -- built from others is refused, as is any isolation but read committed.
    IF p_registry IS DISTINCT FROM 'e696efecaabe0542'
        OR current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'lash_owner_commit: registry or isolation refused' USING ERRCODE = 'LX000';
    END IF;
    IF p_lock_timeout IS NOT NULL THEN
        PERFORM set_config('lock_timeout', p_lock_timeout, true);
    END IF;
    PERFORM pg_advisory_xact_lock_shared(715425, 0);
    WITH fenced AS (
                 UPDATE lash_actors SET state_revision = state_revision + 1
                 WHERE actor_key = (p_actor) AND epoch = (p_epoch) AND state = 'owned'
                 RETURNING state_revision
             )
             SELECT (SELECT format_version FROM lash_fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    pg_current_xact_id()::text,
                    f.state_revision,
                    c.turn_id, c.request_id, c.origin, c.reason, c.disposition, c.mode
             FROM (SELECT 1) AS one
             LEFT JOIN fenced f ON TRUE
             LEFT JOIN lash_session_runs r ON r.session_id = (p_session) AND f.state_revision IS NOT NULL
                 AND r.admission_json IS NOT NULL AND r.terminal_kind IS NULL
             LEFT JOIN lash_turn_phases p ON p.session_id = r.session_id AND p.run = r.run
             LEFT JOIN lash_turn_cancel_requests c ON c.session_id = p.session_id
                 AND c.turn_id = p.run
    INTO v_fleet, v_clock, v_xact, v_rev, v_cancel_turn, v_cancel_request, v_cancel_origin,
        v_cancel_reason, v_cancel_disposition, v_cancel_mode;
    v_now := COALESCE(p_now, v_clock);
    IF v_fleet IS NULL OR v_fleet < p_fleet_low OR v_fleet > p_fleet_high THEN
        RAISE EXCEPTION 'lash_owner_commit: the writer fence refused' USING ERRCODE = 'LX002';
    END IF;
    IF v_rev IS NULL THEN
        RAISE EXCEPTION 'lash_owner_commit: the owner fence refused' USING ERRCODE = 'LX003';
    END IF;
    -- The checkpoint bodies a head commit supplies are stored before the
    -- graph that names them (FIG-5942).
    IF cardinality(p_body_hashes) > 0 THEN
        INSERT INTO lash_blobs (hash, content, encoding)
             SELECT hash, content, 0
               FROM unnest((p_body_hashes)::bytea[], (p_bodies)::bytea[]) AS blob(hash, content)
              ORDER BY hash
             ON CONFLICT (hash) DO NOTHING;
    END IF;
    IF cardinality(p_named) > 0 THEN
        PERFORM hash, encoding, CASE WHEN hash = ANY((p_named)::bytea[]) THEN NULL ELSE content END
             FROM lash_blobs
             WHERE hash = ANY((p_named)::bytea[])
                OR hash = ANY(ARRAY(SELECT digest FROM UNNEST((p_digests)::bytea[]) AS digest))
             ORDER BY hash FOR KEY SHARE;
        SELECT count(*) FROM unnest((p_named)::bytea[]) AS named(hash)
             LEFT JOIN lash_blobs AS stored ON stored.hash = named.hash
             WHERE stored.hash IS NULL
                OR (named.hash = ANY((p_roots)::bytea[]) AND stored.encoding <> 1) INTO v_rows;
        IF v_rows > 0 THEN
            RAISE EXCEPTION 'lash_owner_commit: the graph refused' USING ERRCODE = 'LX004';
        END IF;
    END IF;
    IF cardinality(p_digests) > 0 THEN
        WITH inserted AS (INSERT INTO lash_blobs (hash, content, encoding)
             SELECT staged.digest, staged.content, 1
             FROM UNNEST((p_digests)::bytea[], (p_contents)::bytea[]) AS staged(digest, content)
             ORDER BY staged.digest
             ON CONFLICT (hash) DO NOTHING
             RETURNING hash)
        SELECT COALESCE(array_agg(hash), '{}') INTO v_inserted FROM inserted;
        -- A staged node another writer stored is held against a peel; one a
        -- peel took first is a conflict, which the explicit form installs
        -- again (FIG-5949).
        PERFORM hash, content, encoding FROM lash_blobs WHERE hash = ANY((ARRAY(SELECT staged.digest FROM unnest(p_digests) AS staged(digest) WHERE NOT (staged.digest = ANY(v_inserted))))::bytea[])
             ORDER BY hash FOR KEY SHARE;
        SELECT count(*)
             FROM unnest((p_digests)::bytea[], (p_contents)::bytea[]) AS staged(digest, content)
             LEFT JOIN lash_blobs AS stored ON stored.hash = staged.digest
             WHERE NOT (staged.digest = ANY((v_inserted)::bytea[]))
               AND (stored.hash IS NULL OR stored.encoding <> 1
                    OR stored.content <> staged.content) INTO v_rows;
        IF v_rows > 0 THEN
            RAISE EXCEPTION 'lash_owner_commit: the graph refused' USING ERRCODE = 'LX004';
        END IF;
        SELECT COALESCE(array_agg(staged.parent), '{}'), COALESCE(array_agg(staged.child), '{}')
        INTO v_parents, v_children
        FROM unnest(p_parents, p_children) AS staged(parent, child)
        WHERE staged.parent = ANY(v_inserted);
        IF cardinality(v_parents) > 0 THEN
            INSERT INTO lash_blob_edges (parent_hash, child_hash)
             SELECT parent, child
             FROM UNNEST((v_parents)::bytea[], (v_children)::bytea[]) AS staged(parent, child);
        END IF;
        SELECT COALESCE(sum(octet_length(staged.content)), 0)::bigint
             FROM unnest((p_digests)::bytea[], (p_contents)::bytea[]) AS staged(digest, content)
             WHERE staged.digest = ANY((v_inserted)::bytea[]) INTO v_installed;
    END IF;
    FOR v_step IN SELECT value FROM jsonb_array_elements(p_steps) LOOP
        v_index := v_index + 1;
        -- A step runs only when the slot it names holds what it names: a
        -- row, or a row whose column is true (FIG-5942).
        IF v_step ? 'w' THEN
            v_when := v_slots->(v_step->'w'->>0);
            IF (CASE
                    WHEN v_when IS NULL OR v_when = 'null'::JSONB THEN FALSE
                    WHEN v_step->'w'->>1 IS NULL THEN TRUE
                    ELSE COALESCE((v_when->>(v_step->'w'->>1))::BOOLEAN, FALSE)
                END) IS DISTINCT FROM (v_step->'w'->>2)::BOOLEAN THEN
                CONTINUE;
            END IF;
        END IF;
        v_p := v_step->'p';
        v_first := NULL;
        CASE v_step->>'s'
        WHEN 'durable_run_record.append' THEN
            v_rows := 0;
            FOR v_row IN INSERT INTO lash_run_records
                 (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
             SELECT (v_p->>0), ((v_p->>1)::BIGINT), ((v_p->>2)::BIGINT), (v_p->>3), (v_p->>4), (decode(v_p->>5, 'hex')), ((v_p->>6)::BIGINT)
             WHERE ((v_p->>2)::BIGINT) = 0 OR EXISTS (
                 SELECT 1 FROM lash_run_records
                 WHERE owner_key = (v_p->>0) AND run_seq = ((v_p->>1)::BIGINT) AND ordinal = ((v_p->>2)::BIGINT) - 1)
             ON CONFLICT DO NOTHING
             RETURNING ordinal LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_run_record.prune' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_run_records WHERE owner_key = (v_p->>0) AND run_seq < ((v_p->>1)::BIGINT)
             RETURNING record_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."record_ref" IS NOT NULL THEN v_retired := v_retired || v_row."record_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_snapshot.put_first' THEN
            v_rows := 0;
            FOR v_row IN INSERT INTO lash_exec_snapshots (exec_key, rev, root_ref, commit_id, written_epoch)
             SELECT (v_p->>0), 1, (decode(v_p->>1, 'hex')), (decode(v_p->>2, 'hex')), ((v_p->>3)::BIGINT)
             WHERE NOT EXISTS (SELECT 1 FROM lash_exec_snapshots WHERE exec_key = (v_p->>0))
             RETURNING rev LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_snapshot.put_next' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_exec_snapshots
             SET rev = rev + 1, root_ref = (decode(v_p->>2, 'hex')), commit_id = (decode(v_p->>3, 'hex')), written_epoch = ((v_p->>4)::BIGINT)
             WHERE exec_key = (v_p->>0) AND rev = ((v_p->>1)::BIGINT) AND root_ref = (decode(v_p->>5, 'hex'))
             RETURNING rev LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_snapshot.retire' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_exec_snapshots WHERE exec_key = (v_p->>0) AND root_ref = (decode(v_p->>1, 'hex'))
             RETURNING exec_key LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_snapshot.clear' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_exec_snapshots WHERE exec_key = (v_p->>0) RETURNING root_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."root_ref" IS NOT NULL THEN v_retired := v_retired || v_row."root_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_turn.admit_run' THEN
            v_rows := 0;
            FOR v_row IN INSERT INTO lash_session_runs (session_id, run, admission_json)
             SELECT (v_p->>0), (v_p->>1), (v_p->>2)
             WHERE NOT EXISTS (
                 SELECT 1 FROM lash_session_runs
                 WHERE session_id = (v_p->>0) AND admission_json IS NOT NULL
                   AND terminal_kind IS NULL)
             RETURNING run LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn.insert_phase' THEN
            INSERT INTO lash_turn_phases
                 (session_id, run, phase, phase_arg, iteration, checkpoint_ref,
                  model_request_ref, model_deadline_ms, turn_deadline_ms, written_epoch)
             VALUES ((v_p->>0), (v_p->>1), (v_p->>2), ((v_p->>3)::BIGINT), ((v_p->>4)::BIGINT), (decode(v_p->>5, 'hex')), (v_p->>6), ((v_p->>7)::BIGINT), ((v_p->>8)::BIGINT), ((v_p->>9)::BIGINT));
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_turn.advance_phase' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_turn_phases
             SET phase = (v_p->>2), phase_arg = ((v_p->>3)::BIGINT), iteration = ((v_p->>4)::BIGINT), checkpoint_ref = (decode(v_p->>5, 'hex')),
                 model_request_ref = (v_p->>6), model_deadline_ms = ((v_p->>7)::BIGINT), written_epoch = ((v_p->>8)::BIGINT),
                 model_calls = COALESCE(((v_p->>9)::BIGINT), model_calls), model_stream_from = (v_p->>10),
                 answer_ref = NULL
             WHERE session_id = (v_p->>0) AND run = (v_p->>1)
             RETURNING run LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn.answer_set' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_turn_phases SET answer_ref = (decode(v_p->>2, 'hex')), written_epoch = ((v_p->>3)::BIGINT)
             WHERE session_id = (v_p->>0) AND run = (v_p->>1) AND phase = 'model' AND answer_ref IS NULL
             RETURNING run LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn.add_tool_calls' THEN
            UPDATE lash_turn_phases SET tool_calls = tool_calls + ((v_p->>2)::BIGINT)
             WHERE session_id = (v_p->>0) AND run = (v_p->>1);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_turn.open_named_run' THEN
            v_rows := 0;
            FOR v_row IN SELECT run FROM lash_session_runs
             WHERE session_id = (v_p->>0) AND run = (v_p->>1)
               AND admission_json IS NOT NULL AND terminal_kind IS NULL LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn_postgres.upsert_namespaces' THEN
            INSERT INTO lash_turn_namespaces
                 (session_id, run, plugin, entry, graph_root_ref)
             SELECT (v_p->>0), (v_p->>1), written.plugin, written.entry, written.root
             FROM UNNEST((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->2) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[], (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->3) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[], (ARRAY(SELECT decode(element, 'hex') FROM jsonb_array_elements_text(v_p->4) WITH ORDINALITY AS listed(element, position) ORDER BY position))::bytea[], (ARRAY(SELECT element::BOOLEAN FROM jsonb_array_elements_text(v_p->5) WITH ORDINALITY AS listed(element, position) ORDER BY position))::boolean[])
                 AS written(plugin, entry, root, held)
             ON CONFLICT (session_id, run, plugin)
             DO UPDATE SET entry = excluded.entry,
                 graph_root_ref = CASE
                     WHEN (SELECT written.held FROM UNNEST((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->2) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[], (ARRAY(SELECT element::BOOLEAN FROM jsonb_array_elements_text(v_p->5) WITH ORDINALITY AS listed(element, position) ORDER BY position))::boolean[])
                               AS written(plugin, held)
                           WHERE written.plugin = excluded.plugin)
                     THEN lash_turn_namespaces.graph_root_ref
                     ELSE excluded.graph_root_ref END;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_turn.working_set' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_turn_phases SET working_execution_ref = (decode(v_p->>2, 'hex')), written_epoch = ((v_p->>4)::BIGINT)
             WHERE session_id = (v_p->>0) AND run = (v_p->>1)
               AND working_execution_ref IS NOT DISTINCT FROM (decode(v_p->>3, 'hex'))
             RETURNING run LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn.phase_roots' THEN
            v_rows := 0;
            FOR v_row IN SELECT checkpoint_ref, answer_ref FROM lash_turn_phases
             WHERE session_id = (v_p->>0) AND run = (v_p->>1) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."checkpoint_ref" IS NOT NULL THEN v_retired := v_retired || v_row."checkpoint_ref"::BYTEA; END IF;
                IF v_row."answer_ref" IS NOT NULL THEN v_retired := v_retired || v_row."answer_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_turn_postgres.namespace_roots' THEN
            v_rows := 0;
            FOR v_row IN SELECT graph_root_ref FROM lash_turn_namespaces
             WHERE session_id = (v_p->>0) AND run = (v_p->>1) AND plugin = ANY((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->2) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[]) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."graph_root_ref" IS NOT NULL THEN v_retired := v_retired || v_row."graph_root_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_session_mail_postgres.bind_inputs' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_pending_turn_inputs AS input
             SET admitted_run = (v_p->>2), admitted_by = (v_p->>2)
             FROM UNNEST((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->1) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[]) AS named(input_id)
             WHERE input.session_id = (v_p->>0) AND input.input_id = named.input_id
               AND (input.admitted_run IS NULL OR input.admitted_run = (v_p->>2))
               AND input.state IN ('deferred_next_turn', 'pending_active')
             RETURNING input.input_id LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_session_mail_postgres.bind_batches' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_queued_work_batches AS batch
             SET admitted_run = (v_p->>2), admitted_by = (v_p->>2)
             FROM UNNEST((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->1) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[]) AS named(batch_id)
             WHERE batch.session_id = (v_p->>0) AND batch.batch_id = named.batch_id
               AND (batch.admitted_run IS NULL OR batch.admitted_run = (v_p->>2))
               AND batch.terminal_cause IS NULL
             RETURNING batch.batch_id LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_prompt.insert_snapshot' THEN
            v_rows := 0;
            FOR v_row IN INSERT INTO lash_prompt_snapshots
                 (session_id, owner, call, snapshot, request, written_epoch)
             VALUES ((v_p->>0), (v_p->>1), (v_p->>2), (v_p->>3), (v_p->>4), ((v_p->>5)::BIGINT))
             ON CONFLICT (session_id, owner, call) DO NOTHING
             RETURNING call LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'postgres_connection.lock_xact_shared_by_keys_seeded' THEN
            v_rows := 0;
            FOR v_row IN SELECT pg_advisory_xact_lock_shared(hashtextextended(encode(ordered.key, 'hex'), ((v_p->>1)::BIGINT)))
             FROM (SELECT DISTINCT key FROM unnest((ARRAY(SELECT decode(element, 'hex') FROM jsonb_array_elements_text(v_p->0) WITH ORDINALITY AS listed(element, position) ORDER BY position))::BYTEA[]) AS target(key) ORDER BY key) AS ordered LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_prompt_postgres.unsent_absent' THEN
            v_rows := 0;
            FOR v_row IN SELECT batch.hash
             FROM unnest((ARRAY(SELECT decode(element, 'hex') FROM jsonb_array_elements_text(v_p->0) WITH ORDINALITY AS listed(element, position) ORDER BY position))::BYTEA[], (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->1) WITH ORDINALITY AS listed(element, position) ORDER BY position))::TEXT[]) AS batch(hash, text)
             WHERE batch.text IS NULL
               AND NOT EXISTS (SELECT 1 FROM lash_prompt_texts AS stored WHERE stored.hash = batch.hash) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_prompt_postgres.insert_sent_texts' THEN
            INSERT INTO lash_prompt_texts (hash, text)
             SELECT hash, text FROM unnest((ARRAY(SELECT decode(element, 'hex') FROM jsonb_array_elements_text(v_p->0) WITH ORDINALITY AS listed(element, position) ORDER BY position))::BYTEA[], (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->1) WITH ORDINALITY AS listed(element, position) ORDER BY position))::TEXT[]) AS batch(hash, text)
             WHERE batch.text IS NOT NULL
             ON CONFLICT (hash) DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_prompt_postgres.insert_edges' THEN
            INSERT INTO lash_prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT (v_p->>0), (v_p->>1), (v_p->>2), part, hash
               FROM unnest((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->3) WITH ORDINALITY AS listed(element, position) ORDER BY position))::TEXT[], (ARRAY(SELECT decode(element, 'hex') FROM jsonb_array_elements_text(v_p->4) WITH ORDINALITY AS listed(element, position) ORDER BY position))::BYTEA[]) AS batch(part, hash);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_graph.admission_put' THEN
            v_rows := 0;
            FOR v_row IN INSERT INTO lash_effect_admissions
                 (exec_key, effect_id, owner_key, run_seq, member_ordinal, admission_ref,
                  written_epoch)
             VALUES ((v_p->>0), (decode(v_p->>1, 'hex')), (v_p->>2), ((v_p->>3)::BIGINT), ((v_p->>4)::BIGINT), (decode(v_p->>5, 'hex')), ((v_p->>6)::BIGINT))
             ON CONFLICT (exec_key, effect_id) DO NOTHING
             RETURNING exec_key LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_graph.admission_remove' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_effect_admissions
             WHERE exec_key = (v_p->>0) AND effect_id = (decode(v_p->>1, 'hex')) AND admission_ref = (decode(v_p->>2, 'hex'))
             RETURNING exec_key LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_run_record.delete_owner' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_run_records WHERE owner_key = (v_p->>0) RETURNING record_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."record_ref" IS NOT NULL THEN v_retired := v_retired || v_row."record_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_graph.admissions_clear' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_effect_admissions WHERE exec_key = (v_p->>0)
             RETURNING admission_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."admission_ref" IS NOT NULL THEN v_retired := v_retired || v_row."admission_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_actor_park.set_park' THEN
            UPDATE lash_actors
             SET failed_activations = CASE WHEN CAST((v_p->>1) AS TEXT) IS NULL AND park_json IS NOT NULL
                                           THEN 0 ELSE failed_activations END,
                 park_json = (v_p->>1)
             WHERE actor_key = (v_p->>0);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'postgres_connection.lock_xact_by_text' THEN
            v_rows := 0;
            FOR v_row IN SELECT pg_advisory_xact_lock(hashtextextended((v_p->>0), 0)) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn.end_run' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_session_runs
             SET terminal_kind = (v_p->>2), terminal_cause_json = (v_p->>3),
                 terminal_head_revision = ((v_p->>4)::BIGINT), terminal_at_ms = (v_now),
                 terminal_transcript_ref = (decode(v_p->>6, 'hex')),
                 terminal_window_revision = (
                     SELECT revision.head_revision FROM lash_session_head AS head
                     JOIN lash_session_revisions AS revision
                       ON revision.session_id = head.session_id
                      AND revision.head_revision = head.head_revision
                     WHERE head.session_id = (v_p->>0))
             WHERE session_id = (v_p->>0) AND run = (v_p->>1)
               AND admission_json IS NOT NULL AND terminal_kind IS NULL
             RETURNING run LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_turn_postgres.mark_terminal' THEN
            WITH marked AS (
                 UPDATE lash_session_revisions SET terminal_ordinal = (
                     SELECT terminal_ordinal + 1 FROM lash_session_head WHERE session_id = (v_p->>0)
                 ) WHERE session_id = (v_p->>0) AND head_revision = ((v_p->>1)::BIGINT) AND terminal_ordinal IS NULL
                 RETURNING terminal_ordinal
             )
             UPDATE lash_session_head SET terminal_ordinal = marked.terminal_ordinal
             FROM marked WHERE lash_session_head.session_id = (v_p->>0);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_turn_postgres.retention_releases' THEN
            v_rows := 0;
            FOR v_row IN SELECT retention_kind <> 'until_gc' AS releases
             FROM lash_session_meta WHERE session_id = (v_p->>0) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_session_mail.settle_held_inputs' THEN
            UPDATE lash_pending_turn_inputs
             SET state = (v_p->>2), terminal_at_ms = (v_now), admitted_by = NULL
             WHERE session_id = (v_p->>0) AND admitted_run = (v_p->>1) AND state NOT IN ('cancelled', 'completed');
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_session_mail.settle_held_batches' THEN
            UPDATE lash_queued_work_batches
             SET admitted_run = NULL, admitted_by = NULL,
                 terminal_cause = 'cancelled', terminal_at_ms = (v_now)
             WHERE session_id = (v_p->>0) AND admitted_run = (v_p->>1) AND terminal_cause IS NULL;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'artifact_referrer_edge.end_local' THEN
            BEGIN
                SELECT lash_artifact_end_local((v_p->>0), (v_p->>1), (v_p->>2), (v_p->>3), (v_now), (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->5) WITH ORDINALITY AS listed(element, position) ORDER BY position))) INTO v_answer;
            EXCEPTION WHEN invalid_text_representation THEN
                RAISE EXCEPTION 'lash_owner_commit: an end read a stored cleanup that is not JSON'
                    USING ERRCODE = 'LX006';
            END;
            v_frame := '[' || to_json(p_session)::TEXT || ',"';
            IF EXISTS (
                SELECT 1 FROM jsonb_array_elements(v_answer->'plans') AS planned(item)
                WHERE NOT COALESCE(
                    planned.item->>0 = 'frame_environment'
                    AND planned.item->>2 = v_p->>6
                    AND starts_with(planned.item->>1, v_frame)
                    AND right(planned.item->>1, 2) = '"]'
                    AND substr(planned.item->>1, length(v_frame) + 1,
                        length(planned.item->>1) - length(v_frame) - 2) ~ '^[^"\\[:cntrl:]]+$',
                    FALSE)
            ) OR EXISTS (
                SELECT 1 FROM jsonb_array_elements(v_answer->'checks') AS checked(item)
                WHERE NOT COALESCE(
                    (checked.item->>0 = v_p->>0 AND checked.item->>1 = v_p->>1)
                    OR EXISTS (
                        SELECT 1 FROM jsonb_array_elements(v_answer->'plans') AS planned(item)
                        WHERE planned.item->>0 = checked.item->>0
                          AND planned.item->>1 = checked.item->>1),
                    FALSE)
            ) THEN
                RAISE EXCEPTION 'lash_owner_commit: an end answered what the call cannot vouch for'
                    USING ERRCODE = 'LX006';
            END IF;
            v_rows := 1;
        WHEN 'durable_snapshot.clear_under' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_exec_snapshots
             WHERE substr(exec_key, 1, length(CAST((v_p->>0) AS TEXT))) = CAST((v_p->>0) AS TEXT)
             RETURNING root_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."root_ref" IS NOT NULL THEN v_retired := v_retired || v_row."root_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_graph.admissions_clear_under' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_effect_admissions
             WHERE substr(exec_key, 1, length(CAST((v_p->>0) AS TEXT))) = CAST((v_p->>0) AS TEXT)
             RETURNING admission_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."admission_ref" IS NOT NULL THEN v_retired := v_retired || v_row."admission_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_run_record.delete_under' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_run_records
             WHERE substr(owner_key, 1, length(CAST((v_p->>0) AS TEXT))) = CAST((v_p->>0) AS TEXT)
             RETURNING record_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."record_ref" IS NOT NULL THEN v_retired := v_retired || v_row."record_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_turn_postgres.delete_terminal_state' THEN
            v_rows := 0;
            FOR v_row IN WITH phase AS (
                 DELETE FROM lash_turn_phases WHERE session_id = (v_p->>0) AND run = (v_p->>1)
                 RETURNING checkpoint_ref, working_execution_ref, answer_ref
             ), namespaces AS (
                 DELETE FROM lash_turn_namespaces WHERE session_id = (v_p->>0) AND run = (v_p->>1)
                 RETURNING graph_root_ref
             ), cancel AS (
                 DELETE FROM lash_turn_cancel_requests WHERE session_id = (v_p->>0) AND turn_id = (v_p->>1)
             )
             SELECT (SELECT checkpoint_ref FROM phase) AS checkpoint_ref,
                 (SELECT working_execution_ref FROM phase) AS working_execution_ref,
                 ARRAY(SELECT answer_ref FROM phase WHERE answer_ref IS NOT NULL
                     UNION ALL
                     SELECT graph_root_ref FROM namespaces WHERE graph_root_ref IS NOT NULL)
                     AS closure_roots LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."checkpoint_ref" IS NOT NULL THEN v_phase_ends := v_phase_ends || v_row."checkpoint_ref"::BYTEA; END IF;
                IF v_row."working_execution_ref" IS NOT NULL THEN v_retired := v_retired || v_row."working_execution_ref"::BYTEA; END IF;
                v_retired := v_retired || COALESCE(v_row."closure_roots", '{}'::BYTEA[]);
            END LOOP;
        WHEN 'durable_wait.revoke_scope' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_waits SET state = 'revoked', resolved_at_ms = (v_now)
             WHERE owner_scope = (v_p->>0) AND state = 'pending'
             RETURNING owner_actor LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."owner_actor" IS NOT NULL THEN v_wakes := v_wakes || v_row."owner_actor"::TEXT; END IF;
            END LOOP;
        WHEN 'durable_wait.compact_scope' THEN
            UPDATE lash_waits
             SET compacted = TRUE, owner_actor = NULL, target_process = NULL,
                 deadline_ms = NULL, resolution_ref = NULL, call_id = NULL, tool_id = NULL,
                 key_name = NULL
             WHERE owner_scope = (v_p->>0) AND state <> 'pending' AND NOT compacted;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_wait.resolve_process_terminal' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_waits
             SET state = 'resolved', resolution_digest = (v_p->>1), resolution_ref = (v_p->>2),
                 resolved_at_ms = (v_now)
             WHERE target_process = (v_p->>0) AND kind = 'process_terminal' AND state = 'pending'
             RETURNING owner_actor LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."owner_actor" IS NOT NULL THEN v_wakes := v_wakes || v_row."owner_actor"::TEXT; END IF;
            END LOOP;
        WHEN 'durable_wait.resolve_child_session' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_waits
             SET state = 'resolved', resolution_digest = (v_p->>1), resolution_ref = (v_p->>2),
                 resolved_at_ms = (v_now)
             WHERE target_process = (v_p->>0) AND kind = 'child_session' AND state = 'pending'
             RETURNING owner_actor LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."owner_actor" IS NOT NULL THEN v_wakes := v_wakes || v_row."owner_actor"::TEXT; END IF;
            END LOOP;
        WHEN 'parent_end_plan.lock_turn' THEN
            v_rows := 0;
            FOR v_row IN SELECT run FROM lash_session_runs
             WHERE session_id = (v_p->>0) AND (run = (v_p->>1) OR run IN (
                 SELECT admitted_run FROM lash_pending_turn_inputs
                 WHERE session_id = (v_p->>0) AND COALESCE(source_key, input_id) = (v_p->>1)))
             ORDER BY run FOR UPDATE LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'parent_end_plan.turn_close' THEN
            v_rows := 0;
            FOR v_row IN SELECT
                 COALESCE((SELECT scope_has_holds FROM lash_session_runs
                     WHERE session_id = (v_p->>0) AND run = (v_p->>1)), FALSE) AS has_holds,
                 EXISTS(SELECT 1 FROM lash_processes
                     WHERE lifetime = 'until' AND lifetime_scope_kind = 'turn'
                       AND lifetime_scope_id = (v_p->>2) AND cancel_requested_at_ms IS NULL
                       AND status IN ('running', 'waiting')) AS has_until LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'process.release_consumer_holds_owned_by' THEN
            UPDATE lash_processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL, consumer_hold_cancels = NULL
             WHERE consumer_hold_scope_kind = (v_p->>0) AND consumer_hold_scope_id = (v_p->>1);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'abandoned_consumer_hold.forget_owned_by' THEN
            DELETE FROM lash_abandoned_consumer_holds
             WHERE owner_scope_kind = (v_p->>0) AND owner_scope_id = (v_p->>1);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_session_close.scope_ending' THEN
            INSERT INTO lash_session_scope_ends
                 (session_id, scope_key, begun_at_ms, written_epoch)
             VALUES ((v_p->>0), (v_p->>1), (v_now), ((v_p->>3)::BIGINT))
             ON CONFLICT (session_id, scope_key) DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_prompt_postgres.settle' THEN
            v_rows := 0;
            FOR v_row IN WITH released AS (
                 DELETE FROM lash_prompt_snapshot_texts
                  WHERE session_id = (v_p->>0) AND owner = (v_p->>1) AND part = 'request'
                    AND ((v_p->>2)::TEXT IS NULL OR call <> (v_p->>2)::TEXT)
                 RETURNING hash
             ), bare AS (
                 DELETE FROM lash_prompt_snapshots
                  WHERE session_id = (v_p->>0) AND owner = (v_p->>1) AND snapshot IS NULL AND request IS NOT NULL
                    AND ((v_p->>2)::TEXT IS NULL OR call <> (v_p->>2)::TEXT)
             ), kept AS (
                 UPDATE lash_prompt_snapshots SET request = NULL
                  WHERE session_id = (v_p->>0) AND owner = (v_p->>1)
                    AND snapshot IS NOT NULL AND request IS NOT NULL
                    AND ((v_p->>2)::TEXT IS NULL OR call <> (v_p->>2)::TEXT)
             )
             SELECT DISTINCT hash FROM released LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."hash" IS NOT NULL THEN v_texts := v_texts || v_row."hash"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_prompt_postgres.expire_audits' THEN
            v_rows := 0;
            FOR v_row IN WITH candidates AS MATERIALIZED (
                 SELECT audit.session_id, audit.owner, audit.call
                 FROM lash_prompt_snapshots AS audit
                 JOIN lash_session_runs AS ended ON ended.session_id = audit.session_id
                   AND audit.owner = 'turn:' || ended.run
                 WHERE ((v_p->>0)::TEXT IS NULL OR audit.session_id = (v_p->>0))
                   AND audit.request IS NULL AND audit.snapshot IS NOT NULL
                   AND ended.terminal_kind IS NOT NULL
                   AND ended.terminal_window_revision IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_session_revisions AS revision
                       WHERE revision.session_id = ended.session_id
                         AND revision.head_revision = ended.terminal_window_revision
                   )
                 ORDER BY audit.session_id, audit.owner, audit.call LIMIT ((v_p->>1)::BIGINT)
                 FOR UPDATE OF audit
             ), released AS (
                 DELETE FROM lash_prompt_snapshot_texts
                 WHERE (session_id, owner, call) IN (SELECT * FROM candidates)
                   AND part = 'snapshot'
                 RETURNING hash
             ), expired AS (
                 UPDATE lash_prompt_snapshots SET snapshot = NULL
                 WHERE (session_id, owner, call) IN (SELECT * FROM candidates)
                 RETURNING call
             ) SELECT (SELECT COUNT(*) FROM expired) AS released_count, released.hash
               FROM (SELECT 1) AS singleton LEFT JOIN released ON TRUE LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."hash" IS NOT NULL THEN v_texts := v_texts || v_row."hash"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_prompt_postgres.hold_prefix' THEN
            v_rows := 0;
            FOR v_row IN SELECT hash FROM lash_prompt_prefixes WHERE session_id = (v_p->>0) FOR SHARE LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."hash" IS NOT NULL THEN v_protect := v_protect || v_row."hash"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_prompt.release_prefix' THEN
            v_rows := 0;
            FOR v_row IN DELETE FROM lash_prompt_prefixes WHERE session_id = (v_p->>0) RETURNING hash LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."hash" IS NOT NULL THEN v_texts := v_texts || v_row."hash"::BYTEA; END IF;
            END LOOP;
        WHEN 'durable_prompt_postgres.reclaim_texts' THEN
            v_tfront := ARRAY(SELECT DISTINCT hash FROM unnest(v_texts) AS released(hash) ORDER BY hash);
            IF cardinality(v_tfront) > 0 THEN
                v_closure := ARRAY(WITH RECURSIVE closure AS (
                 SELECT unnest((v_tfront)::BYTEA[]) AS hash
                 UNION
                 SELECT edge.child FROM closure
                   JOIN lash_prompt_text_edges AS edge ON edge.parent = closure.hash
                  WHERE NOT (closure.hash = ANY((v_protect)::BYTEA[]))
             ) SELECT hash FROM closure WHERE NOT (hash = ANY((v_protect)::BYTEA[])) ORDER BY hash);
                PERFORM pg_advisory_xact_lock(hashtextextended(encode(ordered.key, 'hex'), (2)))
             FROM (SELECT DISTINCT key FROM unnest((v_closure)::BYTEA[]) AS target(key) ORDER BY key) AS ordered;
                v_tfront := ARRAY(SELECT hash FROM unnest(v_tfront) AS released(hash)
                    WHERE NOT (hash = ANY(v_protect)) ORDER BY hash);
                WHILE cardinality(v_tfront) > 0 LOOP
                    SELECT COALESCE(array_agg(level.hash), '{}'), COALESCE(array_agg(edge.child), '{}')
                    INTO v_tparents, v_tchildren
                    FROM unnest(v_tfront) AS level(hash), LATERAL (SELECT child FROM lash_prompt_text_edges WHERE parent = (level.hash) ORDER BY child) AS edge;
                    WITH gone AS (DELETE FROM lash_prompt_texts AS stored
             WHERE stored.hash = ANY((v_tfront)::BYTEA[])
               AND NOT EXISTS (
                   SELECT 1 FROM lash_prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM lash_prompt_prefixes AS prefix WHERE prefix.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM lash_prompt_text_edges AS edge WHERE edge.child = stored.hash
               ) RETURNING hash)
                    SELECT COALESCE(array_agg(hash), '{}') INTO v_closure FROM gone;
                    v_tfront := ARRAY(SELECT DISTINCT edge.child
                        FROM unnest(v_tparents, v_tchildren) AS edge(parent, child)
                        WHERE edge.parent = ANY(v_closure) AND NOT (edge.child = ANY(v_protect))
                        ORDER BY edge.child);
                END LOOP;
            END IF;
            v_texts := '{}';
            v_protect := '{}';
            v_rows := 0;
        WHEN 'durable_head_commit.plugin_writers_held' THEN
            v_rows := 0;
            FOR v_row IN SELECT plugin_id FROM lash_fleet_plugin_writers
             WHERE (plugin_id, min_format, max_format) IN (
                 SELECT * FROM unnest((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->0) WITH ORDINALITY AS listed(element, position) ORDER BY position))::TEXT[], (ARRAY(SELECT element::BIGINT FROM jsonb_array_elements_text(v_p->1) WITH ORDINALITY AS listed(element, position) ORDER BY position))::INTEGER[], (ARRAY(SELECT element::BIGINT FROM jsonb_array_elements_text(v_p->2) WITH ORDINALITY AS listed(element, position) ORDER BY position))::INTEGER[])) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_postgres.lock_session_writes' THEN
            v_rows := 0;
            FOR v_row IN SELECT held.locked, pg_advisory_xact_lock(hashtextextended((v_p->>1), 1::bigint))
             FROM (
                 SELECT count(*) AS locked FROM (
                     SELECT 1 FROM lash_actors WHERE actor_key = (v_p->>0) FOR NO KEY UPDATE
                 ) AS actor
             ) AS held
             WHERE held.locked > 0 LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'deleted_session.exists' THEN
            v_rows := 0;
            FOR v_row IN SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = (v_p->>0)
             ) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                IF v_row."exists" THEN v_rows := v_rows + 1; END IF;
            END LOOP;
        WHEN 'session_meta.exists_materialized' THEN
            v_rows := 0;
            FOR v_row IN SELECT EXISTS(
                 SELECT 1 FROM lash_session_head WHERE session_id = (v_p->>0)
                 UNION ALL
                 SELECT 1 FROM lash_session_meta WHERE session_id = (v_p->>0)
             ) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                IF v_row."exists" THEN v_rows := v_rows + 1; END IF;
            END LOOP;
        WHEN 'turn_commit.select_receipt' THEN
            v_rows := 0;
            FOR v_row IN SELECT turn_commit_hash, result_json, outcome_code,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count, committed_at_ms, head_revision
                 FROM lash_runtime_turn_commits
                 WHERE session_id = (v_p->>0) AND turn_id = (v_p->>1) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'durable_head_commit.head_at' THEN
            v_rows := 0;
            FOR v_row IN SELECT head_revision = ((v_p->>1)::BIGINT) AS current FROM lash_session_head
             WHERE session_id = (v_p->>0) FOR UPDATE LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                IF v_row."current" THEN v_rows := v_rows + 1; END IF;
            END LOOP;
        WHEN 'session_head.select_revision_for_update' THEN
            v_rows := 0;
            FOR v_row IN SELECT head_revision
             FROM lash_session_head
             WHERE session_id = (v_p->>0)
             FOR UPDATE LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'graph_node.select_parent_facts_for_update' THEN
            v_rows := 0;
            FOR v_row IN SELECT generation, frame_node_id, session_id FROM lash_graph_nodes
                 WHERE node_id = (v_p->>0) AND tombstoned = FALSE
                 FOR UPDATE LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'graph_node.select_occupied' THEN
            v_rows := 0;
            FOR v_row IN SELECT node_id
             FROM lash_graph_nodes
             WHERE node_id = ANY((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->0) WITH ORDINALITY AS listed(element, position) ORDER BY position))) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'graph_node.insert_batch' THEN
            v_rows := 0;
            FOR v_row IN INSERT INTO lash_graph_nodes
                 (session_id, node_id, parent_node_id, generation, frame_node_id, body_bytes,
                  node_json)
             SELECT (v_p->>0), node.node_id, node.parent_node_id, node.generation, node.frame_node_id,
                    node.body_bytes, node.node_json
             FROM UNNEST((ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->1) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[], (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->2) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[], (ARRAY(SELECT element::BIGINT FROM jsonb_array_elements_text(v_p->3) WITH ORDINALITY AS listed(element, position) ORDER BY position))::bigint[], (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->4) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[], (ARRAY(SELECT element::BIGINT FROM jsonb_array_elements_text(v_p->5) WITH ORDINALITY AS listed(element, position) ORDER BY position))::bigint[],
                         (ARRAY(SELECT element FROM jsonb_array_elements_text(v_p->6) WITH ORDINALITY AS listed(element, position) ORDER BY position))::text[])
                 AS node(node_id, parent_node_id, generation, frame_node_id, body_bytes,
                         node_json)
             ON CONFLICT DO NOTHING
             RETURNING node_id LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'session_revision.insert' THEN
            INSERT INTO lash_session_revisions
                 (session_id, head_revision, leaf_node_id, checkpoint_ref, config_ref)
             VALUES ((v_p->>0), ((v_p->>1)::BIGINT), (v_p->>2), (decode(v_p->>3, 'hex')), (decode(v_p->>4, 'hex')));
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'session_head.upsert_cas' THEN
            INSERT INTO lash_session_head
             (session_id, head_revision)
             VALUES ((v_p->>0), ((v_p->>1)::BIGINT))
             ON CONFLICT (session_id) DO UPDATE SET
                head_revision = EXCLUDED.head_revision
             WHERE lash_session_head.head_revision = ((v_p->>2)::BIGINT);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'durable_head_commit.touch' THEN
            v_rows := 0;
            FOR v_row IN UPDATE lash_session_meta SET last_commit_at_ms = (v_now) WHERE session_id = (v_p->>0)
             RETURNING retention_kind <> 'until_gc' AS releases LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'session_revision.prune_unretained' THEN
            v_rows := 0;
            FOR v_row IN WITH scoped_heads AS (
                 SELECT session_id, head_revision, terminal_ordinal FROM lash_session_head
                 WHERE session_id = (v_p->>1)
                 UNION ALL
                 SELECT session_id, head_revision, terminal_ordinal FROM lash_session_head
                 WHERE (v_p->>1) IS NULL
             ), policies AS (
                 SELECT head.session_id, head.head_revision, head.terminal_ordinal,
                        COALESCE(meta.retention_kind, 'until_gc') AS kind,
                        COALESCE(meta.retention_last_turns, 0) AS last_turns
                 FROM scoped_heads AS head LEFT JOIN lash_session_meta AS meta
                   ON meta.session_id = head.session_id
             ), candidates AS (
                 SELECT revision.session_id, revision.head_revision FROM policies
                 JOIN lash_session_revisions AS revision ON revision.session_id = policies.session_id
                 WHERE policies.kind = 'head_only' OR (policies.kind = 'until_gc' AND ((v_p->>0)::BIGINT) = 1)
                 UNION ALL
                 SELECT revision.session_id, revision.head_revision FROM policies
                 JOIN lash_session_revisions AS revision ON revision.session_id = policies.session_id
                   AND revision.terminal_ordinal IS NULL
                 WHERE policies.kind = 'last_turns'
                 UNION ALL
                 SELECT revision.session_id, revision.head_revision FROM policies
                 JOIN lash_session_revisions AS revision ON revision.session_id = policies.session_id
                   AND revision.terminal_ordinal <= policies.terminal_ordinal - policies.last_turns
                 WHERE policies.kind = 'last_turns'
             ) DELETE FROM lash_session_revisions AS revision
             WHERE (revision.session_id, revision.head_revision) IN (SELECT * FROM candidates)
               AND revision.head_revision != (
                   SELECT head.head_revision FROM lash_session_head AS head
                   WHERE head.session_id = revision.session_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_pins AS pin
                   WHERE pin.session_id = revision.session_id
                     AND (
                         (pin.target_kind = 'revision'
                          AND pin.target_id = CAST(revision.head_revision AS TEXT))
                         OR (pin.target_kind = 'turn' AND EXISTS (
                             SELECT 1 FROM lash_session_runs AS ended
                             WHERE ended.session_id = pin.session_id
                               AND ended.run = pin.target_id
                               AND ended.terminal_head_revision = revision.head_revision
                         ))
                         OR (pin.target_kind = 'input' AND EXISTS (
                             SELECT 1 FROM lash_pending_turn_inputs AS bound
                             JOIN lash_session_runs AS ended
                               ON ended.session_id = bound.session_id
                              AND ended.run = bound.admitted_run
                             WHERE bound.session_id = pin.session_id
                               AND bound.input_id = pin.target_id
                               AND ended.terminal_head_revision = revision.head_revision
                         ))
                     )
               )
             RETURNING session_id, leaf_node_id, checkpoint_ref, config_ref LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
                IF v_row."leaf_node_id" IS NOT NULL THEN v_leaves := v_leaves || v_row."leaf_node_id"::TEXT; END IF;
                IF v_row."checkpoint_ref" IS NOT NULL THEN v_retired := v_retired || v_row."checkpoint_ref"::BYTEA; END IF;
                IF v_row."config_ref" IS NOT NULL THEN v_retired := v_retired || v_row."config_ref"::BYTEA; END IF;
            END LOOP;
        WHEN 'pending_turn_input.release_settled_bodies' THEN
            UPDATE lash_pending_turn_inputs SET input_json = NULL
             WHERE (session_id, enqueue_seq) IN (
                 SELECT held.session_id, held.enqueue_seq
                 FROM lash_pending_turn_inputs AS held
                 JOIN lash_session_runs AS ended
                   ON ended.session_id = held.session_id AND ended.run = held.admitted_run
                 WHERE (held.session_id = (v_p->>0) OR (v_p->>0) IS NULL)
                   AND held.state IN ('cancelled', 'completed')
                   AND held.input_json IS NOT NULL
                   AND ended.terminal_kind IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_session_revisions AS kept
                       WHERE kept.session_id = ended.session_id
                         AND kept.head_revision = COALESCE(
                             ended.terminal_window_revision, ended.terminal_head_revision)
                   )
                 LIMIT ((v_p->>1)::BIGINT)
             );
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'queued_work_batch.release_settled_payloads' THEN
            UPDATE lash_queued_work_batches SET payload_json = NULL
             WHERE (session_id, enqueue_seq) IN (
                 SELECT command.session_id, command.enqueue_seq
                 FROM lash_queued_work_batches AS command
                 JOIN lash_runtime_turn_commits AS receipt
                   ON receipt.session_id = command.session_id
                  AND receipt.turn_id = command.settled_operation_key
                 WHERE (command.session_id = (v_p->>0) OR (v_p->>0) IS NULL)
                   AND command.terminal_cause IS NOT NULL
                   AND command.payload_json IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_session_revisions AS kept
                       WHERE kept.session_id = receipt.session_id
                         AND kept.head_revision = receipt.head_revision
                   )
                 LIMIT ((v_p->>1)::BIGINT)
             );
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'tool_intent_submission.release_settled_payloads' THEN
            WITH scoped AS (
                 SELECT replay_key, owner_session_id, owner_run FROM lash_tool_intent_submissions
                 WHERE owner_session_id = (v_p->>0)
                   AND intent_json IS NOT NULL AND settlement_json IS NOT NULL
                 UNION ALL
                 SELECT replay_key, owner_session_id, owner_run FROM lash_tool_intent_submissions
                 WHERE (v_p->>0) IS NULL AND owner_session_id IS NOT NULL
                   AND intent_json IS NOT NULL AND settlement_json IS NOT NULL
             ), released AS (
                 SELECT scoped.replay_key FROM scoped
                 JOIN lash_session_runs AS ended
                   ON ended.session_id = scoped.owner_session_id
                  AND ended.run = scoped.owner_run
                 WHERE ended.terminal_head_revision IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_session_revisions AS revision
                       WHERE revision.session_id = ended.session_id
                         AND revision.head_revision = ended.terminal_head_revision
                   )
                 ORDER BY scoped.replay_key
                 LIMIT ((v_p->>1)::BIGINT)
             ) UPDATE lash_tool_intent_submissions SET intent_json = NULL
             WHERE replay_key IN (SELECT replay_key FROM released);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'queued_work_batch.settle_command' THEN
            UPDATE lash_queued_work_batches
             SET terminal_cause = (v_p->>3),
                 terminal_at_ms = (v_now),
                 settled_operation_key = (v_p->>4)
             WHERE session_id = (v_p->>0)
               AND batch_id = (v_p->>1)
               AND admitted_run IS NULL
               AND terminal_cause IS NULL;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'pending_turn_input.settle_admitted' THEN
            UPDATE lash_pending_turn_inputs
             SET state = (v_p->>2),
                 terminal_at_ms = (v_now),
                 admitted_by = NULL
             WHERE session_id = (v_p->>0) AND input_id = (v_p->>1) AND admitted_run = (v_p->>3);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'pending_turn_input.release_admitted' THEN
            UPDATE lash_pending_turn_inputs
             SET state = CASE WHEN state IN ('accepted') THEN 'pending_active' ELSE state END,
                 admitted_run = NULL,
                 admitted_by = NULL
             WHERE session_id = (v_p->>0) AND input_id = (v_p->>1) AND admitted_run = (v_p->>2);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'queued_work_batch.settle_admitted' THEN
            UPDATE lash_queued_work_batches
             SET admitted_run = NULL,
                 admitted_by = NULL,
                 terminal_cause = (v_p->>3),
                 terminal_at_ms = (v_now)
             WHERE session_id = (v_p->>0)
               AND batch_id = (v_p->>1)
               AND admitted_run = (v_p->>2);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'queued_work_batch.release_admitted' THEN
            UPDATE lash_queued_work_batches
             SET admitted_run = NULL,
                 admitted_by = NULL
             WHERE session_id = (v_p->>0) AND batch_id = (v_p->>1) AND admitted_run = (v_p->>2);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'artifact_referrer_fence.select_is_fenced' THEN
            v_rows := 0;
            FOR v_row IN SELECT EXISTS (
                 SELECT 1 FROM lash_referrer_fences
                 WHERE referrer_kind = (v_p->>0) AND referrer_id = (v_p->>1)
             ) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                IF v_row."exists" THEN v_rows := v_rows + 1; END IF;
            END LOOP;
        WHEN 'durable_head_commit.lock_attachment' THEN
            v_rows := 0;
            FOR v_row IN SELECT pg_advisory_xact_lock(CAST(((v_p->>0)::BIGINT) AS INTEGER), hashtext((v_p->>1))) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'attachment_referrer_edge.select_deleting' THEN
            v_rows := 0;
            FOR v_row IN SELECT EXISTS (SELECT 1 FROM lash_attachment_condemnations WHERE attachment_id = (decode(v_p->>0, 'hex')) AND phase = 'deleting') LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                IF v_row."exists" THEN v_rows := v_rows + 1; END IF;
            END LOOP;
        WHEN 'attachment_upload.select_evidence' THEN
            v_rows := 0;
            FOR v_row IN SELECT 1 FROM lash_attachment_uploads WHERE attachment_id = (decode(v_p->>0, 'hex')) LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'attachment_condemnation.delete_unclaimed_condemned' THEN
            DELETE FROM lash_attachment_condemnations
             WHERE attachment_id = (decode(v_p->>0, 'hex')) AND phase = 'condemned' AND write_token IS NULL;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'attachment_referrer_edge.insert' THEN
            INSERT INTO lash_attachment_referrer_edges (attachment_id, referrer_kind, referrer_id)
             VALUES ((decode(v_p->>0, 'hex')), (v_p->>1), (v_p->>2)) ON CONFLICT (attachment_id, referrer_kind, referrer_id) DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'attachment_session_hold.insert' THEN
            INSERT INTO lash_attachment_session_holds (session_id, attachment_id, holder_kind, holder_id)
             VALUES ((v_p->>0), (decode(v_p->>1, 'hex')), (v_p->>2), (v_p->>3))
             ON CONFLICT (session_id, attachment_id, holder_kind, holder_id) DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'attachment_session_hold.queue_released_ingress' THEN
            INSERT INTO lash_attachment_session_releases (session_id, attachment_id)
             SELECT hold.session_id, hold.attachment_id FROM lash_attachment_session_holds AS hold
             WHERE (hold.session_id = (v_p->>0) OR (v_p->>0) IS NULL)
               AND ((hold.holder_kind = 'input' AND EXISTS (
                       SELECT 1 FROM lash_pending_turn_inputs AS released
                       WHERE released.session_id = hold.session_id
                         AND released.input_id = hold.holder_id
                         AND released.input_json IS NULL
                   ))
                 OR (hold.holder_kind = 'batch' AND EXISTS (
                       SELECT 1 FROM lash_queued_work_batches AS released
                       WHERE released.session_id = hold.session_id
                         AND released.batch_id = hold.holder_id
                         AND released.payload_json IS NULL
                   )))
             ON CONFLICT (session_id, attachment_id) DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'attachment_session_hold.end_released_ingress' THEN
            DELETE FROM lash_attachment_session_holds AS hold
             WHERE (hold.session_id = (v_p->>0) OR (v_p->>0) IS NULL)
               AND ((hold.holder_kind = 'input' AND EXISTS (
                       SELECT 1 FROM lash_pending_turn_inputs AS released
                       WHERE released.session_id = hold.session_id
                         AND released.input_id = hold.holder_id
                         AND released.input_json IS NULL
                   ))
                 OR (hold.holder_kind = 'batch' AND EXISTS (
                       SELECT 1 FROM lash_queued_work_batches AS released
                       WHERE released.session_id = hold.session_id
                         AND released.batch_id = hold.holder_id
                         AND released.payload_json IS NULL
                   )));
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'attachment_session_hold.release_edges' THEN
            DELETE FROM lash_attachment_referrer_edges AS edge
             WHERE edge.referrer_kind = 'session' AND edge.referrer_id = (v_p->>0)
               AND edge.attachment_id IN (
                   SELECT queued.attachment_id FROM lash_attachment_session_releases AS queued
                   WHERE queued.session_id = (v_p->>0)
                   ORDER BY queued.attachment_id LIMIT ((v_p->>1)::BIGINT)
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_attachment_session_holds AS hold
                   WHERE hold.session_id = (v_p->>0) AND hold.attachment_id = edge.attachment_id
               );
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'attachment_session_hold.dequeue' THEN
            DELETE FROM lash_attachment_session_releases
             WHERE session_id = (v_p->>0) AND attachment_id IN (
                 SELECT queued.attachment_id FROM lash_attachment_session_releases AS queued
                 WHERE queued.session_id = (v_p->>0)
                 ORDER BY queued.attachment_id LIMIT ((v_p->>1)::BIGINT)
             );
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'turn_ingress.has_admissible_work' THEN
            v_rows := 0;
            FOR v_row IN SELECT EXISTS(
                SELECT 1 FROM lash_session_runs
                WHERE session_id = (v_p->>0)
                  AND admission_json IS NOT NULL
                  AND terminal_kind IS NULL
             ) OR EXISTS(
                SELECT 1
                FROM lash_queued_work_batches qwb
                WHERE qwb.session_id = (v_p->>0)
                  AND qwb.admitted_run IS NULL
                  AND qwb.terminal_cause IS NULL
             ) OR EXISTS(
                SELECT 1
                FROM lash_pending_turn_inputs pti
                WHERE pti.session_id = (v_p->>0)
                  AND pti.state IN ('pending_active', 'deferred_next_turn')
                  AND pti.admitted_run IS NULL
             ) AS work_remaining LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                v_rows := v_rows + 1;
            END LOOP;
        WHEN 'turn_commit.insert_staged' THEN
            INSERT INTO lash_runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, outcome_code, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version,
                failure_evidence, head_revision
             )
             VALUES ((v_p->>0), (v_p->>1), (v_p->>2), (v_p->>3), (v_p->>4), (v_now), (v_p->>6), ((v_p->>7)::BIGINT), ((v_p->>8)::BIGINT), ((v_p->>9)::BOOLEAN), ((v_p->>10)::BIGINT));
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'artifact_referrer_fence.insert_fence' THEN
            INSERT INTO lash_referrer_fences
             (referrer_kind, referrer_id, ended_at_ms)
             VALUES ((v_p->>0), (v_p->>1), (v_now))
             ON CONFLICT DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'artifact_cleanup.insert_if_absent' THEN
            INSERT INTO lash_artifact_cleanup_obligations
             (referrer_kind, referrer_id, cleanup_json, obligation_id, obligation_state, obligation_due_at_ms)
             VALUES ((v_p->>0), (v_p->>1), (v_p->>2), (v_p->>3), 'due', (v_now))
             ON CONFLICT (referrer_kind, referrer_id) DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'artifact_cleanup.replace_guard_with_ended' THEN
            UPDATE lash_artifact_cleanup_obligations
             SET cleanup_json = (v_p->>2), obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = (v_now), obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL, obligation_last_error_code = NULL,
                 obligation_settled_at_ms = NULL
             WHERE referrer_kind = (v_p->>0) AND referrer_id = (v_p->>1);
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        WHEN 'artifact_referrer_edge.insert_edge' THEN
            INSERT INTO lash_artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             VALUES ((v_p->>0), (v_p->>1), (v_p->>2), (v_p->>3))
             ON CONFLICT DO NOTHING;
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{}'::JSONB; END IF;
        ELSE
            RAISE EXCEPTION 'lash_owner_commit: no step %', v_step->>'s' USING ERRCODE = 'LX000';
        END CASE;
        IF v_step ? 'c' THEN
            v_slots := v_slots || jsonb_build_object(v_step->>'c', COALESCE(v_first, 'null'::JSONB));
        END IF;
        IF v_step ? 'n' AND v_rows <> (v_step->>'n')::BIGINT THEN
            RAISE EXCEPTION 'lash_owner_commit: step % refused', v_index - 1 USING ERRCODE = 'LX001';
        END IF;
        IF p_statement_ms IS NOT NULL
            AND clock_timestamp() - statement_timestamp() > p_statement_ms * INTERVAL '1 millisecond' THEN
            RAISE EXCEPTION 'canceling statement due to statement timeout' USING ERRCODE = '57014';
        END IF;
        IF v_step ? 'r' THEN
            v_retired := v_retired || ARRAY(SELECT decode(jsonb_array_elements_text(v_step->'r'), 'hex'));
        END IF;
        IF v_step ? 'a' THEN
            v_leaves := v_leaves || ARRAY(SELECT jsonb_array_elements_text(v_step->'a'));
        END IF;
        -- Retire the ancestry only the step's leaves kept alive, each leaf
        -- in id order: a node goes while no live child, revision or
        -- terminal window names it.
        IF cardinality(v_leaves) > 0 THEN
            FOR v_node IN SELECT DISTINCT leaf FROM unnest(v_leaves) AS named(leaf) ORDER BY leaf LOOP
                LOOP
                    SELECT parent_node_id FROM lash_graph_nodes
             WHERE node_id = (v_node) AND tombstoned = FALSE
             FOR UPDATE
                    INTO v_parent;
                    EXIT WHEN NOT FOUND;
                    SELECT
                EXISTS(
                    SELECT 1 FROM lash_graph_nodes
                    WHERE parent_node_id = (v_node) AND tombstoned = FALSE
                )
                OR EXISTS(
                    SELECT 1 FROM lash_session_revisions WHERE leaf_node_id = (v_node)
                )
                    INTO v_reached;
                    EXIT WHEN v_reached;
                    UPDATE lash_graph_nodes SET tombstoned = TRUE WHERE node_id = (v_node);
                    EXIT WHEN v_parent IS NULL;
                    v_node := v_parent;
                END LOOP;
            END LOOP;
            v_leaves := '{}';
        END IF;
        -- Wake each owner the step settled but the committing actor; one
        -- that ended or is gone has nothing to wake.
        IF cardinality(v_wakes) > 0 THEN
            FOR v_owner IN SELECT DISTINCT owner FROM unnest(v_wakes) AS woke(owner) ORDER BY owner LOOP
                IF v_owner <> p_actor THEN
                    WITH woken AS (UPDATE lash_actors
             SET mail_seq = mail_seq + 1,
                 state = CASE WHEN state IN ('idle', 'waiting') THEN 'ready' ELSE state END,
                 ready_at_ms = CASE WHEN state IN ('idle', 'waiting') THEN CAST((v_now) AS BIGINT)
                                    ELSE ready_at_ms END,
                 next_due_ms = CASE WHEN state IN ('idle', 'waiting') THEN NULL
                                    ELSE next_due_ms END
             WHERE actor_key = (v_owner) AND state <> 'terminal'
             RETURNING mail_seq, state, owner_node, owner_boot)
                    SELECT count(*) INTO v_woken FROM woken;
                END IF;
            END LOOP;
            v_wakes := '{}';
        END IF;
    END LOOP;
    -- Hold what the steps retired for the bounded retirement steps
    -- (FIG-6191, D-RET): each retired root's closure, and an ended turn's
    -- PhaseRoot alone, every node locked before any hold row (FIG-6193).
    IF cardinality(v_retired) > 0 OR cardinality(v_phase_ends) > 0 THEN
        SELECT ARRAY(SELECT hash FROM lash_blobs
             WHERE hash = ANY((v_retired)::bytea[]) OR hash = ANY((v_phase_ends)::bytea[])
             ORDER BY hash FOR KEY SHARE)
        INTO v_held;
        INSERT INTO lash_retirement_holds AS held (root_ref, closure, queued_at_ms)
             SELECT node.hash, bool_or(node.closure), (v_now)::bigint
             FROM (SELECT UNNEST((v_retired)::bytea[]) AS hash, TRUE AS closure
                   UNION ALL SELECT UNNEST((v_phase_ends)::bytea[]), FALSE) AS node
             WHERE node.hash = ANY((v_held)::bytea[])
             GROUP BY node.hash
             ORDER BY node.hash
             ON CONFLICT (root_ref) DO UPDATE SET closure = held.closure OR excluded.closure;
    END IF;
    IF p_formats IS NOT NULL THEN
        UPDATE lash_actors SET formats = (p_formats) WHERE actor_key = (p_actor);
    END IF;
    IF p_ack IS NOT NULL THEN
        WITH acked AS (
                 UPDATE lash_actors SET acked_seq = (p_ack) WHERE actor_key = (p_actor) AND acked_seq < (p_ack)
             )
             DELETE FROM lash_actor_mail WHERE actor_key = (p_actor) AND seq <= (p_ack);
    END IF;
    CASE p_release
    WHEN 'terminal' THEN
        DELETE FROM lash_actor_mail WHERE actor_key = (p_actor);
        UPDATE lash_actors
             SET state = 'terminal', ready_at_ms = NULL, next_due_ms = NULL,
                 acked_seq = mail_seq, epoch = epoch + 1,
                 owner_node = NULL, owner_boot = NULL
             WHERE actor_key = (p_actor)
             RETURNING state
        INTO v_state;
    WHEN 'idle', 'waiting', 'ready' THEN
        UPDATE lash_actors
             SET state = CASE WHEN mail_seq > acked_seq OR CAST((p_release) AS TEXT) = 'ready'
                              THEN 'ready' ELSE CAST((p_release) AS TEXT) END,
                 ready_at_ms = CASE WHEN mail_seq > acked_seq OR CAST((p_release) AS TEXT) = 'ready'
                                    THEN CAST((v_now) AS BIGINT) ELSE NULL END,
                 next_due_ms = CASE WHEN mail_seq > acked_seq OR CAST((p_release) AS TEXT) = 'ready'
                                    THEN NULL ELSE CAST((p_due) AS BIGINT) END,
                 epoch = epoch + 1, owner_node = NULL, owner_boot = NULL
             WHERE actor_key = (p_actor)
             RETURNING state
        INTO v_state;
    WHEN 'ready_unadvanced' THEN
        UPDATE lash_actors
             SET state = CASE WHEN mail_seq > acked_seq OR CAST(('ready') AS TEXT) = 'ready'
                              THEN 'ready' ELSE CAST(('ready') AS TEXT) END,
                 ready_at_ms = CASE WHEN mail_seq > acked_seq OR CAST(('ready') AS TEXT) = 'ready'
                                    THEN CAST((v_now) AS BIGINT) ELSE NULL END,
                 next_due_ms = CASE WHEN mail_seq > acked_seq OR CAST(('ready') AS TEXT) = 'ready'
                                    THEN NULL ELSE CAST((p_due) AS BIGINT) END,
                 epoch = epoch + 1, owner_node = NULL, owner_boot = NULL
             WHERE actor_key = (p_actor)
             RETURNING state
        INTO v_state;
        UPDATE lash_actors SET claimed_revision = state_revision
             WHERE actor_key = (p_actor) AND claimed_revision = state_revision - 1;
    ELSE
        NULL;
    END CASE;
    INSERT INTO lash_owner_commit_latches (token, created_ms, answer)
             VALUES ((p_token), (v_clock), (jsonb_build_object('revision', v_rev, 'state', v_state, 'cancel', jsonb_build_array(v_cancel_turn, v_cancel_request, v_cancel_origin, v_cancel_reason, v_cancel_disposition, v_cancel_mode))));
    DELETE FROM lash_owner_commit_latches WHERE token IN (
                 SELECT token FROM lash_owner_commit_latches WHERE created_ms < (v_clock - 900000)
                 ORDER BY created_ms
                 LIMIT 16
                 FOR UPDATE SKIP LOCKED
             );
    RETURN QUERY SELECT v_rev, v_now, v_fleet, v_state, v_installed, v_cancel_turn,
        v_cancel_request, v_cancel_origin, v_cancel_reason, v_cancel_disposition, v_cancel_mode;
END
$lash_owner_commit$;
CREATE OR REPLACE FUNCTION lash_artifact_end_local(
    p_kind text, p_id text, p_gate text, p_pattern text, p_now bigint, p_namespaces text[]
) RETURNS jsonb LANGUAGE plpgsql VOLATILE AS $lash_artifact_end_local$
DECLARE
    v_refs jsonb := jsonb_build_array(jsonb_build_array(p_kind, p_id));
    v_plans jsonb := '[]'::jsonb;
    v_artifacts jsonb;
    v_checks jsonb;
    v_body jsonb;
    item jsonb;
    held record;
    v_frontier BYTEA[];
    v_locked BYTEA[];
    v_rows BIGINT;
BEGIN
    IF p_gate IS NOT NULL THEN
        SELECT COALESCE(jsonb_agg(jsonb_build_array(referrer_kind, referrer_id, cleanup_json)
            ORDER BY referrer_id), '[]'::jsonb) INTO v_plans
        FROM lash_artifact_cleanup_obligations
        WHERE referrer_kind = 'frame_environment' AND cleanup_json LIKE p_pattern ESCAPE '\';
        FOR item IN SELECT value FROM jsonb_array_elements(v_plans) LOOP
            v_body := (item->>2)::jsonb;
            IF v_body->>'plan' = 'ended' AND v_body->>'gate' = p_gate
                AND v_body->'carries' = '[]'::jsonb THEN
                v_refs := v_refs || jsonb_build_array(jsonb_build_array(item->>0, item->>1));
            END IF;
        END LOOP;
    END IF;
    FOR held IN SELECT DISTINCT value->>0 AS kind, value->>1 AS id
        FROM jsonb_array_elements(v_refs) ORDER BY kind, id LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('lash-artifact-referrer:' || held.kind || ':' || held.id, 0));
    END LOOP;
    SELECT COALESCE(jsonb_agg(jsonb_build_array(namespace, artifact_ref)
        ORDER BY namespace, artifact_ref), '[]'::jsonb) INTO v_artifacts
    FROM (SELECT DISTINCT edge.namespace, edge.artifact_ref
        FROM lash_artifact_referrer_edges edge
        JOIN jsonb_array_elements(v_refs) ref ON edge.referrer_kind = ref.value->>0
            AND edge.referrer_id = ref.value->>1
        WHERE edge.namespace = ANY(p_namespaces)) named;
    FOR held IN SELECT value->>0 AS namespace, value->>1 AS artifact_ref
        FROM jsonb_array_elements(v_artifacts) ORDER BY namespace, artifact_ref LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('lash-artifact:' || held.namespace || ':' || held.artifact_ref, 0));
    END LOOP;
    -- This statement starts after every lock wait. Include all remaining
    -- readers in the answer so the client validates their canonical pairs.
    SELECT v_refs || COALESCE(jsonb_agg(jsonb_build_array(edge.referrer_kind, edge.referrer_id)), '[]'::jsonb)
        INTO v_checks FROM lash_artifact_referrer_edges edge
        JOIN jsonb_array_elements(v_artifacts) named ON edge.namespace = named.value->>0
            AND edge.artifact_ref = named.value->>1;
    INSERT INTO lash_referrer_fences (referrer_kind, referrer_id, ended_at_ms)
        SELECT DISTINCT value->>0, value->>1, p_now FROM jsonb_array_elements(v_refs)
        ON CONFLICT DO NOTHING;
    IF jsonb_array_length(v_artifacts) > 0 THEN
        DELETE FROM lash_artifact_referrer_edges edge USING jsonb_array_elements(v_refs) ref
            WHERE edge.referrer_kind = ref.value->>0 AND edge.referrer_id = ref.value->>1
                AND edge.namespace = ANY(p_namespaces);
        WITH gone AS (
            DELETE FROM lash_lash_vm_artifacts artifact USING jsonb_array_elements(v_artifacts) named
            WHERE artifact.namespace = named.value->>0 AND artifact.artifact_ref = named.value->>1
                AND NOT EXISTS (SELECT 1 FROM lash_artifact_referrer_edges edge
                    WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref)
            RETURNING artifact.graph_ref)
        SELECT ARRAY(SELECT graph_ref FROM gone WHERE graph_ref IS NOT NULL) INTO v_frontier;
        -- A bridged artifact's row was its graph node's root: peel it
        -- (FIG-5949).
        WHILE cardinality(v_frontier) > 0 LOOP
        SELECT ARRAY(SELECT hash FROM lash_blobs WHERE hash = ANY((v_frontier)::bytea[])
             ORDER BY hash FOR UPDATE SKIP LOCKED)
        INTO v_locked;
        EXIT WHEN cardinality(v_locked) = 0;
        WITH dead AS (
                 DELETE FROM lash_blobs AS candidate
                 WHERE candidate.hash = ANY((v_locked)::bytea[])
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_graph_root_catalog AS root
                       WHERE root.root_ref = candidate.hash
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_blob_edges AS edge WHERE edge.child_hash = candidate.hash
                   )
                 RETURNING candidate.hash
             )
             SELECT (SELECT count(*) FROM dead),
                 ARRAY(SELECT DISTINCT edge.child_hash
                     FROM dead JOIN lash_blob_edges AS edge ON edge.parent_hash = dead.hash)
        INTO v_rows, v_frontier;
    END LOOP;
    END IF;
    RETURN jsonb_build_object('checks', v_checks, 'plans', v_plans);
END
$lash_artifact_end_local$;
CREATE OR REPLACE FUNCTION lash_node_renew(
    p_node TEXT, p_boot TEXT, p_ttl BIGINT, p_fleet_low INTEGER, p_fleet_high INTEGER,
    p_lock_timeout TEXT, p_statement_ms BIGINT, p_now BIGINT)
RETURNS TABLE (o_fleet INTEGER, o_expires BIGINT)
LANGUAGE plpgsql
AS $lash_node_renew$
DECLARE
    v_fleet INTEGER;
    v_clock BIGINT;
    v_now BIGINT;
    v_expires BIGINT;
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'lash_node_renew: requires read committed' USING ERRCODE = '25000';
    END IF;
    IF p_lock_timeout IS NOT NULL THEN
        PERFORM set_config('lock_timeout', p_lock_timeout, true);
    END IF;
    PERFORM pg_advisory_xact_lock_shared(715425, 0);
    SELECT (SELECT format_version FROM lash_fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
    INTO v_fleet, v_clock;
    IF v_fleet IS NULL OR v_fleet < p_fleet_low OR v_fleet > p_fleet_high THEN
        RETURN QUERY SELECT v_fleet, NULL::BIGINT;
        RETURN;
    END IF;
    v_now := COALESCE(p_now, v_clock);
    UPDATE lash_nodes SET heartbeat_expires_at_ms = (GREATEST(LEAST(v_now::NUMERIC + p_ttl, 9223372036854775807), -9223372036854775808)::BIGINT)
             WHERE node_id = (p_node) AND boot_id = (p_boot)
             RETURNING heartbeat_expires_at_ms
    INTO v_expires;
    -- As in the owner routine, an imported pool's statement budget is
    -- checked before success: raising rolls the renewal back as a whole.
    IF p_statement_ms IS NOT NULL
        AND clock_timestamp() - statement_timestamp() > p_statement_ms * INTERVAL '1 millisecond' THEN
        RAISE EXCEPTION 'canceling statement due to statement timeout' USING ERRCODE = '57014';
    END IF;
    RETURN QUERY SELECT v_fleet, v_expires;
END
$lash_node_renew$;
CREATE OR REPLACE FUNCTION lash_prompt_held_texts(p_keys BYTEA[], p_seed BIGINT)
RETURNS SETOF BYTEA
LANGUAGE plpgsql VOLATILE
AS $lash_prompt_held_texts$
BEGIN
    PERFORM pg_advisory_xact_lock_shared(hashtextextended(encode(ordered.key, 'hex'), (p_seed)))
             FROM (SELECT DISTINCT key FROM unnest((p_keys)::BYTEA[]) AS target(key) ORDER BY key) AS ordered;
    RETURN QUERY SELECT hash FROM lash_prompt_texts WHERE hash = ANY((p_keys)::BYTEA[]);
END
$lash_prompt_held_texts$;
-- END GENERATED lash_owner_commit
