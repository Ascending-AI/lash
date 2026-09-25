-- lash-postgres-store schema, component version 130.
--
-- Generated artifact. These bytes are exactly the DDL `PostgresStorage`
-- executes at open; `PostgresStorage::schema_ddl()` returns this file
-- verbatim. A host that provisions the database itself must copy this file
-- byte-for-byte into its own migration tooling rather than transcribe it: lash
-- verifies the resulting structure at open and rejects a mismatch with a
-- per-object diff.
--
-- The component schema is a reject-and-recreate boundary except for explicit
-- migrations implemented by the owning build. Every statement in this artifact
-- is creation-only and idempotent, so applying the file twice is a no-op, and
-- nothing here is schema-qualified, so the file provisions into whichever schema
-- the session's `search_path` resolves.

CREATE TABLE IF NOT EXISTS lash_schema_versions (
    component TEXT PRIMARY KEY,
    version INTEGER NOT NULL
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
    leaf_node_id TEXT
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
    -- The complete typed disposition, hole identities included: a reopened
    -- runtime rebuilds the attempts it still owes usage for from this column.
    usage_disposition_json TEXT NOT NULL,
    UNIQUE (
        session_id,
        operation_storage_key,
        entry_ordinal,
        payload_encoding_version,
        payload_hash
    )
);

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
    admission_base_checkpoint_ref TEXT,
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')),
    CONSTRAINT ck_session_meta_relation_family CHECK ((relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL AND source_node_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))),
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'trigger_occurrence' AND caused_by_occurrence_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node'))))
);
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_catalog
    ON lash_session_meta(created_at_ms, session_id);
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_state_version
    ON lash_session_meta(session_state_version, session_id);

