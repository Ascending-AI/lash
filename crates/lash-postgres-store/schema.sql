-- lash-postgres-store schema, component version 59.
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
    seq BIGSERIAL,
    node_id TEXT PRIMARY KEY,
    parent_node_id TEXT,
    generation BIGINT NOT NULL CHECK (generation >= 0),
    frame_node_id TEXT NOT NULL,
    node_json TEXT NOT NULL,
    tombstoned BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (session_id, generation)
);
CREATE INDEX IF NOT EXISTS idx_lash_graph_nodes_seq
    ON lash_graph_nodes(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_lash_graph_nodes_parent
    ON lash_graph_nodes(parent_node_id);

CREATE TABLE IF NOT EXISTS lash_fork_lineage (
    session_id TEXT NOT NULL,
    ancestor_session_id TEXT NOT NULL,
    fork_node_id TEXT NOT NULL,
    fork_generation BIGINT NOT NULL CHECK (fork_generation >= 0),
    PRIMARY KEY (session_id, ancestor_session_id)
);

CREATE TABLE IF NOT EXISTS lash_usage_deltas (
    seq BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    operation_storage_key TEXT NOT NULL,
    entry_ordinal BIGINT NOT NULL,
    payload_encoding_version INTEGER NOT NULL,
    payload_hash TEXT NOT NULL,
    entry_json TEXT NOT NULL,
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
    created_at_ms BIGINT,
    last_commit_at_ms BIGINT,
    relation_kind TEXT NOT NULL,
    observer_intent_depth BIGINT NOT NULL,
    parent_session_id TEXT,
    caused_by_kind TEXT,
    caused_by_session_id TEXT,
    caused_by_turn_id TEXT,
    caused_by_effect_id TEXT,
    caused_by_call_id TEXT,
    caused_by_process_id TEXT,
    caused_by_process_event_sequence TEXT,
    caused_by_occurrence_id TEXT,
    caused_by_subscription_id TEXT,
    caused_by_subscription_incarnation TEXT,
    caused_by_subscription_revision TEXT,
    caused_by_node_id TEXT,
    source_session_id TEXT,
    source_node_id TEXT,
    observer_inheritance_kind TEXT
);
CREATE INDEX IF NOT EXISTS idx_lash_session_meta_catalog
    ON lash_session_meta(created_at_ms, session_id);

CREATE TABLE IF NOT EXISTS lash_session_meta_observer_intent_processes (
    session_id TEXT NOT NULL,
    layer_index BIGINT NOT NULL,
    process_index BIGINT NOT NULL,
    process_id TEXT NOT NULL,
    PRIMARY KEY (session_id, layer_index, process_index),
    FOREIGN KEY (session_id) REFERENCES lash_session_meta(session_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS lash_session_meta_fork_pending_observer_processes (
    session_id TEXT NOT NULL,
    process_index BIGINT NOT NULL,
    process_id TEXT NOT NULL,
    PRIMARY KEY (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES lash_session_meta(session_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS lash_session_meta_fork_inheritance_processes (
    session_id TEXT NOT NULL,
    process_index BIGINT NOT NULL,
    process_id TEXT NOT NULL,
    PRIMARY KEY (session_id, process_index),
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
    requested_ancestor_node_id TEXT,
    identity_encoding_version INTEGER,
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS lash_turn_cancel_requests (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    origin TEXT,
    reason TEXT,
    disposition TEXT NOT NULL DEFAULT 'defer',
    affected_input_ids TEXT[] NOT NULL DEFAULT '{}',
    affected_dispositions TEXT[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS lash_session_execution_leases (
    session_id TEXT PRIMARY KEY,
    lease_owner_id TEXT,
    lease_owner_incarnation_id TEXT,
    lease_executor_id TEXT,
    lease_owner_liveness_json TEXT,
    lease_token TEXT,
    lease_fencing_token BIGINT NOT NULL DEFAULT 0,
    lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
    lease_term_ms BIGINT NOT NULL DEFAULT 0,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0
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
    claim_id TEXT,
    claim_owner_id TEXT,
    claim_owner_incarnation_id TEXT,
    claim_owner_liveness_json TEXT,
    claim_token TEXT,
    claim_fencing_token BIGINT NOT NULL DEFAULT 0,
    claim_session_lease_generation BIGINT NOT NULL DEFAULT 0,
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
    enqueued_at_ms BIGINT NOT NULL,
    claim_id TEXT,
    claim_owner_id TEXT,
    claim_owner_incarnation_id TEXT,
    claim_owner_liveness_json TEXT,
    claim_token TEXT,
    claim_fencing_token BIGINT NOT NULL DEFAULT 0,
    claim_session_lease_generation BIGINT NOT NULL DEFAULT 0,
    UNIQUE (session_id, source_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_session
    ON lash_pending_turn_inputs(session_id, state, enqueue_seq);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_input_order
    ON lash_pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq);
CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_claim
    ON lash_pending_turn_inputs(session_id, claim_id, claim_token);

CREATE TABLE IF NOT EXISTS lash_attachment_manifest (
    attachment_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    canonical_uri TEXT NOT NULL,
    intent_at_ms BIGINT NOT NULL,
    committed_at_ms BIGINT,
    owner_kind TEXT CHECK (owner_kind IN ('turn', 'process')),
    owner_id TEXT,
    CHECK ((owner_kind IS NULL) = (owner_id IS NULL)),
    PRIMARY KEY (session_id, attachment_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_uncommitted
    ON lash_attachment_manifest(committed_at_ms)
    WHERE committed_at_ms IS NULL;
CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_owner
    ON lash_attachment_manifest(session_id, owner_kind, owner_id, committed_at_ms);

-- Attachment GC fence state, one row per condemned digest. Deliberately
-- timestampless: the protocol is CAS transitions only, never an expiry.
CREATE TABLE IF NOT EXISTS lash_attachment_condemnations (
    attachment_id TEXT PRIMARY KEY,
    phase TEXT NOT NULL CHECK (phase IN ('condemned', 'deleting'))
);

CREATE TABLE IF NOT EXISTS lash_process_change_clock (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    current_seq BIGINT NOT NULL,
    CHECK (singleton)
);
CREATE TABLE IF NOT EXISTS lash_processes (
    process_id TEXT PRIMARY KEY,
    registration_fingerprint TEXT NOT NULL,
    originator_id TEXT NOT NULL,
    wake_session_id TEXT,
    identity_kind TEXT NOT NULL,
    identity_label TEXT,
    is_waiting BOOLEAN NOT NULL,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    change_seq BIGINT NOT NULL,
    status TEXT NOT NULL,
    record_json TEXT NOT NULL
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
CREATE INDEX IF NOT EXISTS idx_lash_processes_waiting
    ON lash_processes(is_waiting);
CREATE INDEX IF NOT EXISTS idx_lash_processes_created
    ON lash_processes(created_at_ms);
CREATE INDEX IF NOT EXISTS idx_lash_processes_wake_session
    ON lash_processes(wake_session_id);

CREATE TABLE IF NOT EXISTS lash_process_events (
    process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    idempotency_key TEXT,
    occurred_at_ms BIGINT NOT NULL,
    event_json TEXT NOT NULL,
    PRIMARY KEY (process_id, sequence)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_process_events_key
    ON lash_process_events(process_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS lash_wake_allocation_floors (
    target_session_id TEXT NOT NULL,
    process_id TEXT NOT NULL,
    allocation_floor BIGINT NOT NULL,
    PRIMARY KEY (target_session_id, process_id)
);

CREATE TABLE IF NOT EXISTS lash_process_wake_deliveries (
    delivery_id TEXT PRIMARY KEY,
    process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    target_session_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    state TEXT NOT NULL,
    claim_token TEXT,
    attempts BIGINT NOT NULL DEFAULT 0,
    first_attempt_ms BIGINT,
    next_attempt_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    discard_reason TEXT,
    delivery_json TEXT NOT NULL
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
    process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    PRIMARY KEY (session_id, process_id)
);
CREATE INDEX IF NOT EXISTS idx_lash_process_observers_process
    ON lash_process_observers(process_id, session_id);

CREATE TABLE IF NOT EXISTS lash_process_tombstones (
    process_id TEXT PRIMARY KEY,
    terminal_label TEXT NOT NULL,
    pruned_at_ms BIGINT NOT NULL,
    pruned_change_seq BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_process_tombstones_change
    ON lash_process_tombstones(pruned_change_seq);

CREATE TABLE IF NOT EXISTS lash_process_leases (
    process_id TEXT PRIMARY KEY REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    lease_owner_id TEXT,
    lease_owner_incarnation_id TEXT,
    lease_owner_liveness_json TEXT,
    lease_token TEXT,
    lease_fencing_token BIGINT NOT NULL DEFAULT 0,
    lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS lash_process_segment_handovers (
    process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    segment_ordinal BIGINT NOT NULL,
    handover_json TEXT NOT NULL,
    PRIMARY KEY (process_id, segment_ordinal)
);

CREATE TABLE IF NOT EXISTS lash_process_parent_end_plans (
    process_id TEXT PRIMARY KEY REFERENCES lash_processes(process_id) ON DELETE CASCADE,
    actions_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS lash_tool_intent_submissions (
    replay_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    execution_scope_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    intent_index BIGINT NOT NULL,
    kind TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    submission_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_tool_intent_submissions_scope
    ON lash_tool_intent_submissions(session_id, execution_scope_id, intent_index);

CREATE TABLE IF NOT EXISTS lash_runtime_effect_replay (
    scope_id TEXT NOT NULL,
    session_id TEXT,
    replay_key TEXT NOT NULL,
    envelope_hash TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    status TEXT NOT NULL,
    outcome_json TEXT,
    error_json TEXT,
    lease_owner_id TEXT,
    lease_token TEXT,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0,
    due_at_ms BIGINT,
    group_key TEXT,
    settlement_seq BIGINT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
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
-- The loser drain's queue read: one group's children that hold no rank yet. The
-- predicate keeps the index to exactly the rows a drain can act on, so it
-- shrinks as a group settles and holds nothing at all for a drained one.
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_replay_group_unsettled
    ON lash_runtime_effect_replay(group_key, replay_key)
    WHERE group_key IS NOT NULL AND settlement_seq IS NULL;

-- One row per open effect group. `next_seq` is the group's settlement counter:
-- a finalizing child bumps it inside its own fenced transaction, which is the
-- only allocator that cannot lose an update the way `MAX(settlement_seq) + 1`
-- can under concurrent finalize.
CREATE TABLE IF NOT EXISTS lash_runtime_effect_group (
    group_key TEXT PRIMARY KEY,
    scope_id TEXT NOT NULL,
    session_id TEXT,
    wake TEXT NOT NULL,
    loser_disposition TEXT NOT NULL,
    children BIGINT NOT NULL,
    next_seq BIGINT NOT NULL DEFAULT 0,
    created_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_group_session
    ON lash_runtime_effect_group(session_id);
CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_group_scope
    ON lash_runtime_effect_group(scope_id);

CREATE TABLE IF NOT EXISTS lash_await_event_meta (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    signing_secret BYTEA NOT NULL,
    CHECK (singleton)
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

CREATE TABLE IF NOT EXISTS lash_trigger_subscriptions (
    subscription_id TEXT PRIMARY KEY,
    owner_scope TEXT NOT NULL,
    subscription_key TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    revision BIGINT NOT NULL,
    definition_fingerprint TEXT NOT NULL,
    source_type TEXT NOT NULL,
    source_key TEXT NOT NULL,
    enabled BOOLEAN NOT NULL,
    tombstoned BOOLEAN NOT NULL,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    record_json TEXT NOT NULL,
    UNIQUE(owner_scope, subscription_key)
);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_subscriptions_registrant
    ON lash_trigger_subscriptions(owner_scope, subscription_key);
CREATE INDEX IF NOT EXISTS idx_lash_trigger_subscriptions_source
    ON lash_trigger_subscriptions(source_type, source_key, enabled);

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
    request_fingerprint TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL
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

-- Seed rows. Every open mode requires all three: the component version stamp,
-- the transactional process-change clock row, and the store-resident
-- await-event signing secret. `gen_random_uuid()` is core PostgreSQL and draws
-- from the server's strong RNG, so the 32-byte secret needs no extension.
INSERT INTO lash_schema_versions (component, version)
VALUES ('lash-postgres-store', 59)
ON CONFLICT (component) DO NOTHING;

INSERT INTO lash_process_change_clock (singleton, current_seq)
VALUES (TRUE, 0)
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
