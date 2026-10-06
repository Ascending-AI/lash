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
pub(crate) use crate::trigger_schema::TRIGGER_SCHEMA;

mod database;
pub use database::SqliteDatabase;

/// Canonical SQLite schema for a factory-wide lash durable-core catalog.
///
/// This is the *only* schema the store supports. Older durable-core databases
/// must be deleted before opening with this binary. Lash's broader durable
/// contract still lives one level up in per-record `schema_version` stamps,
/// not in compatibility reads.
/// Each `checkpoint_blob_refs` row is owned by the session whose retained
/// revision owns the checkpoint root named by `checkpoint_ref`. Owner-scoped session
/// delete or process prune deletes an unreferenced root and cascades its edges
/// in the same transaction. Component blobs are shared and have no
/// component-side cascade.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS wait_receipts (
    wait_id TEXT PRIMARY KEY,
    owner_key TEXT NOT NULL,
    session_id TEXT,
    started_at_ms INTEGER NOT NULL,
    request_json TEXT NOT NULL,
    resolution_json TEXT,
    resolved_at_ms INTEGER,
    retired_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_wait_receipts_owner ON wait_receipts(owner_key);

CREATE TABLE IF NOT EXISTS tool_call_receipts (
    request_key TEXT PRIMARY KEY,
    session_id TEXT,
    owner_key TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    requested_at_ms INTEGER NOT NULL,
    request_json TEXT NOT NULL,
    completion_json TEXT,
    completed_at_ms INTEGER,
    retired_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_tool_call_receipts_retention ON tool_call_receipts(session_id, completed_at_ms);

CREATE TABLE IF NOT EXISTS worker_recovery (
    scope_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    cpu_nanos INTEGER NOT NULL,
    replacement INTEGER NOT NULL,
    unknown_cpu_attempts INTEGER NOT NULL,
    in_flight INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS blobs (
    hash    TEXT PRIMARY KEY,
    content BLOB NOT NULL
);

-- One row per published head revision a session still retains (FIG-4731).
-- The name of a state is (session_id, head_revision). Membership is the
-- retained-revisions relation every reclaimer roots what it keeps in. Each
-- row carries the head document its revision was published with: the
-- configuration a fork of it copies.
CREATE TABLE IF NOT EXISTS session_revisions (
    session_id     TEXT NOT NULL,
    head_revision  INTEGER NOT NULL,
    leaf_node_id   TEXT,
    checkpoint_ref TEXT,
    head_json      TEXT NOT NULL,
    PRIMARY KEY (session_id, head_revision)
);
CREATE INDEX IF NOT EXISTS idx_session_revisions_leaf
    ON session_revisions(leaf_node_id);
CREATE INDEX IF NOT EXISTS idx_session_revisions_checkpoint_ref
    ON session_revisions(checkpoint_ref);

-- The current revision is one pointer; per-revision facts live in revisions.
CREATE TABLE IF NOT EXISTS session_head (
    session_id TEXT PRIMARY KEY,
    head_revision INTEGER NOT NULL,
    pending_follow_on_json TEXT,
    FOREIGN KEY (session_id, head_revision)
        REFERENCES session_revisions(session_id, head_revision)
        DEFERRABLE INITIALLY DEFERRED
);

-- One row per target a host asked a session to retain: an input, a turn or a
-- head revision. A pin names a target, never a state, so it can be written
-- before the target exists. Pins are deleted with their session.
CREATE TABLE IF NOT EXISTS pins (
    session_id  TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id   TEXT NOT NULL,
    PRIMARY KEY (session_id, target_kind, target_id),
    CONSTRAINT ck_pins_target_kind CHECK (target_kind IN ('input', 'turn', 'revision'))
);

-- Indexed projection of the exact manifest -> component edges carried in each
-- checkpoint blob. Each row is owned by the session whose retained revision owns
-- the checkpoint root named by checkpoint_ref. Owner-scoped session delete or
-- process prune deletes an unreferenced root and cascades its edges in the same
-- transaction. Components are shared and have no component-side cascade. This
-- is reference data, never a cached reference count.
CREATE TABLE IF NOT EXISTS checkpoint_blob_refs (
    checkpoint_ref TEXT NOT NULL,
    blob_ref       TEXT NOT NULL,
    PRIMARY KEY (checkpoint_ref, blob_ref),
    FOREIGN KEY (checkpoint_ref) REFERENCES blobs(hash) ON DELETE CASCADE,
    FOREIGN KEY (blob_ref) REFERENCES blobs(hash)
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
    shift_epoch                       INTEGER NOT NULL DEFAULT 0,
    shift_admission_id                TEXT,
    shift_run_start                  TEXT,
    admission_base_checkpoint_ref     TEXT,
    closing_intent                    INTEGER,
    owning_process_id                 TEXT,
    retention_kind                    TEXT NOT NULL DEFAULT 'until_gc',
    retention_last_turns              INTEGER,
    obligation_id                    TEXT,
    obligation_state                 TEXT,
    obligation_attempts              INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms             INTEGER,
    obligation_claim_token           TEXT,
    obligation_stall_reason          TEXT,
    obligation_last_error            TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_session_meta_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms         INTEGER,
    -- The session's standing fault (ADR 0109 §9) and when it was recorded.
    fault_json                       TEXT,
    fault_at_ms                      INTEGER CONSTRAINT ck_session_meta_fault CHECK ((fault_json IS NULL) = (fault_at_ms IS NULL)),
    -- The shift authority holds exactly the states a raise writes: unraised,
    -- sealed by an execution (its start marker), or raised by a control verb
    -- (no marker). A closing session was raised by its close.
    CONSTRAINT ck_session_meta_shift_authority CHECK ((shift_epoch = 0 AND shift_admission_id IS NULL AND shift_run_start IS NULL AND closing_intent IS NULL) OR (shift_epoch > 0 AND shift_admission_id IS NOT NULL AND (shift_run_start IS NULL OR closing_intent IS NULL))),
    CONSTRAINT ck_session_meta_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_session_meta_retention CHECK ((retention_kind IN ('until_gc', 'head_only') AND retention_last_turns IS NULL) OR (retention_kind = 'last_turns' AND retention_last_turns > 0)),
    CONSTRAINT ck_session_meta_relation_kind CHECK (relation_kind IN ('root', 'child', 'fork')),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')),
    CONSTRAINT ck_session_meta_relation_family CHECK ((relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))),
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
-- The fault listing (ADR 0109 §9).
CREATE INDEX IF NOT EXISTS idx_session_meta_fault
    ON session_meta(session_id)
    WHERE fault_json IS NOT NULL;

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
    outcome_code                TEXT CONSTRAINT ck_runtime_turn_commits_outcome CHECK (outcome_code IN ('completed', 'frame_switch', 'segment_boundary', 'cancelled', 'failed_incomplete', 'failed_invalid_input', 'failed_max_turns', 'failed_tool_failure', 'failed_provider_error', 'failed_context_overflow', 'failed_plugin_abort', 'failed_runtime_error', 'failed_submitted_error', 'failed_tool_error')),
    change_seq INTEGER NOT NULL UNIQUE CONSTRAINT ck_runtime_turn_commits_change_seq CHECK (change_seq > 0),
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


CREATE INDEX IF NOT EXISTS idx_runtime_turn_commits_change_seq
    ON runtime_turn_commits(change_seq) WHERE outcome_code IS NOT NULL;

-- Transactional clock: a cursor never overtakes an uncommitted terminal.
CREATE TABLE IF NOT EXISTS turn_change_clock (
    singleton INTEGER PRIMARY KEY CONSTRAINT ck_turn_change_clock_singleton CHECK (singleton = 1),
    current_seq INTEGER NOT NULL CONSTRAINT ck_turn_change_clock_current_seq CHECK (current_seq >= 0),
    retention_horizon INTEGER NOT NULL CONSTRAINT ck_turn_change_clock_retention_horizon CHECK (retention_horizon >= 0 AND retention_horizon <= current_seq)
);
INSERT OR IGNORE INTO turn_change_clock VALUES (1, 0, 0);

-- Session faults outlive their standing state and the session's physical delete.
CREATE TABLE IF NOT EXISTS session_terminal_changes (
    change_seq INTEGER PRIMARY KEY CONSTRAINT ck_session_terminal_changes_change_seq CHECK (change_seq > 0),
    session_id TEXT NOT NULL,
    fault_json TEXT,
    recorded_at_ms INTEGER NOT NULL CONSTRAINT ck_session_terminal_changes_recorded_at_ms CHECK (recorded_at_ms >= 0)
);
CREATE INDEX IF NOT EXISTS idx_session_terminal_changes_session
    ON session_terminal_changes(session_id, change_seq);

CREATE TABLE IF NOT EXISTS turn_cancel_requests (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    origin TEXT,
    reason TEXT,
    disposition TEXT NOT NULL CONSTRAINT ck_turn_cancel_requests_disposition CHECK (disposition IN ('defer', 'drop')),
    mode TEXT NOT NULL CONSTRAINT ck_turn_cancel_requests_mode CHECK (mode IN ('immediate', 'after_step')),
    intent_revision INTEGER NOT NULL CONSTRAINT ck_turn_cancel_requests_intent_revision CHECK (intent_revision >= 1),
    PRIMARY KEY (session_id, turn_id)
);

-- Affected-input evidence for a cancellation receipt, one row per input.
-- `input_json` deliberately snapshots the pending-input payload at
-- disposition time: the pending row is vacuum-eligible once settled, and the
-- receipt must stay readable afterwards. The (session_id, turn_id, item_kind, input_id)
-- uniqueness makes duplicate evidence impossible rather than checked.
CREATE TABLE IF NOT EXISTS turn_cancel_affected_inputs (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CONSTRAINT ck_turn_cancel_affected_inputs_ordinal CHECK (ordinal >= 0),
    input_id TEXT NOT NULL,
    disposition TEXT NOT NULL CONSTRAINT ck_turn_cancel_affected_inputs_disposition CHECK (disposition IN ('defer', 'drop')),
    input_json TEXT NOT NULL,
    item_kind TEXT NOT NULL,
    batch_id TEXT,
    CONSTRAINT ck_turn_cancel_affected_inputs_item_kind CHECK ((item_kind = 'input' AND batch_id IS NULL) OR (item_kind = 'process_wake' AND batch_id IS NOT NULL AND input_id = batch_id AND disposition = 'defer')),
    PRIMARY KEY (session_id, turn_id, ordinal),
    UNIQUE (session_id, turn_id, item_kind, input_id),
    FOREIGN KEY (session_id, turn_id) REFERENCES turn_cancel_requests (session_id, turn_id) ON DELETE CASCADE
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
    cause_json       TEXT,
    reason_json TEXT,
    at_ms       INTEGER NOT NULL,
    park_build_generation TEXT,
    redrive_intent INTEGER,
    CONSTRAINT ck_turn_park_events_parked_reason CHECK (((kind = 'parked' AND reason_json IS NOT NULL AND cause_json IS NULL AND redrive_intent IS NULL) OR (kind IN ('unparked', 'cancelled') AND reason_json IS NULL AND cause_json IS NOT NULL AND redrive_intent IS NULL AND park_build_generation IS NULL) OR (kind = 'redrive_requested' AND reason_json IS NULL AND cause_json IS NULL AND redrive_intent IS NOT NULL AND redrive_intent >= 0 AND park_build_generation IS NULL)) IS TRUE)
);

CREATE TABLE IF NOT EXISTS queued_work_batches (
    enqueue_seq       INTEGER NOT NULL,
    batch_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    delivery_policy   TEXT NOT NULL,
    work_kind         TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    authority_json    TEXT NOT NULL,
    merge_key         TEXT,
    enqueued_at_ms    INTEGER NOT NULL,
    submission_digest TEXT NOT NULL, -- Written once at admission (ADR 0101 §8).
    settled_operation_key TEXT, -- The original applying commit receipt; no separate completion marker.
    admitted_run     TEXT, -- The run whose fenced admission holds the batch; NULL while open.
    admitted_by       TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    terminal_cause    TEXT, -- NULL while open or admitted; the tombstone's cause after.
    terminal_at_ms    INTEGER,
    trace_cause_json  TEXT, -- The batch's trace cause, written once at enqueue; NULL is a root cause.
    obligation_id     TEXT,
    obligation_state  TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_queued_work_batches_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_queued_work_batches_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_queued_work_batches_work_kind CHECK ((json_valid(payload_json) AND json_type(payload_json) = 'object' AND ((work_kind = 'turn' AND json_extract(payload_json, '$.type') = 'process_wake') OR (work_kind = 'control' AND json_extract(payload_json, '$.type') = 'session_command'))) IS TRUE),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK (delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')),
    CONSTRAINT ck_queued_work_batches_admission_all_or_none CHECK ((admitted_run IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_queued_work_batches_terminal CHECK ((terminal_cause IS NULL AND terminal_at_ms IS NULL) OR (terminal_cause IN ('delivered', 'applied', 'cancelled', 'stale_config_revision') AND terminal_at_ms IS NOT NULL AND admitted_run IS NULL)),
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

CREATE TABLE IF NOT EXISTS wake_redelivery_fences (
    session_id       TEXT NOT NULL,
    process_id       TEXT NOT NULL,
    allocation_floor INTEGER NOT NULL,
    PRIMARY KEY (session_id, process_id)
);

-- Both scans range over live batches only: a tombstone never lengthens an
-- open-work scan (ADR 0101 §8).
CREATE INDEX IF NOT EXISTS idx_queued_work_session_command_order
    ON queued_work_batches(session_id, work_kind, enqueued_at_ms, enqueue_seq)
    WHERE terminal_cause IS NULL;

DROP INDEX IF EXISTS idx_queued_work_admitted;
CREATE INDEX IF NOT EXISTS idx_queued_work_admission_order
    ON queued_work_batches(session_id, admitted_run, enqueue_seq)
    WHERE terminal_cause IS NULL;

CREATE TABLE IF NOT EXISTS pending_turn_inputs (
    enqueue_seq       INTEGER NOT NULL,
    input_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    ingress_json      TEXT NOT NULL, -- The submitted delivery, written once (ADR 0101 §5.1).
    state             TEXT NOT NULL,
    input_json        TEXT NOT NULL,
    submission_digest TEXT NOT NULL,
    enqueued_at_ms    INTEGER NOT NULL,
    admitted_run     TEXT, -- The run whose fenced admission holds the input; NULL while open.
    admitted_by       TEXT, -- The recorded step that bound it: `admit` or a checkpoint's replay key.
    run_spec_hash     TEXT,
    terminal_at_ms    INTEGER, -- When the input's tombstone was written; NULL until then.
    trace_cause_json  TEXT, -- The submission's trace cause, written once at acceptance; NULL is a root cause.
    obligation_id     TEXT,
    obligation_state  TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_pending_turn_inputs_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_pending_turn_inputs_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_pending_turn_inputs_state CHECK (state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK ((json_extract(ingress_json, '$.scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR (json_extract(ingress_json, '$.scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))),
    CONSTRAINT ck_pending_turn_inputs_admission_all_or_none CHECK ((admitted_run IS NULL) = (admitted_by IS NULL)),
    CONSTRAINT ck_pending_turn_inputs_settled_unadmitted CHECK (admitted_run IS NULL OR state NOT IN ('cancelled', 'completed')),
    CONSTRAINT ck_pending_turn_inputs_terminal_at CHECK ((state IN ('cancelled', 'completed')) = (terminal_at_ms IS NOT NULL)),
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

-- All undelivered inputs, including ones a run already holds. State is the
-- partial predicate, so settled rows cannot lengthen an open-input scan.
-- Enqueue order in the key lets list_undelivered avoid a history scan or sort.
DROP INDEX IF EXISTS idx_pending_turn_inputs_open;
CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_open_state
    ON pending_turn_inputs(session_id, enqueue_seq)
    WHERE state IN ('pending_active', 'deferred_next_turn');

CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_accepted_state
    ON pending_turn_inputs(session_id, enqueue_seq)
    WHERE state IN ('accepted');

DROP INDEX IF EXISTS idx_pending_turn_inputs_admitted;
CREATE INDEX IF NOT EXISTS idx_pending_turn_inputs_bound_run
    ON pending_turn_inputs(session_id, admitted_run)
    WHERE admitted_run IS NOT NULL;

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
-- Exact attachment referrer edges (ADR 0124). The edge is the liveness fact.
-- The kind CHECK admits any non-empty label (ADR 0115 section 5): every write
-- binds ArtifactReferrerKind::as_str, and every read decodes through
-- ArtifactReferrer::decode.
CREATE TABLE IF NOT EXISTS attachment_referrer_edges (
    attachment_id TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_attachment CHECK (length(attachment_id) > 0),
    referrer_kind TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_kind CHECK (length(referrer_kind) > 0),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_attachment_referrer_edges_id CHECK (length(referrer_id) > 0),
    PRIMARY KEY (attachment_id, referrer_kind, referrer_id)
);
CREATE INDEX IF NOT EXISTS idx_attachment_referrer_edges_referrer
    ON attachment_referrer_edges(referrer_kind, referrer_id);

-- One row per granted write attempt that has neither completed nor aborted.
-- `write_id` is AttachmentWriteToken::as_hex.
CREATE TABLE IF NOT EXISTS attachment_pending_writes (
    write_id      TEXT PRIMARY KEY CONSTRAINT ck_attachment_pending_writes_write_id CHECK (length(write_id) = 32),
    attachment_id TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_attachment CHECK (length(attachment_id) > 0),
    referrer_kind TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_kind CHECK (length(referrer_kind) > 0),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_attachment_pending_writes_id CHECK (length(referrer_id) > 0),
    begun_at_ms   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_attachment_pending_writes_attachment
    ON attachment_pending_writes(attachment_id);
CREATE INDEX IF NOT EXISTS idx_attachment_pending_writes_referrer
    ON attachment_pending_writes(referrer_kind, referrer_id);

-- Upload evidence: one row per digest a completed write proved uploaded.
-- Deleted only by condemnation, under the digest fence.
CREATE TABLE IF NOT EXISTS attachment_uploads (
    attachment_id TEXT PRIMARY KEY CONSTRAINT ck_attachment_uploads_attachment CHECK (length(attachment_id) > 0),
    written_at_ms INTEGER NOT NULL
);

-- Attachment GC fence per condemned digest (ADR 0067 section 6). A claim is the
-- pending write that holds it; deleting that write releases the claim.
CREATE TABLE IF NOT EXISTS attachment_condemnations (
    attachment_id     TEXT PRIMARY KEY,
    phase             TEXT NOT NULL CONSTRAINT ck_attachment_condemnations_phase CHECK (phase IN ('condemned', 'deleting')),
    write_token       TEXT REFERENCES attachment_pending_writes(write_id) ON DELETE SET NULL,
    next_delete_at_ms BIGINT NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_next_delete CHECK (next_delete_at_ms >= 0),
    sweep_generation  INTEGER NOT NULL,
    delete_attempts   INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_attachment_condemnations_delete_attempts CHECK (delete_attempts >= 0),
    last_delete_error TEXT CONSTRAINT ck_attachment_condemnations_failure_pairing CHECK ((delete_attempts = 0) = (last_delete_error IS NULL)),
    stall_reason      TEXT CONSTRAINT ck_attachment_condemnations_stall_reason CHECK (stall_reason IN ('attempts_exhausted', 'refused')) CONSTRAINT ck_attachment_condemnations_stall_attempts CHECK (stall_reason IS NULL OR delete_attempts > 0),
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
CREATE TABLE IF NOT EXISTS referrer_fences (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_referrer_fences_kind CHECK (length(referrer_kind) > 0),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_referrer_fences_id CHECK (length(referrer_id) > 0),
    ended_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (referrer_kind, referrer_id)
);

-- Adoption asks one question of the whole table: does any row for this digest
-- carry upload evidence?
CREATE INDEX IF NOT EXISTS idx_artifact_refs_blob_ref
    ON artifact_refs(blob_ref);

CREATE INDEX IF NOT EXISTS idx_artifact_referrer_edges_referrer
    ON artifact_referrer_edges(referrer_kind, referrer_id);

CREATE TABLE IF NOT EXISTS artifact_cleanup_obligations (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_cleanup_obligations_database CHECK (length(referrer_kind) > 0 AND referrer_kind <> 'process_record'),
    referrer_id TEXT NOT NULL CHECK (length(referrer_id) > 0),
    cleanup_json TEXT NOT NULL,
    obligation_id TEXT NOT NULL,
    obligation_state TEXT NOT NULL,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_artifact_cleanup_obligations_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
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

-- The fleet record's per-plugin writer ranges (FIG-4746), beside `F`: the
-- format versions the fleet permits each plugin's state and config namespaces
-- to be published in. Every publication is admitted against its plugin's row
-- inside its write transaction, and only finalize moves a recorded range, in
-- the transaction that moves `fleet_format`. Bounds are validated when read,
-- so a malformed row refuses typed instead of being rejected unseen.
CREATE TABLE IF NOT EXISTS lash_plugin_writers (
    plugin_id  TEXT PRIMARY KEY,
    min_format INTEGER NOT NULL,
    max_format INTEGER NOT NULL
);
";

// This database's schema version, and the history of what each value
// changed, is `lash_core_store::compat::SQLITE_CORE_SCHEMA_VERSION`.

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
    start_obligation_last_error_code TEXT CONSTRAINT ck_processes_start_obligation_error_code CHECK ((start_obligation_last_error IS NULL) = (start_obligation_last_error_code IS NULL)),
    start_obligation_settled_at_ms INTEGER,
    obligation_id         TEXT,
    obligation_state      TEXT,
    obligation_attempts   INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms  INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_processes_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms INTEGER,
    consumer_hold_key     TEXT,
    consumer_hold_scope_kind TEXT,
    consumer_hold_scope_id TEXT,
    consumer_hold_cancels INTEGER,
    trigger_delivery_pin_occurrence_id TEXT,
    trigger_delivery_pin_subscription_id TEXT,
    CONSTRAINT ck_processes_consumer_hold CHECK ((consumer_hold_key IS NULL) = (consumer_hold_scope_kind IS NULL) AND (consumer_hold_key IS NULL) = (consumer_hold_scope_id IS NULL)),
    CONSTRAINT ck_processes_trigger_delivery_pin CHECK ((trigger_delivery_pin_occurrence_id IS NULL) = (trigger_delivery_pin_subscription_id IS NULL)),
    CONSTRAINT ck_processes_start_obligation CHECK (((start_obligation_state IS NULL AND start_obligation_id IS NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'due' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NOT NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'claimed' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NOT NULL AND start_obligation_claim_token IS NOT NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NULL) OR (start_obligation_state = 'delivered' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IS NULL AND start_obligation_settled_at_ms IS NOT NULL) OR (start_obligation_state = 'stalled' AND start_obligation_id IS NOT NULL AND start_obligation_due_at_ms IS NULL AND start_obligation_claim_token IS NULL AND start_obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND start_obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_processes_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    CONSTRAINT ck_processes_parked CHECK ((parked_since_ms IS NULL) = (parked_reason_code IS NULL)),
    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned')),
    CONSTRAINT ck_processes_lifetime CHECK (lifetime IN ('until', 'detached')),
    CONSTRAINT ck_processes_lifetime_scope CHECK ((lifetime = 'detached' AND lifetime_scope_kind IS NULL AND lifetime_scope_id IS NULL) OR (lifetime = 'until' AND lifetime_scope_kind IN ('turn', 'session_operation', 'process', 'session') AND lifetime_scope_id IS NOT NULL))
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
-- A row pinned by its trigger delivery until the bind commits (FIG-4203).
CREATE INDEX IF NOT EXISTS idx_processes_trigger_delivery_pin
    ON processes(process_id)
    WHERE trigger_delivery_pin_occurrence_id IS NOT NULL;
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
-- the terminal statuses. It must stay
-- byte-identical to the generated nonterminal predicate, or SQLite plans
-- the query without it.
CREATE INDEX IF NOT EXISTS idx_processes_pending_cancel
    ON processes(cancel_requested_at_ms, process_id)
    WHERE cancel_requested_at_ms IS NOT NULL
      AND status NOT IN ('completed', 'failed', 'cancelled', 'abandoned');
-- The scope-close sweep's only scan: processes living `Until` one closed
-- scope that still owe a cancel. The predicate names the live statuses rather than a NOT
-- IN so a status added later cannot silently widen the index; it is exactly
-- `LIVE_PROCESS_STATUS_LABELS`.
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
    kind        TEXT NOT NULL CONSTRAINT ck_process_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled', 'redrive_requested')),
    cause_json       TEXT,
    reason_json TEXT,
    at_ms       INTEGER NOT NULL,
    park_build_generation TEXT,
    redrive_intent INTEGER,
    CONSTRAINT ck_process_park_events_parked_reason CHECK (((kind = 'parked' AND reason_json IS NOT NULL AND cause_json IS NULL AND redrive_intent IS NULL) OR (kind IN ('unparked', 'cancelled') AND reason_json IS NULL AND cause_json IS NOT NULL AND redrive_intent IS NULL AND park_build_generation IS NULL) OR (kind = 'redrive_requested' AND reason_json IS NULL AND cause_json IS NULL AND redrive_intent IS NOT NULL AND redrive_intent >= 0 AND park_build_generation IS NULL)) IS TRUE)
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
    released_payload_digest TEXT,
    PRIMARY KEY (process_id, sequence),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_process_events_key
    ON process_events(process_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- The event prefix a host released of a still-retained process (FIG-3482):
-- events at or below `released_through` keep their row without a payload,
-- and reads below it are refused typed.
CREATE TABLE IF NOT EXISTS process_event_horizons (
    process_id        TEXT PRIMARY KEY,
    released_through  INTEGER NOT NULL CONSTRAINT ck_process_event_horizons_positive CHECK (released_through > 0),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

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
    CONSTRAINT ck_process_wake_deliveries_discard_reason CHECK (discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound', 'source_unreadable', 'content_conflict')),
    CONSTRAINT ck_process_wake_deliveries_lifecycle CHECK ((state = 'enqueuing') = (claim_token IS NOT NULL) AND (state = 'discarded') = (discard_reason IS NOT NULL)),
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
    pruned_change_seq   INTEGER NOT NULL,
    CONSTRAINT ck_process_tombstones_terminal_label CHECK (terminal_label IN ('completed', 'failed', 'cancelled', 'abandoned'))
);
CREATE INDEX IF NOT EXISTS idx_process_tombstones_change
    ON process_tombstones(pruned_change_seq);

-- Process-record cleanup is owned by this registry. Every other kind lives
-- in the durable core, whose vocabulary remains open (ADR 0115 section 5).
CREATE TABLE IF NOT EXISTS artifact_cleanup_obligations (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_cleanup_obligations_database CHECK (referrer_kind = 'process_record'),
    referrer_id TEXT NOT NULL CHECK (length(referrer_id) > 0),
    cleanup_json TEXT NOT NULL,
    obligation_id TEXT NOT NULL,
    obligation_state TEXT NOT NULL,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_artifact_cleanup_obligations_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
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
    committed_at_ms INTEGER NOT NULL,
    handover_json    TEXT NOT NULL,
    started_json     TEXT,
    written_generation TEXT NOT NULL,
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
    obligation_id    TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_parent_end_plans_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_parent_end_plans_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    PRIMARY KEY (parent_kind, parent_id),
    CONSTRAINT ck_parent_end_plans_kind CHECK (parent_kind IN ('turn', 'session_operation', 'process', 'session'))
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
    owner               TEXT NOT NULL,
    execution_scope_id  TEXT NOT NULL,
    tool_call_id        TEXT NOT NULL,
    intent_index        INTEGER NOT NULL,
    payload_hash        TEXT NOT NULL,
    submission_json     TEXT NOT NULL,
    admitted_at_ms      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tool_intent_submissions_scope
    ON tool_intent_submissions(owner, execution_scope_id, intent_index);

-- The owners whose submission ledger the retained-evidence lever reclaimed
-- (FIG-1509): each is a durably deleted session, and the fence refuses every
-- later submission under it so a reclaimed identity cannot realize again.
CREATE TABLE IF NOT EXISTS tool_intent_retired_owners (
    owner TEXT PRIMARY KEY
);

-- The build generations an operator marked draining (FIG-3799): the recovery
-- leader wakes every live process whose current segment a marked generation
-- admitted, so each hands its open wait to a successor on the newest build.
CREATE TABLE IF NOT EXISTS draining_generations (
    generation   TEXT PRIMARY KEY,
    marked_at_ms INTEGER NOT NULL
);

";

// This database's schema version, and the history of what each value
// changed, is `lash_core_store::compat::SQLITE_REGISTRY_SCHEMA_VERSION`.

// This database's schema version, and the history of what each value
// changed, is `lash_core_store::compat::SQLITE_TRIGGERS_SCHEMA_VERSION`.

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
pub(crate) fn prepare_versioned_schema<'connection>(
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
        for statements in database.provisioning_statements() {
            conn.execute_batch(statements)?;
        }
        Ok(())
    };
    let (admission, mut fleet) = crate::compat::admit(tx, database, writable)?;
    match admission {
        lash_core_execution::compat::CompatAdmission::Provision => {
            apply_schema(tx)?;
            crate::compat::provision(tx, database, writable)?;
            // The handle answers the epoch the row was just seeded at, not
            // this build's newest: a synthetic N+1 seeds a fresh store at N.
            fleet = lash_core_execution::FleetFormat::seed(writable);
        }
        lash_core_execution::compat::CompatAdmission::Native => {
            crate::compat::refuse_unmigrated(tx, database)?;
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
                     UPDATE lash_compat SET version = version + 1",
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
            let above = lash_core_execution::compat::descriptor(database.component())
                .expect("database descriptor")
                .reads
                .max()
                + 1;
            let mut connection = Connection::open_in_memory().expect("open database");
            provision(&mut connection, database);
            connection
                .execute(
                    "UPDATE lash_compat SET version = ?1, min_reader = ?1",
                    [above],
                )
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
                    refusal: CompatRefusal::ReaderFloorAbove { min_reader, .. }
                } if min_reader == above
            ));
        }
    }

    #[test]
    fn sqlite_refuses_a_partially_advanced_set() {
        let root = tempfile::tempdir().expect("database run");
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
            .execute(
                "UPDATE lash_compat SET version = ?1",
                [SqliteDatabase::Triggers.expected_version() + 1],
            )
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
        // Which side of the build's range 43 falls on is the admission
        // rule's to say; either way the stamp is refused typed.
        let descriptor =
            lash_core_execution::compat::descriptor(SqliteDatabase::DurableCore.component())
                .expect("core descriptor");
        let expected = lash_core_execution::compat::admit(
            descriptor,
            lash_core_execution::compat::StampRead::Present(
                lash_core_execution::compat::CompatStamp {
                    version: 43,
                    min_reader: 43,
                },
            ),
        )
        .expect_err("a retired stamp is refused");
        assert!(
            matches!(
                &verdict,
                StoreError::Incompatible { refusal }
                    if refusal.clone().with_writing_release(None) == expected
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

#[cfg(test)]
mod nested_format_tests {
    #[test]
    fn checkpoint_edges_require_a_component_blob() {
        let connection = rusqlite::Connection::open_in_memory().expect("SQLite");
        connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("foreign keys");
        connection.execute_batch(super::SCHEMA).expect("schema");
        connection
            .execute(
                "INSERT INTO blobs(hash, content) VALUES ('root', X'00')",
                [],
            )
            .expect("root");
        assert!(connection.execute(
            "INSERT INTO checkpoint_blob_refs(checkpoint_ref, blob_ref) VALUES ('root', 'missing')", []
        ).is_err(), "a checkpoint edge cannot name a missing component");
        connection
            .execute(
                "INSERT INTO blobs(hash, content) VALUES ('component', X'01')",
                [],
            )
            .expect("component");
        connection.execute("INSERT INTO checkpoint_blob_refs(checkpoint_ref, blob_ref) VALUES ('root', 'component')", []).expect("edge");
        assert!(
            connection
                .execute("DELETE FROM blobs WHERE hash = 'component'", [])
                .is_err(),
            "a rooted component cannot be deleted"
        );
        connection
            .execute("DELETE FROM blobs WHERE hash = 'root'", [])
            .expect("root deletion cascades its edges");
        connection
            .execute("DELETE FROM blobs WHERE hash = 'component'", [])
            .expect("an unreferenced component can be deleted");
    }
}
