//! Canonical SQLite schema + the open/ensure helpers built on
//! [`SqliteConnection`].
//!
//! The `SCHEMA` / `PROCESS_SCHEMA` / `EFFECT_SCHEMA` strings are plain SQLite
//! and are copied verbatim from the prior store. The only thing that changes in
//! the rusqlite port is the *open path*: the prior store's `Builder::new_local` +
//! `experimental_multiprocess_wal` + `PRAGMA journal_mode='mvcc'` is replaced by
//! [`SqliteConnection::open`], which applies real `journal_mode=WAL` and a
//! 15-second `busy_timeout` (see `conn.rs`).

use super::*;
use crate::schema_fragments::{AWAIT_EVENT_TABLES, SCOPE_RETIREMENT_TABLE, SESSION_INGRESS_TABLE};

#[derive(Clone, Copy)]
struct SqliteDatabaseDefinition {
    name: &'static str,
    schema: &'static str,
    /// Shared table sets this database also carries; see
    /// [`SqliteDatabase::fragments`].
    fragments: &'static [&'static str],
    version: i32,
}

/// One of the four independently versioned SQLite databases a lash backend
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
    /// The effect-replay journal and await-event ledger.
    EffectReplay,
}

impl SqliteDatabase {
    /// Every database a backend holds.
    pub(crate) const ALL: [Self; 4] = [
        Self::DurableCore,
        Self::ProcessRegistry,
        Self::Triggers,
        Self::EffectReplay,
    ];

    /// The file this database is kept in under a file backend's root.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::DurableCore => crate::DURABLE_CORE_DB_FILE,
            Self::ProcessRegistry => "process-registry.db",
            Self::Triggers => "triggers.db",
            Self::EffectReplay => "effect-replay.db",
        }
    }

    /// The last segment of this database's `memdb` name in a memory
    /// backend.
    pub(crate) const fn memory_name(self) -> &'static str {
        match self {
            Self::DurableCore => "core",
            Self::ProcessRegistry => "registry",
            Self::Triggers => "triggers",
            Self::EffectReplay => "effects",
        }
    }

    const fn definition(self) -> SqliteDatabaseDefinition {
        match self {
            Self::DurableCore => SqliteDatabaseDefinition {
                name: "durable core",
                schema: SCHEMA,
                fragments: &[SESSION_INGRESS_TABLE],
                version: SCHEMA_VERSION,
            },
            Self::ProcessRegistry => SqliteDatabaseDefinition {
                name: "process registry",
                schema: PROCESS_SCHEMA,
                fragments: &[SCOPE_RETIREMENT_TABLE],
                version: PROCESS_SCHEMA_VERSION,
            },
            Self::Triggers => SqliteDatabaseDefinition {
                name: "trigger store",
                schema: TRIGGER_SCHEMA,
                fragments: &[],
                version: TRIGGER_SCHEMA_VERSION,
            },
            Self::EffectReplay => SqliteDatabaseDefinition {
                name: "effect replay",
                schema: EFFECT_SCHEMA,
                fragments: &[AWAIT_EVENT_TABLES, SCOPE_RETIREMENT_TABLE],
                version: EFFECT_SCHEMA_VERSION,
            },
        }
    }

    fn schema(self) -> &'static str {
        self.definition().schema
    }

    fn schema_version(self) -> i32 {
        self.definition().version
    }

    /// The `PRAGMA user_version` this build requires of the database.
    pub fn expected_version(self) -> i64 {
        i64::from(self.schema_version())
    }

    /// The operator-facing name used in reports and refusal messages.
    pub fn name(self) -> &'static str {
        self.definition().name
    }

    /// Shared DDL fragments applied after `schema` inside the same
    /// initialization transaction; see [`crate::schema_fragments`].
    fn fragments(self) -> &'static [&'static str] {
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
    checkpoint_ref TEXT
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
    seq                  INTEGER PRIMARY KEY,
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
    -- The complete typed disposition, hole identities included: a reopened
    -- runtime rebuilds the attempts it still owes usage for from this column.
    usage_disposition_json   TEXT NOT NULL,
    UNIQUE (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash)
);
CREATE INDEX IF NOT EXISTS idx_usage_deltas_session_seq
    ON usage_deltas(session_id, seq);

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
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')),
    CONSTRAINT ck_session_meta_relation_family CHECK ((relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL AND source_node_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))),
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'trigger_occurrence' AND caused_by_occurrence_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node'))))
);

CREATE TABLE IF NOT EXISTS session_meta_pending_observer_intents (
    session_id    TEXT NOT NULL,
    process_index INTEGER NOT NULL,
    process_id    TEXT NOT NULL,
    process_incarnation INTEGER,
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
    committed_at_ms             INTEGER NOT NULL,
    request_identity_hash       TEXT,
    requested_node_count        INTEGER,
    identity_encoding_version   INTEGER,
    PRIMARY KEY (session_id, turn_id),
    CONSTRAINT ck_runtime_turn_commits_identity CHECK ((request_identity_hash IS NULL) = (identity_encoding_version IS NULL) AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL))
);

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