CREATE TABLE IF NOT EXISTS lash_session_meta_pending_observer_intents (
    session_id TEXT NOT NULL,
    process_index BIGINT NOT NULL,
    process_id TEXT NOT NULL,
    process_incarnation BIGINT,
    PRIMARY KEY (session_id, process_id),
    UNIQUE (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES lash_session_meta(session_id) ON DELETE CASCADE
);


CREATE TABLE IF NOT EXISTS lash_runtime_turn_commits (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    turn_commit_hash TEXT NOT NULL,
    result_json TEXT NOT NULL,
    committed_at_ms BIGINT NOT NULL,
    request_identity_hash TEXT,
    requested_node_count BIGINT,
    identity_encoding_version INTEGER,
    PRIMARY KEY (session_id, turn_id),
    -- Identity families: all-NULL is a plain commit; hash+version+count is an
    -- append identity; hash+version without a count is a semantic-boundary
    -- identity (FIG-2480). A count without a hash is representable nowhere.
    CONSTRAINT ck_runtime_turn_commits_identity CHECK ((request_identity_hash IS NULL) = (identity_encoding_version IS NULL) AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL))
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
    PRIMARY KEY (session_id, turn_id, ordinal),
    UNIQUE (session_id, turn_id, input_id),
    FOREIGN KEY (session_id, turn_id) REFERENCES lash_turn_cancel_requests (session_id, turn_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS lash_turn_cancellation_bindings (
    session_id TEXT PRIMARY KEY,
    binding_id TEXT NOT NULL CONSTRAINT ck_turn_cancellation_bindings_binding_id CHECK (length(binding_id) > 0),
    admitted_scope_json TEXT
);

CREATE TABLE IF NOT EXISTS lash_queued_runs (
    session_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    status TEXT NOT NULL CONSTRAINT ck_queued_runs_status CHECK (status IN ('pending', 'settled')),
    revision BIGINT NOT NULL CONSTRAINT ck_queued_runs_revision CHECK (revision >= 0),
    admission_json TEXT NOT NULL,
    PRIMARY KEY (session_id, scope_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS lash_queued_runs_pending ON lash_queued_runs(session_id) WHERE status = 'pending';
CREATE TABLE IF NOT EXISTS lash_queued_run_members (
    session_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    collection_kind TEXT NOT NULL CONSTRAINT ck_queued_run_members_collection_kind CHECK (collection_kind IN ('initial', 'current', 'withheld', 'assigned')),
    ordinal BIGINT NOT NULL CONSTRAINT ck_queued_run_members_ordinal CHECK (ordinal >= 0),
    member_kind TEXT NOT NULL CONSTRAINT ck_queued_run_members_member_kind CHECK (member_kind IN ('input', 'batch')),
    member_id TEXT NOT NULL,
    PRIMARY KEY (session_id, scope_id, collection_kind, ordinal),
    UNIQUE (session_id, scope_id, collection_kind, member_kind, member_id),
    FOREIGN KEY (session_id, scope_id) REFERENCES lash_queued_runs(session_id, scope_id)
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
    attempts BIGINT NOT NULL CONSTRAINT ck_turn_parks_attempts CHECK (attempts >= 1)
);
CREATE INDEX IF NOT EXISTS idx_lash_turn_parks_since
    ON lash_turn_parks(since_ms, session_id);

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
    kind TEXT NOT NULL CONSTRAINT ck_turn_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled')),
    cause TEXT,
    reason_json TEXT,
    at_ms BIGINT NOT NULL,
    CONSTRAINT ck_turn_park_events_parked_reason CHECK ((kind = 'parked' AND reason_json IS NOT NULL AND cause IS NULL) OR (kind <> 'parked' AND reason_json IS NULL AND cause IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS lash_session_execution_leases (
    session_id TEXT PRIMARY KEY,
    lease_owner_id TEXT,
    lease_owner_incarnation_id TEXT,
    lease_executor_id TEXT,
    lease_token TEXT,
    lease_fencing_token BIGINT NOT NULL DEFAULT 0,
    lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
    lease_term_ms BIGINT NOT NULL DEFAULT 0,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT ck_session_execution_leases_identity_all_or_none CHECK ((lease_owner_id IS NULL AND lease_owner_incarnation_id IS NULL AND lease_executor_id IS NULL AND lease_token IS NULL) OR (lease_owner_id IS NOT NULL AND lease_owner_incarnation_id IS NOT NULL AND lease_executor_id IS NOT NULL AND lease_token IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS lash_queued_work_batches (
    enqueue_seq BIGSERIAL PRIMARY KEY,
    batch_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    delivery_policy TEXT NOT NULL,
    work_kind TEXT NOT NULL,
    authority_json TEXT NOT NULL,
    merge_key TEXT,
    available_at_ms BIGINT NOT NULL,
    enqueued_at_ms BIGINT NOT NULL,
    claim_id TEXT, -- With claim_token, names a live claim for a nonzero generation.
    claim_token TEXT, -- At generation zero, the pair is an abandon-restored predecessor.
    claim_fencing_token BIGINT NOT NULL DEFAULT 0,
    claim_session_lease_generation BIGINT NOT NULL DEFAULT 0, -- Zero disambiguates the predecessor record from a live claim.
    CONSTRAINT ck_queued_work_batches_work_kind CHECK (work_kind IN ('turn', 'control')),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK (delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')),
    CONSTRAINT ck_queued_work_batches_claim_id_token_all_or_none CHECK ((claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)),
    UNIQUE (session_id, source_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_ready
    ON lash_queued_work_batches(session_id, available_at_ms, enqueue_seq);
CREATE INDEX IF NOT EXISTS idx_lash_queued_work_claim
    ON lash_queued_work_batches(session_id, claim_id, enqueue_seq);
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
    enqueue_seq BIGSERIAL PRIMARY KEY,
    input_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    source_key TEXT,
    ingress_json TEXT NOT NULL,
    state TEXT NOT NULL,
    input_json TEXT NOT NULL,
    submitted_ingress_json TEXT NOT NULL,
    submission_digest TEXT NOT NULL,
    enqueued_at_ms BIGINT NOT NULL,
    claim_id TEXT,
    claim_owner_id TEXT,
    claim_owner_incarnation_id TEXT,
    claim_token TEXT,
    claim_fencing_token BIGINT NOT NULL DEFAULT 0,
    claim_session_lease_generation BIGINT NOT NULL DEFAULT 0,
    claim_bound_turn_id TEXT,
    claim_bound_receipt_input_id TEXT,
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK (((ingress_json::jsonb ->> 'scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR ((ingress_json::jsonb ->> 'scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_claim_identity_all_or_none CHECK ((claim_id IS NULL AND claim_owner_id IS NULL AND claim_owner_incarnation_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_owner_id IS NOT NULL AND claim_owner_incarnation_id IS NOT NULL AND claim_token IS NOT NULL)),
    CONSTRAINT ck_pending_turn_inputs_bound_claim_is_next_turn CHECK ((claim_bound_turn_id IS NULL AND claim_bound_receipt_input_id IS NULL) OR (claim_bound_turn_id IS NOT NULL AND claim_bound_receipt_input_id IS NOT NULL AND claim_token IS NOT NULL AND state = 'deferred_next_turn')),
    UNIQUE (session_id, source_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_session
    ON lash_pending_turn_inputs(session_id, state, enqueue_seq);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_input_order
    ON lash_pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_claim
    ON lash_pending_turn_inputs(session_id, claim_id, claim_token);

-- The one session ingress (ADR 0101): one row per admitted item, one
-- per-session order taken under the session history lock, two class-level
-- lanes. `delivery_*` is the submitted delivery, written once and never
-- rewritten; `submission_digest` likewise. A claim's columns are set exactly
-- on an `accepted` row, and a tombstone carries its closed cause and no claim.
CREATE TABLE IF NOT EXISTS lash_session_ingress (
    enqueue_seq BIGSERIAL PRIMARY KEY,
    item_id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL,
    lane TEXT NOT NULL,
    kind TEXT NOT NULL,
    source_key TEXT,
    delivery_scope TEXT NOT NULL,
    delivery_turn_id TEXT,
    delivery_min_boundary TEXT,
    submission_digest TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    authority_json TEXT,
    merge_key TEXT,
    wake_process_id TEXT,
    wake_sequence BIGINT,
    state TEXT NOT NULL,
    terminal_cause_json TEXT,
    enqueued_at_ms BIGINT NOT NULL,
    terminal_at_ms BIGINT,
    claim_id TEXT,
    claim_token TEXT,
    claim_admission_id TEXT,
    claim_fencing_token BIGINT NOT NULL DEFAULT 0,
    claim_drive_epoch BIGINT,
    claim_turn_id TEXT,
    CONSTRAINT ck_session_ingress_kind CHECK (kind IN ('input', 'process_wake', 'session_command')),
    CONSTRAINT ck_session_ingress_lane CHECK ((kind = 'session_command' AND lane = 'command') OR (kind IN ('input', 'process_wake') AND lane = 'turn')),
    CONSTRAINT ck_session_ingress_state CHECK (state IN ('open', 'accepted', 'completed', 'cancelled')),
    CONSTRAINT ck_session_ingress_delivery CHECK ((delivery_scope = 'turn' AND delivery_turn_id IS NOT NULL AND delivery_min_boundary IN ('after_work', 'before_completion')) OR (delivery_scope IN ('any_boundary', 'next_turn') AND delivery_turn_id IS NULL AND delivery_min_boundary IS NULL)),
    CONSTRAINT ck_session_ingress_kind_delivery CHECK (kind = 'input' OR (kind = 'process_wake' AND delivery_scope = 'any_boundary') OR (kind = 'session_command' AND delivery_scope = 'next_turn')),
    CONSTRAINT ck_session_ingress_wake_source CHECK ((kind = 'process_wake' AND wake_process_id IS NOT NULL AND wake_sequence IS NOT NULL) OR (kind <> 'process_wake' AND wake_process_id IS NULL AND wake_sequence IS NULL)),
    CONSTRAINT ck_session_ingress_claim CHECK ((state = 'accepted' AND claim_id IS NOT NULL AND claim_token IS NOT NULL AND claim_admission_id IS NOT NULL AND claim_drive_epoch IS NOT NULL) OR (state <> 'accepted' AND claim_id IS NULL AND claim_token IS NULL AND claim_admission_id IS NULL AND claim_drive_epoch IS NULL AND claim_turn_id IS NULL)),
    CONSTRAINT ck_session_ingress_terminal CHECK ((state IN ('completed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL) OR (state IN ('open', 'accepted') AND terminal_cause_json IS NULL AND terminal_at_ms IS NULL)),
    UNIQUE (session_id, source_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_session_ingress_open
    ON lash_session_ingress(session_id, lane, enqueue_seq) WHERE state IN ('open', 'accepted');
CREATE INDEX IF NOT EXISTS idx_lash_session_ingress_addressed
    ON lash_session_ingress(session_id, delivery_turn_id, enqueue_seq) WHERE delivery_turn_id IS NOT NULL AND state IN ('open', 'accepted');
CREATE INDEX IF NOT EXISTS idx_lash_session_ingress_claim
    ON lash_session_ingress(session_id, claim_id) WHERE claim_id IS NOT NULL;

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
    owner_incarnation BIGINT,
    CONSTRAINT ck_lash_attachment_manifest_owner_identity CHECK ((owner_kind IS NULL AND owner_id IS NULL AND owner_incarnation IS NULL) OR (owner_kind = 'turn' AND owner_id IS NOT NULL AND owner_incarnation IS NULL) OR (owner_kind = 'process' AND owner_id IS NOT NULL AND owner_incarnation IS NOT NULL)),
    PRIMARY KEY (session_id, attachment_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_uncommitted
    ON lash_attachment_manifest(committed_at_ms)
    WHERE committed_at_ms IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_owner
    ON lash_attachment_manifest(session_id, owner_kind, owner_id, owner_incarnation, committed_at_ms);
-- Adoption asks one question of the whole table: does any row for this digest
-- carry upload evidence?
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_written
    ON lash_attachment_manifest(attachment_id, written_at_ms);

-- Attachment GC fence state, one row per condemned digest. Deliberately
-- timestampless: the protocol is CAS transitions only, never an expiry.
CREATE TABLE IF NOT EXISTS lash_attachment_condemnations (
    attachment_id TEXT PRIMARY KEY,
    phase TEXT NOT NULL CONSTRAINT ck_attachment_condemnations_phase CHECK (phase IN ('condemned', 'deleting')),
    write_token TEXT,
    write_session_id TEXT,
    CONSTRAINT ck_attachment_condemnations_write_token_pairing CHECK ((write_token IS NULL) = (write_session_id IS NULL)),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (write_token IS NULL OR phase = 'condemned')
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
    incarnation BIGINT NOT NULL,
    registration_fingerprint TEXT NOT NULL,
    originator_id TEXT NOT NULL,
    wake_session_id TEXT,
    identity_kind TEXT NOT NULL,
    identity_label TEXT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    last_event_sequence BIGINT NOT NULL,
    change_seq BIGINT NOT NULL,
    status TEXT NOT NULL,
    parent_scope_kind TEXT NOT NULL,
    parent_scope_id TEXT COLLATE "C",
    on_parent_end TEXT NOT NULL,
    cancel_requested_at_ms BIGINT,
    record_json TEXT NOT NULL,
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')),
    CONSTRAINT ck_processes_parent_scope_kind CHECK (parent_scope_kind IN ('turn', 'queue_drain', 'process', 'host')),
    CONSTRAINT ck_processes_parent_scope_id CHECK ((parent_scope_kind = 'host' AND parent_scope_id IS NULL) OR (parent_scope_kind IN ('turn', 'queue_drain', 'process') AND parent_scope_id IS NOT NULL)),
    CONSTRAINT ck_processes_on_parent_end CHECK (on_parent_end IN ('abandon', 'cancel')),
    UNIQUE(process_id, incarnation)
);
CREATE INDEX IF NOT EXISTS idx_lash_processes_status
    ON lash_processes(status);
CREATE INDEX IF NOT EXISTS idx_lash_processes_live_worklist
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
CREATE INDEX IF NOT EXISTS idx_lash_processes_parent_scope
    ON lash_processes(parent_scope_kind, parent_scope_id, process_id);
-- The parent-end sweep's only scan. The predicate names the live statuses
-- rather than a NOT IN so a status added later cannot silently widen the
-- index; it is exactly LIVE_PROCESS_STATUS_LABELS, so `caller_departed` is out
-- for the reason it is out of every other worklist: lash may never act on such
-- a row nor assert an outcome for it, and a cancel request is both.
CREATE INDEX IF NOT EXISTS idx_lash_processes_parent_end_pending
    ON lash_processes(parent_scope_kind, parent_scope_id, process_id)
    WHERE on_parent_end = 'cancel'
      AND cancel_requested_at_ms IS NULL
      AND status IN ('running', 'waiting');

CREATE TABLE IF NOT EXISTS lash_process_events (
    process_id TEXT COLLATE "C" NOT NULL,
    process_incarnation BIGINT NOT NULL,
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    idempotency_key TEXT,
    event_json TEXT NOT NULL,
    PRIMARY KEY (process_id, process_incarnation, sequence),
    FOREIGN KEY (process_id, process_incarnation) REFERENCES lash_processes(process_id, incarnation) ON DELETE CASCADE
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
    process_incarnation BIGINT NOT NULL,
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
    FOREIGN KEY (process_id, process_incarnation) REFERENCES lash_processes(process_id, incarnation) ON DELETE CASCADE
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
    process_incarnation BIGINT NOT NULL,
    PRIMARY KEY (session_id, process_id, process_incarnation),
    FOREIGN KEY (process_id, process_incarnation) REFERENCES lash_processes(process_id, incarnation) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lash_process_observers_process
    ON lash_process_observers(process_id, session_id);

CREATE TABLE IF NOT EXISTS lash_process_tombstones (
    process_id TEXT COLLATE "C" NOT NULL,
    incarnation BIGINT NOT NULL,
    terminal_label TEXT NOT NULL,
    pruned_at_ms BIGINT NOT NULL,
    pruned_change_seq BIGINT NOT NULL,
    PRIMARY KEY (process_id, incarnation)
);
CREATE INDEX IF NOT EXISTS idx_lash_process_tombstones_change
    ON lash_process_tombstones(pruned_change_seq);

CREATE TABLE IF NOT EXISTS lash_process_artifact_cleanup (
    process_id TEXT COLLATE "C" NOT NULL,
    incarnation BIGINT NOT NULL,
    cleanup_json TEXT NOT NULL,
    PRIMARY KEY (process_id, incarnation),
    FOREIGN KEY (process_id, incarnation) REFERENCES lash_process_tombstones(process_id, incarnation) ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS lash_process_leases (
    process_id TEXT COLLATE "C" PRIMARY KEY REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    lease_owner_id TEXT,
    lease_owner_incarnation_id TEXT,
    lease_token TEXT,
    lease_fencing_token BIGINT NOT NULL DEFAULT 0,
    lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS lash_process_segment_handovers (
    process_id TEXT COLLATE "C" NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    segment_ordinal BIGINT NOT NULL,
    handover_json TEXT NOT NULL,
    started_json TEXT,
    PRIMARY KEY (process_id, segment_ordinal)
);

-- One row per ended parent scope, keyed by the scope itself rather than by a
-- process row: a turn-scoped parent has no process row at all, and a
-- process-scoped parent's row may be pruned before its children settle.
CREATE TABLE IF NOT EXISTS lash_parent_end_plans (
    parent_kind TEXT NOT NULL,
    parent_id TEXT COLLATE "C" NOT NULL,
    parent_payload TEXT NOT NULL,
    ended_at_ms BIGINT NOT NULL,
    settled_at_ms BIGINT,
    PRIMARY KEY (parent_kind, parent_id),
    CONSTRAINT ck_parent_end_plans_kind CHECK (parent_kind IN ('turn', 'queue_drain', 'process'))
);
CREATE INDEX IF NOT EXISTS idx_lash_parent_end_plans_pending
    ON lash_parent_end_plans(ended_at_ms, parent_kind, parent_id)
    WHERE settled_at_ms IS NULL;

CREATE TABLE IF NOT EXISTS lash_tool_intent_submissions (
    replay_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    execution_scope_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    intent_index BIGINT NOT NULL,
    kind TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    submission_json TEXT NOT NULL,
    CONSTRAINT ck_tool_intent_submissions_kind CHECK (kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger'))
);
CREATE INDEX IF NOT EXISTS idx_lash_tool_intent_submissions_scope
    ON lash_tool_intent_submissions(session_id, execution_scope_id, intent_index);

-- One row per open effect group. `next_seq` is the group's settlement counter:
-- a discharging child bumps it inside its own transaction, which is the only
-- allocator that cannot lose an update the way `MAX(settlement_seq) + 1`
-- can under concurrent discharge. `next_commit_seq` is the §4 twin: the
-- final-commit counter a winning final record bumps at the linearization
-- point (ADR 0099). `expected_children` is the write-time arity the opener
-- declared; actual cardinality is COUNT of membership rows. `lifecycle` is
-- the enum-per-phase column — live today; closing/settled are FIG-3410's
-- writes on this column, not new columns.
-- Replay and group keys use byte order on every host locale (FIG-3586): a
-- lashlang run reads its key namespace as one range bounded by a sentinel
-- that must sort after every ordinal, and a locale collation that ignores
-- punctuation would move it.
CREATE TABLE IF NOT EXISTS lash_runtime_effect_group (
    group_key TEXT COLLATE "C" PRIMARY KEY,
    scope_id TEXT NOT NULL,
    session_id TEXT,
    wake TEXT NOT NULL,
    loser_disposition TEXT NOT NULL,
    expected_children BIGINT NOT NULL,
    next_seq BIGINT NOT NULL DEFAULT 0,
    next_commit_seq BIGINT NOT NULL DEFAULT 0,
    lifecycle JSONB NOT NULL DEFAULT '{"type":"live"}',
    created_at_ms BIGINT NOT NULL,
    CONSTRAINT ck_runtime_effect_group_wake CHECK (wake IN ('first', 'first_success', 'all')),
    CONSTRAINT ck_runtime_effect_group_loser_disposition CHECK (loser_disposition IN ('run_to_completion', 'cancel')),
    CONSTRAINT ck_runtime_effect_group_lifecycle CHECK (lifecycle->>'type' IN ('live', 'closing', 'settled'))
);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_group_session
    ON lash_runtime_effect_group(session_id);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_group_scope
    ON lash_runtime_effect_group(scope_id);

-- One row per accepted child of a group, carrying the request that
-- reconstructs it (ADR 0099 section 3). Retained input only: the section 4/5
-- arbitration state lives on the replay row as commit_state/commit_seq,
-- because the CAS that decides a child runs under the replay row's lock and
-- must not reach a second row to win. `command_version` is the command
-- encoding the retained envelope was minted under, checked at decode.
-- Written with the group row in one transaction, children first
-- (ADR 0065 N2), so a recorded group always has discoverable complete input.
-- No scope_id: the group row owns that fact.
CREATE TABLE IF NOT EXISTS lash_runtime_effect_group_child (
    group_key        TEXT COLLATE "C" NOT NULL,
    position         BIGINT NOT NULL,
    replay_key       TEXT COLLATE "C" NOT NULL,
    envelope_json    TEXT NOT NULL,
    command_version  BIGINT NOT NULL,
    created_at_ms    BIGINT NOT NULL,
    -- Membership rows are written before their group row inside the open
    -- transaction (ADR 0065 N2), so the reference must settle at commit, not
    -- at the statement.
    CONSTRAINT fk_runtime_effect_group_child_group FOREIGN KEY (group_key) REFERENCES lash_runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED,
    PRIMARY KEY (group_key, position)
);
-- Reopens, drains and the unsettled-children join all reach a membership row
-- by (group_key, replay_key), which the position primary key does not serve.
CREATE UNIQUE INDEX IF NOT EXISTS uq_lash_runtime_effect_group_child_replay_key
    ON lash_runtime_effect_group_child(group_key, replay_key);

CREATE TABLE IF NOT EXISTS lash_runtime_effect_replay (
    scope_id TEXT NOT NULL,
    session_id TEXT,
    replay_key TEXT COLLATE "C" NOT NULL,
    envelope_hash TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    status TEXT NOT NULL,
    outcome_json TEXT,
    error_json TEXT,
    lease_owner_id TEXT,
    lease_token TEXT,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0,
    due_at_ms BIGINT,
    group_key TEXT COLLATE "C",
    settlement_seq BIGINT,
    commit_state TEXT NOT NULL DEFAULT 'pending',
    commit_seq BIGINT,
    drain_input TEXT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    CONSTRAINT ck_runtime_effect_replay_status CHECK (status IN ('in_progress', 'completed', 'failed')),
    CONSTRAINT ck_runtime_effect_replay_commit_state CHECK (commit_state IN ('pending', 'committed', 'drained', 'cancel_decided')),
    CONSTRAINT ck_runtime_effect_replay_commit_seq CHECK ((commit_seq IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))) AND (group_key IS NULL OR NOT (commit_state IN ('committed', 'drained')) OR commit_seq IS NOT NULL)),
    CONSTRAINT ck_runtime_effect_replay_drain_input CHECK (drain_input IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))),
    CONSTRAINT ck_runtime_effect_replay_outcome_json CHECK ((status = 'completed' AND outcome_json IS NOT NULL) OR (status <> 'completed' AND outcome_json IS NULL)),
    CONSTRAINT ck_runtime_effect_replay_error_json CHECK ((status = 'failed' AND error_json IS NOT NULL) OR (status <> 'failed' AND error_json IS NULL)),
    CONSTRAINT ck_runtime_effect_replay_settlement_seq CHECK ((settlement_seq IS NULL AND NOT (commit_state IN ('drained', 'cancel_decided'))) OR (settlement_seq IS NOT NULL AND commit_state IN ('drained', 'cancel_decided'))),
    CONSTRAINT fk_runtime_effect_replay_group FOREIGN KEY (group_key) REFERENCES lash_runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED,
    PRIMARY KEY (scope_id, replay_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_replay_lease
    ON lash_runtime_effect_replay(status, lease_expires_at_ms);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_replay_session
    ON lash_runtime_effect_replay(session_id);
-- Settlement ranks are read by position, so a group must never record the same
-- sequence twice; the partial index leaves ungrouped and unsettled children
-- (both NULL-bearing) entirely unconstrained.
CREATE UNIQUE INDEX IF NOT EXISTS uq_lash_runtime_effect_replay_group_seq
    ON lash_runtime_effect_replay(group_key, settlement_seq)
    WHERE group_key IS NOT NULL AND settlement_seq IS NOT NULL;
-- One commit position per child, per group: the §4 linearization point's
-- backstop, the same role the settlement-seq unique index plays for ranks.
CREATE UNIQUE INDEX IF NOT EXISTS uq_lash_runtime_effect_replay_commit_seq
    ON lash_runtime_effect_replay(group_key, commit_seq)
    WHERE commit_seq IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_await_event_meta (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    signing_secret BYTEA NOT NULL,
    CONSTRAINT ck_await_event_meta_singleton CHECK (singleton)
);

CREATE TABLE IF NOT EXISTS lash_await_event_waits (
    key_id TEXT PRIMARY KEY,
    scope_json TEXT NOT NULL,
    wait_json TEXT NOT NULL,
    session_id TEXT,
    turn_control BOOLEAN NOT NULL,
    terminal_json TEXT,
    created_at_ms BIGINT NOT NULL,
    resolved_at_ms BIGINT
);
CREATE INDEX IF NOT EXISTS idx_lash_await_event_waits_session
    ON lash_await_event_waits(session_id);

-- Permanent by design: session ids cannot be reused, so revocation
-- evidence must remain after every retention-pruning pass.
CREATE TABLE IF NOT EXISTS lash_await_event_revoked_sessions (
    session_id TEXT PRIMARY KEY,
    revoked_at_ms BIGINT NOT NULL
);

-- Permanent by design: process and runtime-operation ids are single-use, so a
-- retired scope's fence must outlive every retention pass and every restart.
-- Keyed by the scope's journal identity, the same key its effect rows carry.
CREATE TABLE IF NOT EXISTS lash_effect_scope_retirements (
    scope_id TEXT PRIMARY KEY,
    retired_at_ms BIGINT NOT NULL,
    artifact_cleanup_completed BOOLEAN NOT NULL DEFAULT FALSE
);

-- A bound session catalog registers before admitting cancellation work and
-- releases only after it has durably retired the physical scope. The effect
-- owner refuses irreversible retirement while any catalog still participates.
CREATE TABLE IF NOT EXISTS lash_turn_cancel_closure_participants (
    scope_id TEXT NOT NULL,
    participant_id TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    PRIMARY KEY (scope_id, participant_id)
);

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
    process_id TEXT NOT NULL,
    subscription_incarnation TEXT NOT NULL,
    subscription_revision BIGINT NOT NULL,
    subscription_snapshot_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
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

CREATE TABLE IF NOT EXISTS lash_lashlang_artifacts (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    artifact_bytes BYTEA NOT NULL,
    PRIMARY KEY (namespace, artifact_ref)
);
CREATE TABLE IF NOT EXISTS lash_artifact_owners (
    namespace TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    owner_kind TEXT NOT NULL CONSTRAINT ck_artifact_owners_owner_kind CHECK (owner_kind IN ('host', 'process', 'execution')),
    owner_id TEXT NOT NULL,
    PRIMARY KEY (namespace, artifact_ref, owner_kind, owner_id),
    FOREIGN KEY (namespace, artifact_ref) REFERENCES lash_lashlang_artifacts(namespace, artifact_ref) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lash_artifact_owners_owner
    ON lash_artifact_owners(owner_kind, owner_id);
CREATE TABLE IF NOT EXISTS lash_artifact_owner_retirements (
    owner_kind TEXT NOT NULL CONSTRAINT ck_artifact_owner_retirements_owner_kind CHECK (owner_kind = 'execution'),
    owner_id TEXT NOT NULL,
    PRIMARY KEY (owner_kind, owner_id)
);

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

-- Seed rows. Every open mode requires all three: the component version stamp,
-- the transactional process-change clock row, and the store-resident
-- await-event signing secret. `gen_random_uuid()` is core PostgreSQL and draws
-- from the server's strong RNG, so the 32-byte secret needs no extension.
INSERT INTO lash_schema_versions (component, version)
VALUES ('lash-postgres-store', 130)
ON CONFLICT (component) DO NOTHING;

INSERT INTO lash_process_change_clock (
    singleton, current_seq, tombstone_compaction_horizon
) VALUES (TRUE, 0, 0)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_turn_park_clock (
    singleton, current_seq, compaction_horizon
) VALUES (TRUE, 0, 0)
ON CONFLICT (singleton) DO NOTHING;

INSERT INTO lash_await_event_meta (singleton, signing_secret)
VALUES (
    TRUE,
    decode(
        replace(gen_random_uuid()::text, '-', '')
            || replace(gen_random_uuid()::text, '-', ''),
        'hex'
    )
)
ON CONFLICT (singleton) DO NOTHING;
