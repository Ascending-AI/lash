//! Canonical SQLite schema + the open/ensure helpers built on
//! [`SqliteConnection`].
//!
//! The `SCHEMA` and `PROCESS_SCHEMA` strings are plain SQLite
//! and are copied verbatim from the prior store. The only thing that changes in
//! the rusqlite port is the *open path*: the prior store's `Builder::new_local` +
//! `experimental_multiprocess_wal` + `PRAGMA journal_mode='mvcc'` is replaced by
//! [`SqliteConnection::open`], which applies real `journal_mode=WAL` and a
//! 15-second `busy_timeout` (see `conn.rs`).

use super::*;
use crate::schema_fragments::{SESSION_INGRESS_TABLE, SESSION_ROOTS_TABLES};
pub(crate) use crate::trigger_schema::TRIGGER_SCHEMA;

#[derive(Clone, Copy)]
struct SqliteDatabaseDefinition {
    name: &'static str,
    schema: &'static str,
    /// Shared table sets this database also carries; see
    /// [`SqliteDatabase::fragments`].
    fragments: &'static [&'static str],
}

/// One of the three independently versioned SQLite databases a lash backend
/// can hold.
///
/// The variant is the single table for each database's schema SQL, version,
/// and operator-facing name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqliteDatabase {
    /// Sessions, graph nodes, checkpoints, leases, queued work.
    DurableCore,
    /// The process registry.
    ProcessRegistry,
    /// The trigger store.
    Triggers,
}

impl SqliteDatabase {
    /// Every database a backend holds.
    pub(crate) const ALL: [Self; 3] = [Self::DurableCore, Self::ProcessRegistry, Self::Triggers];

    /// The file this database is kept in under a file backend's root.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::DurableCore => crate::DURABLE_CORE_DB_FILE,
            Self::ProcessRegistry => "process-registry.db",
            Self::Triggers => "triggers.db",
        }
    }

    /// The last segment of this database's `memdb` name in a memory
    /// backend.
    pub(crate) const fn memory_name(self) -> &'static str {
        match self {
            Self::DurableCore => "core",
            Self::ProcessRegistry => "registry",
            Self::Triggers => "triggers",
        }
    }

    const fn definition(self) -> SqliteDatabaseDefinition {
        match self {
            Self::DurableCore => SqliteDatabaseDefinition {
                name: "durable core",
                schema: SCHEMA,
                fragments: &[SESSION_INGRESS_TABLE, SESSION_ROOTS_TABLES],
            },
            Self::ProcessRegistry => SqliteDatabaseDefinition {
                name: "process registry",
                schema: PROCESS_SCHEMA,
                fragments: &[],
            },
            Self::Triggers => SqliteDatabaseDefinition {
                name: "trigger store",
                schema: TRIGGER_SCHEMA,
                fragments: &[],
            },
        }
    }

    pub(crate) fn schema(self) -> &'static str {
        self.definition().schema
    }

    pub(crate) const fn component(self) -> lash_core_execution::compat::ComponentId {
        use lash_core_execution::compat::ComponentId;
        match self {
            Self::DurableCore => ComponentId::SQLITE_CORE,
            Self::ProcessRegistry => ComponentId::SQLITE_REGISTRY,
            Self::Triggers => ComponentId::SQLITE_TRIGGERS,
        }
    }

    /// This build's compatibility version for this database.
    pub fn expected_version(self) -> i64 {
        lash_core_execution::compat::descriptor(self.component())
            .map_or(1, |descriptor| i64::from(descriptor.writes.max()))
    }

    /// The operator-facing name used in reports and refusal messages.
    pub fn name(self) -> &'static str {
        self.definition().name
    }

    /// Shared DDL fragments applied after `schema` inside the same
    /// initialization transaction; see [`crate::schema_fragments`].
    pub(crate) fn fragments(self) -> &'static [&'static str] {
        self.definition().fragments
    }

    /// The shared fragments provisioning applies after `schema`, in order.
    #[cfg(feature = "testing")]
    pub(crate) fn fragment_statements(self) -> impl Iterator<Item = &'static str> {
        self.definition().fragments.iter().copied()
    }

    /// Everything provisioning applies, in order: the schema body followed by
    /// the shared fragments. Fixtures that shadow one table apply this to
    /// complete the catalog — every statement is idempotent, so the shadowed
    /// declaration stands while every other table is created.
    #[cfg(feature = "testing")]
    pub(crate) fn provisioning_statements(self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.schema()).chain(self.fragment_statements())
    }
}