CREATE TABLE IF NOT EXISTS queued_runs (
    session_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    status TEXT NOT NULL CONSTRAINT ck_queued_runs_status CHECK (status IN ('pending', 'settled')),
    revision INTEGER NOT NULL CONSTRAINT ck_queued_runs_revision CHECK (revision >= 0),
    admission_json TEXT NOT NULL,
    PRIMARY KEY (session_id, scope_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS queued_runs_pending ON queued_runs(session_id) WHERE status = 'pending';
CREATE TABLE IF NOT EXISTS queued_run_members (
    session_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    collection_kind TEXT NOT NULL CONSTRAINT ck_queued_run_members_collection_kind CHECK (collection_kind IN ('initial', 'current', 'withheld', 'assigned')),
    ordinal INTEGER NOT NULL CONSTRAINT ck_queued_run_members_ordinal CHECK (ordinal >= 0),
    member_kind TEXT NOT NULL CONSTRAINT ck_queued_run_members_member_kind CHECK (member_kind IN ('input', 'batch')),
    member_id TEXT NOT NULL,
    PRIMARY KEY (session_id, scope_id, collection_kind, ordinal),
    UNIQUE (session_id, scope_id, collection_kind, member_kind, member_id),
    FOREIGN KEY (session_id, scope_id) REFERENCES queued_runs(session_id, scope_id)
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
    attempts INTEGER NOT NULL CONSTRAINT ck_turn_parks_attempts CHECK (attempts >= 1)
);
CREATE INDEX IF NOT EXISTS idx_turn_parks_since
    ON turn_parks(since_ms, session_id);

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
    kind        TEXT NOT NULL CONSTRAINT ck_turn_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled')),
    cause       TEXT,
    reason_json TEXT,
    at_ms       INTEGER NOT NULL,
    CONSTRAINT ck_turn_park_events_parked_reason CHECK ((kind = 'parked' AND reason_json IS NOT NULL AND cause IS NULL) OR (kind <> 'parked' AND reason_json IS NULL AND cause IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS session_execution_leases (
    session_id               TEXT PRIMARY KEY,
    lease_owner_id           TEXT,
    lease_owner_incarnation_id TEXT,
    lease_executor_id        TEXT,
    lease_token              TEXT,
    lease_fencing_token      INTEGER NOT NULL DEFAULT 0,
    lease_claimed_at_ms      INTEGER NOT NULL DEFAULT 0,
    lease_term_ms            INTEGER NOT NULL DEFAULT 0,
    lease_expires_at_ms      INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT ck_session_execution_leases_identity_all_or_none CHECK ((lease_owner_id IS NULL AND lease_owner_incarnation_id IS NULL AND lease_executor_id IS NULL AND lease_token IS NULL) OR (lease_owner_id IS NOT NULL AND lease_owner_incarnation_id IS NOT NULL AND lease_executor_id IS NOT NULL AND lease_token IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS queued_work_batches (
    enqueue_seq       INTEGER PRIMARY KEY,
    batch_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    delivery_policy   TEXT NOT NULL,
    work_kind         TEXT NOT NULL,
    authority_json    TEXT NOT NULL,
    merge_key         TEXT,
    available_at_ms   INTEGER NOT NULL,
    enqueued_at_ms    INTEGER NOT NULL,
    claim_id          TEXT, -- With claim_token, names a live claim for a nonzero generation.
    claim_token       TEXT, -- At generation zero, the pair is an abandon-restored predecessor.
    claim_fencing_token INTEGER NOT NULL DEFAULT 0,
    claim_session_lease_generation INTEGER NOT NULL DEFAULT 0, -- Zero disambiguates the predecessor record from a live claim.
    CONSTRAINT ck_queued_work_batches_work_kind CHECK (work_kind IN ('turn', 'control')),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK (delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')),
    CONSTRAINT ck_queued_work_batches_claim_id_token_all_or_none CHECK ((claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)),
    UNIQUE (session_id, source_key)
        ON CONFLICT IGNORE
);

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

CREATE INDEX IF NOT EXISTS idx_queued_work_ready
    ON queued_work_batches(session_id, available_at_ms, enqueue_seq);

CREATE INDEX IF NOT EXISTS idx_queued_work_session_command_order
    ON queued_work_batches(session_id, work_kind, enqueued_at_ms, enqueue_seq);

CREATE INDEX IF NOT EXISTS idx_queued_work_claim
    ON queued_work_batches(session_id, claim_id, claim_token);

CREATE TABLE IF NOT EXISTS pending_turn_inputs (
    enqueue_seq       INTEGER PRIMARY KEY,
    input_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    ingress_json      TEXT NOT NULL,
    state             TEXT NOT NULL,
    input_json        TEXT NOT NULL,
    submitted_ingress_json TEXT NOT NULL,
    submission_digest TEXT NOT NULL,
    enqueued_at_ms    INTEGER NOT NULL,
    claim_id          TEXT,
    claim_owner_id    TEXT,
    claim_owner_incarnation_id TEXT,
    claim_token       TEXT,
    claim_fencing_token INTEGER NOT NULL DEFAULT 0,
    claim_session_lease_generation INTEGER NOT NULL DEFAULT 0,
    claim_bound_turn_id TEXT,
    claim_bound_receipt_input_id TEXT,
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK ((json_extract(ingress_json, '$.scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR (json_extract(ingress_json, '$.scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_claim_identity_all_or_none CHECK ((claim_id IS NULL AND claim_owner_id IS NULL AND claim_owner_incarnation_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_owner_id IS NOT NULL AND claim_owner_incarnation_id IS NOT NULL AND claim_token IS NOT NULL)),
    CONSTRAINT ck_pending_turn_inputs_bound_claim_is_next_turn CHECK ((claim_bound_turn_id IS NULL AND claim_bound_receipt_input_id IS NULL) OR (claim_bound_turn_id IS NOT NULL AND claim_bound_receipt_input_id IS NOT NULL AND claim_token IS NOT NULL AND state = 'deferred_next_turn')),
    UNIQUE (session_id, source_key)
        ON CONFLICT IGNORE
);

CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_session
    ON pending_turn_inputs(session_id, state, enqueue_seq);

CREATE INDEX IF NOT EXISTS idx_pending_turn_input_order
    ON pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq);

CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_claim
    ON pending_turn_inputs(session_id, claim_id, claim_token);

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
    owner_incarnation INTEGER,
    CONSTRAINT ck_attachment_manifest_owner_identity CHECK ((owner_kind IS NULL AND owner_id IS NULL AND owner_incarnation IS NULL) OR (owner_kind = 'turn' AND owner_id IS NOT NULL AND owner_incarnation IS NULL) OR (owner_kind = 'process' AND owner_id IS NOT NULL AND owner_incarnation IS NOT NULL)),
    PRIMARY KEY (session_id, attachment_id)
);

-- Attachment GC fence state, one row per condemned digest. Deliberately
-- timestampless: the protocol is CAS transitions only (see
-- `lash_core::AttachmentCondemnation`), never an expiry.
CREATE TABLE IF NOT EXISTS attachment_condemnations (
    attachment_id TEXT PRIMARY KEY,
    phase         TEXT NOT NULL CONSTRAINT ck_attachment_condemnations_phase CHECK (phase IN ('condemned', 'deleting')),
    write_token   TEXT,
    write_session_id TEXT,
    CONSTRAINT ck_attachment_condemnations_write_token_pairing CHECK ((write_token IS NULL) = (write_session_id IS NULL)),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (write_token IS NULL OR phase = 'condemned')
);

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

-- Exact owner edges for immutable artifacts. The edge is the liveness fact;
-- no maintained count or last-operation field exists on shared bytes.
CREATE TABLE IF NOT EXISTS artifact_owners (
    namespace    TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    owner_kind   TEXT NOT NULL CONSTRAINT ck_artifact_owners_owner_kind CHECK (owner_kind IN ('host', 'process', 'execution')),
    owner_id     TEXT NOT NULL,
    PRIMARY KEY (namespace, artifact_ref, owner_kind, owner_id),
    FOREIGN KEY (namespace, artifact_ref) REFERENCES artifact_refs(namespace, artifact_ref) ON DELETE CASCADE
);

-- Execution-owner retirement is a permanent publication fence. Host and
-- process releases are ordinary exact-edge severance and never enter here.
CREATE TABLE IF NOT EXISTS artifact_owner_retirements (
    owner_kind TEXT NOT NULL CONSTRAINT ck_artifact_owner_retirements_owner_kind CHECK (owner_kind = 'execution'),
    owner_id   TEXT NOT NULL,
    PRIMARY KEY (owner_kind, owner_id)
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
    ON attachment_manifest(session_id, owner_kind, owner_id, owner_incarnation, committed_at_ms);
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

CREATE INDEX IF NOT EXISTS idx_artifact_owners_owner
    ON artifact_owners(owner_kind, owner_id);

CREATE TABLE IF NOT EXISTS release_stamp (
    singleton           INTEGER PRIMARY KEY CONSTRAINT ck_release_stamp_singleton CHECK (singleton = 1),
    release_version     TEXT NOT NULL,
    schema_versions     TEXT NOT NULL,
    written_at_epoch_ms INTEGER NOT NULL
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
/// pending-turn-input rows replace their per-claim `claim_claimed_at_ms` /
/// `claim_expires_at_ms` columns with a single `claim_session_lease_generation`
/// pinning the session-execution-lease generation the claim was taken under.
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
/// An additive, index-only catalog change does **not** bump this version. Every
/// `CREATE INDEX` above is `IF NOT EXISTS` and open always runs the whole
/// schema, so a same-version file written before the index existed self-heals
/// into the newer index set on first open, and a newer file stays readable by
/// the older binary — the two are mutually compatible on the same stamp. Bumping instead
/// would reject-and-recreate live stores for a change that costs nothing to
/// apply in place. The idle-arbitration ordering indexes
/// (`idx_queued_work_session_command_order`,
/// `idx_pending_turn_input_order`) are added under exactly this carve-out. It
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
/// into the shared `AWAIT_EVENT_TABLES` fragment so the declaration exists
/// once for both carrying databases. The applied DDL is statement-identical,
/// but the guarded `SCHEMA` text changed, so a pre-68 database is rejected at
/// open and recreated like any other schema change.
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
pub(crate) const SCHEMA_VERSION: i32 = 87;

pub(crate) const PROCESS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS processes (
    process_id            TEXT PRIMARY KEY,
    incarnation           INTEGER NOT NULL,
    registration_fingerprint     TEXT NOT NULL,
    originator_id         TEXT NOT NULL,
    wake_session_id       TEXT,
    identity_kind         TEXT NOT NULL,
    identity_label        TEXT,
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL,
    last_event_sequence   INTEGER NOT NULL,
    change_seq            INTEGER NOT NULL,
    status                TEXT NOT NULL,
    parent_scope_kind     TEXT NOT NULL,
    parent_scope_id       TEXT,
    on_parent_end         TEXT NOT NULL,
    cancel_requested_at_ms INTEGER,
    record_json           TEXT NOT NULL,
    UNIQUE(process_id, incarnation),
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')),
    CONSTRAINT ck_processes_parent_scope_kind CHECK (parent_scope_kind IN ('turn', 'queue_drain', 'process', 'host')),
    CONSTRAINT ck_processes_parent_scope_id CHECK ((parent_scope_kind = 'host' AND parent_scope_id IS NULL) OR (parent_scope_kind IN ('turn', 'queue_drain', 'process') AND parent_scope_id IS NOT NULL)),
    CONSTRAINT ck_processes_on_parent_end CHECK (on_parent_end IN ('abandon', 'cancel'))
);

CREATE INDEX IF NOT EXISTS idx_processes_status
    ON processes(status);
CREATE INDEX IF NOT EXISTS idx_processes_live_worklist
    ON processes(process_id) WHERE status IN ('running', 'waiting');

-- The scope-retirement fence this database shares with the effect journal is
-- applied from the shared SCOPE_RETIREMENT_TABLE fragment.

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
-- The parent-end sweep's only scan: children of one ended parent scope that
-- still owe a cancel. The predicate names the live statuses rather than a NOT
-- IN so a status added later cannot silently widen the index; it is exactly
-- `LIVE_PROCESS_STATUS_LABELS`, so `caller_departed` is out for the reason it
-- is out of every other worklist - lash may never act on such a row nor
-- assert an outcome for it, and a cancel request is both.
CREATE INDEX IF NOT EXISTS idx_processes_parent_scope
    ON processes(parent_scope_kind, parent_scope_id, process_id);
CREATE INDEX IF NOT EXISTS idx_processes_parent_end_pending
    ON processes(parent_scope_kind, parent_scope_id, process_id)
    WHERE on_parent_end = 'cancel'
      AND cancel_requested_at_ms IS NULL
      AND status IN ('running', 'waiting');

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
    process_incarnation INTEGER NOT NULL,
    sequence          INTEGER NOT NULL,
    event_type        TEXT NOT NULL,
    idempotency_key   TEXT,
    event_json        TEXT NOT NULL,
    PRIMARY KEY (process_id, process_incarnation, sequence),
    FOREIGN KEY (process_id, process_incarnation) REFERENCES processes(process_id, incarnation) ON DELETE CASCADE
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
    process_incarnation INTEGER NOT NULL,
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
    FOREIGN KEY (process_id, process_incarnation) REFERENCES processes(process_id, incarnation) ON DELETE CASCADE
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
    process_incarnation INTEGER NOT NULL,
    PRIMARY KEY (session_id, process_id, process_incarnation),
    FOREIGN KEY (process_id, process_incarnation) REFERENCES processes(process_id, incarnation) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_process_observers_process
    ON process_observers(process_id, session_id);

CREATE TABLE IF NOT EXISTS process_tombstones (
    process_id          TEXT NOT NULL,
    incarnation         INTEGER NOT NULL,
    terminal_label      TEXT NOT NULL,
    pruned_at_ms        INTEGER NOT NULL,
    pruned_change_seq   INTEGER NOT NULL,
    PRIMARY KEY (process_id, incarnation)
);
CREATE INDEX IF NOT EXISTS idx_process_tombstones_change
    ON process_tombstones(pruned_change_seq);

CREATE TABLE IF NOT EXISTS process_artifact_cleanup (
    process_id       TEXT NOT NULL,
    incarnation      INTEGER NOT NULL,
    cleanup_json     TEXT NOT NULL,
    PRIMARY KEY (process_id, incarnation),
    FOREIGN KEY (process_id, incarnation) REFERENCES process_tombstones(process_id, incarnation) ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS process_leases (
    process_id       TEXT PRIMARY KEY,
    lease_owner_id   TEXT,
    lease_owner_incarnation_id TEXT,
    lease_token      TEXT,
    lease_fencing_token  INTEGER NOT NULL DEFAULT 0,
    lease_claimed_at_ms  INTEGER NOT NULL DEFAULT 0,
    lease_expires_at_ms  INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS process_segment_handovers (
    process_id       TEXT NOT NULL,
    segment_ordinal  INTEGER NOT NULL,
    handover_json    TEXT NOT NULL,
    started_json     TEXT,
    PRIMARY KEY (process_id, segment_ordinal),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

-- One row per ended parent scope, keyed by the scope itself rather than by a
-- process row: a turn-scoped parent has no process row at all, and a
-- process-scoped parent's row may be pruned before its children settle.
CREATE TABLE IF NOT EXISTS parent_end_plans (
    parent_kind      TEXT NOT NULL,
    parent_id        TEXT NOT NULL,
    parent_payload   TEXT NOT NULL,
    ended_at_ms      INTEGER NOT NULL,
    settled_at_ms    INTEGER,
    PRIMARY KEY (parent_kind, parent_id),
    CONSTRAINT ck_parent_end_plans_kind CHECK (parent_kind IN ('turn', 'queue_drain', 'process'))
);
CREATE INDEX IF NOT EXISTS idx_parent_end_plans_pending
    ON parent_end_plans(ended_at_ms, parent_kind, parent_id)
    WHERE settled_at_ms IS NULL;

CREATE TABLE IF NOT EXISTS tool_intent_submissions (
    replay_key          TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL,
    execution_scope_id  TEXT NOT NULL,
    tool_call_id        TEXT NOT NULL,
    intent_index        INTEGER NOT NULL,
    kind                TEXT NOT NULL,
    payload_hash        TEXT NOT NULL,
    submission_json     TEXT NOT NULL,
    CONSTRAINT ck_tool_intent_submissions_kind CHECK (kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger'))
);
CREATE INDEX IF NOT EXISTS idx_tool_intent_submissions_scope
    ON tool_intent_submissions(session_id, execution_scope_id, intent_index);

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
// Version 23 indexes the bounded non-terminal recovery worklist by process id.
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
/// Version 39 moves `effect_scope_retirements` out of this string into the
/// shared `SCOPE_RETIREMENT_TABLE` fragment (FIG-3260) so the fence is declared
/// once for both carrying databases. The applied DDL is statement-identical,
/// but the guarded `PROCESS_SCHEMA` text changed, so a pre-39 registry is
/// rejected at open and recreated.
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
pub(crate) const PROCESS_SCHEMA_VERSION: i32 = 43;

pub(crate) const TRIGGER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trigger_subscriptions (
    subscription_id      TEXT PRIMARY KEY,
    owner_scope          TEXT NOT NULL,
    subscription_key     TEXT NOT NULL,
    incarnation          TEXT NOT NULL,
    revision             INTEGER NOT NULL,
    definition_fingerprint      TEXT NOT NULL,
    source_type          TEXT NOT NULL,
    source_key           TEXT NOT NULL,
    lifecycle            TEXT NOT NULL,
    deleted_at_ms        INTEGER,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    record_json          TEXT NOT NULL,
    CONSTRAINT ck_trigger_subscriptions_lifecycle CHECK (lifecycle IN ('enabled', 'disabled', 'tombstoned')),
    CONSTRAINT ck_trigger_subscriptions_lifecycle_deleted_at CHECK ((lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)),
    UNIQUE(owner_scope, subscription_key)
);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_registrant
    ON trigger_subscriptions(owner_scope, subscription_key);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_source
    ON trigger_subscriptions(source_type, source_key, lifecycle);

CREATE TABLE IF NOT EXISTS trigger_occurrences (
    occurrence_id    TEXT PRIMARY KEY,
    idempotency_key  TEXT NOT NULL UNIQUE,
    source_type      TEXT NOT NULL,
    source_key       TEXT NOT NULL,
    occurred_at_ms   INTEGER NOT NULL,
    reclaimable_at_ms INTEGER,
    record_json      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_source
    ON trigger_occurrences(source_type, source_key, occurred_at_ms);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_reclaimable
    ON trigger_occurrences(reclaimable_at_ms, occurrence_id)
    WHERE reclaimable_at_ms IS NOT NULL;

CREATE TABLE IF NOT EXISTS trigger_deliveries (
    occurrence_id    TEXT NOT NULL,
    subscription_id  TEXT NOT NULL,
    process_id       TEXT NOT NULL,
    subscription_incarnation TEXT NOT NULL,
    subscription_revision INTEGER NOT NULL,
    subscription_snapshot_json TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (occurrence_id, subscription_id),
    FOREIGN KEY (occurrence_id) REFERENCES trigger_occurrences(occurrence_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS trigger_mutation_receipts (
    operation_id    TEXT PRIMARY KEY,
    owner_kind      TEXT NOT NULL,
    owner_id        TEXT NOT NULL,
    request_fingerprint    TEXT NOT NULL,
    result_json     TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    CONSTRAINT ck_trigger_receipts_owner_kind CHECK (owner_kind IN ('session', 'host', 'platform'))
);

CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_process
    ON trigger_deliveries(process_id);

CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_subscription
    ON trigger_deliveries(subscription_id);
";

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
pub(crate) const TRIGGER_SCHEMA_VERSION: i32 = 11;

pub(crate) const EFFECT_SCHEMA: &str = "

CREATE TABLE IF NOT EXISTS runtime_effect_group (
    group_key          TEXT PRIMARY KEY,
    scope_id           TEXT NOT NULL,
    session_id         TEXT,
    wake               TEXT NOT NULL,
    loser_disposition  TEXT NOT NULL,
    expected_children  INTEGER NOT NULL,
    next_seq           INTEGER NOT NULL DEFAULT 0,
    next_commit_seq    INTEGER NOT NULL DEFAULT 0,
    lifecycle          TEXT NOT NULL DEFAULT '{\"type\":\"live\"}',
    created_at_ms      INTEGER NOT NULL,
    CONSTRAINT ck_runtime_effect_group_wake CHECK (wake IN ('first', 'first_success', 'all')),
    CONSTRAINT ck_runtime_effect_group_loser_disposition CHECK (loser_disposition IN ('run_to_completion', 'cancel')),
    CONSTRAINT ck_runtime_effect_group_lifecycle CHECK (json_extract(lifecycle, '$.type') IN ('live', 'closing', 'settled'))
);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_group_session
    ON runtime_effect_group(session_id);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_group_scope
    ON runtime_effect_group(scope_id);

-- One row per accepted child of a group, carrying the request that
-- reconstructs it (ADR 0099 section 3). Retained input only: the section 4/5
-- arbitration state lives on the replay row as commit_state/commit_seq,
-- because the CAS that decides a child runs under the replay row's lock and
-- must not reach a second row to win. `command_version` is the command
-- encoding the retained envelope was minted under, checked at decode.
-- Written with the group row in one transaction, children first
-- (ADR 0065 N2), so a recorded group always has discoverable complete input.
-- No scope_id: the group row owns that fact.
CREATE TABLE IF NOT EXISTS runtime_effect_group_child (
    group_key        TEXT NOT NULL,
    position         INTEGER NOT NULL,
    replay_key       TEXT NOT NULL,
    envelope_json    TEXT NOT NULL,
    command_version  INTEGER NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    -- Membership rows are written before their group row inside the open
    -- transaction (ADR 0065 N2), so the reference must settle at commit, not
    -- at the statement.
    CONSTRAINT fk_runtime_effect_group_child_group FOREIGN KEY (group_key) REFERENCES runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED,
    PRIMARY KEY (group_key, position)
);

-- Reopens, drains and the unsettled-children join all reach a membership row
-- by (group_key, replay_key), which the position primary key does not serve.
CREATE UNIQUE INDEX IF NOT EXISTS uq_runtime_effect_group_child_replay_key
    ON runtime_effect_group_child(group_key, replay_key);

CREATE TABLE IF NOT EXISTS runtime_effect_replay (
    scope_id             TEXT NOT NULL,
    session_id           TEXT,
    replay_key           TEXT NOT NULL,
    envelope_hash        TEXT NOT NULL,
    envelope_json        TEXT NOT NULL,
    status               TEXT NOT NULL,
    outcome_json         TEXT,
    error_json           TEXT,
    lease_owner_id       TEXT,
    lease_token          TEXT,
    lease_expires_at_ms  INTEGER NOT NULL DEFAULT 0,
    due_at_ms            INTEGER,
    group_key            TEXT,
    settlement_seq       INTEGER,
    commit_state         TEXT NOT NULL DEFAULT 'pending',
    commit_seq           INTEGER,
    drain_input          TEXT,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    CONSTRAINT ck_runtime_effect_replay_status CHECK (status IN ('in_progress', 'completed', 'failed')),
    CONSTRAINT ck_runtime_effect_replay_commit_state CHECK (commit_state IN ('pending', 'committed', 'drained', 'cancel_decided')),
    CONSTRAINT ck_runtime_effect_replay_commit_seq CHECK ((commit_seq IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))) AND (group_key IS NULL OR NOT (commit_state IN ('committed', 'drained')) OR commit_seq IS NOT NULL)),
    CONSTRAINT ck_runtime_effect_replay_drain_input CHECK (drain_input IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))),
    CONSTRAINT ck_runtime_effect_replay_outcome_json CHECK ((status = 'completed' AND outcome_json IS NOT NULL) OR (status <> 'completed' AND outcome_json IS NULL)),
    CONSTRAINT ck_runtime_effect_replay_error_json CHECK ((status = 'failed' AND error_json IS NOT NULL) OR (status <> 'failed' AND error_json IS NULL)),
    CONSTRAINT ck_runtime_effect_replay_settlement_seq CHECK ((settlement_seq IS NULL AND NOT (commit_state IN ('drained', 'cancel_decided'))) OR (settlement_seq IS NOT NULL AND commit_state IN ('drained', 'cancel_decided'))),
    CONSTRAINT fk_runtime_effect_replay_group FOREIGN KEY (group_key) REFERENCES runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED,
    PRIMARY KEY (scope_id, replay_key)
);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_replay_lease
    ON runtime_effect_replay(status, lease_expires_at_ms);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_replay_session
    ON runtime_effect_replay(session_id);

-- Backstop for the group counter, not the allocator. Ranks are allocated by a
-- single-row bump on runtime_effect_group; this index is what makes a
-- regression to a read-then-max allocator fail closed on a constraint violation
-- instead of silently seating two children at one rank. It doubles as the
-- ordered index the rank read scans, which is why its predicate is exactly the
-- read's filter.
CREATE UNIQUE INDEX IF NOT EXISTS uq_runtime_effect_replay_group_seq
    ON runtime_effect_replay(group_key, settlement_seq)
    WHERE group_key IS NOT NULL AND settlement_seq IS NOT NULL;

-- One commit position per child, per group: the §4 linearization point's
-- backstop, the same role the settlement-seq unique index plays for ranks.
CREATE UNIQUE INDEX IF NOT EXISTS uq_runtime_effect_replay_commit_seq
    ON runtime_effect_replay(group_key, commit_seq)
    WHERE commit_seq IS NOT NULL;

-- The await-event tables this database shares with durable core and the
-- scope-retirement fence it shares with the process registry are applied
-- from the shared AWAIT_EVENT_TABLES and SCOPE_RETIREMENT_TABLE fragments.

-- Durable catalogs that may hold an authorized cancellation closure under a
-- physical scope. Owner retirement and participant registration serialize on
-- this database; a participant is released only after its catalog fences new
-- authorizations and proves that none remain.
CREATE TABLE IF NOT EXISTS turn_cancel_closure_participants (
    scope_id       TEXT NOT NULL,
    participant_id TEXT NOT NULL,
    scope_json     TEXT NOT NULL,
    PRIMARY KEY (scope_id, participant_id)
);
";

/// Version 6 keys session-owned effects by the permanent session id and removes
/// the incarnation join column. Effect databases follow the crate's alpha
/// reject-and-recreate convention rather than carrying a migration chain.
/// Version 7 rejects live-serde await-event and direct replay identities.
/// Version 8 rejects the former live-serde tool-batch and process-transfer
/// replay names.
/// Version 9 rejects completed tool-attempt outcomes whose frame-switch control
/// still carries the pre-cutover `frame_id` field.
/// Version 10 adds the versioned tool-intent carrier to recorded tool-attempt
/// outcomes and the typed execution outcomes to completed tool batches.
/// Version 11 adds the durable effect-group journal (FIG-1416, ADR 0065): the
/// `runtime_effect_group` counter row, the `group_key`/`settlement_seq` columns
/// that seat a child in its group's settlement order, and the unique backstop
/// over the pair. Effect databases follow the crate's reject-and-recreate
/// convention, so an existing effect database is deleted on upgrade rather than
/// migrated — release-notes material, not a host's discovery.
/// Version 12 removes the duplicated `LlmResponse.full_text` member from
/// runtime-effect outcomes. Pre-cutover journal JSON is upgraded only at the
/// effect replay decode boundary.
/// Version 13 merges each journaled exec observation with its projection
/// metadata. Pre-13 effect databases are rejected at open; there is no migration
/// arm.
///
/// The index-only carve-out documented on `SCHEMA_VERSION` applies here for the
/// same reason and with the same limit: an additive non-unique
/// `CREATE INDEX IF NOT EXISTS` self-heals into an existing effect database on
/// open and leaves it readable by an older binary, so it does not bump — while
/// any table, column, unique index, or semantic change still does.
/// `idx_runtime_effect_replay_group_unsettled` (FIG-1564) is added under it,
/// because bumping would delete live effect databases to buy a query plan. That
/// index serves `read_unsettled_group_children`, whose predicate is the opposite
/// half of the unique backstop's: without it the read scans the whole effect
/// journal on every child completion after a close and on every drain pass
/// (FIG-1536). `replay_key` trails the group key so the read's `ORDER BY` is the
/// index order and the plan needs no sort.
///
/// The rationale lives here rather than beside the statement on purpose: the
/// version guard's projection elides a *new* index statement but not the SQL
/// comments around it, so prose inside `EFFECT_SCHEMA` would demand the very
/// bump the carve-out exists to avoid.
/// Version 14 switches durable effect identities to domain-tagged BLAKE3.
/// Version 15 adds the DDL-enforced effect-replay status vocabulary. Existing
/// effect journals are rejected rather than migrated.
/// Version 16 merges the two journaled exec dispatch ledgers. Pre-16 effect
/// databases are rejected at open; there is no compatibility decoder.
/// Version 17 adds the permanent `effect_scope_retirements` fence (FIG-2499,
/// FIG-2500): retiring a process or runtime-operation scope deletes its effect
/// children, groups, and await-event promises in one transaction and leaves a
/// tombstone every admission path refuses. Pre-17 effect databases are
/// rejected at open; there is no migration arm.
/// Version 18 persists the admitted execution scope with every replay key.
/// Pre-18 journals are rejected because their keys cannot identify the scope
/// whose authority admitted the effect.
/// Version 19 rejects the retired trigger-list envelope shape. Older journals
/// are recreated rather than replayed across this encoding cutover.
/// Version 20 makes lifecycle evidence own execution-artifact cleanup completion.
/// Pre-20 effect databases are rejected rather than migrated.
/// Version 21 adds owner-side cancellation-closure participants. This makes
/// scope retirement serialize with authorization held in separate session
/// catalogs; pre-21 effect databases are rejected and recreated.
/// Version 22 drains journals written before the canonical `SleepSpec` encoding
/// (FIG-2968, FIG-2983). A sleep row written at generation 21 carries the
/// resolved `Sleep { duration_ms }` command in `envelope_json`; this build
/// re-encodes the same intent as `Sleep { spec }`, so the row's replay-hash
/// fence no longer reconstructs and a redrive reported `ReplayMismatch` instead
/// of a refusal. The table shape is unchanged — the cutover is in the journaled
/// command encoding — so the generation moves to refuse those journals at open.
/// Pre-22 effect databases are rejected and recreated; there is no migration arm
/// because the resolved duration cannot be turned back into the deadline the
/// guest asked for.
/// Version 23 moves the await-event tables and `effect_scope_retirements` out of
/// this string into the shared `AWAIT_EVENT_TABLES` and `SCOPE_RETIREMENT_TABLE`
/// fragments (FIG-3260) so each declaration exists once for every carrying
/// database. The applied DDL is statement-identical, but the guarded
/// `EFFECT_SCHEMA` text changed, so a pre-23 journal is rejected at open and
/// recreated.
/// Version 24 names the formerly-anonymous CHECKs in the shared fragments
/// (FIG-3261) so the required-constraints gate can see them; a pre-24 journal
/// is rejected at open and recreated.
/// Version 25 constrains the effect-group wake and loser-disposition
/// vocabularies at the DDL level (FIG-2811): both columns held closed enums
/// enforced only at read time. A pre-25 journal is rejected at open and
/// recreated.
/// Version 26 (FIG-3376) moves the `SessionCreateRequest` carried in effect
/// payloads to the spawn-time plugin-init cutover and drops `usage_source`;
/// a pre-26 journal is rejected at open and recreated.
/// Version 27 adds the accepted-membership table `runtime_effect_group_child`
/// (FIG-3408, ADR 0099 §3). A pre-27 journal is rejected at open and recreated.
/// Version 28 (FIG-2362) types the journaled `exec_code` outcome failure: the
/// erased `Err(String)` becomes `Err(ExecCodeFailure { reason, message })` so
/// the closed reason reaches the trace event on replay. The new decoder still
/// accepts a bare string as `reason: "erased"`, but the written encoding moved,
/// so a pre-28 journal is rejected at open and recreated rather than replayed
/// under mixed spellings.
/// Version 29 (FIG-3418) rewrites the `ParentScope` nested inside journaled
/// start declarations from `{turn|process|host}` to `Owned(EffectOpener) |
/// Host`: a pre-29 journal's command bytes no longer decode to the current
/// shape, so it is rejected at open and recreated.
/// Version 30 (FIG-3409) carries ADR 0099 §§4–5: `commit_state`/`commit_seq`
/// on `runtime_effect_replay` give the §4 arbitration point and the
/// final-commit order one enum-plus-counter shape, `drain_input` seals the
/// committed drain input, `next_commit_seq` and `lifecycle` join
/// `runtime_effect_group`, the group arity column is renamed
/// `expected_children`, and the membership's `request_version` becomes
/// `command_version`. A pre-30 journal is rejected at open and recreated.
/// Version 31 (FIG-3411) is a journaled-encoding cutover with no relational
/// change: the `ToolAttempt` outcome's `ToolAttemptCapture` usage deltas carry
/// the `(source, model)` labels the incorporation charge needs, and the
/// journaled `ToolInvocation` outcome's `ToolDispatchOutcome` carries the
/// aggregated captures and trigger receipts to the settlement boundary. A
/// pre-31 journal is rejected at open and recreated rather than replayed
/// under the old carrier shape.
/// Generation 32 rejects journaled settlements/captures with the retired message body.
/// Version 33 (FIG-3410) puts ADR 0099 §7's group lifecycle in service: the
/// `lifecycle` column version 30 reserved now carries `closing` and `settled`
/// values beside `live`. No DDL changes — a pre-33 build reads the column as
/// always `live` and would permit the retries §7 forbids, so a pre-33 journal
/// is rejected at open and recreated.
/// Version 34 (FIG-1947) enforces the effect-replay payload, rank, and
/// group-membership invariants at the DDL level: terminal status now requires
/// its own payload column (`completed` owns `outcome_json`, `failed` owns
/// `error_json`, `in_progress` owns neither), a `settlement_seq` rank exists
/// exactly on `drained`/`cancel_decided` rows, and both `group_key` references
/// resolve to `runtime_effect_group` rows — the membership table's
/// children-before-group write order riding a deferred foreign key. A pre-34
/// journal is rejected at open and recreated.
pub(crate) const EFFECT_SCHEMA_VERSION: i32 = 34;

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

/// Apply `schema` if the database is already at `schema_version`, initialise it
/// (under one transaction stamping `user_version`) if the database is empty, or
/// reject the open if the on-disk `user_version` is anything else. Runs entirely
/// on the connection thread so the version check and DDL share one connection.
pub(crate) async fn ensure_versioned_schema(
    conn: &SqliteConnection,
    database: SqliteDatabase,
) -> rusqlite::Result<()> {
    conn.call(move |c| {
        let tx = prepare_versioned_schema(c, database)?;
        tx.commit()
    })
    .await
}

fn prepare_versioned_schema<'connection>(
    connection: &'connection mut Connection,
    database: SqliteDatabase,
) -> rusqlite::Result<Transaction<'connection>> {
    prepare_versioned_schema_at_version(connection, database, database.schema_version())
}

fn prepare_versioned_schema_at_version<'connection>(
    connection: &'connection mut Connection,
    database: SqliteDatabase,
    schema_version: i32,
) -> rusqlite::Result<Transaction<'connection>> {
    // The whole check-then-initialise runs inside one `BEGIN IMMEDIATE`
    // transaction so the write lock is held across the `user_version` read.
    // Reading the version outside the transaction and only then upgrading to
    // a writer races concurrent first-openers into a lock-upgrade deadlock
    // (SQLite returns "database is locked" immediately, bypassing
    // `busy_timeout`). Holding the write lock from the first statement makes
    // every contender serialise on the busy handler instead.
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let apply_schema = |tx: &rusqlite::Transaction<'_>| -> rusqlite::Result<()> {
        tx.execute_batch(database.schema())?;
        for fragment in database.fragments() {
            tx.execute_batch(fragment)?;
        }
        Ok(())
    };
    let user_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version == schema_version {
        apply_schema(&tx)?;
        stamp_writing_release(&tx, database)?;
        return Ok(tx);
    }
    if user_version == 0 && !has_user_schema_objects(&tx)? {
        apply_schema(&tx)?;
        tx.pragma_update(None, "user_version", schema_version)?;
        stamp_writing_release(&tx, database)?;
        return Ok(tx);
    }
    let writing_release = release_stamp_holder(database)
        .then(|| crate::release_stamp::read_release(&tx))
        .flatten();
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
        Some(unsupported_schema_message(
            database,
            schema_version,
            user_version,
            writing_release.as_deref(),
        )),
    ))
}

/// Whether this database is the one that carries the deployment's release stamp.
///
/// The four SQLite databases share one trust domain and are opened together, so
/// one stamp describes the deployment. The durable core carries it: it is the
/// database every deployment has.
pub(crate) fn release_stamp_holder(database: SqliteDatabase) -> bool {
    database == SqliteDatabase::DurableCore
}

fn stamp_writing_release(tx: &Transaction<'_>, database: SqliteDatabase) -> rusqlite::Result<()> {
    if release_stamp_holder(database) {
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

/// The expected and found `PRAGMA user_version` values are reported accurately.
/// Every database kind belongs to the one trust domain described by ADR 0049, so a refusal
/// must prescribe one coordinated reset rather than an independent wipe.
///
/// `writing_release` names the lash release that wrote the store when the
/// release stamp could still be read. It rides as a trailing sentence: every
/// substring other tests pin — the "supports schema version {n}" clause, the
/// remedy, the ADR pointer — is produced byte-identically, and a store with no
/// readable stamp produces the message unchanged rather than a hedge about an
/// unknown release.
pub(crate) fn unsupported_schema_message(
    database: SqliteDatabase,
    expected_version: i32,
    found_version: i32,
    writing_release: Option<&str>,
) -> String {
    let release_clause = match writing_release {
        Some(release) => format!(" This store was last written by lash release {release}."),
        None => String::new(),
    };
    format!(
        "Unsupported lash {} schema: this binary supports schema version {expected_version}, but \
         the database reports version {found_version}. There is no \
         migration chain — drain affected sessions and recreate the whole Lash trust domain with \
         this version. Reset the tombstones, await-event revocation ledger, effect journal, and \
         Restate state together; see docs/adr/0049-session-ids-are-used-once.md.{release_clause}",
        database.name()
    )
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
                 PRAGMA user_version = 43;",
            )
            .expect("build component-43 observer-intent fixture");

        let retired_seam =
            prepare_versioned_schema_at_version(&mut connection, SqliteDatabase::DurableCore, 44)
                .expect_err("the retired 43-to-44 arm must not accept its old source");
        assert!(
            retired_seam
                .to_string()
                .contains("supports schema version 44, but the database reports version 43"),
            "the retired seam must refuse with the recreate message: {retired_seam}"
        );
        let production = prepare_versioned_schema(&mut connection, SqliteDatabase::DurableCore)
            .expect_err("a component-43 stamp is refused at the current version");
        assert!(
            production.to_string().contains(&format!(
                "supports schema version {}, but the database reports version 43",
                SCHEMA_VERSION
            )),
            "open must refuse a pre-cutover stamp: {production}"
        );

        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
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
mod schema_metadata_tests {
    use super::*;

    #[test]
    fn every_database_kind_prescribes_the_coordinated_trust_domain_reset() {
        let cases = [
            (SqliteDatabase::DurableCore, "durable core"),
            (SqliteDatabase::ProcessRegistry, "process registry"),
            (SqliteDatabase::Triggers, "trigger store"),
            (SqliteDatabase::EffectReplay, "effect replay"),
        ];
        for (database, name) in cases {
            assert_eq!(database.name(), name);
            assert_eq!(
                unsupported_schema_message(database, 123, 45, None),
                format!(
                    "Unsupported lash {name} schema: this binary supports schema version 123, but \
                     the database reports version 45. There is no migration chain — drain affected \
                     sessions and recreate the whole Lash trust domain with this version. Reset the \
                     tombstones, await-event revocation ledger, effect journal, and Restate state \
                     together; see docs/adr/0049-session-ids-are-used-once.md."
                )
            );
        }
    }
}

#[cfg(test)]
mod check_constraint_tests {
    use super::*;

    /// The fragment dedup's whole point: every database that carries a shared
    /// table must end up with the same stored DDL for it. This is the
    /// invariant the two copy-pasted declarations silently assumed.
    #[test]
    fn shared_fragment_tables_carry_identical_ddl_in_every_carrier_database() {
        let carriers: &[(&[&str], &[SqliteDatabase])] = &[(
            &["effect_scope_retirements"],
            &[
                SqliteDatabase::ProcessRegistry,
                SqliteDatabase::EffectReplay,
            ],
        )];
        for &(objects, databases) in carriers {
            for &object in objects {
                let mut rendered = Vec::new();
                for &database in databases {
                    let mut connection =
                        Connection::open_in_memory().expect("open shared-DDL fixture");
                    prepare_versioned_schema(&mut connection, database)
                        .expect("apply database schema and fragments")
                        .commit()
                        .expect("commit shared-DDL fixture");
                    let sql: String = connection
                        .query_row(
                            "SELECT sql FROM sqlite_master WHERE name = ?1",
                            [object],
                            |row| row.get(0),
                        )
                        .unwrap_or_else(|error| {
                            panic!("{object} missing from {}: {error}", database.name())
                        });
                    rendered.push((database.name(), sql));
                }
                let (first_database, first_sql) = &rendered[0];
                for (database, sql) in &rendered[1..] {
                    assert_eq!(
                        first_sql, sql,
                        "{object} DDL drifted between {first_database} and {database}"
                    );
                }
            }
        }
    }

    fn assert_check_rejects(connection: &Connection, statement: &str, constraint: &str) {
        let error = connection
            .execute_batch(statement)
            .expect_err("an illegal durable vocabulary must violate its schema CHECK");
        assert!(
            error.to_string().contains(constraint),
            "SQLite reported the wrong CHECK for {constraint}: {error}"
        );
    }

    #[test]
    fn sqlite_checks_reject_every_registered_illegal_vocabulary_cluster() {
        let core = Connection::open_in_memory().expect("open durable-core constraint fixture");
        core.execute_batch(SCHEMA)
            .expect("create durable-core constraint fixture");
        // The three illegal scope/state pairs must name the correlation CHECK.
        // An ingress_json without a scope key passes both CHECKs under SQL NULL
        // semantics; serde cannot emit it, so both backends behave identically.
        assert_check_rejects(
            &core,
            "INSERT INTO pending_turn_inputs (
                 input_id, session_id, ingress_json, state, input_json,
                 submitted_ingress_json, submission_digest, enqueued_at_ms
             ) VALUES (
                 'bad-turn-input-state', 'session',
                 '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
                 'waiting', '{}', '{}', 'digest', 0
             )",
            "ck_pending_turn_inputs_state",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO pending_turn_inputs (
                 input_id, session_id, ingress_json, state, input_json,
                 submitted_ingress_json, submission_digest, enqueued_at_ms
             ) VALUES (
                 'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
                 'pending_active', '{}', '{}', 'digest', 0
             )",
            "ck_pending_turn_inputs_state_ingress",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO pending_turn_inputs (
                 input_id, session_id, ingress_json, state, input_json,
                 submitted_ingress_json, submission_digest, enqueued_at_ms
             ) VALUES (
                 'bad-turn-input-accepted-pair', 'session', '{\"scope\":\"next_turn\"}',
                 'accepted', '{}', '{}', 'digest', 0
             )",
            "ck_pending_turn_inputs_state_ingress",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO pending_turn_inputs (
                 input_id, session_id, ingress_json, state, input_json,
                 submitted_ingress_json, submission_digest, enqueued_at_ms
             ) VALUES (
                 'bad-turn-input-deferred-pair', 'session',
                 '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
                 'deferred_next_turn', '{}', '{}', 'digest', 0
             )",
            "ck_pending_turn_inputs_state_ingress",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind) VALUES ('bad-relation', 'sibling')",
            "ck_session_meta_relation_kind",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind) VALUES ('bad-cause', 'child', 'parent', 'timer')",
            "ck_session_meta_caused_by_kind",
        );
        core.execute_batch(
            "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, caused_by_effect_id) VALUES ('effect-address-cause', 'child', 'parent', 'effect_address', '{}')",
        )
        .expect("current effect-address discriminator is admitted");
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind) VALUES ('legacy-effect-cause', 'child', 'parent', 'effect')",
            "ck_session_meta_caused_by_kind",
        );

        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind) VALUES ('childless-child', 'child')",
            "ck_session_meta_relation_family",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, caused_by_kind, caused_by_session_id, caused_by_turn_id) VALUES ('caused-root', 'root', 'turn', 'cause-session', 'cause-turn')",
            "ck_session_meta_relation_family",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind) VALUES ('bare-discriminator', 'child', 'parent', 'turn')",
            "ck_session_meta_caused_by_family",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, caused_by_session_id, caused_by_turn_id, caused_by_node_id) VALUES ('crossed-family', 'child', 'parent', 'turn', 'cause-session', 'cause-turn', 'stray-node')",
            "ck_session_meta_caused_by_family",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_session_id) VALUES ('kindless-payload', 'child', 'parent', 'cause-session')",
            "ck_session_meta_caused_by_family",
        );

        let process = Connection::open_in_memory().expect("open process constraint fixture");
        process
            .execute_batch(PROCESS_SCHEMA)
            .expect("create process constraint fixture");
        let process_columns = "process_id, incarnation, registration_fingerprint, originator_id,
            identity_kind, created_at_ms, updated_at_ms, last_event_sequence, change_seq,
            status, parent_scope_kind, parent_scope_id, on_parent_end, record_json";
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('bad-status', 1, 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'paused', 'host', NULL, 'abandon', '{{}}')"
            ),
            "ck_processes_status",
        );
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('bad-parent-kind', 1, 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'running', 'session', 'scope', 'abandon', '{{}}')"
            ),
            "ck_processes_parent_scope_kind",
        );
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('host-with-id', 1, 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'running', 'host', 'scope', 'abandon', '{{}}')"
            ),
            "ck_processes_parent_scope_id",
        );
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('turn-without-id', 1, 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'running', 'turn', NULL, 'abandon', '{{}}')"
            ),
            "ck_processes_parent_scope_id",
        );
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('bad-on-parent-end', 1, 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'running', 'host', NULL, 'detach', '{{}}')"
            ),
            "ck_processes_on_parent_end",
        );
        assert_check_rejects(
            &process,
            "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
             VALUES ('host', 'scope', '{}', 0)",
            "ck_parent_end_plans_kind",
        );
        process
            .execute_batch(&format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('wake-parent', 1, 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'running', 'host', NULL, 'abandon', '{{}}')"
            ))
            .expect("insert valid wake parent");
        assert_check_rejects(
            &process,
            "INSERT INTO process_wake_deliveries (
                 delivery_id, process_id, process_incarnation, target_session_id, sequence, state,
                 next_attempt_at_ms, expires_at_ms, delivery_json
             ) VALUES ('bad-state', 'wake-parent', 1, 'target', 1, 'claimed', 0, 1, '{}')",
            "ck_process_wake_deliveries_state",
        );
        assert_check_rejects(
            &process,
            "INSERT INTO process_wake_deliveries (
                 delivery_id, process_id, process_incarnation, target_session_id, sequence, state,
                 next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
             ) VALUES (
                 'bad-discard', 'wake-parent', 1, 'target', 2, 'discarded', 0, 1,
                 'unroutable', '{}'
             )",
            "ck_process_wake_deliveries_discard_reason",
        );
        assert_check_rejects(
            &process,
            "INSERT INTO tool_intent_submissions (
                 replay_key, session_id, execution_scope_id, tool_call_id,
                 intent_index, kind, payload_hash, submission_json
             ) VALUES ('bad-tool-kind', 'session', 'scope', 'call', 0,
                       'restart_process', 'hash', '{}')",
            "ck_tool_intent_submissions_kind",
        );

        let triggers = Connection::open_in_memory().expect("open trigger constraint fixture");
        triggers
            .execute_batch(TRIGGER_SCHEMA)
            .expect("create trigger constraint fixture");
        assert_check_rejects(
            &triggers,
            "INSERT INTO trigger_subscriptions (
                 subscription_id, owner_scope, subscription_key, incarnation, revision,
                 definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
                 created_at_ms, updated_at_ms, record_json
             ) VALUES (
                 'bad-vocabulary', 'owner', 'key', 'incarnation', 1, 'fingerprint',
                 'source', 'key', 'archived', NULL, 0, 0, '{}'
             )",
            "ck_trigger_subscriptions_lifecycle",
        );
        assert_check_rejects(
            &triggers,
            "INSERT INTO trigger_subscriptions (
                 subscription_id, owner_scope, subscription_key, incarnation, revision,
                 definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
                 created_at_ms, updated_at_ms, record_json
             ) VALUES (
                 'tombstone-without-time', 'owner', 'key', 'incarnation', 1, 'fingerprint',
                 'source', 'key', 'tombstoned', NULL, 0, 0, '{}'
             )",
            "ck_trigger_subscriptions_lifecycle_deleted_at",
        );
        assert_check_rejects(
            &triggers,
            "INSERT INTO trigger_subscriptions (
                 subscription_id, owner_scope, subscription_key, incarnation, revision,
                 definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
                 created_at_ms, updated_at_ms, record_json
             ) VALUES (
                 'live-with-a-deletion-time', 'owner', 'key', 'incarnation', 1, 'fingerprint',
                 'source', 'key', 'enabled', 7, 0, 0, '{}'
             )",
            "ck_trigger_subscriptions_lifecycle_deleted_at",
        );
        assert_check_rejects(
            &triggers,
            "INSERT INTO trigger_mutation_receipts (
                 operation_id, owner_kind, owner_id,
                 request_fingerprint, result_json, created_at_ms
             ) VALUES ('bad-owner-kind', 'workflow', 'owner', 'fingerprint', '{}', 0)",
            "ck_trigger_receipts_owner_kind",
        );

        let effects = Connection::open_in_memory().expect("open effect constraint fixture");
        effects
            .execute_batch(EFFECT_SCHEMA)
            .expect("create effect constraint fixture");
        assert_check_rejects(
            &effects,
            "INSERT INTO runtime_effect_replay (
                 scope_id, replay_key, envelope_hash, envelope_json, status,
                 created_at_ms, updated_at_ms
             ) VALUES ('scope', 'bad-effect-status', 'hash', '{}', 'cancelled', 0, 0)",
            "ck_runtime_effect_replay_status",
        );
        assert_check_rejects(
            &effects,
            "INSERT INTO runtime_effect_group (
                 group_key, scope_id, session_id, wake, loser_disposition,
                 expected_children, next_seq, next_commit_seq, created_at_ms
             ) VALUES ('bad-wake', 'scope', 'session', 'majority', 'cancel', 0, 0, 0, 0)",
            "ck_runtime_effect_group_wake",
        );
        assert_check_rejects(
            &effects,
            "INSERT INTO runtime_effect_group (
                 group_key, scope_id, session_id, wake, loser_disposition,
                 expected_children, next_seq, next_commit_seq, created_at_ms
             ) VALUES ('bad-disposition', 'scope', 'session', 'all', 'retry', 0, 0, 0, 0)",
            "ck_runtime_effect_group_loser_disposition",
        );
    }
}
