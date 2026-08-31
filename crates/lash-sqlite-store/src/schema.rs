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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreBacking {
    File,
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaRemedy {
    DeleteDatabase,
    RecreateTrustDomain,
}

#[derive(Clone, Copy)]
struct SqliteDatabaseDefinition {
    name: &'static str,
    schema: &'static str,
    version: i32,
    remedy: SchemaRemedy,
}

/// One of the four independently versioned SQLite databases a lash deployment
/// can hold.
///
/// The variant is the single table for each database's schema SQL, version,
/// operator-facing name, and schema-drift remedy.
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
    const fn definition(self) -> SqliteDatabaseDefinition {
        match self {
            Self::DurableCore => SqliteDatabaseDefinition {
                name: "durable core",
                schema: SCHEMA,
                version: SCHEMA_VERSION,
                remedy: SchemaRemedy::DeleteDatabase,
            },
            Self::ProcessRegistry => SqliteDatabaseDefinition {
                name: "process registry",
                schema: PROCESS_SCHEMA,
                version: PROCESS_SCHEMA_VERSION,
                remedy: SchemaRemedy::DeleteDatabase,
            },
            Self::Triggers => SqliteDatabaseDefinition {
                name: "trigger store",
                schema: TRIGGER_SCHEMA,
                version: TRIGGER_SCHEMA_VERSION,
                remedy: SchemaRemedy::DeleteDatabase,
            },
            Self::EffectReplay => SqliteDatabaseDefinition {
                name: "effect replay",
                schema: EFFECT_SCHEMA,
                version: EFFECT_SCHEMA_VERSION,
                remedy: SchemaRemedy::RecreateTrustDomain,
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

    fn remedy(self) -> SchemaRemedy {
        self.definition().remedy
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
    generation     INTEGER NOT NULL CHECK (generation >= 0),
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
    fork_generation    INTEGER NOT NULL CHECK (fork_generation >= 0),
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
    observer_inheritance_kind         TEXT,
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')),
    CONSTRAINT ck_session_meta_observer_inheritance_kind CHECK (observer_inheritance_kind IN ('all', 'none', 'only'))
);

CREATE TABLE IF NOT EXISTS session_meta_pending_observer_intents (
    session_id    TEXT NOT NULL,
    process_index INTEGER NOT NULL,
    process_id    TEXT NOT NULL,
    process_incarnation INTEGER,
    attribution TEXT NOT NULL CHECK (attribution IN ('host_requested', 'fork_inherited')),
    PRIMARY KEY (session_id, process_id),
    UNIQUE (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES session_meta(session_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS session_meta_fork_inheritance_processes (
    session_id    TEXT NOT NULL,
    process_index INTEGER NOT NULL,
    process_id    TEXT NOT NULL,
    PRIMARY KEY (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES session_meta(session_id) ON DELETE CASCADE
);

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
    CHECK ((request_identity_hash IS NULL) = (requested_node_count IS NULL) AND (request_identity_hash IS NULL) = (identity_encoding_version IS NULL))
);

CREATE TABLE IF NOT EXISTS turn_cancel_requests (
    session_id TEXT NOT NULL,
    turn_id    TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS session_execution_leases (
    session_id               TEXT PRIMARY KEY,
    lease_owner_id           TEXT,
    lease_owner_incarnation_id TEXT,
    lease_executor_id        TEXT,
    lease_owner_liveness_json TEXT,
    lease_token              TEXT,
    lease_fencing_token      INTEGER NOT NULL DEFAULT 0,
    lease_claimed_at_ms      INTEGER NOT NULL DEFAULT 0,
    lease_term_ms            INTEGER NOT NULL DEFAULT 0,
    lease_expires_at_ms      INTEGER NOT NULL DEFAULT 0
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
    claim_id          TEXT,
    claim_owner_id    TEXT,
    claim_owner_incarnation_id TEXT,
    claim_owner_liveness_json TEXT,
    claim_token       TEXT,
    claim_fencing_token INTEGER NOT NULL DEFAULT 0,
    claim_session_lease_generation INTEGER NOT NULL DEFAULT 0,
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
    enqueued_at_ms    INTEGER NOT NULL,
    claim_id          TEXT,
    claim_owner_id    TEXT,
    claim_owner_incarnation_id TEXT,
    claim_owner_liveness_json TEXT,
    claim_token       TEXT,
    claim_fencing_token INTEGER NOT NULL DEFAULT 0,
    claim_session_lease_generation INTEGER NOT NULL DEFAULT 0,
    UNIQUE (session_id, source_key)
        ON CONFLICT IGNORE
);

CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_session
    ON pending_turn_inputs(session_id, state, enqueue_seq);

CREATE INDEX IF NOT EXISTS idx_pending_turn_input_order
    ON pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq);

CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_claim
    ON pending_turn_inputs(session_id, claim_id, claim_token);

CREATE TABLE IF NOT EXISTS attachment_manifest (
    attachment_id    TEXT NOT NULL,
    session_id       TEXT NOT NULL,
    canonical_uri    TEXT NOT NULL,
    intent_at_ms     INTEGER NOT NULL,
    committed_at_ms  INTEGER,
    owner_kind       TEXT CHECK (owner_kind IN ('turn', 'process')),
    owner_id         TEXT,
    CHECK ((owner_kind IS NULL) = (owner_id IS NULL)),
    PRIMARY KEY (session_id, attachment_id)
);

-- Attachment GC fence state, one row per condemned digest. Deliberately
-- timestampless: the protocol is CAS transitions only (see
-- `lash_core::AttachmentCondemnation`), never an expiry.
CREATE TABLE IF NOT EXISTS attachment_condemnations (
    attachment_id TEXT PRIMARY KEY,
    phase         TEXT NOT NULL CHECK (phase IN ('condemned', 'deleting'))
);

CREATE TABLE IF NOT EXISTS artifact_refs (
    namespace    TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    blob_ref     TEXT NOT NULL,
    PRIMARY KEY (namespace, artifact_ref)
);

CREATE INDEX IF NOT EXISTS idx_attachment_manifest_session
    ON attachment_manifest(session_id, committed_at_ms);
CREATE INDEX IF NOT EXISTS idx_attachment_manifest_uncommitted
    ON attachment_manifest(committed_at_ms)
    WHERE committed_at_ms IS NULL;
CREATE INDEX IF NOT EXISTS idx_attachment_manifest_owner
    ON attachment_manifest(session_id, owner_kind, owner_id, committed_at_ms);
CREATE INDEX IF NOT EXISTS idx_artifact_refs_blob_ref
    ON artifact_refs(blob_ref);
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
/// schema, so a version-43 file written by an older binary self-heals into the
/// newer index set on first open, and a newer file stays readable by the older
/// binary — the two are mutually compatible on the same path. Bumping instead
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
/// attributed table and removes the relation-wrapper depth counter. Version 43
/// is migrated forward in place; older stores remain recreate-only.
/// Version 45 switches content and semantic identities to domain-tagged BLAKE3.
/// Existing stores are rejected rather than reinterpreting SHA-256 rows.
/// Version 46 adds DDL-enforced session relation, causal-reference, and observer-
/// inheritance vocabularies. Existing durable-core catalogs are rejected rather
/// than migrated.
pub(crate) const SCHEMA_VERSION: i32 = 46;

const SESSION_43_TO_44_MIGRATION: &str = "
CREATE TABLE session_meta_pending_observer_intents (
    session_id          TEXT NOT NULL,
    process_index       INTEGER NOT NULL,
    process_id          TEXT NOT NULL,
    process_incarnation INTEGER,
    attribution         TEXT NOT NULL CHECK (attribution IN ('host_requested', 'fork_inherited')),
    PRIMARY KEY (session_id, process_id),
    UNIQUE (session_id, process_index),
    FOREIGN KEY (session_id) REFERENCES session_meta(session_id) ON DELETE CASCADE
);

WITH candidates AS (
    SELECT session_id, process_id, 0 AS attribution_rank,
           layer_index AS source_group, process_index AS source_index,
           'host_requested' AS attribution
      FROM session_meta_observer_intent_processes
    UNION ALL
    SELECT session_id, process_id, 1 AS attribution_rank,
           0 AS source_group, process_index AS source_index,
           'fork_inherited' AS attribution
      FROM session_meta_fork_pending_observer_processes
), occurrences AS (
    SELECT *, ROW_NUMBER() OVER (
        PARTITION BY session_id, process_id
        ORDER BY attribution_rank, source_group, source_index
    ) AS occurrence
      FROM candidates
), indexed AS (
    SELECT session_id, process_id, attribution,
           ROW_NUMBER() OVER (
               PARTITION BY session_id
               ORDER BY attribution_rank, source_group, source_index, process_id
           ) - 1 AS process_index
      FROM occurrences
     WHERE occurrence = 1
)
INSERT INTO session_meta_pending_observer_intents
    (session_id, process_index, process_id, process_incarnation, attribution)
SELECT session_id, process_index, process_id, NULL, attribution
  FROM indexed;

DROP TABLE session_meta_observer_intent_processes;
DROP TABLE session_meta_fork_pending_observer_processes;
ALTER TABLE session_meta DROP COLUMN observer_intent_depth;
";

pub(crate) const PROCESS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS processes (
    process_id            TEXT PRIMARY KEY,
    registration_fingerprint     TEXT NOT NULL,
    originator_id         TEXT NOT NULL,
    wake_session_id       TEXT,
    identity_kind         TEXT NOT NULL,
    identity_label        TEXT,
    is_waiting            INTEGER NOT NULL,
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL,
    change_seq            INTEGER NOT NULL,
    status                TEXT NOT NULL,
    record_json           TEXT NOT NULL,
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed'))
);

CREATE INDEX IF NOT EXISTS idx_processes_status
    ON processes(status);
CREATE INDEX IF NOT EXISTS idx_processes_live_worklist
    ON processes(process_id) WHERE status IN ('running', 'waiting');

CREATE INDEX IF NOT EXISTS idx_processes_change_seq
    ON processes(change_seq);
CREATE INDEX IF NOT EXISTS idx_processes_originator
    ON processes(originator_id);
CREATE INDEX IF NOT EXISTS idx_processes_identity
    ON processes(identity_kind, identity_label);
CREATE INDEX IF NOT EXISTS idx_processes_waiting
    ON processes(is_waiting);
CREATE INDEX IF NOT EXISTS idx_processes_created
    ON processes(created_at_ms);
CREATE INDEX IF NOT EXISTS idx_processes_recent_retired
    ON processes(updated_at_ms, process_id)
    WHERE status NOT IN ('running', 'waiting');
CREATE INDEX IF NOT EXISTS idx_processes_wake_session
    ON processes(wake_session_id);

CREATE TABLE IF NOT EXISTS process_change_clock (
    singleton    INTEGER PRIMARY KEY CHECK (singleton = 1),
    current_seq  INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO process_change_clock (singleton, current_seq)
VALUES (1, 0);

CREATE TABLE IF NOT EXISTS process_events (
    process_id        TEXT NOT NULL,
    sequence          INTEGER NOT NULL,
    event_type        TEXT NOT NULL,
    idempotency_key   TEXT,
    occurred_at_ms    INTEGER NOT NULL,
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

CREATE TABLE IF NOT EXISTS process_leases (
    process_id       TEXT PRIMARY KEY,
    lease_owner_id   TEXT,
    lease_owner_incarnation_id TEXT,
    lease_owner_liveness_json TEXT,
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
    PRIMARY KEY (process_id, segment_ordinal),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS process_parent_end_plans (
    process_id       TEXT PRIMARY KEY,
    actions_json     TEXT NOT NULL,
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

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
pub(crate) const PROCESS_SCHEMA_VERSION: i32 = 26;

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
    enabled              INTEGER NOT NULL,
    tombstoned           INTEGER NOT NULL,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    record_json          TEXT NOT NULL,
    CONSTRAINT ck_trigger_subscriptions_live_enabled CHECK (NOT (enabled AND tombstoned)),
    UNIQUE(owner_scope, subscription_key)
);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_registrant
    ON trigger_subscriptions(owner_scope, subscription_key);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_source
    ON trigger_subscriptions(source_type, source_key, enabled);

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
    request_fingerprint    TEXT NOT NULL,
    result_json     TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL
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
// Existing trigger stores are rejected rather than migrated.
pub(crate) const TRIGGER_SCHEMA_VERSION: i32 = 8;

pub(crate) const EFFECT_SCHEMA: &str = "
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
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    CONSTRAINT ck_runtime_effect_replay_status CHECK (status IN ('in_progress', 'completed', 'failed')),
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

CREATE INDEX IF NOT EXISTS idx_runtime_effect_replay_group_unsettled
    ON runtime_effect_replay(group_key, replay_key)
    WHERE group_key IS NOT NULL AND settlement_seq IS NULL;

CREATE TABLE IF NOT EXISTS runtime_effect_group (
    group_key          TEXT PRIMARY KEY,
    scope_id           TEXT NOT NULL,
    session_id         TEXT,
    wake               TEXT NOT NULL,
    loser_disposition  TEXT NOT NULL,
    children           INTEGER NOT NULL,
    next_seq           INTEGER NOT NULL DEFAULT 0,
    created_at_ms      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_group_session
    ON runtime_effect_group(session_id);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_group_scope
    ON runtime_effect_group(scope_id);

CREATE TABLE IF NOT EXISTS await_event_meta (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    signing_secret  BLOB NOT NULL
);

INSERT INTO await_event_meta (singleton, signing_secret)
VALUES (1, randomblob(32))
ON CONFLICT(singleton) DO NOTHING;

CREATE TABLE IF NOT EXISTS await_event_waits (
    key_id          TEXT PRIMARY KEY,
    scope_json      TEXT NOT NULL,
    wait_json       TEXT NOT NULL,
    session_id      TEXT,
    turn_control    INTEGER NOT NULL CHECK (turn_control IN (0, 1)),
    terminal_json   TEXT,
    created_at_ms   INTEGER NOT NULL,
    resolved_at_ms  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_await_event_waits_session
    ON await_event_waits(session_id);

-- Permanent by design: session ids cannot be reused, so revocation evidence
-- must remain after every retention-pruning pass.
CREATE TABLE IF NOT EXISTS await_event_revoked_sessions (
    session_id      TEXT PRIMARY KEY,
    revoked_at_ms   INTEGER NOT NULL
);
";

// Version 6 keys session-owned effects by the permanent session id and removes
// the incarnation join column. Effect databases follow the crate's alpha
// reject-and-recreate convention rather than carrying a migration chain.
// Version 7 rejects live-serde await-event and direct replay identities.
// Version 8 rejects the former live-serde tool-batch and process-transfer
// replay names.
// Version 9 rejects completed tool-attempt outcomes whose frame-switch control
// still carries the pre-cutover `frame_id` field.
// Version 10 adds the versioned tool-intent carrier to recorded tool-attempt
// outcomes and the typed execution outcomes to completed tool batches.
// Version 11 adds the durable effect-group journal (FIG-1416, ADR 0065): the
// `runtime_effect_group` counter row, the `group_key`/`settlement_seq` columns
// that seat a child in its group's settlement order, and the unique backstop
// over the pair. Effect databases follow the crate's reject-and-recreate
// convention, so an existing effect database is deleted on upgrade rather than
// migrated — release-notes material, not a host's discovery.
// Version 12 removes the duplicated `LlmResponse.full_text` member from
// runtime-effect outcomes. Pre-cutover journal JSON is upgraded only at the
// effect replay decode boundary.
// Version 13 merges each journaled exec observation with its projection
// metadata. Pre-13 effect databases are rejected at open; there is no migration
// arm.
//
// The index-only carve-out documented on `SCHEMA_VERSION` applies here for the
// same reason and with the same limit: an additive non-unique
// `CREATE INDEX IF NOT EXISTS` self-heals into an existing effect database on
// open and leaves it readable by an older binary, so it does not bump — while
// any table, column, unique index, or semantic change still does.
// `idx_runtime_effect_replay_group_unsettled` (FIG-1564) is added under it,
// because bumping would delete live effect databases to buy a query plan. That
// index serves `read_unsettled_group_children`, whose predicate is the opposite
// half of the unique backstop's: without it the read scans the whole effect
// journal on every child completion after a close and on every drain pass
// (FIG-1536). `replay_key` trails the group key so the read's `ORDER BY` is the
// index order and the plan needs no sort.
//
// The rationale lives here rather than beside the statement on purpose: the
// version guard's projection elides a *new* index statement but not the SQL
// comments around it, so prose inside `EFFECT_SCHEMA` would demand the very
// bump the carve-out exists to avoid.
// Version 14 switches durable effect identities to domain-tagged BLAKE3.
// Version 15 adds the DDL-enforced effect-replay status vocabulary. Existing
// effect journals are rejected rather than migrated.
pub(crate) const EFFECT_SCHEMA_VERSION: i32 = 15;

pub(crate) async fn apply_pragmas(
    conn: &SqliteConnection,
    backing: StoreBacking,
) -> rusqlite::Result<()> {
    // WAL + busy_timeout are already applied in `SqliteConnection::open` /
    // `open_in_memory`. The remaining tuning PRAGMAs match the prior store. The
    // `backing` argument is retained so the lifecycle call sites read the same
    // as the prior store port; WAL is only meaningful for file-backed databases.
    let _ = backing;
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
    let user_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version == schema_version {
        tx.execute_batch(database.schema())?;
        return Ok(tx);
    }
    if user_version == 0 && !has_user_schema_objects(&tx)? {
        tx.execute_batch(database.schema())?;
        tx.pragma_update(None, "user_version", schema_version)?;
        return Ok(tx);
    }
    // Deliberately historical: tests pin the 43-to-44 migration, but the arm is
    // unreachable for production opens now that SCHEMA_VERSION is 46.
    if database == SqliteDatabase::DurableCore && user_version == 43 && schema_version == 44 {
        tx.execute_batch(SESSION_43_TO_44_MIGRATION)?;
        tx.execute_batch(database.schema())?;
        tx.pragma_update(None, "user_version", schema_version)?;
        return Ok(tx);
    }
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
        Some(unsupported_schema_message(
            database,
            schema_version,
            user_version,
        )),
    ))
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

/// Build the error message for an unsupported on-disk schema. The expected and
/// found `PRAGMA user_version` values are reported accurately. Effect replay is
/// part of the trust domain documented in `docs/persistence.html#delete-sessions`,
/// so its remedy must never prescribe an independent database wipe.
pub(crate) fn unsupported_schema_message(
    database: SqliteDatabase,
    expected_version: i32,
    found_version: i32,
) -> String {
    let remedy = match database.remedy() {
        SchemaRemedy::DeleteDatabase => {
            format!("delete the {} database and start fresh.", database.name())
        }
        SchemaRemedy::RecreateTrustDomain => {
            "drain affected sessions and recreate the whole Lash trust domain with this version. \
         Reset the tombstones, await-event revocation ledger, effect journal, and Restate state \
         together; see docs/persistence.html#delete-sessions."
                .to_string()
        }
    };
    format!(
        "Unsupported lash {} schema: this binary supports schema version {expected_version}, but \
         the database reports version {found_version}. There is no \
         migration chain — {remedy}",
        database.name()
    )
}

#[cfg(test)]
mod observer_intent_migration_tests {
    use super::*;

    #[test]
    fn historical_component_43_to_44_migration_stays_pinned() {
        let mut connection = Connection::open_in_memory().expect("open migration fixture");
        prepare_versioned_schema_at_version(&mut connection, SqliteDatabase::DurableCore, 44)
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

        prepare_versioned_schema_at_version(&mut connection, SqliteDatabase::DurableCore, 44)
            .expect("migrate component 43")
            .commit()
            .expect("commit component-44 migration");

        let rows = connection
            .prepare(
                "SELECT process_index, process_id, process_incarnation, attribution
                 FROM session_meta_pending_observer_intents
                 WHERE session_id = 'fold-session' ORDER BY process_index",
            )
            .expect("prepare folded intent read")
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("read folded intents")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode folded intents");
        assert_eq!(
            rows,
            vec![
                (
                    0,
                    "shared-process".to_string(),
                    None,
                    "host_requested".to_string()
                ),
                (
                    1,
                    "host-only-process".to_string(),
                    None,
                    "host_requested".to_string()
                ),
                (
                    2,
                    "fork-only-process".to_string(),
                    None,
                    "fork_inherited".to_string()
                ),
            ]
        );
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("read migrated version"),
            44
        );
        assert!(
            connection
                .execute(
                    "UPDATE session_meta SET observer_intent_depth = 1 WHERE session_id = 'fold-session'",
                    [],
                )
                .is_err(),
            "the removed depth counter cannot represent a rows/counter disagreement"
        );
        assert!(
            connection
                .execute(
                    "INSERT INTO session_meta_pending_observer_intents
                     (session_id, process_index, process_id, attribution)
                     VALUES ('fold-session', 3, 'invalid-process', 'relation_injected')",
                    [],
                )
                .is_err(),
            "the attribution CHECK must reject relation-borne intent labels"
        );
    }
}

#[cfg(test)]
mod schema_metadata_tests {
    use super::*;

    #[test]
    fn effect_replay_selects_the_trust_domain_remedy_structurally() {
        assert_eq!(
            SqliteDatabase::EffectReplay.remedy(),
            SchemaRemedy::RecreateTrustDomain
        );
        assert_eq!(
            SqliteDatabase::DurableCore.remedy(),
            SchemaRemedy::DeleteDatabase
        );
    }
}

#[cfg(test)]
mod check_constraint_tests {
    use super::*;

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
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind)
             VALUES ('bad-relation', 'sibling')",
            "ck_session_meta_relation_kind",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (session_id, relation_kind, caused_by_kind)
             VALUES ('bad-cause', 'child', 'timer')",
            "ck_session_meta_caused_by_kind",
        );
        assert_check_rejects(
            &core,
            "INSERT INTO session_meta (
                 session_id, relation_kind, observer_inheritance_kind
             ) VALUES ('bad-inheritance', 'fork', 'selected')",
            "ck_session_meta_observer_inheritance_kind",
        );

        let process = Connection::open_in_memory().expect("open process constraint fixture");
        process
            .execute_batch(PROCESS_SCHEMA)
            .expect("create process constraint fixture");
        let process_columns = "process_id, registration_fingerprint, originator_id,
            identity_kind, is_waiting, created_at_ms, updated_at_ms, change_seq,
            status, record_json";
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('bad-status', 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'paused', '{{}}')"
            ),
            "ck_processes_status",
        );
        process
            .execute_batch(&format!(
                "INSERT INTO processes ({process_columns}) VALUES
                 ('wake-parent', 'fingerprint', 'originator', 'standard', 0, 0, 0, 0,
                  'running', '{{}}')"
            ))
            .expect("insert valid wake parent");
        assert_check_rejects(
            &process,
            "INSERT INTO process_wake_deliveries (
                 delivery_id, process_id, target_session_id, sequence, state,
                 next_attempt_at_ms, expires_at_ms, delivery_json
             ) VALUES ('bad-state', 'wake-parent', 'target', 1, 'claimed', 0, 1, '{}')",
            "ck_process_wake_deliveries_state",
        );
        assert_check_rejects(
            &process,
            "INSERT INTO process_wake_deliveries (
                 delivery_id, process_id, target_session_id, sequence, state,
                 next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
             ) VALUES (
                 'bad-discard', 'wake-parent', 'target', 2, 'discarded', 0, 1,
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
                 definition_fingerprint, source_type, source_key, enabled, tombstoned,
                 created_at_ms, updated_at_ms, record_json
             ) VALUES (
                 'bad-pair', 'owner', 'key', 'incarnation', 1, 'fingerprint',
                 'source', 'key', 1, 1, 0, 0, '{}'
             )",
            "ck_trigger_subscriptions_live_enabled",
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
    }
}
