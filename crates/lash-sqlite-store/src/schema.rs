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

/// Canonical SQLite schema for a factory-wide lash durable-core catalog.
///
/// This is the *only* schema the store supports. Older durable-core databases —
/// including any rolled forward through prior migration chains — must be
/// deleted before opening with this binary; [`ensure_schema`] rejects any
/// `PRAGMA user_version` that does not match [`SCHEMA_VERSION`] exactly. We
/// run with no on-the-fly migrations on purpose: lash's durable contract
/// lives one level up in the per-record `schema_version` stamps, not in
/// SQL DDL juggling.
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

CREATE TABLE IF NOT EXISTS node_anchors (
    node_id           TEXT PRIMARY KEY,
    checkpoint_ref    TEXT NOT NULL,
    source_session_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_nodes (
    seq            INTEGER PRIMARY KEY,
    session_id     TEXT NOT NULL,
    node_id        TEXT NOT NULL UNIQUE,
    parent_node_id TEXT,
    node_json      TEXT NOT NULL,
    tombstoned     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_graph_nodes_session_seq
    ON graph_nodes(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_graph_nodes_parent
    ON graph_nodes(parent_node_id);

CREATE TABLE IF NOT EXISTS usage_deltas (
    seq                  INTEGER PRIMARY KEY,
    session_id            TEXT NOT NULL,
    source               TEXT NOT NULL,
    model                TEXT NOT NULL,
    input_tokens         INTEGER NOT NULL,
    output_tokens        INTEGER NOT NULL,
    cache_read_input_tokens  INTEGER NOT NULL,
    cache_write_input_tokens INTEGER NOT NULL,
    reasoning_output_tokens     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_deltas_session_seq
    ON usage_deltas(session_id, seq);

CREATE TABLE IF NOT EXISTS session_meta (
    session_id    TEXT PRIMARY KEY,
    incarnation_id TEXT NOT NULL,
    session_name  TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    model         TEXT NOT NULL,
    cwd           TEXT,
    relation_json TEXT
);

CREATE TABLE IF NOT EXISTS runtime_turn_commits (
    session_id        TEXT NOT NULL,
    turn_id           TEXT NOT NULL,
    turn_commit_hash  TEXT NOT NULL,
    result_json       TEXT NOT NULL,
    committed_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (session_id, turn_id)
);

CREATE TABLE IF NOT EXISTS session_execution_leases (
    session_id               TEXT PRIMARY KEY,
    lease_owner_id           TEXT,
    lease_owner_incarnation_id TEXT,
    lease_owner_liveness_json TEXT,
    lease_token              TEXT,
    lease_fencing_token      INTEGER NOT NULL DEFAULT 0,
    lease_claimed_at_ms      INTEGER NOT NULL DEFAULT 0,
    lease_expires_at_ms      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS queued_work_batches (
    enqueue_seq       INTEGER PRIMARY KEY,
    batch_id          TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    source_key        TEXT,
    delivery_policy   TEXT NOT NULL,
    slot_policy       TEXT NOT NULL,
    merge_key_json    TEXT NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_queued_work_ready
    ON queued_work_batches(session_id, available_at_ms, enqueue_seq);

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
pub(crate) const SCHEMA_VERSION: i32 = 19;

pub(crate) const PROCESS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS processes (
    process_id            TEXT PRIMARY KEY,
    registration_hash     TEXT NOT NULL,
    owner_scope_id       TEXT NOT NULL,
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL,
    change_seq            INTEGER NOT NULL,
    status                TEXT NOT NULL,
    record_json           TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_processes_status
    ON processes(status);

CREATE INDEX IF NOT EXISTS idx_processes_change_seq
    ON processes(change_seq);

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
    payload_hash      TEXT NOT NULL,
    idempotency_key   TEXT,
    occurred_at_ms    INTEGER NOT NULL,
    event_json        TEXT NOT NULL,
    PRIMARY KEY (process_id, sequence),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_process_events_key
    ON process_events(process_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS process_wake_acks (
    process_id  TEXT NOT NULL,
    sequence    INTEGER NOT NULL,
    PRIMARY KEY (process_id, sequence),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS process_handle_grants (
    session_id       TEXT NOT NULL,
    scope_id        TEXT NOT NULL,
    process_id       TEXT NOT NULL,
    descriptor_json  TEXT NOT NULL,
    PRIMARY KEY (scope_id, process_id),
    FOREIGN KEY (process_id) REFERENCES processes(process_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_process_handle_grants_session
    ON process_handle_grants(session_id);

CREATE INDEX IF NOT EXISTS idx_process_handle_grants_process
    ON process_handle_grants(process_id);

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
pub(crate) const PROCESS_SCHEMA_VERSION: i32 = 13;

pub(crate) const TRIGGER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trigger_subscriptions (
    subscription_id      TEXT PRIMARY KEY,
    owner_scope          TEXT NOT NULL,
    subscription_key     TEXT NOT NULL,
    incarnation          TEXT NOT NULL,
    revision             INTEGER NOT NULL,
    definition_hash      TEXT NOT NULL,
    source_type          TEXT NOT NULL,
    source_key           TEXT NOT NULL,
    enabled              INTEGER NOT NULL,
    tombstoned           INTEGER NOT NULL,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    record_json          TEXT NOT NULL,
    UNIQUE(owner_scope, subscription_key)
);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_registrant
    ON trigger_subscriptions(owner_scope, subscription_key);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_source
    ON trigger_subscriptions(source_type, source_key, enabled);

CREATE TABLE IF NOT EXISTS trigger_occurrences (
    occurrence_id    TEXT PRIMARY KEY,
    idempotency_key  TEXT NOT NULL UNIQUE,
    request_hash     TEXT NOT NULL,
    source_type      TEXT NOT NULL,
    source_key       TEXT NOT NULL,
    occurred_at_ms   INTEGER NOT NULL,
    record_json      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_source
    ON trigger_occurrences(source_type, source_key, occurred_at_ms);

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
    request_hash    TEXT NOT NULL,
    result_json     TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_process
    ON trigger_deliveries(process_id);

CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_subscription
    ON trigger_deliveries(subscription_id);
";

// FIG-504 keyed subscriptions are an alpha reject-and-recreate cutover. Version
// 2 has no keyless-row compatibility path and preserves revision snapshots in
// delivery rows instead of cascading them away with a subscription.
pub(crate) const TRIGGER_SCHEMA_VERSION: i32 = 2;

pub(crate) const EFFECT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runtime_effect_replay (
    scope_id             TEXT NOT NULL,
    session_id           TEXT,
    incarnation_id       TEXT,
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
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    PRIMARY KEY (scope_id, replay_key)
);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_replay_lease
    ON runtime_effect_replay(status, lease_expires_at_ms);

CREATE INDEX IF NOT EXISTS idx_runtime_effect_replay_session_lifetime
    ON runtime_effect_replay(session_id, incarnation_id);

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

CREATE TABLE IF NOT EXISTS await_event_revoked_sessions (
    session_id      TEXT PRIMARY KEY,
    revoked_at_ms   INTEGER NOT NULL
);
";

// Version 5 replaces lossy effect scope ids with canonical typed identities
// and adds indexed session-lifetime join columns for controller-owned
// retirement. Effect databases follow the crate's alpha reject-and-recreate
// convention rather than carrying a migration chain.
pub(crate) const EFFECT_SCHEMA_VERSION: i32 = 5;

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

pub(crate) async fn ensure_schema(conn: &SqliteConnection) -> rusqlite::Result<()> {
    ensure_versioned_schema(conn, "session", SCHEMA, SCHEMA_VERSION).await
}

pub(crate) async fn ensure_process_schema(conn: &SqliteConnection) -> rusqlite::Result<()> {
    ensure_versioned_schema(
        conn,
        "process registry",
        PROCESS_SCHEMA,
        PROCESS_SCHEMA_VERSION,
    )
    .await
}

pub(crate) async fn ensure_trigger_schema(conn: &SqliteConnection) -> rusqlite::Result<()> {
    ensure_versioned_schema(
        conn,
        "trigger store",
        TRIGGER_SCHEMA,
        TRIGGER_SCHEMA_VERSION,
    )
    .await
}

pub(crate) async fn ensure_effect_schema(conn: &SqliteConnection) -> rusqlite::Result<Vec<u8>> {
    conn.call(|connection| {
        let tx = prepare_versioned_schema(
            connection,
            "effect replay",
            EFFECT_SCHEMA,
            EFFECT_SCHEMA_VERSION,
        )?;
        let signing_secret = tx.query_row(
            "SELECT signing_secret FROM await_event_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(signing_secret)
    })
    .await
}

/// Apply `schema` if the database is already at `schema_version`, initialise it
/// (under one transaction stamping `user_version`) if the database is empty, or
/// reject the open if the on-disk `user_version` is anything else. Runs entirely
/// on the connection thread so the version check and DDL share one connection.
async fn ensure_versioned_schema(
    conn: &SqliteConnection,
    database_kind: &'static str,
    schema: &'static str,
    schema_version: i32,
) -> rusqlite::Result<()> {
    conn.call(move |c| {
        let tx = prepare_versioned_schema(c, database_kind, schema, schema_version)?;
        tx.commit()
    })
    .await
}

fn prepare_versioned_schema<'connection>(
    connection: &'connection mut Connection,
    database_kind: &'static str,
    schema: &'static str,
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
        tx.execute_batch(schema)?;
        return Ok(tx);
    }
    if user_version == 0 && !has_user_schema_objects(&tx)? {
        tx.execute_batch(schema)?;
        tx.pragma_update(None, "user_version", schema_version)?;
        return Ok(tx);
    }
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
        Some(unsupported_schema_message(
            database_kind,
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
/// found `PRAGMA user_version` values are reported accurately. There is no
/// migration chain — the database must be deleted before reopening.
pub(crate) fn unsupported_schema_message(
    database_kind: &str,
    expected_version: i32,
    found_version: i32,
) -> String {
    format!(
        "Unsupported lash {database_kind} schema: this binary supports schema version \
         {expected_version}, but the database reports version {found_version}. There is no \
         migration chain — delete the {database_kind} database and start fresh."
    )
}
