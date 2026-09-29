-- lash-postgres-store schema, DDL revision 141; compatibility stamp 1/1.
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

-- The migration ledger (FIG-3816): `lash migrate` records each applied step
-- here so a rerun is a no-op and an interrupted run resumes from the rows
-- that committed. Runtime code never reads or writes it — it is the
-- operational record, and the migrate runner is its only writer. An expand
-- step commits atomically, so `applied` is the only state a committed row can
-- carry today; `running` is admitted for the operations arc's multi-transaction
-- phases (FIG-3817).
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
    PRIMARY KEY (phase, migration)
);

-- The durable-format generation every writer in the fleet emits (ADR 0106
-- §1 `F`). The installer seeds the row below; opens read the recorded
-- generation, never record one, and keep writing it until `finalize-upgrade`
-- (FIG-3800) moves it. One row, like the other deployment-scoped singletons in
-- this schema.
CREATE TABLE IF NOT EXISTS lash_fleet_format (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    format_version INTEGER NOT NULL,
    CONSTRAINT ck_fleet_format_singleton CHECK (singleton)
);

CREATE TABLE IF NOT EXISTS lash_blobs (
    hash TEXT PRIMARY KEY,
    content BYTEA NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_sessions (
    session_id TEXT PRIMARY KEY,
    head_revision BIGINT NOT NULL DEFAULT 0,
    head_json TEXT NOT NULL,
    checkpoint_ref TEXT,
    leaf_node_id TEXT,
    pending_follow_on_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_lash_sessions_leaf
    ON lash_sessions(leaf_node_id);
CREATE INDEX IF NOT EXISTS idx_lash_sessions_checkpoint_ref
    ON lash_sessions(checkpoint_ref);

CREATE TABLE IF NOT EXISTS lash_node_anchors (
    node_id TEXT PRIMARY KEY,
    checkpoint_ref TEXT NOT NULL,
    source_session_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_node_anchors_checkpoint_ref
    ON lash_node_anchors(checkpoint_ref);

-- Indexed projection of exact checkpoint-manifest component edges. Each row is
-- owned by the session whose head or anchor owns the checkpoint root named by
-- checkpoint_ref. Owner-scoped session delete or process prune deletes an
-- unreferenced root and cascades its edges in the same transaction. The
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

-- The build generations an operator marked draining (FIG-3799): the recovery
-- leader wakes every live process whose current segment a marked generation
-- admitted, so each hands its open wait to a successor on the newest build.
CREATE TABLE IF NOT EXISTS lash_draining_generations (
    generation TEXT PRIMARY KEY,
    marked_at_ms BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_deleted_sessions (
    session_id TEXT PRIMARY KEY,
    created_at_ms BIGINT,
    last_commit_at_ms BIGINT,
    head_revision BIGINT,
    relation_kind TEXT,
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

CREATE TABLE IF NOT EXISTS lash_usage_deltas (
    seq BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    operation_storage_key TEXT NOT NULL,
    entry_ordinal BIGINT NOT NULL,
    -- These two columns are write-time durable identity, not read-side integrity checks.
    payload_encoding_version INTEGER NOT NULL,
    payload_hash TEXT NOT NULL,
    source TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cache_read_input_tokens BIGINT NOT NULL,
    cache_write_input_tokens BIGINT NOT NULL,
    reasoning_output_tokens BIGINT NOT NULL,
    reconciled_call_id TEXT,
    reconciled_attempt_ordinal BIGINT,
    CONSTRAINT ck_usage_deltas_reconciliation_pair CHECK ((reconciled_call_id IS NULL) = (reconciled_attempt_ordinal IS NULL)),
    UNIQUE (
        session_id,
        operation_storage_key,
        entry_ordinal,
        payload_encoding_version,
        payload_hash
    )
);

CREATE TABLE IF NOT EXISTS lash_usage_delta_holes (
    session_id TEXT NOT NULL,
    seq BIGINT NOT NULL REFERENCES lash_usage_deltas(seq) ON DELETE CASCADE,
    call_id TEXT NOT NULL,
    attempt_ordinal BIGINT NOT NULL,
    generation_id TEXT,
    PRIMARY KEY (session_id, seq, call_id, attempt_ordinal)
);
CREATE INDEX IF NOT EXISTS idx_lash_usage_delta_holes_identity
    ON lash_usage_delta_holes(session_id, call_id, attempt_ordinal);

CREATE TABLE IF NOT EXISTS lash_session_meta (
    session_id TEXT PRIMARY KEY,
    session_state_version INTEGER,
    created_at_ms BIGINT,
    last_commit_at_ms BIGINT,
    relation_kind TEXT NOT NULL,
    parent_session_id TEXT,
    caused_by_kind TEXT,
    caused_by_session_id TEXT,
    caused_by_turn_id TEXT,
    caused_by_effect_id TEXT,
    caused_by_call_id TEXT,
    caused_by_process_id TEXT,
    -- caused_by_process_event_sequence and caused_by_subscription_revision are
    -- u64 in `CausalRef`; the full u64 range does not fit PostgreSQL's signed
    -- BIGINT, so both are stored as decimal TEXT and parsed on read.
    caused_by_process_event_sequence TEXT,
    caused_by_occurrence_id TEXT,
    caused_by_subscription_id TEXT,
    caused_by_subscription_incarnation TEXT,
    caused_by_subscription_revision TEXT,
    caused_by_node_id TEXT,
    source_session_id TEXT,
    source_node_id TEXT,
    drive_epoch BIGINT NOT NULL DEFAULT 0,
    drive_admission_id TEXT,
    drive_root_start TEXT,
    admission_base_checkpoint_ref TEXT,
    closing_intent BIGINT,
    owning_process_id TEXT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_session_meta_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')),
    CONSTRAINT ck_session_meta_relation_family CHECK ((relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL AND source_node_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))),
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'trigger_occurrence' AND caused_by_occurrence_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node'))))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_session_meta_obligation_id
    ON lash_session_meta(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_obligation_due
    ON lash_session_meta(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_obligation_stalled
    ON lash_session_meta(obligation_id)
    WHERE obligation_state = 'stalled';
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


CREATE TABLE IF NOT EXISTS lash_runtime_turn_commits (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    turn_commit_hash TEXT NOT NULL,
    result_json TEXT NOT NULL,
    outcome_code TEXT CONSTRAINT ck_runtime_turn_commits_outcome CHECK (outcome_code IN ('completed', 'frame_switch', 'cancelled', 'failed_incomplete', 'failed_invalid_input', 'failed_max_turns', 'failed_tool_failure', 'failed_provider_error', 'failed_context_overflow', 'failed_plugin_abort', 'failed_runtime_error', 'failed_submitted_error', 'failed_tool_error')),
    committed_at_ms BIGINT NOT NULL,
    failure_evidence BOOLEAN NOT NULL,
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

CREATE TABLE IF NOT EXISTS lash_turn_capture_turns (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    root TEXT NOT NULL,
    base BIGINT NOT NULL DEFAULT 0 CONSTRAINT ck_turn_capture_turns_base CHECK (base >= 0),
    next_sequence BIGINT NOT NULL DEFAULT 1 CONSTRAINT ck_turn_capture_turns_next_sequence CHECK (next_sequence >= 1),
    recovered BIGINT NOT NULL DEFAULT 0 CONSTRAINT ck_turn_capture_turns_recovered CHECK (recovered IN (0, 1)),
    PRIMARY KEY (session_id, turn_id)
);
CREATE TABLE IF NOT EXISTS lash_turn_capture_writers (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    invocation TEXT NOT NULL,
    attempt_epoch BIGINT NOT NULL CONSTRAINT ck_turn_capture_writers_attempt_epoch CHECK (attempt_epoch >= 0),
    state TEXT NOT NULL CONSTRAINT ck_turn_capture_writers_state CHECK (state IN ('live', 'retracted', 'fenced')),
    PRIMARY KEY (session_id, turn_id, invocation, attempt_epoch)
);
CREATE TABLE IF NOT EXISTS lash_turn_capture_frames (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    sequence BIGINT NOT NULL CONSTRAINT ck_turn_capture_frames_sequence CHECK (sequence >= 1),
    base BIGINT NOT NULL CONSTRAINT ck_turn_capture_frames_base CHECK (base >= 0),
    invocation TEXT NOT NULL,
    attempt_epoch BIGINT NOT NULL CONSTRAINT ck_turn_capture_frames_attempt_epoch CHECK (attempt_epoch >= 0),
    batch_ordinal BIGINT NOT NULL CONSTRAINT ck_turn_capture_frames_batch_ordinal CHECK (batch_ordinal >= 0),
    frame_json TEXT NOT NULL,
    PRIMARY KEY (session_id, turn_id, sequence),
    UNIQUE (session_id, turn_id, invocation, attempt_epoch, batch_ordinal, sequence)
);
CREATE INDEX IF NOT EXISTS idx_lash_turn_capture_frames_batch ON lash_turn_capture_frames
    (session_id, turn_id, invocation, attempt_epoch, batch_ordinal);
CREATE TABLE IF NOT EXISTS lash_stopped_partials (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    root TEXT NOT NULL,
    base BIGINT NOT NULL CONSTRAINT ck_stopped_partials_base CHECK (base >= 0),
    sealed_through BIGINT NOT NULL CONSTRAINT ck_stopped_partials_sealed_through CHECK (sealed_through >= 0),
    reason TEXT NOT NULL,
    recovered BIGINT NOT NULL CONSTRAINT ck_stopped_partials_recovered CHECK (recovered IN (0, 1)),
    digest TEXT NOT NULL,
    partial_json TEXT NOT NULL,
    body_bytes BIGINT NOT NULL CONSTRAINT ck_stopped_partials_body_bytes CHECK (body_bytes >= 0),
    sealed_at_ms BIGINT NOT NULL,
    committed_at_ms BIGINT,
    PRIMARY KEY (session_id, turn_id),
    UNIQUE (session_id, root)
);

CREATE TABLE IF NOT EXISTS lash_turn_cancel_requests (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    origin TEXT,
    reason TEXT,
    disposition TEXT NOT NULL DEFAULT 'defer',
    mode TEXT NOT NULL DEFAULT 'immediate',
    intent_revision BIGINT NOT NULL CONSTRAINT ck_turn_cancel_requests_intent_revision CHECK (intent_revision >= 1),
    PRIMARY KEY (session_id, turn_id)
);

-- Affected-input evidence for a cancellation receipt, one row per input.
-- `input_json` deliberately snapshots the pending-input payload at
-- disposition time: the pending row is vacuum-eligible once settled, and the
-- receipt must stay readable afterwards. The (session_id, turn_id, input_id)
-- uniqueness makes duplicate evidence impossible rather than checked.
CREATE TABLE IF NOT EXISTS lash_turn_cancel_affected_inputs (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    ordinal BIGINT NOT NULL,
    input_id TEXT NOT NULL,
    disposition TEXT NOT NULL CONSTRAINT ck_turn_cancel_affected_inputs_disposition CHECK (disposition IN ('defer', 'drop')),
    input_json TEXT NOT NULL,
    item_kind TEXT NOT NULL,
    batch_id TEXT,
    CONSTRAINT ck_turn_cancel_affected_inputs_item_kind CHECK ((item_kind = 'input' AND batch_id IS NULL) OR (item_kind = 'process_wake' AND batch_id IS NOT NULL)),
    PRIMARY KEY (session_id, turn_id, ordinal),
    UNIQUE (session_id, turn_id, input_id),
    FOREIGN KEY (session_id, turn_id) REFERENCES lash_turn_cancel_requests (session_id, turn_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS lash_turn_cancellation_bindings (
    session_id TEXT PRIMARY KEY,
    binding_id TEXT NOT NULL CONSTRAINT ck_turn_cancellation_bindings_binding_id CHECK (length(binding_id) > 0),
    admitted_scope_json TEXT
);

CREATE TABLE IF NOT EXISTS lash_turn_cancel_closure_authorizations (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    authorization_json TEXT NOT NULL,
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS lash_turn_cancel_retired_scopes (
    scope_id TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS lash_turn_parks (
    session_id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL,
    park_id BIGINT NOT NULL,
    reason_code TEXT NOT NULL,
    reason_json TEXT NOT NULL,
    since_ms BIGINT NOT NULL,
    last_refused_ms BIGINT NOT NULL,
    attempts BIGINT NOT NULL CONSTRAINT ck_turn_parks_attempts CHECK (attempts >= 1),
    park_executable_generation TEXT,
    engine_ref TEXT,
    resume_intent BIGINT,
    park_build_generation TEXT
);
CREATE INDEX IF NOT EXISTS idx_lash_turn_parks_since
    ON lash_turn_parks(since_ms, session_id);
CREATE INDEX IF NOT EXISTS idx_lash_turn_parks_executable_generation
    ON lash_turn_parks(park_executable_generation) WHERE park_executable_generation IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lash_turn_parks_build_generation
    ON lash_turn_parks(park_build_generation) WHERE park_build_generation IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_turn_park_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    current_seq BIGINT NOT NULL DEFAULT 0,
    compaction_horizon BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT ck_turn_park_clock_singleton CHECK (singleton)
);

CREATE TABLE IF NOT EXISTS lash_turn_park_events (
    seq BIGINT PRIMARY KEY,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    park_id BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_turn_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled', 'redrive_requested')),
    cause TEXT,
    reason_json TEXT,
    at_ms BIGINT NOT NULL,
    park_build_generation TEXT,
    CONSTRAINT ck_turn_park_events_parked_reason CHECK ((kind = 'parked' AND reason_json IS NOT NULL AND cause IS NULL) OR (kind <> 'parked' AND reason_json IS NULL AND cause IS NOT NULL))
);


CREATE TABLE IF NOT EXISTS lash_queued_work_batches (
    enqueue_seq BIGINT NOT NULL,
    batch_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    delivery_policy TEXT NOT NULL,
    work_kind TEXT NOT NULL,
    authority_json TEXT NOT NULL,
    merge_key TEXT,
    enqueued_at_ms BIGINT NOT NULL,
    admitted_root TEXT, -- The root whose fenced admission holds the batch; NULL while open.
    admitted_by TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_queued_work_batches_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    CONSTRAINT ck_queued_work_batches_work_kind CHECK (work_kind IN ('turn', 'control')),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK (delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')),
    CONSTRAINT ck_queued_work_batches_admission_all_or_none CHECK ((admitted_root IS NULL) = (admitted_by IS NULL)),
    UNIQUE (session_id, source_key),
    PRIMARY KEY (session_id, enqueue_seq)
);
-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_queued_work_batches_obligation_id
    ON lash_queued_work_batches(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_batches_obligation_due
    ON lash_queued_work_batches(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_batches_obligation_stalled
    ON lash_queued_work_batches(obligation_id)
    WHERE obligation_state = 'stalled';
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_admission_order
    ON lash_queued_work_batches(session_id, admitted_root, enqueue_seq);
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_session_command_order
    ON lash_queued_work_batches(session_id, work_kind, enqueued_at_ms, enqueue_seq);

CREATE TABLE IF NOT EXISTS lash_queued_work_items (
    batch_id TEXT NOT NULL REFERENCES lash_queued_work_batches(batch_id) ON DELETE CASCADE,
    item_index INTEGER NOT NULL,
    item_id TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    PRIMARY KEY (batch_id, item_index)
);

CREATE TABLE IF NOT EXISTS lash_wake_redelivery_fences (
    session_id TEXT NOT NULL,
    process_id TEXT NOT NULL,
    allocation_floor BIGINT NOT NULL,
    PRIMARY KEY (session_id, process_id)
);

CREATE TABLE IF NOT EXISTS lash_pending_turn_inputs (
    enqueue_seq BIGINT NOT NULL,
    input_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    ingress_json TEXT NOT NULL,
    state TEXT NOT NULL,
    input_json TEXT NOT NULL,
    submitted_ingress_json TEXT NOT NULL,
    submission_digest TEXT NOT NULL,
    enqueued_at_ms BIGINT NOT NULL,
    admitted_root TEXT, -- The root whose fenced admission holds the input; NULL while open.
    admitted_by TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    run_spec_hash TEXT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_pending_turn_inputs_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK (((ingress_json::jsonb ->> 'scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR ((ingress_json::jsonb ->> 'scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_admission_all_or_none CHECK ((admitted_root IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_pending_turn_inputs_settled_unadmitted CHECK (admitted_root IS NULL OR state NOT IN ('cancelled', 'completed')),
    UNIQUE (session_id, source_key),
    PRIMARY KEY (session_id, enqueue_seq)
);
-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_obligation_id
    ON lash_pending_turn_inputs(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_obligation_due
    ON lash_pending_turn_inputs(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_obligation_stalled
    ON lash_pending_turn_inputs(obligation_id)
    WHERE obligation_state = 'stalled';
-- All undelivered inputs, including ones a root already holds. Settled rows
-- cannot lengthen an open-input scan, and the key retains enqueue order.
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_open_state
    ON lash_pending_turn_inputs(session_id, enqueue_seq)
    WHERE state IN ('pending_active', 'deferred_next_turn');
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_bound_root
    ON lash_pending_turn_inputs(session_id, admitted_root)
    WHERE admitted_root IS NOT NULL;

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

-- The logical-root family (FIG-3600 S7). `lash_session_roots` holds one row
-- per (session, root) a drive admitted work under, with the exact result of
-- an input root's claim, committed in the claim's own transaction, and the
-- root's terminal evidence once it has one: the `terminal_*` columns are set
-- together, exactly once. `lash_session_root_inputs` binds each accepted input to the
-- root that drives it. `lash_control_intents` records an operator's verb or a
-- session's close; a `close_session` row outlives its session as the
-- deletion tombstone.
CREATE TABLE IF NOT EXISTS lash_session_roots (
    session_id TEXT NOT NULL,
    root TEXT NOT NULL,
    admission_json TEXT,
    admitted_generation TEXT,
    terminal_kind TEXT,
    terminal_cause_json TEXT,
    terminal_head_revision BIGINT,
    terminal_at_ms BIGINT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_session_roots_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    PRIMARY KEY (session_id, root),
    CONSTRAINT ck_session_roots_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_session_roots_obligation_id
    ON lash_session_roots(obligation_id);
CREATE UNIQUE INDEX IF NOT EXISTS ux_lash_session_roots_unfinished
    ON lash_session_roots(session_id)
    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_session_roots_admitted_generation
    ON lash_session_roots(admitted_generation)
    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_session_roots_obligation_due
    ON lash_session_roots(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_session_roots_obligation_stalled
    ON lash_session_roots(obligation_id)
    WHERE obligation_state = 'stalled';
CREATE INDEX IF NOT EXISTS idx_lash_session_roots_open
    ON lash_session_roots(session_id, root)
    WHERE terminal_kind IS NULL;

CREATE TABLE IF NOT EXISTS lash_session_root_inputs (
    session_id TEXT NOT NULL,
    input_id TEXT NOT NULL,
    root TEXT NOT NULL,
    PRIMARY KEY (session_id, input_id)
);

CREATE TABLE IF NOT EXISTS lash_control_intents (
    intent_id BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    format BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_control_intents_kind CHECK (kind IN ('redrive', 'cancel', 'fork', 'close_session')),
    kind_json TEXT NOT NULL,
    state TEXT NOT NULL CONSTRAINT ck_control_intents_state CHECK (state IN ('pending', 'acknowledged', 'superseded', 'failed_retryable', 'failed')),
    state_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    engine_ref TEXT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_control_intents_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_control_intents_obligation_id
    ON lash_control_intents(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_control_intents_obligation_due
    ON lash_control_intents(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_control_intents_obligation_stalled
    ON lash_control_intents(obligation_id)
    WHERE obligation_state = 'stalled';
CREATE INDEX IF NOT EXISTS idx_lash_control_intents_session
    ON lash_control_intents(session_id, kind);

CREATE TABLE IF NOT EXISTS lash_attachment_manifest (
    attachment_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    canonical_uri TEXT NOT NULL,
    intent_at_ms BIGINT NOT NULL,
    -- Identity of the write attempt that currently owns this row, minted by
    -- begin_attachment_write.
    -- NULL on a row created by adoption, which owns no write attempt.
    write_id TEXT,
    -- Upload evidence: set when the owning attempt reported a successful backend
    -- put. Adoption of a digest requires some row to carry it.
    written_at_ms BIGINT,
    committed_at_ms BIGINT,
    owner_kind TEXT CONSTRAINT ck_attachment_manifest_owner_kind CHECK (owner_kind IN ('turn', 'process')),
    owner_id TEXT,
    CONSTRAINT ck_lash_attachment_manifest_owner_identity CHECK ((owner_kind IS NULL AND owner_id IS NULL) OR (owner_kind IN ('turn', 'process') AND owner_id IS NOT NULL)),
    PRIMARY KEY (session_id, attachment_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_uncommitted
    ON lash_attachment_manifest(committed_at_ms)
    WHERE committed_at_ms IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_owner
    ON lash_attachment_manifest(session_id, owner_kind, owner_id, committed_at_ms);
-- Adoption asks one question of the whole table: does any row for this digest
-- carry upload evidence?
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_written
    ON lash_attachment_manifest(attachment_id, written_at_ms);

-- Attachment GC fence state, one row per condemned digest. Deliberately
-- timestampless: the protocol is CAS transitions only, never an expiry.
-- `sweep_generation` is the sweep pass that owns the row; a later pass adopts
-- it only once that pass is dead (ADR 0067 §6). A failed delete counts in
-- `delete_attempts`, and a stalled one is never adopted again.
CREATE TABLE IF NOT EXISTS lash_attachment_condemnations (
    attachment_id TEXT PRIMARY KEY,
    phase TEXT NOT NULL CONSTRAINT ck_attachment_condemnations_phase CHECK (phase IN ('condemned', 'deleting')),
    write_token TEXT,
    write_session_id TEXT,
    sweep_generation BIGINT NOT NULL,
    delete_attempts INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_delete_attempts CHECK (delete_attempts >= 0),
    last_delete_error TEXT,
    stall_reason TEXT CONSTRAINT ck_attachment_condemnations_stall_reason CHECK (stall_reason IN ('attempts_exhausted', 'refused')),
    CONSTRAINT ck_attachment_condemnations_write_token_pairing CHECK ((write_token IS NULL) = (write_session_id IS NULL)),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (write_token IS NULL OR phase = 'condemned'),
    CONSTRAINT ck_attachment_condemnations_failure_pairing CHECK ((delete_attempts = 0) = (last_delete_error IS NULL)),
    CONSTRAINT ck_attachment_condemnations_stall_phase CHECK (stall_reason IS NULL OR (phase = 'condemned' AND delete_attempts > 0))
);

-- The counter every attachment sweep pass mints its generation from.
CREATE TABLE IF NOT EXISTS lash_attachment_sweep_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    generation BIGINT NOT NULL,
    CONSTRAINT ck_attachment_sweep_clock_singleton CHECK (singleton)
);

CREATE TABLE IF NOT EXISTS lash_process_change_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    current_seq BIGINT NOT NULL,
    tombstone_compaction_horizon BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT ck_process_change_clock_singleton CHECK (singleton)
);
-- Opaque process identifiers use byte order on every host locale. The primary
-- key, live-worklist index, MAX, and keyset bounds inherit this collation.
CREATE TABLE IF NOT EXISTS lash_processes (
    process_id TEXT COLLATE "C" PRIMARY KEY,
    start_key TEXT COLLATE "C",
    originator_id TEXT NOT NULL,
    wake_session_id TEXT,
    identity_kind TEXT NOT NULL,
    identity_label TEXT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    last_event_sequence BIGINT NOT NULL,
    change_seq BIGINT NOT NULL,
    status TEXT NOT NULL,
    lifetime TEXT NOT NULL,
    lifetime_scope_kind TEXT,
    lifetime_scope_id TEXT COLLATE "C",
    cancel_requested_at_ms BIGINT,
    parked_since_ms BIGINT,
    parked_reason_code TEXT,
    park_executable_generation TEXT,
    park_build_generation TEXT,
    segment_generation TEXT,
    record_json TEXT NOT NULL,
    start_obligation_id TEXT,
    start_obligation_state TEXT,
    start_obligation_attempts INTEGER NOT NULL DEFAULT 0,
    start_obligation_due_at_ms BIGINT,
    start_obligation_claim_token TEXT,
    start_obligation_stall_reason TEXT,
    start_obligation_last_error TEXT,
    start_obligation_settled_at_ms BIGINT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    consumer_hold_key TEXT,
    consumer_hold_scope_kind TEXT,
    consumer_hold_scope_id TEXT COLLATE "C",
    consumer_hold_cancels BOOLEAN,
    CONSTRAINT ck_processes_consumer_hold CHECK ((consumer_hold_key IS NULL) = (consumer_hold_scope_kind IS NULL) AND (consumer_hold_key IS NULL) = (consumer_hold_scope_id IS NULL)),
    CONSTRAINT ck_processes_start_obligation CHECK ((start_obligation_state IS NULL AND start_obligation_id IS NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'due' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NOT NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'claimed' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NOT NULL AND start_obligation_claim_token IS NOT NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'delivered' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NOT NULL) OR (start_obligation_state = 'stalled' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND start_obligation_settled_at_ms IS NOT NULL)),
    CONSTRAINT ck_processes_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    CONSTRAINT ck_processes_parked CHECK ((parked_since_ms IS NULL) = (parked_reason_code IS NULL)),
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')),
    CONSTRAINT ck_processes_lifetime CHECK (lifetime IN ('until', 'detached')),
    CONSTRAINT ck_processes_lifetime_scope CHECK ((lifetime = 'detached' AND lifetime_scope_kind IS NULL AND lifetime_scope_id IS NULL) OR (lifetime = 'until' AND lifetime_scope_kind IN ('turn', 'queue_drain', 'process', 'session') AND lifetime_scope_id IS NOT NULL))
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

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_processes_obligation_id
    ON lash_processes(obligation_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_processes_start_obligation_id
    ON lash_processes(start_obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_processes_start_obligation_due
    ON lash_processes(start_obligation_due_at_ms, start_obligation_id)
    WHERE start_obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_processes_start_obligation_stalled
    ON lash_processes(start_obligation_id)
    WHERE start_obligation_state = 'stalled';
CREATE INDEX IF NOT EXISTS idx_lash_processes_obligation_due
    ON lash_processes(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_processes_obligation_stalled
    ON lash_processes(obligation_id)
    WHERE obligation_state = 'stalled';
-- A start key maps to the one retained process minted for it (ADR 0107).
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_processes_start_key
    ON lash_processes(start_key) WHERE start_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lash_processes_status
    ON lash_processes(status);
CREATE INDEX IF NOT EXISTS idx_lash_processes_non_terminal
    ON lash_processes(process_id) WHERE status IN ('running', 'waiting');
CREATE INDEX IF NOT EXISTS idx_lash_processes_change_seq
    ON lash_processes(change_seq);
CREATE INDEX IF NOT EXISTS idx_lash_processes_originator
    ON lash_processes(originator_id);
CREATE INDEX IF NOT EXISTS idx_lash_processes_identity
    ON lash_processes(identity_kind, identity_label);
CREATE INDEX IF NOT EXISTS idx_lash_processes_created
    ON lash_processes(created_at_ms);
CREATE INDEX IF NOT EXISTS idx_lash_processes_updated
    ON lash_processes(updated_at_ms);
CREATE INDEX IF NOT EXISTS idx_lash_processes_wake_session
    ON lash_processes(wake_session_id);
-- The pending-cancel sweep's scan: rows whose cancel request is older than a
-- horizon and whose outcome is still open. The predicate is the negation of
-- the terminal statuses, so `caller_departed` is in: nothing may ever
-- terminalize such a row, so a cancel request on it stays unanswered forever
-- and is exactly what an operator asks this index for. It must stay
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
-- index; it is exactly LIVE_PROCESS_STATUS_LABELS, so `caller_departed` is out
-- for the reason it is out of every other worklist: lash may never act on such
-- a row nor assert an outcome for it, and a cancel request is both.
CREATE INDEX IF NOT EXISTS idx_lash_processes_lifetime_pending
    ON lash_processes(lifetime_scope_kind, lifetime_scope_id, process_id)
    WHERE lifetime = 'until'
      AND cancel_requested_at_ms IS NULL
      AND status IN ('running', 'waiting');

-- The parked projection (FIG-3659 NOW-B): the parked-process list and the
-- park summary read only parked rows, in `(since, process)` keyset order.
CREATE INDEX IF NOT EXISTS idx_lash_processes_parked
    ON lash_processes(parked_since_ms, process_id)
    WHERE parked_since_ms IS NOT NULL;
-- The retired generation a `retired_generation` park names (FIG-3571): the
-- drain counts retired process parks per executable generation off it.
CREATE INDEX IF NOT EXISTS idx_lash_processes_park_executable_generation
    ON lash_processes(park_executable_generation) WHERE park_executable_generation IS NOT NULL;
-- The build generation of the parked checkpoint a park resumes (FIG-3795):
-- drain status counts retired parks by it.
CREATE INDEX IF NOT EXISTS idx_lash_processes_park_build_generation
    ON lash_processes(park_build_generation) WHERE park_build_generation IS NOT NULL;
-- The build generation that admitted each live process's current segment
-- (FIG-3795 S2): the drain routes a refused redrive to the build that wrote
-- the segment's journal. Partial: a terminal segment's writer is no route,
-- and a NULL stamp is no lookup key — the IS NOT NULL clause also keeps the
-- planner from preferring this index for the unprefixed non-terminal scans.
CREATE INDEX IF NOT EXISTS idx_lash_processes_live_generation
    ON lash_processes(segment_generation) WHERE status IN ('running', 'waiting') AND segment_generation IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_process_park_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    current_seq BIGINT NOT NULL DEFAULT 0,
    compaction_horizon BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT ck_process_park_clock_singleton CHECK (singleton)
);

CREATE TABLE IF NOT EXISTS lash_process_park_events (
    seq BIGINT PRIMARY KEY,
    process_id TEXT COLLATE "C" NOT NULL,
    park_id BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_process_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled')),
    cause TEXT,
    reason_json TEXT,
    at_ms BIGINT NOT NULL,
    park_build_generation TEXT,
    CONSTRAINT ck_process_park_events_parked_reason CHECK ((kind = 'parked' AND reason_json IS NOT NULL AND cause IS NULL) OR (kind <> 'parked' AND reason_json IS NULL AND cause IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS lash_process_events (
    process_id TEXT COLLATE "C" NOT NULL,
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    idempotency_key TEXT,
    event_json TEXT NOT NULL,
    PRIMARY KEY (process_id, sequence),
    FOREIGN KEY (process_id) REFERENCES lash_processes(process_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_process_events_key
    ON lash_process_events(process_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_wake_allocation_floors (
    target_session_id TEXT NOT NULL,
    process_id TEXT COLLATE "C" NOT NULL,
    allocation_floor BIGINT NOT NULL,
    PRIMARY KEY (target_session_id, process_id)
);

CREATE TABLE IF NOT EXISTS lash_process_wake_deliveries (
    delivery_id TEXT PRIMARY KEY,
    process_id TEXT COLLATE "C" NOT NULL,
    target_session_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    state TEXT NOT NULL,
    claim_token TEXT,
    attempts BIGINT NOT NULL DEFAULT 0,
    first_attempt_ms BIGINT,
    next_attempt_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    discard_reason TEXT,
    delivery_json TEXT NOT NULL,
    CONSTRAINT ck_process_wake_deliveries_state CHECK (state IN ('pending', 'enqueuing', 'enqueued', 'discarded')),
    CONSTRAINT ck_process_wake_deliveries_discard_reason CHECK (discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound')),
    FOREIGN KEY (process_id) REFERENCES lash_processes(process_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lash_wake_deliveries_pending
    ON lash_process_wake_deliveries(
        next_attempt_at_ms, target_session_id, process_id, sequence
    )
    WHERE state IN ('pending', 'enqueuing');
CREATE INDEX IF NOT EXISTS idx_lash_wake_deliveries_group_sequence
    ON lash_process_wake_deliveries(target_session_id, process_id, sequence)
    WHERE state <> 'enqueued';

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
    pruned_change_seq BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_process_tombstones_change
    ON lash_process_tombstones(pruned_change_seq);

CREATE TABLE IF NOT EXISTS lash_process_segment_handovers (
    process_id TEXT COLLATE "C" NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    segment_ordinal BIGINT NOT NULL,
    handover_json TEXT NOT NULL,
    started_json TEXT,
    written_generation TEXT,
    route TEXT NOT NULL,
    PRIMARY KEY (process_id, segment_ordinal)
);
-- The route a retained handover's successor was sent under (FIG-3795 S3):
-- drain re-routing finds every successor addressed to a retired deployment.
CREATE INDEX IF NOT EXISTS idx_lash_process_segment_handovers_route
    ON lash_process_segment_handovers(route);

-- One row per ended parent scope, keyed by the scope itself rather than by a
-- process row: a turn-scoped parent has no process row at all, and a
-- process-scoped parent's row may be pruned before its children settle.
CREATE TABLE IF NOT EXISTS lash_parent_end_plans (
    parent_kind TEXT NOT NULL,
    parent_id TEXT COLLATE "C" NOT NULL,
    parent_payload TEXT NOT NULL,
    ended_at_ms BIGINT NOT NULL,
    settled_at_ms BIGINT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_parent_end_plans_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    PRIMARY KEY (parent_kind, parent_id),
    CONSTRAINT ck_parent_end_plans_kind CHECK (parent_kind IN ('turn', 'queue_drain', 'process', 'session'))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_parent_end_plans_obligation_id
    ON lash_parent_end_plans(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_parent_end_plans_obligation_due
    ON lash_parent_end_plans(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_parent_end_plans_obligation_stalled
    ON lash_parent_end_plans(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE TABLE IF NOT EXISTS lash_tool_intent_submissions (
    replay_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    execution_scope_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    intent_index BIGINT NOT NULL,
    kind TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    submission_json TEXT NOT NULL,
    CONSTRAINT ck_tool_intent_submissions_kind CHECK (kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger', 'register_process_definition', 'register_trigger'))
);
CREATE INDEX IF NOT EXISTS idx_lash_tool_intent_submissions_scope
    ON lash_tool_intent_submissions(session_id, execution_scope_id, intent_index);

CREATE TABLE IF NOT EXISTS lash_trigger_subscriptions (
    subscription_id TEXT PRIMARY KEY,
    owner_scope TEXT NOT NULL,
    subscription_key TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    revision BIGINT NOT NULL,
    definition_fingerprint TEXT NOT NULL,
    source_type TEXT NOT NULL,
    source_key TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    deleted_at_ms BIGINT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    record_json TEXT NOT NULL,
    CONSTRAINT ck_trigger_subscriptions_lifecycle
        CHECK (lifecycle IN ('enabled', 'disabled', 'tombstoned')),
    CONSTRAINT ck_trigger_subscriptions_lifecycle_deleted_at CHECK ((lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)),
    UNIQUE(owner_scope, subscription_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_subscriptions_registrant
    ON lash_trigger_subscriptions(owner_scope, subscription_key);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_subscriptions_source
    ON lash_trigger_subscriptions(source_type, source_key, lifecycle);

-- The named process-definition registry (FIG-2995, ADR 0095): owner scope,
-- name, revision, pinned definition fingerprint, lifecycle tombstone and
-- change sequence, unique on owner scope and name. The pinned
-- ProcessDefinitionRef travels in record_json; the fingerprint column is what
-- the revision-and-fingerprint compare-and-swap compares. The lifecycle is
-- the FIG-1951 one-column enum with a paired-nullable delete timestamp.
-- Session-scoped names follow the ADR 0049 deletion frontier; host- and
-- platform-scoped tombstones are never collected (ADR 0067).
CREATE TABLE IF NOT EXISTS lash_process_definitions (
    definition_id TEXT PRIMARY KEY,
    owner_scope TEXT NOT NULL,
    name TEXT NOT NULL,
    revision BIGINT NOT NULL,
    fingerprint TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    deleted_at_ms BIGINT,
    change_seq BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    record_json TEXT NOT NULL,
    CONSTRAINT ck_process_definitions_lifecycle CHECK ((lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)),
    UNIQUE(owner_scope, name)
);
CREATE INDEX IF NOT EXISTS idx_lash_process_definitions_registrant
    ON lash_process_definitions(owner_scope, name);
CREATE INDEX IF NOT EXISTS idx_lash_process_definitions_change
    ON lash_process_definitions(change_seq);

CREATE TABLE IF NOT EXISTS lash_trigger_occurrences (
    occurrence_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    source_type TEXT NOT NULL,
    source_key TEXT NOT NULL,
    occurred_at_ms BIGINT NOT NULL,
    reclaimable_at_ms BIGINT,
    record_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_occurrences_source
    ON lash_trigger_occurrences(source_type, source_key, occurred_at_ms);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_occurrences_reclaimable
    ON lash_trigger_occurrences(reclaimable_at_ms, occurrence_id)
    WHERE reclaimable_at_ms IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_trigger_deliveries (
    occurrence_id TEXT NOT NULL REFERENCES lash_trigger_occurrences(occurrence_id) ON DELETE CASCADE,
    subscription_id TEXT NOT NULL,
    process_id TEXT,
    subscription_incarnation TEXT NOT NULL,
    subscription_revision BIGINT NOT NULL,
    subscription_snapshot_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_trigger_deliveries_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    PRIMARY KEY (occurrence_id, subscription_id)
);
CREATE TABLE IF NOT EXISTS lash_trigger_mutation_receipts (
    operation_id TEXT PRIMARY KEY,
    owner_kind TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    CONSTRAINT ck_trigger_receipts_owner_kind CHECK (owner_kind IN ('session', 'host', 'platform'))
);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_subscription
    ON lash_trigger_deliveries(subscription_id);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_process
    ON lash_trigger_deliveries(process_id);
-- A reserved delivery owes its start (ADR 0109, ADR 0021): its obligation id,
-- the relay's due read and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_obligation_id
    ON lash_trigger_deliveries(obligation_id);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_obligation_due
    ON lash_trigger_deliveries(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_obligation_stalled
    ON lash_trigger_deliveries(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE TABLE IF NOT EXISTS lash_lashlang_artifacts (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    artifact_bytes BYTEA NOT NULL,
    PRIMARY KEY (namespace, artifact_ref)
);
CREATE TABLE IF NOT EXISTS lash_artifact_referrer_edges (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'subscription_revision', 'start', 'execution', 'host_pin', 'definition_revision')),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_id CHECK (char_length(referrer_id) > 0),
    PRIMARY KEY (namespace, artifact_ref, referrer_kind, referrer_id),
    FOREIGN KEY (namespace, artifact_ref) REFERENCES lash_lashlang_artifacts(namespace, artifact_ref) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lash_artifact_referrer_edges_referrer
    ON lash_artifact_referrer_edges(referrer_kind, referrer_id);
CREATE TABLE IF NOT EXISTS lash_artifact_referrer_fences (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_fences_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'subscription_revision', 'start', 'execution', 'host_pin', 'definition_revision')),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_referrer_fences_id CHECK (char_length(referrer_id) > 0),
    ended_at_ms BIGINT NOT NULL,
    PRIMARY KEY (referrer_kind, referrer_id)
);
CREATE TABLE IF NOT EXISTS lash_artifact_cleanup_obligations (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_cleanup_obligations_kind CHECK (referrer_kind IN ('frame_environment', 'process_record', 'subscription_revision', 'start', 'execution', 'host_pin', 'definition_revision')),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_cleanup_obligations_id CHECK (char_length(referrer_id) > 0),
    cleanup_json TEXT NOT NULL,
    obligation_id TEXT NOT NULL,
    obligation_state TEXT NOT NULL DEFAULT 'due',
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_artifact_cleanup_obligations_obligation CHECK ((obligation_state = 'due' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'stalled' AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
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

INSERT INTO lash_process_park_clock (
    singleton, current_seq, compaction_horizon
) VALUES (TRUE, 0, 0)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_turn_park_clock (
    singleton, current_seq, compaction_horizon
) VALUES (TRUE, 0, 0)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_attachment_sweep_clock (singleton, generation)
VALUES (TRUE, 0)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_catalog_identity (singleton, catalog_id)
VALUES (TRUE, gen_random_uuid()::text)
ON CONFLICT (singleton) DO NOTHING;