/// Canonical SQLite schema for a factory-wide lash durable-core catalog.
///
/// This is the *only* schema the store supports. Older durable-core databases
/// must be deleted before opening with this binary. Lash's broader durable
/// contract still lives one level up in per-record `schema_version` stamps,
/// not in compatibility reads.
/// Each `checkpoint_blob_refs` row is owned by the session whose head or anchor
/// owns the checkpoint root named by `checkpoint_ref`. Owner-scoped session
/// delete or process prune deletes an unreferenced root and cascades its edges
/// in the same transaction. Component blobs are shared and have no
/// component-side cascade.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blobs (
    hash    TEXT PRIMARY KEY,
    content BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS session_head (
    session_id     TEXT PRIMARY KEY,
    head_json      TEXT NOT NULL DEFAULT '{}',
    head_revision  INTEGER NOT NULL DEFAULT 0,
    leaf_node_id   TEXT,
    checkpoint_ref TEXT,
    pending_follow_on_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_session_head_leaf
    ON session_head(leaf_node_id);
CREATE INDEX IF NOT EXISTS idx_session_head_checkpoint_ref
    ON session_head(checkpoint_ref);

CREATE TABLE IF NOT EXISTS node_anchors (
    node_id           TEXT PRIMARY KEY,
    checkpoint_ref    TEXT NOT NULL,
    source_session_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_node_anchors_checkpoint_ref
    ON node_anchors(checkpoint_ref);

-- Indexed projection of the exact manifest -> component edges carried in each
-- checkpoint blob. Each row is owned by the session whose head or anchor owns
-- the checkpoint root named by checkpoint_ref. Owner-scoped session delete or
-- process prune deletes an unreferenced root and cascades its edges in the same
-- transaction. Components are shared and have no component-side cascade. This
-- is reference data, never a cached reference count.
CREATE TABLE IF NOT EXISTS checkpoint_blob_refs (
    checkpoint_ref TEXT NOT NULL,
    blob_ref       TEXT NOT NULL,
    PRIMARY KEY (checkpoint_ref, blob_ref),
    FOREIGN KEY (checkpoint_ref) REFERENCES blobs(hash) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_checkpoint_blob_refs_blob_ref
    ON checkpoint_blob_refs(blob_ref, checkpoint_ref);

-- The recovery leader lease (ADR 0109 §1.6): one row per engine authority
-- naming the deployment that runs the leader-only recovery duties. Load
-- control, never a fence; every comparison runs on the database clock.
CREATE TABLE IF NOT EXISTS recovery_leader (
    name            TEXT PRIMARY KEY,
    holder_id       TEXT NOT NULL,
    generation_rank INTEGER NOT NULL,
    term            INTEGER NOT NULL,
    elected_at_ms   INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deleted_sessions (
    session_id        TEXT PRIMARY KEY,
    created_at_ms     INTEGER NOT NULL,
    last_commit_at_ms INTEGER,
    head_revision     INTEGER NOT NULL,
    relation_kind     TEXT NOT NULL,
    parent_session_id TEXT
);

CREATE TABLE IF NOT EXISTS graph_nodes (
    session_id     TEXT NOT NULL,
    node_id        TEXT NOT NULL UNIQUE,
    parent_node_id TEXT,
    generation     INTEGER NOT NULL CONSTRAINT ck_graph_nodes_generation CHECK (generation >= 0),
    frame_node_id  TEXT NOT NULL,
    body_bytes     INTEGER NOT NULL CONSTRAINT ck_graph_nodes_body_bytes CHECK (body_bytes >= 0),
    node_json      TEXT NOT NULL,
    tombstoned     INTEGER NOT NULL DEFAULT 0,
    UNIQUE (session_id, generation)
);
CREATE INDEX IF NOT EXISTS idx_graph_nodes_parent
    ON graph_nodes(parent_node_id);

CREATE TABLE IF NOT EXISTS fork_lineage (
    session_id         TEXT NOT NULL,
    ancestor_session_id TEXT NOT NULL,
    fork_node_id       TEXT NOT NULL,
    fork_generation    INTEGER NOT NULL CONSTRAINT ck_fork_lineage_fork_generation CHECK (fork_generation >= 0),
    PRIMARY KEY (session_id, ancestor_session_id)
);

CREATE TABLE IF NOT EXISTS usage_deltas (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL,
    operation_storage_key TEXT NOT NULL,
    entry_ordinal         INTEGER NOT NULL,
    payload_encoding_version INTEGER NOT NULL,
    payload_hash          TEXT NOT NULL,
    source               TEXT NOT NULL,
    model                TEXT NOT NULL,
    input_tokens         INTEGER NOT NULL,
    output_tokens        INTEGER NOT NULL,
    cache_read_input_tokens  INTEGER NOT NULL,
    cache_write_input_tokens INTEGER NOT NULL,
    reasoning_output_tokens     INTEGER NOT NULL,
    reconciled_call_id TEXT,
    reconciled_attempt_ordinal INTEGER,
    CONSTRAINT ck_usage_deltas_reconciled_pair CHECK ((reconciled_call_id IS NULL) = (reconciled_attempt_ordinal IS NULL)),
    UNIQUE (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash)
);
CREATE INDEX IF NOT EXISTS idx_usage_deltas_session_seq
    ON usage_deltas(session_id, seq);

CREATE TABLE IF NOT EXISTS usage_delta_holes (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    call_id TEXT NOT NULL,
    attempt_ordinal INTEGER NOT NULL,
    generation_id TEXT,
    PRIMARY KEY (session_id, seq, call_id, attempt_ordinal),
    FOREIGN KEY (seq) REFERENCES usage_deltas(seq) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_usage_delta_holes_attempt
    ON usage_delta_holes(session_id, call_id, attempt_ordinal);

CREATE TABLE IF NOT EXISTS session_meta (
    session_id                       TEXT PRIMARY KEY,
    session_state_version            INTEGER,
    created_at_ms                    INTEGER NOT NULL DEFAULT 0,
    last_commit_at_ms                INTEGER,
    relation_kind                    TEXT NOT NULL,
    parent_session_id                TEXT,
    caused_by_kind                   TEXT,
    caused_by_session_id             TEXT,
    caused_by_turn_id                TEXT,
    caused_by_effect_id              TEXT,
    caused_by_call_id                TEXT,
    caused_by_process_id             TEXT,
    caused_by_process_event_sequence TEXT,
    caused_by_occurrence_id           TEXT,
    caused_by_subscription_id         TEXT,
    caused_by_subscription_incarnation TEXT,
    caused_by_subscription_revision   TEXT,
    caused_by_node_id                 TEXT,
    source_session_id                 TEXT,
    source_node_id                    TEXT,
    drive_epoch                       INTEGER NOT NULL DEFAULT 0,
    drive_admission_id                TEXT,
    drive_root_start                  TEXT,
    admission_base_checkpoint_ref     TEXT,
    closing_intent                    INTEGER,
    owning_process_id                 TEXT,
    obligation_id                    TEXT,
    obligation_state                 TEXT,
    obligation_attempts              INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms             INTEGER,
    obligation_claim_token           TEXT,
    obligation_stall_reason          TEXT,
    obligation_last_error            TEXT,
    obligation_settled_at_ms         INTEGER,
    CONSTRAINT ck_session_meta_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')),
    CONSTRAINT ck_session_meta_relation_family CHECK ((relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL AND source_node_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))),
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'trigger_occurrence' AND caused_by_occurrence_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node'))))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_session_meta_obligation_id
    ON session_meta(obligation_id);
CREATE INDEX IF NOT EXISTS idx_session_meta_obligation_due
    ON session_meta(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_session_meta_obligation_stalled
    ON session_meta(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE TABLE IF NOT EXISTS session_meta_pending_observer_intents (
    session_id    TEXT NOT NULL,
    process_index INTEGER NOT NULL,
    process_id    TEXT NOT NULL,
    PRIMARY KEY (session_id, process_id),
    UNIQUE (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES session_meta(session_id) ON DELETE CASCADE
);

-- Identity families: all-NULL is a plain commit; hash+version+count is an
-- append identity; hash+version without a count is a semantic-boundary
-- identity (FIG-2480). A count without a hash is representable nowhere.
CREATE TABLE IF NOT EXISTS runtime_turn_commits (
    session_id                  TEXT NOT NULL,
    turn_id                     TEXT NOT NULL,
    turn_commit_hash            TEXT NOT NULL,
    result_json                 TEXT NOT NULL,
    outcome_code                TEXT CONSTRAINT ck_runtime_turn_commits_outcome CHECK (outcome_code IN ('completed', 'frame_switch', 'cancelled', 'failed_incomplete', 'failed_invalid_input', 'failed_max_turns', 'failed_tool_failure', 'failed_provider_error', 'failed_context_overflow', 'failed_plugin_abort', 'failed_runtime_error', 'failed_submitted_error', 'failed_tool_error')),
    committed_at_ms             INTEGER NOT NULL,
    request_identity_hash       TEXT,
    requested_node_count        INTEGER,
    identity_encoding_version   INTEGER,
    failure_evidence            INTEGER NOT NULL,
    PRIMARY KEY (session_id, turn_id),
    CONSTRAINT ck_runtime_turn_commits_identity CHECK ((request_identity_hash IS NULL) = (identity_encoding_version IS NULL) AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_runtime_turn_commits_failure_evidence
    ON runtime_turn_commits(session_id, committed_at_ms, turn_id)
    WHERE failure_evidence;

CREATE TABLE IF NOT EXISTS turn_cancel_requests (
    session_id TEXT NOT NULL,
    turn_id    TEXT NOT NULL,
    record_json TEXT NOT NULL,
    intent_revision INTEGER NOT NULL CONSTRAINT ck_turn_cancel_requests_intent_revision CHECK (intent_revision >= 1),
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS turn_cancellation_bindings (
    session_id TEXT PRIMARY KEY,
    binding_id TEXT NOT NULL CONSTRAINT ck_turn_cancellation_bindings_binding_id CHECK (length(binding_id) > 0),
    admitted_scope_json TEXT
);

CREATE TABLE IF NOT EXISTS turn_cancel_closure_authorizations (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    authorization_json TEXT NOT NULL,
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS turn_cancel_retired_scopes (
    scope_id TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS turn_parks (
    session_id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL,
    park_id INTEGER NOT NULL,
    reason_code TEXT NOT NULL,
    reason_json TEXT NOT NULL,
    since_ms INTEGER NOT NULL,
    last_refused_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL CONSTRAINT ck_turn_parks_attempts CHECK (attempts >= 1),
    park_executable_generation TEXT,
    engine_ref TEXT,
    resume_intent INTEGER,
    park_build_generation TEXT
);
CREATE INDEX IF NOT EXISTS idx_turn_parks_since
    ON turn_parks(since_ms, session_id);
CREATE INDEX IF NOT EXISTS idx_turn_parks_executable_generation
    ON turn_parks(park_executable_generation) WHERE park_executable_generation IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_turn_parks_build_generation
    ON turn_parks(park_build_generation) WHERE park_build_generation IS NOT NULL;

CREATE TABLE IF NOT EXISTS turn_park_clock (
    singleton           INTEGER PRIMARY KEY CONSTRAINT ck_turn_park_clock_singleton CHECK (singleton = 1),
    current_seq         INTEGER NOT NULL DEFAULT 0,
    compaction_horizon  INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO turn_park_clock (
    singleton, current_seq, compaction_horizon
) VALUES (1, 0, 0);

CREATE TABLE IF NOT EXISTS turn_park_events (
    seq         INTEGER PRIMARY KEY,
    session_id  TEXT NOT NULL,
    turn_id     TEXT NOT NULL,
    park_id     INTEGER NOT NULL,
    kind        TEXT NOT NULL CONSTRAINT ck_turn_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled', 'redrive_requested')),
    cause       TEXT,
    reason_json TEXT,
    at_ms       INTEGER NOT NULL,
    park_build_generation TEXT,
    CONSTRAINT ck_turn_park_events_parked_reason CHECK ((kind = 'parked' AND reason_json IS NOT NULL AND cause IS NULL) OR (kind <> 'parked' AND reason_json IS NULL AND cause IS NOT NULL))
);


CREATE TABLE IF NOT EXISTS queued_work_batches (
    enqueue_seq       INTEGER NOT NULL,
    batch_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    delivery_policy   TEXT NOT NULL,
    work_kind         TEXT NOT NULL,
    authority_json    TEXT NOT NULL,
    merge_key         TEXT,
    enqueued_at_ms    INTEGER NOT NULL,
    admitted_root     TEXT, -- The root whose fenced admission holds the batch; NULL while open.
    admitted_by       TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    obligation_id     TEXT,
    obligation_state  TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_queued_work_batches_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_queued_work_batches_work_kind CHECK (work_kind IN ('turn', 'control')),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK (delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')),
    CONSTRAINT ck_queued_work_batches_admission_all_or_none CHECK ((admitted_root IS NULL) = (admitted_by IS NULL)),
    UNIQUE (session_id, source_key),
    PRIMARY KEY (session_id, enqueue_seq)
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_queued_work_batches_obligation_id
    ON queued_work_batches(obligation_id);
CREATE INDEX IF NOT EXISTS idx_queued_work_batches_obligation_due
    ON queued_work_batches(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_queued_work_batches_obligation_stalled
    ON queued_work_batches(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE TABLE IF NOT EXISTS queued_work_items (
    batch_id      TEXT NOT NULL,
    item_index    INTEGER NOT NULL,
    item_id       TEXT NOT NULL,
    payload_json  TEXT NOT NULL,
    PRIMARY KEY (batch_id, item_index),
    FOREIGN KEY (batch_id) REFERENCES queued_work_batches(batch_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS wake_redelivery_fences (
    session_id       TEXT NOT NULL,
    process_id       TEXT NOT NULL,
    allocation_floor INTEGER NOT NULL,
    PRIMARY KEY (session_id, process_id)
);

CREATE INDEX IF NOT EXISTS idx_queued_work_session_command_order
    ON queued_work_batches(session_id, work_kind, enqueued_at_ms, enqueue_seq);

DROP INDEX IF EXISTS idx_queued_work_admitted;
CREATE INDEX IF NOT EXISTS idx_queued_work_admission_order
    ON queued_work_batches(session_id, admitted_root, enqueue_seq);

CREATE TABLE IF NOT EXISTS pending_turn_inputs (
    enqueue_seq       INTEGER NOT NULL,
    input_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    ingress_json      TEXT NOT NULL,
    state             TEXT NOT NULL,
    input_json        TEXT NOT NULL,
    submitted_ingress_json TEXT NOT NULL,
    submission_digest TEXT NOT NULL,
    enqueued_at_ms    INTEGER NOT NULL,
    admitted_root     TEXT, -- The root whose fenced admission holds the input; NULL while open.
    admitted_by       TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    run_spec_hash     TEXT,
    obligation_id     TEXT,
    obligation_state  TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_pending_turn_inputs_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK ((json_extract(ingress_json, '$.scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR (json_extract(ingress_json, '$.scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_admission_all_or_none CHECK ((admitted_root IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_pending_turn_inputs_settled_unadmitted CHECK (admitted_root IS NULL OR state NOT IN ('cancelled', 'completed')),
    UNIQUE (session_id, source_key),
    PRIMARY KEY (session_id, enqueue_seq)
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_pending_turn_inputs_obligation_id
    ON pending_turn_inputs(obligation_id);
CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_obligation_due
    ON pending_turn_inputs(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_obligation_stalled
    ON pending_turn_inputs(obligation_id)
    WHERE obligation_state = 'stalled';

DROP INDEX IF EXISTS idx_pending_turn_inputs_session;

DROP INDEX IF EXISTS idx_pending_turn_input_order;

-- All undelivered inputs, including ones a root already holds. State is the
-- partial predicate, so settled rows cannot lengthen an open-input scan.
-- Enqueue order in the key lets list_undelivered avoid a history scan or sort.
DROP INDEX IF EXISTS idx_pending_turn_inputs_open;
CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_open_state
    ON pending_turn_inputs(session_id, enqueue_seq)
    WHERE state IN ('pending_active', 'deferred_next_turn');

DROP INDEX IF EXISTS idx_pending_turn_inputs_admitted;
CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_bound_root
    ON pending_turn_inputs(session_id, admitted_root)
    WHERE admitted_root IS NOT NULL;

-- One row per run spec a session's inputs carry (FIG-3838), interned once per
-- hash in the transaction that admits the input naming it, immutable, and
-- owned by the session: reclaimed when the session is deleted. The default
-- spec is never interned; its inputs carry a NULL `run_spec_hash`.
CREATE TABLE IF NOT EXISTS session_run_specs (
    session_id TEXT NOT NULL,
    spec_hash  TEXT NOT NULL,
    spec_json  TEXT NOT NULL,
    PRIMARY KEY (session_id, spec_hash)
);

-- `write_id` is the identity of the write attempt that currently owns a row,
-- minted by `begin_attachment_write`; completion and abort are matched on it,
-- and it is NULL on a row created by adoption, which owns no write attempt.
-- `written_at_ms` is the upload evidence: set when the owning attempt reported
-- a successful backend put. Adoption of a digest requires some row to carry it.
CREATE TABLE IF NOT EXISTS attachment_manifest (
    attachment_id    TEXT NOT NULL,
    session_id       TEXT NOT NULL,
    canonical_uri    TEXT NOT NULL,
    intent_at_ms     INTEGER NOT NULL,
    write_id         TEXT,
    written_at_ms    INTEGER,
    committed_at_ms  INTEGER,
    owner_kind       TEXT CONSTRAINT ck_attachment_manifest_owner_kind CHECK (owner_kind IN ('turn', 'process')),
    owner_id         TEXT,
    CONSTRAINT ck_attachment_manifest_owner_identity CHECK ((owner_kind IS NULL AND owner_id IS NULL) OR (owner_kind IN ('turn', 'process') AND owner_id IS NOT NULL)),
    PRIMARY KEY (session_id, attachment_id)
);

-- Attachment GC fence per condemned digest, owned by a sweep generation (ADR 0067 §6).
CREATE TABLE IF NOT EXISTS attachment_condemnations (
    attachment_id TEXT PRIMARY KEY,
    phase         TEXT NOT NULL CONSTRAINT ck_attachment_condemnations_phase CHECK (phase IN ('condemned', 'deleting')),
    write_token   TEXT,
    write_session_id TEXT,
    next_delete_at_ms BIGINT NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_next_delete CHECK (next_delete_at_ms >= 0),
    sweep_generation INTEGER NOT NULL,
    delete_attempts  INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_delete_attempts CHECK (delete_attempts >= 0),
    last_delete_error TEXT CONSTRAINT ck_attachment_condemnations_failure_pairing CHECK ((delete_attempts = 0) = (last_delete_error IS NULL)),
    stall_reason     TEXT CONSTRAINT ck_attachment_condemnations_stall_reason CHECK (stall_reason IN ('attempts_exhausted', 'refused')) CONSTRAINT ck_attachment_condemnations_stall_attempts CHECK (stall_reason IS NULL OR delete_attempts > 0),
    CONSTRAINT ck_attachment_condemnations_write_token_pairing CHECK ((write_token IS NULL) = (write_session_id IS NULL)),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (write_token IS NULL OR phase = 'condemned')
);
CREATE TABLE IF NOT EXISTS attachment_sweep_clock (singleton INTEGER PRIMARY KEY CONSTRAINT ck_attachment_sweep_clock_singleton CHECK (singleton = 1), generation INTEGER NOT NULL);

-- Attachment bytes when the session catalog is the deployment's attachment
-- backend (`SqliteAttachmentStore`), keyed by content id. Separate from `blobs`:
-- the attachment GC deletes every listed blob the manifest does not root, and
-- `blobs` holds checkpoint and artifact bytes rooted elsewhere. `stored_at_ms`
-- is the freshness a repeated put restamps and the GC's write grace reads.
CREATE TABLE IF NOT EXISTS attachment_blobs (
    attachment_id TEXT PRIMARY KEY,
    content       BLOB NOT NULL,
    stored_at_ms  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS artifact_refs (
    namespace    TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    blob_ref     TEXT NOT NULL,
    PRIMARY KEY (namespace, artifact_ref)
);

-- Exact referrer edges for immutable artifacts. The edge is the liveness fact;
-- no maintained count or last-operation field exists on shared bytes.
--
-- The referrer-kind CHECKs here, on the fences and on both cleanup tables
-- admit any non-empty label, not the kind list (ADR 0115 section 5). SQLite
-- cannot alter a CHECK, so a list would make a later build's kind a table
-- rebuild. The vocabulary is enforced where it is typed: every write binds
-- ArtifactReferrerKind::as_str, and every read decodes through
-- ArtifactReferrer::decode, which refuses a label this build does not know
-- as Incompatible(UnknownVocabulary) and never reads the row as absent.
CREATE TABLE IF NOT EXISTS artifact_referrer_edges (
    namespace    TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_kind CHECK (length(referrer_kind) > 0),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_id CHECK (length(referrer_id) > 0),
    PRIMARY KEY (namespace, artifact_ref, referrer_kind, referrer_id),
    FOREIGN KEY (namespace, artifact_ref) REFERENCES artifact_refs(namespace, artifact_ref) ON DELETE CASCADE
);

-- Every ended referrer has a permanent publication fence.
CREATE TABLE IF NOT EXISTS artifact_referrer_fences (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_fences_kind CHECK (length(referrer_kind) > 0),
    referrer_id TEXT NOT NULL CONSTRAINT ck_artifact_referrer_fences_id CHECK (length(referrer_id) > 0),
    ended_at_ms INTEGER NOT NULL,
    PRIMARY KEY (referrer_kind, referrer_id)
);

CREATE INDEX IF NOT EXISTS idx_attachment_manifest_session
    ON attachment_manifest(session_id, committed_at_ms);
CREATE INDEX IF NOT EXISTS idx_attachment_manifest_uncommitted
    ON attachment_manifest(committed_at_ms)
    WHERE committed_at_ms IS NULL;
-- Adoption asks one question of the whole table: does any row for this digest
-- carry upload evidence?
CREATE INDEX IF NOT EXISTS idx_attachment_manifest_written
    ON attachment_manifest(attachment_id, written_at_ms);
CREATE INDEX IF NOT EXISTS idx_attachment_manifest_owner
    ON attachment_manifest(session_id, owner_kind, owner_id, committed_at_ms);
CREATE INDEX IF NOT EXISTS idx_artifact_refs_blob_ref
    ON artifact_refs(blob_ref);

-- The named process-definition registry (FIG-2995, ADR 0095): owner scope,
-- name, revision, pinned definition fingerprint, lifecycle tombstone and
-- change sequence, unique on owner scope and name. Written only by the
-- RegisterProcessDefinition intent under revision-and-fingerprint
-- compare-and-swap. The pinned ProcessDefinitionRef travels in record_json;
-- the fingerprint column is what the CAS fence compares. The lifecycle is
-- the FIG-1951 one-column enum with a paired-nullable delete timestamp, not
-- the two-boolean layout the trigger table still carries. Session-scoped
-- names follow the ADR 0049 deletion frontier; host- and platform-scoped
-- tombstones are never collected (ADR 0067).
CREATE TABLE IF NOT EXISTS process_definitions (
    definition_id  TEXT PRIMARY KEY,
    owner_scope    TEXT NOT NULL,
    name           TEXT NOT NULL,
    revision       INTEGER NOT NULL,
    fingerprint    TEXT NOT NULL,
    lifecycle      TEXT NOT NULL,
    deleted_at_ms  INTEGER,
    change_seq     INTEGER NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    record_json    TEXT NOT NULL,
    CONSTRAINT ck_process_definitions_lifecycle CHECK ((lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)),
    UNIQUE(owner_scope, name)
);

CREATE INDEX IF NOT EXISTS idx_process_definitions_registrant
    ON process_definitions(owner_scope, name);
CREATE INDEX IF NOT EXISTS idx_process_definitions_change
    ON process_definitions(change_seq);

CREATE INDEX IF NOT EXISTS idx_artifact_referrer_edges_referrer
    ON artifact_referrer_edges(referrer_kind, referrer_id);

CREATE TABLE IF NOT EXISTS artifact_cleanup_obligations (
    referrer_kind TEXT NOT NULL CHECK (length(referrer_kind) > 0),
    referrer_id TEXT NOT NULL CHECK (length(referrer_id) > 0),
    cleanup_json TEXT NOT NULL,
    obligation_id TEXT NOT NULL,
    obligation_state TEXT NOT NULL,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    PRIMARY KEY (referrer_kind, referrer_id),
    CONSTRAINT ck_artifact_cleanup_obligations_obligation CHECK (((obligation_state = 'due' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'stalled' AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_cleanup_obligations_id
    ON artifact_cleanup_obligations(obligation_id);
CREATE INDEX IF NOT EXISTS idx_artifact_cleanup_obligations_due
    ON artifact_cleanup_obligations(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_artifact_cleanup_obligations_stalled
    ON artifact_cleanup_obligations(obligation_id)
    WHERE obligation_state = 'stalled';


CREATE TABLE IF NOT EXISTS release_stamp (
    singleton           INTEGER PRIMARY KEY CONSTRAINT ck_release_stamp_singleton CHECK (singleton = 1),
    release_version     TEXT NOT NULL,
    schema_versions     TEXT NOT NULL,
    written_at_epoch_ms INTEGER NOT NULL
);

-- The fleet-format row (ADR 0106 §1 `F`): the durable-format generation every
-- writer in the fleet emits. A single-process SQLite deployment finalizes on
-- open, so the schema-open transaction pins this row to the build's own
-- format. PostgreSQL carries the same singleton as `lash_fleet_format`.
CREATE TABLE IF NOT EXISTS lash_compat (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    component TEXT NOT NULL,
    version INTEGER NOT NULL,
    min_reader INTEGER NOT NULL,
    fleet_format INTEGER NOT NULL,
    CHECK (version >= 1 AND min_reader >= 1 AND min_reader <= version AND fleet_format >= 1)
);
";

/// Canonical schema version. There is no migration chain — older databases
/// must be deleted before opening. See the [`SCHEMA`] doc comment for the
/// rationale.
///
/// Bumped to 10 for the attachment three-layer cutover (ADR 0028): the
/// `attachment_manifest` this schema gates carried, pre-cutover, committed refs
/// and canonical URIs that named `sessions/<hash>/...` blob paths the flat
/// content-addressed layout cannot read. Pre-10 session databases are rejected
/// at open and recreated; the old `sessions/` blob trees are unreachable garbage
/// operators delete manually.
///
/// Bumped to 11 for claim generation fencing (ADR 0029): queued-work and
/// pending-turn-input rows replace their per-claim claimed-at and expiry
/// columns with a single column pinning the session-execution-lease generation
/// the claim was taken under (since replaced by root admission, FIG-3927).
/// There is no migration chain — pre-11 session databases are rejected at open
/// and recreated.
/// Bumped to 12 for FIG-546 owner-bound attachment intents. This is a
/// reject-and-recreate cutover: pre-12 manifests have no durable execution
/// owner and cannot participate in reachability-based reclamation.
///
/// Bumped to 13 for FIG-636's factory-wide durable-core catalog. Session heads,
/// metadata, graph rows, and usage deltas are now keyed by `session_id`; node
/// ids remain globally unique across the one database. Pre-13 per-session
/// databases are rejected and must be recreated.
///
/// Bumped to 14 for FIG-654's reachability model. Parent edges, head roots,
/// and cached incoming counts are queryable rows;
/// graph structure no longer lives inside `node_json`.
///
/// Bumped to 15 for FIG-634 first-class forks. `node_anchors` makes explicit
/// continuation pins node and checkpoint roots in the same transaction domain
/// as heads and graph edges.
///
/// Bumped to 16 so an anchor binds the continuation checkpoint and source
/// session as one immutable snapshot rather than selecting either later.
///
/// Bumped to 17 so a reusable session name has a durable per-lifetime
/// incarnation for node and effect-replay identity.
///
/// Bumped to 18 because runtime commit receipts no longer persist the removed
/// realization digest; stores derive their lookup hash from commit content.
///
/// Bumped to 19 to remove cached graph-node reference counts. Node retirement
/// now derives liveness from parent edges, session heads, and anchors.
///
/// Bumped to 20 for permanent session-id tombstones and the removal of
/// per-lifetime incarnation identity. Pre-20 stores are rejected and recreated.
///
/// Bumped to 21 for consumed process-wake source-key evidence that survives
/// queue drain. Pre-21 durable-core catalogs are rejected and recreated.
///
/// Bumped to 22 to replace per-message evidence with monotone consumed
/// high-water marks. Pre-22 durable-core catalogs are rejected and recreated.
///
/// Bumped to 23 for the session-create and process-identity cutover.
///
/// Bumped to 24 to rename consumed wake high-water marks as receiver allocation
/// fences and add durable sender allocation floors. Process-event sequences
/// remain small and monotone across pruned incarnations.
///
/// Bumped to 25 for FIG-850 append-request identity receipts and idempotent
/// usage publication. Receipt identity columns are nullable so a pre-upgrade
/// row copied into the new schema retains exact-commit-hash semantics; usage
/// rows carry a required operation key, ordinal, payload-encoding version, and
/// canonical payload hash unique within a session. This unreleased schema was
/// completed in place; operators still use the store family's reject-and-
/// recreate flow rather than an in-place migration.
/// Version 25 also rejects session and artifact rows carrying pre-FIG-886
/// identities as part of the coordinated cutover.
/// Version 26 rejects pre-FIG-915 usage identities and session rows carrying
/// the former tool-batch or plugin-message names.
/// Version 27 adds the required per-turn budget to session-head configuration,
/// frame policy snapshots, and process execution environment artifacts. Older
/// databases are rejected and recreated; there is no compatibility read path.
/// Version 28 adds immutable graph generations and frame pointers plus
/// zero-copy fork-lineage accelerators. Older databases are rejected and
/// recreated; there is no backfill or compatibility read path.
/// Version 29 replaces the fixed checkpoint slots with a complete keyed
/// component descriptor set carrying per-component encoding versions. Older
/// roots have no honest compatibility interpretation and are rejected with the
/// existing recreate-store remedy.
/// Version 30 removes the CLI-era session name, creation timestamp, model, and
/// working-directory columns from session metadata. Older databases are
/// rejected and recreated; there is no compatibility read path.
/// Version 32 makes nested session metadata strict.
/// Version 33 replaces that JSON carrier with structural columns and narrow
/// ordered child tables. Older databases are rejected and recreated; there is
/// no JSON or compatibility read path.
/// Version 35 adds queued-work batch identity and coalescing metadata.
/// Version 36 adds the runtime-minted executor discriminator and store-authored
/// lease term to session lease rows.
/// Version 37 adds the attachment GC fence's per-digest condemnation table.
/// Older databases are rejected and recreated; there is no compatibility read
/// path.
/// Version 38 projects checkpoint-manifest component edges into an indexed
/// relation so owner-delete reclaim can decide blob liveness inside the
/// severing transaction. Version-37 catalogs are armed in place by decoding
/// every manifest reachable from a session head or node anchor and inserting
/// its exact component edges in the same transaction that stamps version 38.
/// Catalogs below 37 remain reject-and-recreate boundaries.
/// Version 39 adds core-owned creation and last-commit timestamps to session
/// catalog rows and preserves their enumeration projection on permanent
/// deletion tombstones. Older stores cannot reconstruct an honest creation
/// time and are rejected under the existing recreate-store policy.
///
/// An index-only catalog change does **not** bump this version. Every
/// `CREATE INDEX` above is `IF NOT EXISTS`, obsolete indexes are dropped by
/// name, and open always runs the whole schema. A same-version file self-heals
/// into the current index set on first open, and an older binary can still read
/// the newer file. Bumping would reject-and-recreate live stores for a change
/// that can be applied in place. The idle-arbitration ordering index
/// (`idx_queued_work_session_command_order`) was added under exactly this carve-out. It
/// covers index-only additions and nothing else: any table, column, or
/// semantic change bumps.
/// Version 40 persists per-turn cancellation requests and their undelivered
/// input outcomes.
/// Version 41 adds the nullable independently readable session-state generation
/// beside durable session binding metadata. NULL is the version-zero legacy map.
/// Version 42 removes the graph-node sequence column. Per-session generation is
/// the sole durable graph ordering authority.
/// Version 43 makes runtime append receipt identity columns all-or-none and
/// removes the readerless requested-ancestor receipt column. Older stores are
/// rejected and recreated; there is no compatibility read or migration path.
/// Version 44 folds the two pending observer-intent encodings into one
/// attributed table and removes the relation-wrapper depth counter. Version-43
/// catalogs are rejected and recreated like every other predecessor: the
/// in-place fold was deleted under the store-version window.
/// Version 45 switches content and semantic identities to domain-tagged BLAKE3.
/// Existing stores are rejected rather than reinterpreting SHA-256 rows.
/// Version 46 adds DDL-enforced session relation, causal-reference, and observer-
/// inheritance vocabularies. Existing durable-core catalogs are rejected rather
/// than migrated.
/// Version 47 makes session-execution-lease identity all-or-none and removes the
/// unused owner-liveness column. Existing catalogs are rejected rather than
/// migrated.
/// Version 48 constrains queued-work vocabulary and claim correlation while
/// removing its unread owner columns. Existing catalogs are rejected rather
/// than migrated.
/// Version 49 constrains pending-turn-input state and scope correlation while
/// removing the unread claim-owner-liveness column. SQL CHECK NULL semantics
/// let ingress JSON without a `scope` key pass both checks; serde cannot emit
/// that shape, so the behavior is identical across backends. Existing
/// catalogs are rejected rather than migrated.
/// Version 50 stores checked `FrameKey` values in every frame-open node. Existing
/// catalogs contain raw initial-frame keys and are rejected rather than decoded
/// through a legacy path.
/// Version 51 admits semantic-boundary receipt identities (FIG-2480): the
/// runtime-turn-commit identity CHECK now accepts a populated hash and version
/// with a NULL requested-node count. Existing catalogs are rejected rather
/// than migrated.
/// ADR 0078 replaces plugin snapshots with mediated namespace state; older
/// catalogs are refused before any prior payload can be read.
/// Version 53 persists each usage delta's typed disposition
/// (`usage_deltas.usage_disposition_json`, FIG-2765). Version 52 rows carry no
/// disposition at all and their unreported holes cannot be reconstructed, so
/// existing catalogs are rejected rather than migrated with a defaulted column.
/// Version 54 preserves successful attachment deletion as the terminal
/// `reclaimed` phase so adoption can refuse roots whose bytes are absent.
/// Version 55 keeps that phase present under an opaque write token associated
/// with its manifest session until a restoring backend put succeeds, so failed
/// re-puts and explicit host recovery can restore it exactly.
/// Version 56 persists full effect addresses in session causal metadata.
/// Version 57 also requires pending-input claim identity and token to be either
/// both NULL or both populated; both version-56 parent catalogs are recreated.
/// Version 58 adds exact owner edges and permanent execution-owner publication
/// fences. Version-57 catalogs are rejected and recreated.
/// Version 59 qualifies process-owned attachment intents with the registry-minted
/// incarnation. Version-58 catalogs are rejected so a bare process id is never
/// reinterpreted as the current incarnation with the same reusable name.
/// Bumped to 61 for FIG-2795: attachment adoption requires upload evidence.
/// `attachment_manifest` gains `write_id` and `written_at_ms`, and the
/// `attachment_condemnations` phase vocabulary drops `reclaimed` — a pre-61
/// database can hold rows in a phase this schema forbids and manifest rows with
/// no upload evidence for bytes that are present, so it is rejected at open and
/// recreated.
/// Bumped to 62 for FIG-2962/FIG-2963: the parent scope is a registration fact
/// and the end of a scope is one ledger row. `processes` gains
/// `parent_scope_kind`, `parent_scope_id`, `on_parent_end` and
/// a cancel-request column, and `process_parent_end_plans` is replaced by the
/// scope-keyed `parent_end_plans`. A pre-62 catalog holds children with no
/// parent scope and plans keyed by a process id, so it is rejected at open and
/// recreated.
/// Bumped to 63 for FIG-2965: `processes` carries the cancel request as
/// `cancel_requested_at_ms` instead of a boolean, and gains the partial index a
/// pending-cancel list reads. A pre-63 database has the boolean column, so it
/// is rejected at open and recreated.
/// Bumped to 65 for FIG-2995's named process-definition registry: a single
/// new table, the one durable home for a registered definition record. A
/// pre-65 database holds no registry rows, so the whole catalog is recreated
/// under the reject-and-recreate policy rather than migrated midwifing a
/// registry into a database that never had one.
/// Bumped to 66 for FIG-3092's release stamp: `release_stamp` records the lash
/// release, the schema-version tuple and the instant that release first wrote
/// this store, so a host can read which build produced the data before wiring
/// a runtime. A pre-66 database has no such table and, under the
/// reject-and-recreate policy, is refused at open rather than midwifed one.
/// Bumped to 67 for FIG-2885: `session_meta` gains the two family CHECKs that
/// tie `relation_kind` and `caused_by_kind` to exactly their payload columns,
/// so a mispaired discriminator is refused at write rather than silently
/// dropped at decode. `caused_by_process_event_sequence` and
/// `caused_by_subscription_revision` stay TEXT on purpose: both carry a u64
/// `CausalRef` field whose full range exceeds SQLite's signed INTEGER, and the
/// cross-backend differential round-trips u64::MAX through them. A pre-67
/// database lacks the family guards, so it is rejected at open and recreated.
/// Bumped to 68 for FIG-3260: the await-event tables moved out of this string
/// into a shared fragment so the declaration existed once for both carrying
/// databases. The applied DDL is statement-identical, but the guarded
/// `SCHEMA` text changed, so a pre-68 database is rejected at open and
/// recreated like any other schema change.
/// Bumped to 69 for FIG-3261: every formerly-anonymous CHECK gained a
/// `ck_<table>_<concern>` name so the required-constraints gate can see it.
/// Constraint names change the stored DDL text, so a pre-69 database is
/// rejected at open and recreated.
/// Version 70 widens `ck_pending_turn_inputs_claim_identity_all_or_none` to
/// the whole four-column claim identity (FIG-3262): a claim id/token pair
/// with no owner was representable. A pre-70 database is rejected at open and
/// recreated.
/// Version 71 removes the redundant payload-family kind from stored blob
/// envelopes (FIG-1949 layer 2). The durable-core version guards these bytes
/// as well as the DDL. Pre-71 catalogs are rejected at open and recreated;
/// there is no envelope migration or legacy decode path.
// Generation 72 cuts over to ordered plugin parts and the standard-compaction identity.
// Pre-cutover durable-core catalogs are rejected and recreated.
/// Bumped to 74 for FIG-1949 layer 2: the stored artifact-blob envelope now
/// actually drops its `descriptor` field — the pointer table's namespace key
/// is the sole owner of the payload-family fact. A pre-74 database holds
/// envelopes that still carry the field, so it is rejected at open and
/// recreated rather than decoded under the new shape.
/// Version 75 adds durable queued-run admissions and normalized membership.
/// Durable-core 74 catalogs require recreation.
/// Bumped to 76 for FIG-3537: `runtime_turn_commits.result_json` now carries
/// RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION and every receipt read fails closed
/// on a missing, invalid, or unsupported version instead of skipping the
/// row. Pre-versioned receipts are refused; a pre-76 database is rejected at
/// open and recreated.
/// Bumped to 77 for FIG-3544: `pending_turn_inputs` gains the immutable
/// `submitted_ingress_json` and `submission_digest` columns written once at
/// admission, and source-key replay compares the digest instead of the row's
/// mutable current ingress. The digest is computed in Rust from the submitted
/// payload, so no DDL can backfill it; a pre-77 database is rejected at open
/// and recreated.
/// Bumped to 78 for FIG-3578: the catalog gains `attachment_blobs`, the bytes
/// of `SqliteAttachmentStore`, so a SQLite deployment supplies its attachment
/// port from the same database as the manifest that roots them. A pre-78
/// database has no such table and, under the reject-and-recreate policy, is
/// refused at open rather than midwifed one.
/// Bumped to 79 for FIG-3532: the durable `RuntimeErrorCode` and `TurnOutcome`
/// vocabularies queued-run terminals persist change (`turn_input_redrive_set_unavailable`
/// removed, `accepted_turn_input_ceded` and `TurnOutcome::Queued` added). A
/// pre-79 database is rejected at open and recreated.
/// Bumped to 80 for FIG-3589: `pending_turn_inputs` gains
/// `claim_bound_turn_id` and `claim_bound_receipt_input_id`, the aborted direct
/// turn a row's claim is bound to and the input its receipt names, with
/// `ck_pending_turn_inputs_bound_claim_is_next_turn` holding the pair
/// all-or-none and a binding to an open next-turn claim. A pre-80 database is rejected at open and
/// recreated.
/// Bumped to 81 for FIG-3586: the catalog gains `turn_parks`, the typed
/// parked state of a driver-run turn that `drain_status` counts, and the
/// durable `RuntimeErrorCode` vocabulary gains
/// `lashlang_cell_replay_divergence`, `lashlang_cell_replay_key_format_cutover`
/// and `recorded_journal_read_unsupported`. A pre-81 database is rejected at
/// open and recreated.
/// Bumped to 82 for FIG-3598: the durable `RuntimeErrorCode` vocabulary gains
/// `restate_effect_group_protocol_retired`. No relation changes; a pre-82
/// database is rejected at open and recreated.
/// Bumped to 83 for FIG-3587: the durable `RuntimeErrorCode` vocabulary gains
/// `lashlang_cell_binding_drift`, the replay-mismatch report gains
/// `effect_kind`, and a `turn_parks` reason may be `binding_drift` or
/// `effect_replay_divergence`, which a pre-83 build cannot decode. A pre-83
/// database is rejected at open and recreated.
/// Bumped to 84: the durable `RuntimeErrorCode` vocabulary replaces
/// `worker_replacement_abort` with the engine-neutral, parking
/// `effect_replay_divergence`, and the retired code is not aliased. No
/// relation changes; a pre-84 database is rejected at open and recreated.
/// Bumped to 86 for FIG-3540 (S3): the catalog gains `session_ingress`, the
/// one session ingress of ADR 0101: one row per admitted item, one per-session
/// order under the database write lock, two class-level lanes. Its
/// `delivery_*` columns hold the submitted delivery, written once and never
/// rewritten, and `submission_digest` likewise; a claim's columns are set
/// exactly on an `accepted` row, and a tombstone carries its closed cause and
/// no claim. Partial indexes keep tombstones off the claim path. A pre-86
/// database has no such table and is rejected at open and recreated. The
/// number is provisional: the ingress store merges with the FIG-3540
/// cutover, which takes the next free version at its merge.
/// Bumped to 87 for FIG-3659: `turn_parks` reshapes into the enriched parked
/// record — `park_id`, `reason_code`, `since_ms`, `last_refused_ms` and
/// `attempts` — and the catalog gains `turn_park_clock`, the feed's sequence
/// row, and `turn_park_events`, the durable ledger of park transitions. A
/// pre-87 database is rejected at open and recreated.
/// Bumped to 88 for FIG-3585: the durable `RuntimeErrorCode` vocabulary drops
/// `runtime_perf_start_gate_retry` and `tool_completion_key_process_lifetime`,
/// and the durable core no longer carries the await-event tables that
/// store-delegated turn control used (the effect-replay database keeps its
/// own). A pre-88 database is rejected at open and recreated; it is not
/// migrated.
/// Bumped to 89 for FIG-3682: `session_meta` gains
/// `admission_base_checkpoint_ref`, the checkpoint of the head the session's
/// latest turn was admitted on. Maintenance keeps it as a checkpoint root, so
/// a replay of that turn can rebuild its input state from the head it was
/// admitted on after its own commit superseded the head. A pre-89 database is
/// rejected at open and recreated; it is not migrated.
/// Bumped to 90 for FIG-3735: a `turn_parks` reason may be
/// `session_state_generation_refused`, the park of an in-flight turn whose
/// redrive the session-state generation gate refused, which a pre-90 build
/// cannot decode. No relation changes; a pre-90 database is rejected at open
/// and recreated; it is not migrated.
/// Bumped to 91 for FIG-3632: `queued_work_batches.enqueue_seq`,
/// `pending_turn_inputs.enqueue_seq` and `usage_deltas.seq` are now
/// `INTEGER PRIMARY KEY AUTOINCREMENT`, so a delete can never let SQLite
/// reissue the freed maximum rowid the way `session_ingress` already could
/// not. A pre-91 database still declares the reusable rowid columns and is
/// rejected at open and recreated; it is not migrated.
/// Bumped to 92 for FIG-3667: the `postgres_effect_replay_*`,
/// `postgres_await_event_*` and `postgres_effect_journal_retirement` codes
/// leave the durable runtime-error vocabulary. No relation changes; a pre-92
/// database is rejected at open and recreated; it is not migrated.
/// Bumped to 93 for FIG-3571: a turn's admission records the executable
/// generation it runs under (the queued-run admission gained `generation`),
/// a redrive under another one parks with the `retired_generation` reason
/// (replacing `key_format_cutover`, and the durable `RuntimeErrorCode`
/// `lashlang_cell_replay_key_format_cutover` becomes `retired_generation`),
/// and `turn_parks` gains the projected, indexed `park_executable_generation` column the
/// drain counts retired parks by. A pre-93 database is rejected at open and
/// recreated; it is not migrated.
/// Bumped to 94 for FIG-3542: `session_head` gains `pending_follow_on_json`,
/// the follow-on turn a committed agent-frame switch owes the session (ADR
/// 0101 §3), and a frame handoff is no longer a queued-work row: the
/// `agent_frame_task` payload is gone and `runtime_turn_commits.result_json`
/// carries receipt schema 2. A pre-94 database is rejected at open and
/// recreated; it is not migrated.
/// Bumped to 95 for FIG-3796: the catalog gains `fleet_format`, the
/// deployment's own fleet-format row of ADR 0106 §1, recording the
/// durable-format generation every writer in the fleet emits. A pre-95
/// database has no such table and, under the reject-and-recreate policy, is
/// refused at open rather than midwifed one.
/// Bumped to 96 for FIG-3814: the `engine_effect_group_protocol_retired` code
/// leaves the durable runtime-error vocabulary for
/// `engine_object_state_format_unsupported`, with the effect-group protocol's
/// exact-version refusal. No relation changes; a pre-96 database is rejected at
/// open and recreated; it is not migrated.
/// Bumped to 97 for FIG-3815: `session_meta` gains `drive_root_start`, the
/// start marker of the execution of an admitted root that sealed the
/// session's current admission (ADR 0105 L-S8); a later seal of the same
/// admission by another execution is refused. A pre-97 database is rejected
/// at open and recreated; it is not migrated.
/// Bumped to 98 for FIG-3607: a process is named by its minted, never-reused
/// process id, so `attachment_manifest` drops `owner_incarnation` and
/// `session_meta_pending_observer_intents` drops `process_incarnation`, and
/// the durable `RuntimeErrorCode` vocabulary drops
/// `process_incarnation_superseded`. A pre-98 database is rejected at open
/// and recreated; it is not migrated.
/// Bumped to 99 for FIG-3600 S7: the logical-root family. `session_roots`
/// holds each admitted root and its terminal evidence, `session_root_inputs`
/// binds an accepted input to its root, `control_intents` records operator
/// verbs and session closes, `session_meta` gains `closing_intent`, a turn
/// park gains `engine_ref` and `resume_intent`, and a park event may be
/// `redrive_requested`. A pre-99 database is rejected at open and recreated;
/// it is not migrated.
/// Version 99 also lets a parked turn record the drain generation of the
/// build whose checkpoint it resumes (FIG-3795, changed in place under the
/// pre-1.0 version freeze, FIG-3846): `turn_parks` and `turn_park_events`
/// gain the projected `park_build_generation` column, and `turn_parks` the
/// partial index drain status counts it by. `session_roots` records each
/// root's admission (`admission_json`) and the drain generation of the drive
/// that admitted it (`admitted_generation`, indexed for the drain's in-flight
/// count per generation, FIG-3795 S9), with at most one unfinished root per
/// session; the queued-run ledger is gone and a queued-work head is admitted
/// as an ordinary root (FIG-3927). A database written before these changes
/// has the old shape; recreate it.
/// Version 99 also lets tool-intent submissions record process-definition
/// and trigger registration (FIG-4057, changed in place under the version
/// freeze): a catalog whose kind CHECK predates them rejects both kinds, so
/// recreate it.
const BASE_SCHEMA_VERSION: i32 = 99;
#[cfg(not(feature = "synthetic-next"))]
pub(crate) const SCHEMA_VERSION: i32 = BASE_SCHEMA_VERSION;
#[cfg(feature = "synthetic-next")]
pub(crate) const SCHEMA_VERSION: i32 = BASE_SCHEMA_VERSION + 1;

pub(crate) const PROCESS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lash_compat (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    component TEXT NOT NULL,
    version INTEGER NOT NULL,
    min_reader INTEGER NOT NULL,
    fleet_format INTEGER NOT NULL,
    CHECK (version >= 1 AND min_reader >= 1 AND min_reader <= version AND fleet_format >= 1)
);

CREATE TABLE IF NOT EXISTS processes (
    process_id            TEXT PRIMARY KEY,
    start_key             TEXT,
    originator_id         TEXT NOT NULL,
    wake_session_id       TEXT,
    identity_kind         TEXT NOT NULL,
    identity_label        TEXT,
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL,
    last_event_sequence   INTEGER NOT NULL,
    change_seq            INTEGER NOT NULL,
    status                TEXT NOT NULL,
    lifetime              TEXT NOT NULL,
    lifetime_scope_kind   TEXT,
    lifetime_scope_id     TEXT,
    cancel_requested_at_ms INTEGER,
    parked_since_ms       INTEGER,
    parked_reason_code    TEXT,
    park_executable_generation TEXT,
    park_build_generation TEXT,
    segment_generation    TEXT,
    record_json           TEXT NOT NULL,
    start_obligation_id         TEXT,
    start_obligation_state      TEXT,
    start_obligation_attempts   INTEGER NOT NULL DEFAULT 0,
    start_obligation_due_at_ms  INTEGER,
    start_obligation_claim_token TEXT,
    start_obligation_stall_reason TEXT,
    start_obligation_last_error TEXT,
    start_obligation_settled_at_ms INTEGER,
    obligation_id         TEXT,
    obligation_state      TEXT,
    obligation_attempts   INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms  INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    consumer_hold_key     TEXT,
    consumer_hold_scope_kind TEXT,
    consumer_hold_scope_id TEXT,
    consumer_hold_cancels INTEGER,
    CONSTRAINT ck_processes_consumer_hold CHECK ((consumer_hold_key IS NULL) = (consumer_hold_scope_kind IS NULL) AND (consumer_hold_key IS NULL) = (consumer_hold_scope_id IS NULL)),
    CONSTRAINT ck_processes_start_obligation CHECK (((start_obligation_state IS NULL AND start_obligation_id IS NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'due' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NOT NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'claimed' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NOT NULL AND start_obligation_claim_token IS NOT NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'delivered' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NOT NULL) OR (start_obligation_state = 'stalled' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND start_obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_processes_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_processes_parked CHECK ((parked_since_ms IS NULL) = (parked_reason_code IS NULL)),
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')),
    CONSTRAINT ck_processes_lifetime CHECK (lifetime IN ('until', 'detached')),
    CONSTRAINT ck_processes_lifetime_scope CHECK ((lifetime = 'detached' AND lifetime_scope_kind IS NULL AND lifetime_scope_id IS NULL) OR (lifetime = 'until' AND lifetime_scope_kind IN ('turn', 'queue_drain', 'process', 'session') AND lifetime_scope_id IS NOT NULL))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_processes_obligation_id
    ON processes(obligation_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_processes_start_obligation_id
    ON processes(start_obligation_id);
CREATE INDEX IF NOT EXISTS idx_processes_start_obligation_due
    ON processes(start_obligation_due_at_ms, start_obligation_id)
    WHERE start_obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_processes_start_obligation_stalled
    ON processes(start_obligation_id)
    WHERE start_obligation_state = 'stalled';
CREATE INDEX IF NOT EXISTS idx_processes_obligation_due
    ON processes(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_processes_obligation_stalled
    ON processes(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE INDEX IF NOT EXISTS idx_processes_status
    ON processes(status);
-- A start key maps to the one retained process minted for it (ADR 0107).
CREATE UNIQUE INDEX IF NOT EXISTS idx_processes_start_key
    ON processes(start_key) WHERE start_key IS NOT NULL;
-- A held row names the scope whose close releases it (ADR 0116 §3.6).
CREATE INDEX IF NOT EXISTS idx_processes_consumer_hold_owner
    ON processes(consumer_hold_scope_kind, consumer_hold_scope_id)
    WHERE consumer_hold_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_processes_non_terminal
    ON processes(process_id) WHERE status IN ('running', 'waiting');

CREATE INDEX IF NOT EXISTS idx_processes_change_seq
    ON processes(change_seq);
CREATE INDEX IF NOT EXISTS idx_processes_originator
    ON processes(originator_id);
CREATE INDEX IF NOT EXISTS idx_processes_identity
    ON processes(identity_kind, identity_label);
CREATE INDEX IF NOT EXISTS idx_processes_created
    ON processes(created_at_ms);
CREATE INDEX IF NOT EXISTS idx_processes_recent_retired
    ON processes(updated_at_ms, process_id)
    WHERE status NOT IN ('running', 'waiting');
CREATE INDEX IF NOT EXISTS idx_processes_wake_session
    ON processes(wake_session_id);
-- The pending-cancel sweep's scan: rows whose cancel request is older than a
-- horizon and whose outcome is still open. The predicate is the negation of
-- the terminal statuses, so `caller_departed` is in: nothing may ever
-- terminalize such a row, so a cancel request on it stays unanswered forever
-- and is exactly what an operator asks this index for. It must stay
-- byte-identical to the generated nonterminal predicate, or SQLite plans
-- the query without it.
CREATE INDEX IF NOT EXISTS idx_processes_pending_cancel
    ON processes(cancel_requested_at_ms, process_id)
    WHERE cancel_requested_at_ms IS NOT NULL
      AND status NOT IN ('completed', 'failed', 'cancelled', 'abandoned');
-- The scope-close sweep's only scan: processes living `Until` one closed
-- scope that still owe a cancel. The predicate names the live statuses rather than a NOT
-- IN so a status added later cannot silently widen the index; it is exactly
-- `LIVE_PROCESS_STATUS_LABELS`, so `caller_departed` is out for the reason it
-- is out of every other worklist - lash may never act on such a row nor
-- assert an outcome for it, and a cancel request is both.
CREATE INDEX IF NOT EXISTS idx_processes_lifetime_scope
    ON processes(lifetime_scope_kind, lifetime_scope_id, process_id);
CREATE INDEX IF NOT EXISTS idx_processes_lifetime_pending
    ON processes(lifetime_scope_kind, lifetime_scope_id, process_id)
    WHERE lifetime = 'until'
      AND cancel_requested_at_ms IS NULL
      AND status IN ('running', 'waiting');

-- The parked projection (FIG-3659 NOW-B): the parked-process list and the
-- park summary read only parked rows, in `(since, process)` keyset order.
CREATE INDEX IF NOT EXISTS idx_processes_parked
    ON processes(parked_since_ms, process_id)
    WHERE parked_since_ms IS NOT NULL;
-- The retired generation a `retired_generation` park names (FIG-3571): the
-- drain counts retired process parks per executable generation off it.
CREATE INDEX IF NOT EXISTS idx_processes_park_executable_generation
    ON processes(park_executable_generation) WHERE park_executable_generation IS NOT NULL;
-- The build generation of the parked checkpoint a park resumes (FIG-3795):
-- drain status counts retired parks by it.
CREATE INDEX IF NOT EXISTS idx_processes_park_build_generation
    ON processes(park_build_generation) WHERE park_build_generation IS NOT NULL;
-- The build generation that admitted each live process's current segment
-- (FIG-3795 S2): the drain routes a refused redrive to the build that wrote
-- the segment's journal. Partial: a terminal segment's writer is no route,
-- and a NULL stamp is no lookup key.
CREATE INDEX IF NOT EXISTS idx_processes_live_generation
    ON processes(segment_generation) WHERE status IN ('running', 'waiting') AND segment_generation IS NOT NULL;

CREATE TABLE IF NOT EXISTS process_park_clock (
    singleton           INTEGER PRIMARY KEY CONSTRAINT ck_process_park_clock_singleton CHECK (singleton = 1),
    current_seq         INTEGER NOT NULL DEFAULT 0,
    compaction_horizon  INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO process_park_clock (
    singleton, current_seq, compaction_horizon
) VALUES (1, 0, 0);

CREATE TABLE IF NOT EXISTS process_park_events (
    seq         INTEGER PRIMARY KEY,
    process_id  TEXT NOT NULL,
    park_id     INTEGER NOT NULL,
    kind        TEXT NOT NULL CONSTRAINT ck_process_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled')),
    cause       TEXT,
    reason_json TEXT,
    at_ms       INTEGER NOT NULL,
    park_build_generation TEXT,
    CONSTRAINT ck_process_park_events_parked_reason CHECK ((kind = 'parked' AND reason_json IS NOT NULL AND cause IS NULL) OR (kind <> 'parked' AND reason_json IS NULL AND cause IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS process_change_clock (
    singleton    INTEGER PRIMARY KEY CONSTRAINT ck_process_change_clock_singleton CHECK (singleton = 1),
    current_seq  INTEGER NOT NULL DEFAULT 0,
    tombstone_compaction_horizon INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO process_change_clock (
    singleton, current_seq, tombstone_compaction_horizon
) VALUES (1, 0, 0);

CREATE TABLE IF NOT EXISTS process_events (
    process_id        TEXT NOT NULL,
    sequence          INTEGER NOT NULL,
    event_type        TEXT NOT NULL,
    idempotency_key   TEXT,
    event_json        TEXT NOT NULL,
    PRIMARY KEY (process_id, sequence),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_process_events_key
    ON process_events(process_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS wake_allocation_floors (
    target_session_id TEXT NOT NULL,
    process_id        TEXT NOT NULL,
    allocation_floor INTEGER NOT NULL,
    PRIMARY KEY (target_session_id, process_id)
);

CREATE TABLE IF NOT EXISTS process_wake_deliveries (
    delivery_id       TEXT PRIMARY KEY,
    process_id        TEXT NOT NULL,
    target_session_id TEXT NOT NULL,
    sequence          INTEGER NOT NULL,
    state             TEXT NOT NULL,
    claim_token       TEXT,
    attempts          INTEGER NOT NULL DEFAULT 0,
    first_attempt_ms  INTEGER,
    next_attempt_at_ms INTEGER NOT NULL,
    expires_at_ms     INTEGER NOT NULL,
    discard_reason    TEXT,
    delivery_json     TEXT NOT NULL,
    CONSTRAINT ck_process_wake_deliveries_state CHECK (state IN ('pending', 'enqueuing', 'enqueued', 'discarded')),
    CONSTRAINT ck_process_wake_deliveries_discard_reason CHECK (discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound')),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_wake_deliveries_pending
    ON process_wake_deliveries(next_attempt_at_ms, target_session_id, process_id, sequence)
    WHERE state IN ('pending', 'enqueuing');
CREATE INDEX IF NOT EXISTS idx_wake_deliveries_group_sequence
    ON process_wake_deliveries(target_session_id, process_id, sequence)
    WHERE state <> 'enqueued';

CREATE TABLE IF NOT EXISTS process_observers (
    session_id       TEXT NOT NULL,
    process_id       TEXT NOT NULL,
    PRIMARY KEY (session_id, process_id),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_process_observers_process
    ON process_observers(process_id, session_id);

CREATE TABLE IF NOT EXISTS process_tombstones (
    process_id          TEXT PRIMARY KEY,
    terminal_label      TEXT NOT NULL,
    pruned_at_ms        INTEGER NOT NULL,
    pruned_change_seq   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_process_tombstones_change
    ON process_tombstones(pruned_change_seq);

-- The referrer-kind CHECK admits any non-empty label, as the durable core's
-- does: the vocabulary is typed in ArtifactReferrer::decode (ADR 0115
-- section 5), so a later build's kind needs no table rebuild.
CREATE TABLE IF NOT EXISTS artifact_cleanup_obligations (
    referrer_kind TEXT NOT NULL CHECK (length(referrer_kind) > 0),
    referrer_id TEXT NOT NULL CHECK (length(referrer_id) > 0),
    cleanup_json TEXT NOT NULL,
    obligation_id TEXT NOT NULL,
    obligation_state TEXT NOT NULL,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    PRIMARY KEY (referrer_kind, referrer_id),
    CONSTRAINT ck_artifact_cleanup_obligations_obligation CHECK (((obligation_state = 'due' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'stalled' AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_cleanup_obligations_id
    ON artifact_cleanup_obligations(obligation_id);
CREATE INDEX IF NOT EXISTS idx_artifact_cleanup_obligations_due
    ON artifact_cleanup_obligations(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_artifact_cleanup_obligations_stalled
    ON artifact_cleanup_obligations(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE TABLE IF NOT EXISTS process_segment_handovers (
    process_id       TEXT NOT NULL,
    segment_ordinal  INTEGER NOT NULL,
    handover_json    TEXT NOT NULL,
    started_json     TEXT,
    written_generation TEXT,
    route            TEXT NOT NULL,
    PRIMARY KEY (process_id, segment_ordinal),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);
-- The route a retained handover's successor was sent under (FIG-3795 S3):
-- drain re-routing finds every successor addressed to a retired deployment.
CREATE INDEX IF NOT EXISTS idx_process_segment_handovers_route
    ON process_segment_handovers(route);

-- One row per ended parent scope, keyed by the scope itself rather than by a
-- process row: a turn-scoped parent has no process row at all, and a
-- process-scoped parent's row may be pruned before its children settle.
CREATE TABLE IF NOT EXISTS parent_end_plans (
    parent_kind      TEXT NOT NULL,
    parent_id        TEXT NOT NULL,
    parent_payload   TEXT NOT NULL,
    ended_at_ms      INTEGER NOT NULL,
    settled_at_ms    INTEGER,
    obligation_id    TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_parent_end_plans_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    PRIMARY KEY (parent_kind, parent_id),
    CONSTRAINT ck_parent_end_plans_kind CHECK (parent_kind IN ('turn', 'queue_drain', 'process', 'session'))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_parent_end_plans_obligation_id
    ON parent_end_plans(obligation_id);
CREATE INDEX IF NOT EXISTS idx_parent_end_plans_obligation_due
    ON parent_end_plans(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_parent_end_plans_obligation_stalled
    ON parent_end_plans(obligation_id)
    WHERE obligation_state = 'stalled';

-- The consumer holds whose call was abandoned before it consumed its child
-- (ADR 0116 §3.4): a registration under a marked key is refused, and the
-- owning scope's close forgets the marks.
CREATE TABLE IF NOT EXISTS abandoned_consumer_holds (
    hold_key         TEXT PRIMARY KEY,
    owner_scope_kind TEXT NOT NULL,
    owner_scope_id   TEXT NOT NULL,
    abandoned_at_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_abandoned_consumer_holds_owner
    ON abandoned_consumer_holds(owner_scope_kind, owner_scope_id);

CREATE TABLE IF NOT EXISTS tool_intent_submissions (
    replay_key          TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL,
    execution_scope_id  TEXT NOT NULL,
    tool_call_id        TEXT NOT NULL,
    intent_index        INTEGER NOT NULL,
    kind                TEXT NOT NULL,
    payload_hash        TEXT NOT NULL,
    submission_json     TEXT NOT NULL,
    CONSTRAINT ck_tool_intent_submissions_kind CHECK (kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger', 'register_process_definition', 'register_trigger'))
);
CREATE INDEX IF NOT EXISTS idx_tool_intent_submissions_scope
    ON tool_intent_submissions(session_id, execution_scope_id, intent_index);

-- The build generations an operator marked draining (FIG-3799): the recovery
-- leader wakes every live process whose current segment a marked generation
-- admitted, so each hands its open wait to a successor on the newest build.
CREATE TABLE IF NOT EXISTS draining_generations (
    generation   TEXT PRIMARY KEY,
    marked_at_ms INTEGER NOT NULL
);

";

// Bumped to 10: ADR 0020 added a per-store process-row `change_seq` plus the
// process change clock. There is no migration chain — pre-10 process databases
// are rejected at open and must be recreated.
//
// Bumped to 11 for the completion-authority cutover (ADR 0027): terminal
// `process_events` now carry a `completion_authority` in their payload, so the
// replay-key payload hash of a pre-cutover terminal event no longer matches the
// hash a cross-version retry would compute — a replay would spuriously diverge.
// Rejecting pre-11 process databases and recreating them removes that hazard.
//
// Bumped to 13 for the second completion-authority payload cutover (ADR 0027):
// `ExternalOwner` no longer carries the unverified `granted_to` field, changing
// the replay-key payload hash again. Pre-13 process databases are rejected and
// recreated so retries cannot compare terminal events across payload formats.
//
// Bumped to 14 for the durable process-wake outbox and removal of the wake-ack
// lane. Pre-14 process registries are rejected and recreated.
//
// Bumped to 15 so terminal wake deliveries retain a durable exact-evidence
// cleanup reconciliation bit. Pre-15 registries are rejected and recreated.
//
// Bumped to 17 for FIG-661: observer edges replace the former visibility table, wake targets
// are indexed subscription state, filter columns are extracted, and pruning
// leaves payload-free tombstones.
/// Bumped to 19 for per-attempt wake-delivery claim tokens.
/// Bumped to 18 for wake-delivery claims and raw session originator ids.
// Version 20 stores separately versioned registration fingerprints and v2
// process-environment content addresses.
// Version 21 stores shared-framing wake identities and compares replayed event
// payloads structurally instead of retaining a payload-hash column.
// Version 22 stores v3 process-environment refs whose content-addressed policy
// payload includes the required per-turn budget.
// Version 23 indexes the bounded non-terminal registry scan by process id.
// Version 24 durably retains pending process-parent teardown beside terminal completion.
// Version 25 switches durable process identities to domain-tagged BLAKE3.
// Version 26 adds DDL-enforced process status, wake state/discard reason, and
// tool-intent kind vocabularies. Existing process registries are rejected.
/// Version 27 removes the unread process-lease owner-liveness column. Existing
/// process registries are rejected rather than migrated.
/// Version 28 stores process-event time only in the event JSON as epoch
/// milliseconds and removes the unread companion column. Existing process
/// registries are rejected rather than migrated.
/// Version 29 makes the registration change sequence the structural process
/// incarnation and carries it through events, observer edges, wake deliveries,
/// and tombstones. Version-28 registries are rejected rather than rebound.
/// Version 30 folds the newest event sequence into every process row and
/// persists the Process Prune horizon established by Tombstone Compaction.
/// Version-29 registries are rejected rather than migrated.
/// Version 31 removes the unread process waiting projection and its index.
/// Version-30 registries are rejected rather than migrated.
/// Version 33 persists full admitted effect addresses and optional truthful
/// attribution in process registration and wake payloads. Older registries are
/// rejected rather than fabricating an execution scope or session owner.
/// Version 34 requires the host-declared lifecycle policy in every process record.
/// Earlier process registries are rejected rather than inventing a policy.
/// Version 35 replaces prose cancellation events with a typed, record-folded fact.
/// Earlier registries are rejected so an accepted cancellation is never lost.
/// Version 36 makes Process Prune retain exact artifact-release evidence until
/// every configured artifact store acknowledges owner severance. Version-35
/// registries are rejected rather than inventing cleanup acknowledgements.
/// Version 37 replaces the process-keyed parent-end plan table with a ledger
/// keyed by the parent scope itself, and folds the parent scope, the
/// on-parent-end policy and the cancel request into indexed process columns so
/// the sweep selects children by index instead of decoding every record.
/// Version-36 registries are rejected rather than migrated.
/// Version 38 replaces the boolean `cancel_requested` column with
/// `cancel_requested_at_ms`, the timestamp of the first accepted cancel, so a
/// pending-cancel list reads one column through one partial index instead of
/// decoding every record. Version-37 registries carry a boolean this schema no
/// longer has, so they are rejected rather than migrated.
/// Version 40 names the formerly-anonymous CHECKs (FIG-3261) so the
/// required-constraints gate can see them; a pre-40 registry is rejected at
/// open and recreated.
/// Version 41 (FIG-3376) moves the durable `SessionCreateRequest` stored in
/// process payloads to the spawn-time plugin-init cutover and drops
/// `usage_source`; a pre-41 registry is rejected at open and recreated.
/// Version 42 (FIG-3418) makes the parent scope a typed fact: `parent_end_plans`
/// gains the versioned `parent_payload` column the ledger decodes instead of
/// parsing its `(parent_kind, parent_id)` key, both kind CHECKs admit the
/// `queue_drain` arm, and `parent_scope_id` becomes a collision-free canonical
/// projection rather than a delimiter-joined rendering. A pre-42 registry
/// holds non-injective ids and payload-less ledger rows, so it is rejected at
/// open and recreated.
/// Version 43 (FIG-3588) gives `process_segment_handovers` the nullable
/// `started_json` start marker a Restate segment's admission writes
/// set-if-absent before its first effect. A pre-43 registry lacks the column,
/// so it is rejected at open and recreated.
/// Version 44 (FIG-3607) names a process by its minted, never-reused process
/// id: `processes` drops `incarnation` and `registration_fingerprint` and gains
/// the nullable `start_key`, unique while retained, and every table keyed by
/// `(process_id, incarnation)` is keyed by `process_id` alone. A pre-44
/// registry holds reusable names, so it is rejected at open and recreated.
/// Version 44 also records what ends a process (FIG-3607 PR-2, changed in
/// place under the pre-1.0 version freeze, FIG-3846): `processes` replaces
/// `parent_scope_kind`/`parent_scope_id`/`on_parent_end` with the recorded
/// `lifetime` (`until` or `detached`) and the scope it names, which may be a
/// session, and `parent_end_plans` admits a session scope. A registry written
/// before the change holds ADR 0094 lifecycle policies this build does not
/// read; recreate it.
///
/// Version 44 also stamps process rows with drain generations (FIG-3795,
/// changed in place under the same freeze): `processes` gains
/// `segment_generation` — the build generation that admitted the process's
/// current segment — and `park_build_generation` — the build generation of
/// the checkpoint a parked process resumes — each indexed; and
/// `process_segment_handovers` gains `written_generation` and `route`.
/// `written_generation` stays nullable for parity with the Postgres store:
/// a missing stamp is never derived, and the two backends must accept the
/// same writes. `route` is non-null — every write names the route its send
/// took. A registry written before the change lacks the columns; recreate
/// it.
///
/// Version 44 also holds the drain marks (FIG-3799, changed in place under
/// the same freeze): `draining_generations` names each build generation an
/// operator marked draining. A registry written before the change lacks the
/// table until it is next opened, which creates it empty.
///
/// Version 44 also carries consumer holds (ADR 0116 §3.6, changed in place
/// under the same freeze): `processes` gains `consumer_hold_key` and the
/// owning scope's `consumer_hold_scope_kind` and `consumer_hold_scope_id`,
/// set together or not at all, and indexed by owner, with
/// `consumer_hold_cancels` saying whether the holding call owes the process a
/// cancel when it is abandoned. A held row is never
/// pruned. A registry written before the change lacks the columns; recreate
/// it. `abandoned_consumer_holds` marks the holds whose call was abandoned,
/// so a registration under one is refused; a registry written before it
/// lacks the table until it is next opened, which creates it empty.
const BASE_PROCESS_SCHEMA_VERSION: i32 = 44;
#[cfg(not(feature = "synthetic-next"))]
pub(crate) const PROCESS_SCHEMA_VERSION: i32 = BASE_PROCESS_SCHEMA_VERSION;
#[cfg(feature = "synthetic-next")]
pub(crate) const PROCESS_SCHEMA_VERSION: i32 = BASE_PROCESS_SCHEMA_VERSION + 1;

// Version 4 stores FIG-915 trigger identities and compares occurrence requests
// structurally instead of retaining a request-hash column. There is
// deliberately no compatibility read path.
// Version 5 stores v3 process-environment refs and the resulting trigger
// definition fingerprints after the required per-turn budget cutover.
// Version 6 durably arms occurrence reclaim eligibility at fan-out terminality.
// Version 7 switches durable trigger identities to domain-tagged BLAKE3.
// Version 8 prevents a tombstoned trigger subscription from remaining enabled.
// Version 9 (FIG-1951) replaces the `enabled`/`tombstoned` boolean pair with
// one `lifecycle` column over `enabled`/`disabled`/`tombstoned` and a
// `deleted_at_ms` column paired to it by CHECK, so the three legal states are
// the only representable ones and the deletion time stops living solely inside
// `record_json`. Existing trigger stores are rejected rather than migrated.
// Version 10 (FIG-1956) gives the mutation-receipt table typed NOT NULL
// `owner_kind`/`owner_id` columns with a named owner-kind CHECK, deleting the
// `_owner_scope_namespace` JSON encoding entirely. Existing trigger stores
// are rejected rather than migrated.
// Version 11 (FIG-3376) moves the `SessionCreateRequest` carried in trigger
// targets to the spawn-time plugin-init cutover and drops `usage_source`;
// existing trigger stores are rejected rather than migrated.
// Version 12 (FIG-3607) makes a delivery's `process_id` its nullable binding:
// the reservation starts unbound, and the minted id of the process its start
// key registered is bound before the delivery is reported. Existing trigger
// stores hold precomputed process names, so they are rejected rather than
// migrated.
// Version 12 also carries the delivery's `TriggerDelivery` obligation
// (FIG-4090, changed in place under the version freeze): the reserving insert
// arms it and the binding delivers it, so a reservation a crash left unbound
// is started by the relay rather than by a re-emit. A trigger store written
// before the change lacks the columns; recreate it.
const BASE_TRIGGER_SCHEMA_VERSION: i32 = 12;
#[cfg(not(feature = "synthetic-next"))]
pub(crate) const TRIGGER_SCHEMA_VERSION: i32 = BASE_TRIGGER_SCHEMA_VERSION;
#[cfg(feature = "synthetic-next")]
pub(crate) const TRIGGER_SCHEMA_VERSION: i32 = BASE_TRIGGER_SCHEMA_VERSION + 1;

pub(crate) async fn apply_pragmas(conn: &SqliteConnection) -> rusqlite::Result<()> {
    // WAL + busy_timeout are already applied in `SqliteConnection::open`. The
    // remaining tuning PRAGMAs match the prior store.
    conn.call(|c| {
        c.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA cache_size = -2000;",
        )?;
        Ok(())
    })
    .await
}

/// Admit the compatibility row, provision an empty database and its stamp, or
/// refuse a populated database that cannot be read by this build. Runs on the
/// connection thread so admission and DDL share one transaction: the
/// connection's installer, which arms its writer fence (ADR 0115 §2.2).
pub(crate) async fn ensure_versioned_schema(
    conn: &SqliteConnection,
    database: SqliteDatabase,
) -> rusqlite::Result<()> {
    ensure_versioned_schema_with_writable(
        conn,
        database,
        lash_core_execution::FleetFormat::writable(),
    )
    .await
}

pub(crate) async fn ensure_versioned_schema_with_writable(
    conn: &SqliteConnection,
    database: SqliteDatabase,
    writable: lash_core_execution::compat::VersionRange,
) -> rusqlite::Result<()> {
    conn.install(database, writable, move |tx| {
        apply_versioned_schema_tx_with_writable(tx, database, writable)
    })
    .await
}

#[cfg(test)]
fn prepare_versioned_schema<'connection>(
    connection: &'connection mut Connection,
    database: SqliteDatabase,
) -> rusqlite::Result<Transaction<'connection>> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    apply_versioned_schema_tx(&tx, database)?;
    Ok(tx)
}

#[cfg(test)]
fn apply_versioned_schema_tx(
    tx: &Transaction<'_>,
    database: SqliteDatabase,
) -> rusqlite::Result<()> {
    apply_versioned_schema_tx_with_writable(
        tx,
        database,
        lash_core_execution::FleetFormat::writable(),
    )
    .map(drop)
}

/// The installer's transaction body; answers the admitted `F`.
fn apply_versioned_schema_tx_with_writable(
    tx: &Transaction<'_>,
    database: SqliteDatabase,
    writable: lash_core_execution::compat::VersionRange,
) -> rusqlite::Result<lash_core_execution::FleetFormat> {
    let apply_schema = |conn: &Transaction<'_>| -> rusqlite::Result<()> {
        conn.execute_batch(database.schema())?;
        for fragment in database.fragments() {
            conn.execute_batch(fragment)?;
        }
        Ok(())
    };
    let (admission, fleet) = crate::compat::admit(tx, database, writable)?;
    match admission {
        lash_core_execution::compat::CompatAdmission::Provision => {
            apply_schema(tx)?;
            crate::compat::provision(tx, database, writable)?;
        }
        lash_core_execution::compat::CompatAdmission::Native => {
            #[cfg(feature = "synthetic-next")]
            {
                let written_version = lash_core_execution::compat::descriptor(database.component())
                    .ok_or_else(|| {
                        crate::compat::malformed(
                            database,
                            "the build has no descriptor for this database",
                        )
                    })?
                    .writes
                    .max();
                tx.execute(
                    "UPDATE lash_compat SET version = ?1 WHERE singleton = 1 AND version < ?1",
                    [i64::from(written_version)],
                )?;
            }
        }
        lash_core_execution::compat::CompatAdmission::Expanded { .. } => {
            crate::compat::verify_tolerant(tx, database)?;
        }
    }
    stamp_deployment_metadata(tx, database)?;
    Ok(fleet)
}

/// Whether this database is the one that carries the deployment's release stamp.
///
/// All three databases have compatibility rows; only the durable core records
/// the release that last wrote the store.
pub(crate) fn deployment_metadata_holder(database: SqliteDatabase) -> bool {
    database == SqliteDatabase::DurableCore
}

fn stamp_deployment_metadata(
    tx: &Transaction<'_>,
    database: SqliteDatabase,
) -> rusqlite::Result<()> {
    if deployment_metadata_holder(database) {
        crate::release_stamp::write(tx)?;
    }
    Ok(())
}

pub(crate) fn has_user_schema_objects(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE name NOT LIKE 'sqlite_%'
           AND type IN ('table', 'index', 'trigger', 'view')",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod compat_tests {
    use super::*;
    use lash_core_execution::compat::CompatRefusal;

    fn provision(connection: &mut Connection, database: SqliteDatabase) {
        connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .and_then(|tx| {
                apply_versioned_schema_tx(&tx, database)?;
                tx.commit()
            })
            .expect("provision SQLite database");
    }

    #[test]
    fn sqlite_opens_each_expanded_database_under_its_floor() {
        for database in SqliteDatabase::ALL {
            let mut connection = Connection::open_in_memory().expect("open database");
            provision(&mut connection, database);
            connection
                .execute_batch(
                    "CREATE TABLE next_release_table (id INTEGER PRIMARY KEY); \
                     ALTER TABLE lash_compat ADD COLUMN next_release_note TEXT; \
                     UPDATE lash_compat SET version = 2, min_reader = 1",
                )
                .expect("expand catalog");
            provision(&mut connection, database);
        }
    }

    /// The open-time migration seeds `F` at the opening build's writable
    /// floor, never its own epoch: a compatibility release (`[1, 2]`)
    /// provisioning a fresh database records 1, a build writing `[2, 3]`
    /// records 2, and a reopen leaves the recorded epoch alone (ADR 0115
    /// §2.1).
    #[test]
    fn sqlite_provisioning_seeds_the_opening_build_s_fleet_floor() {
        use lash_core_execution::compat::VersionRange;
        for database in SqliteDatabase::ALL {
            for writable in [VersionRange::between(1, 2), VersionRange::between(2, 3)] {
                let mut connection = Connection::open_in_memory().expect("open database");
                for _ in 0..2 {
                    connection
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .and_then(|tx| {
                            apply_versioned_schema_tx_with_writable(&tx, database, writable)?;
                            tx.commit()
                        })
                        .expect("open SQLite database");
                    let fleet: u32 = connection
                        .query_row(
                            "SELECT fleet_format FROM lash_compat WHERE singleton = 1",
                            [],
                            |row| row.get(0),
                        )
                        .expect("read the seeded epoch");
                    assert_eq!(
                        fleet,
                        writable.min(),
                        "{} under {writable} seeded F={fleet}",
                        database.name()
                    );
                }
            }
        }
    }

    #[test]
    fn sqlite_refuses_a_raised_floor_typed() {
        for database in SqliteDatabase::ALL {
            let mut connection = Connection::open_in_memory().expect("open database");
            provision(&mut connection, database);
            connection
                .execute("UPDATE lash_compat SET version = 2, min_reader = 2", [])
                .expect("raise floor");
            let error = crate::sqlite_error(
                apply_versioned_schema_tx(
                    &connection
                        .unchecked_transaction()
                        .expect("start transaction"),
                    database,
                )
                .expect_err("old reader must refuse"),
            );
            assert!(matches!(
                error,
                StoreError::Incompatible {
                    refusal: CompatRefusal::ReaderFloorAbove { min_reader: 2, .. }
                }
            ));
        }
    }

    #[test]
    fn sqlite_refuses_a_partially_advanced_set() {
        let root = tempfile::tempdir().expect("database root");
        let location = crate::location::SqliteLocation::File {
            root: root.path().to_path_buf(),
        };
        for database in SqliteDatabase::ALL {
            let mut connection =
                Connection::open(root.path().join(database.file_name())).expect("open database");
            provision(&mut connection, database);
        }
        let connection = Connection::open(root.path().join(SqliteDatabase::Triggers.file_name()))
            .expect("open trigger database");
        connection
            .execute("UPDATE lash_compat SET version = 2", [])
            .expect("advance one database");
        let error =
            crate::sqlite_error(crate::compat::check_set(&location).expect_err("set must refuse"));
        assert!(matches!(
            error,
            StoreError::Incompatible {
                refusal: CompatRefusal::PartiallyAdvanced { .. }
            }
        ));
    }
}

#[cfg(test)]
mod observer_intent_migration_tests {
    use super::*;

    /// The deleted 43-to-44 arm upgraded exactly this catalog in place. Under
    /// the store-version window a component-43 stamp is pre-cutover data: the
    /// same fixture must now be refused at both the retired seam and the
    /// production version, with its bytes untouched.
    #[test]
    fn component_43_durable_core_is_refused_instead_of_migrated() {
        let mut connection = Connection::open_in_memory().expect("open migration fixture");
        prepare_versioned_schema(&mut connection, SqliteDatabase::DurableCore)
            .expect("create current fixture")
            .commit()
            .expect("commit current fixture");
        connection
            .execute_batch(
                "ALTER TABLE session_meta ADD COLUMN observer_intent_depth INTEGER NOT NULL DEFAULT 0;
                 DROP TABLE session_meta_pending_observer_intents;
                 CREATE TABLE session_meta_observer_intent_processes (
                     session_id TEXT NOT NULL,
                     layer_index INTEGER NOT NULL,
                     process_index INTEGER NOT NULL,
                     process_id TEXT NOT NULL,
                     PRIMARY KEY (session_id, layer_index, process_index),
                     FOREIGN KEY (session_id) REFERENCES session_meta(session_id) ON DELETE CASCADE
                 );
                 CREATE TABLE session_meta_fork_pending_observer_processes (
                     session_id TEXT NOT NULL,
                     process_index INTEGER NOT NULL,
                     process_id TEXT NOT NULL,
                     PRIMARY KEY (session_id, process_index),
                     FOREIGN KEY (session_id) REFERENCES session_meta(session_id) ON DELETE CASCADE
                 );
                 INSERT INTO session_meta
                     (session_id, relation_kind, observer_intent_depth)
                     VALUES ('fold-session', 'root', 2);
                 INSERT INTO session_meta_observer_intent_processes VALUES
                     ('fold-session', 0, 0, 'shared-process'),
                     ('fold-session', 1, 0, 'host-only-process');
                 INSERT INTO session_meta_fork_pending_observer_processes VALUES
                     ('fold-session', 0, 'shared-process'),
                     ('fold-session', 1, 'fork-only-process');
                 UPDATE lash_compat SET version = 43, min_reader = 43;",
            )
            .expect("build component-43 observer-intent fixture");

        let production = prepare_versioned_schema(&mut connection, SqliteDatabase::DurableCore)
            .expect_err("a component-43 stamp is refused at the current version");
        let verdict = crate::sqlite_error(production);
        assert!(
            matches!(
                &verdict,
                StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::ReaderFloorAbove {
                        found: 43,
                        min_reader: 43,
                        ..
                    }
                }
            ),
            "open must refuse a pre-cutover stamp: {verdict}"
        );

        assert_eq!(
            connection
                .query_row("SELECT version FROM lash_compat", [], |row| row
                    .get::<_, i32>(0))
                .expect("read refused version"),
            43,
            "a refused open must not relabel the old catalog"
        );
        let legacy_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM session_meta_observer_intent_processes",
                [],
                |row| row.get(0),
            )
            .expect("the legacy table must survive the refused open");
        assert_eq!(
            legacy_rows, 2,
            "a refused open must not run the deleted fold"
        );
    }
}

#[cfg(test)]
mod schema_version_tests {
    use super::*;

    /// FIG-3975: the compatibility stamp is authoritative. A database
    /// stamped at this build's version was fully laid down by the open that
    /// stamped it, so reopening does not re-run the schema batch. Dropping a
    /// stamped table proves the skip: the open leaves it absent rather than
    /// recreating it.
    #[test]
    fn a_current_stamp_skips_the_schema_batch() {
        let mut connection = Connection::open_in_memory().expect("open schema fixture");
        prepare_versioned_schema(&mut connection, SqliteDatabase::DurableCore)
            .expect("lay down the current schema")
            .commit()
            .expect("commit the fixture");
        connection
            .execute_batch("DROP TABLE pending_turn_inputs")
            .expect("drop a stamped table");

        prepare_versioned_schema(&mut connection, SqliteDatabase::DurableCore)
            .expect("a matching stamp opens without the schema batch")
            .commit()
            .expect("commit the stamp write");

        let present: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'pending_turn_inputs'",
                [],
                |row| row.get(0),
            )
            .expect("check the dropped table");
        assert_eq!(present, 0, "the schema batch re-ran over a current stamp");
    }
}

#[cfg(test)]
#[path = "schema_check_constraint_tests.rs"]
mod check_constraint_tests;

#[cfg(test)]
#[path = "schema_obligation_constraint_tests.rs"]
mod obligation_constraint_tests;
