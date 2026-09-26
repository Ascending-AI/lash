//! Durable-residue digests for the FIG-2841 no-residue law.
//!
//! The law is: a store operation that returns `Err` for a refusal or a corrupt
//! input leaves durable state byte-identical to before the call.
//!
//! [`RawDurableState`] cannot express that on its own. It *decodes* every row
//! it reads, so it panics on a deliberately corrupted record, and it projects
//! away columns that are not cross-backend comparable — exactly the columns a
//! leaked write would land in. The digest here is therefore separate and
//! deliberately different in kind:
//!
//! * it is decode-free on the SQL backends (raw column text), so a corrupt row
//!   is a comparable value rather than a panic;
//! * it is compared only against *the same backend's* pre-call digest, never
//!   across backends, so no normalization is required and no physical layout
//!   choice can read as drift;
//! * what crosses the backend boundary is the mutated-or-not verdict and the
//!   set of logical tables that moved, which are backend-neutral.
//!
//! The durable effect tables — `runtime_effect_group`,
//! `runtime_effect_group_child` and `runtime_effect_replay` — belong to the
//! SQL effect engines, which are not storage (ADR 0104: Restate is the only
//! effect engine; FIG-3667 and FIG-3668 delete the SQL engines). No driven
//! store-trait operation writes them, so they sit in
//! [`RESIDUE_TABLE_EXCLUSIONS`] with the rest of the effect surface.

use super::*;

/// One backend's durable rows, keyed by logical table, rendered without
/// decoding. Only ever compared with another digest from the same backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ResidueDigest {
    tables: BTreeMap<&'static str, Vec<String>>,
}

impl ResidueDigest {
    /// Logical tables whose rows differ between `self` (pre-call) and `after`.
    pub(super) fn changed_tables(&self, after: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        for table in self.tables.keys().chain(after.tables.keys()) {
            if self.tables.get(table) != after.tables.get(table) && !changed.contains(table) {
                changed.push(*table);
            }
        }
        changed.sort_unstable();
        changed
    }
}

/// Session-scoped SQLite reads, by logical table name. `?1` is the session id.
/// A store-wide singleton table carries no session id, so its query binds no
/// parameter at all — the digest binds the session id only to a statement that
/// declares a parameter.
const SQLITE_RESIDUE_QUERIES: &[(&str, &str)] = &[
    (
        "session_head",
        "SELECT * FROM session_head WHERE session_id = ?1",
    ),
    (
        "session_meta",
        "SELECT * FROM session_meta WHERE session_id = ?1",
    ),
    (
        "deleted_sessions",
        "SELECT * FROM deleted_sessions WHERE session_id = ?1",
    ),
    (
        "graph_nodes",
        "SELECT * FROM graph_nodes WHERE session_id = ?1",
    ),
    (
        "node_anchors",
        "SELECT * FROM node_anchors WHERE source_session_id = ?1",
    ),
    (
        "runtime_turn_commits",
        "SELECT * FROM runtime_turn_commits WHERE session_id = ?1",
    ),
    (
        "usage_deltas",
        "SELECT * FROM usage_deltas WHERE session_id = ?1",
    ),
    (
        "attachment_manifest",
        "SELECT * FROM attachment_manifest WHERE session_id = ?1",
    ),
    (
        "fork_lineage",
        "SELECT * FROM fork_lineage WHERE session_id = ?1",
    ),
    (
        "session_meta_pending_observer_intents",
        "SELECT * FROM session_meta_pending_observer_intents WHERE session_id = ?1",
    ),
    (
        "wake_redelivery_fences",
        "SELECT * FROM wake_redelivery_fences WHERE session_id = ?1",
    ),
    // Condemnation rows are keyed by attachment, not by session, so they are
    // scoped through this session's manifest rows.
    (
        "attachment_condemnations",
        "SELECT * FROM attachment_condemnations
         WHERE attachment_id IN
             (SELECT attachment_id FROM attachment_manifest WHERE session_id = ?1)",
    ),
    (
        "session_execution_leases",
        "SELECT * FROM session_execution_leases WHERE session_id = ?1",
    ),
    (
        "turn_parks",
        "SELECT * FROM turn_parks WHERE session_id = ?1",
    ),
    (
        "pending_turn_inputs",
        "SELECT * FROM pending_turn_inputs WHERE session_id = ?1",
    ),
    (
        "queued_work_batches",
        "SELECT * FROM queued_work_batches WHERE session_id = ?1",
    ),
    (
        "queued_work_items",
        "SELECT item.* FROM queued_work_items AS item
         JOIN queued_work_batches AS batch ON batch.batch_id = item.batch_id
         WHERE batch.session_id = ?1",
    ),
    (
        "queued_runs",
        "SELECT * FROM queued_runs WHERE session_id = ?1",
    ),
    (
        "queued_run_members",
        "SELECT * FROM queued_run_members WHERE session_id = ?1",
    ),
    (
        "checkpoint_blob_refs",
        "SELECT * FROM checkpoint_blob_refs
         WHERE checkpoint_ref IN (SELECT checkpoint_ref FROM session_head WHERE session_id = ?1)",
    ),
    // Checkpoint manifest and component bytes reachable from this session's
    // head. Blobs are content-addressed, so a corrupted body is visible here
    // and nowhere else.
    (
        "checkpoint_blobs",
        "SELECT hash, hex(content) FROM blobs
         WHERE hash IN (SELECT checkpoint_ref FROM session_head WHERE session_id = ?1)
            OR hash IN (
                SELECT blob_ref FROM checkpoint_blob_refs
                WHERE checkpoint_ref IN
                    (SELECT checkpoint_ref FROM session_head WHERE session_id = ?1))",
    ),
    // `fleet_format` is a store-wide singleton (durable-format generation),
    // not session state: it carries no session id, so this read binds none.
    ("fleet_format", "SELECT * FROM fleet_format"),
];

/// The same reads on PostgreSQL. `to_jsonb(row)` renders every column without
/// this harness naming them, so a new column is compared the day it lands.
const POSTGRES_RESIDUE_QUERIES: &[(&str, &str)] = &[
    (
        "session_head",
        "SELECT to_jsonb(t)::text FROM lash_sessions t WHERE session_id = $1",
    ),
    (
        "session_meta",
        "SELECT to_jsonb(t)::text FROM lash_session_meta t WHERE session_id = $1",
    ),
    (
        "deleted_sessions",
        "SELECT to_jsonb(t)::text FROM lash_deleted_sessions t WHERE session_id = $1",
    ),
    (
        "graph_nodes",
        "SELECT to_jsonb(t)::text FROM lash_graph_nodes t WHERE session_id = $1",
    ),
    (
        "node_anchors",
        "SELECT to_jsonb(t)::text FROM lash_node_anchors t WHERE source_session_id = $1",
    ),
    (
        "runtime_turn_commits",
        "SELECT to_jsonb(t)::text FROM lash_runtime_turn_commits t WHERE session_id = $1",
    ),
    (
        "usage_deltas",
        "SELECT to_jsonb(t)::text FROM lash_usage_deltas t WHERE session_id = $1",
    ),
    (
        "attachment_manifest",
        "SELECT to_jsonb(t)::text FROM lash_attachment_manifest t WHERE session_id = $1",
    ),
    (
        "fork_lineage",
        "SELECT to_jsonb(t)::text FROM lash_fork_lineage t WHERE session_id = $1",
    ),
    (
        "session_meta_pending_observer_intents",
        "SELECT to_jsonb(t)::text FROM lash_session_meta_pending_observer_intents t \
         WHERE session_id = $1",
    ),
    (
        "wake_redelivery_fences",
        "SELECT to_jsonb(t)::text FROM lash_wake_redelivery_fences t WHERE session_id = $1",
    ),
    (
        "attachment_condemnations",
        "SELECT to_jsonb(t)::text FROM lash_attachment_condemnations t
         WHERE attachment_id IN
             (SELECT attachment_id FROM lash_attachment_manifest WHERE session_id = $1)",
    ),
    (
        "session_execution_leases",
        "SELECT to_jsonb(t)::text FROM lash_session_execution_leases t WHERE session_id = $1",
    ),
    (
        "turn_parks",
        "SELECT to_jsonb(t)::text FROM lash_turn_parks t WHERE session_id = $1",
    ),
    (
        "pending_turn_inputs",
        "SELECT to_jsonb(t)::text FROM lash_pending_turn_inputs t WHERE session_id = $1",
    ),
    (
        "queued_work_batches",
        "SELECT to_jsonb(t)::text FROM lash_queued_work_batches t WHERE session_id = $1",
    ),
    (
        "queued_work_items",
        "SELECT to_jsonb(item)::text FROM lash_queued_work_items AS item
         JOIN lash_queued_work_batches AS batch ON batch.batch_id = item.batch_id
         WHERE batch.session_id = $1",
    ),
    (
        "queued_runs",
        "SELECT to_jsonb(t)::text FROM lash_queued_runs t WHERE session_id = $1",
    ),
    (
        "queued_run_members",
        "SELECT to_jsonb(t)::text FROM lash_queued_run_members t WHERE session_id = $1",
    ),
    (
        "checkpoint_blob_refs",
        "SELECT to_jsonb(t)::text FROM lash_checkpoint_blob_refs t
         WHERE checkpoint_ref IN (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)",
    ),
    (
        "checkpoint_blobs",
        "SELECT hash || ':' || encode(content, 'hex') FROM lash_blobs
         WHERE hash IN (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)
            OR hash IN (
                SELECT blob_ref FROM lash_checkpoint_blob_refs
                WHERE checkpoint_ref IN
                    (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1))",
    ),
    // `lash_fleet_format` is the store-wide singleton SQLite carries as
    // `fleet_format`: no session id, so this read binds none.
    (
        "fleet_format",
        "SELECT to_jsonb(t)::text FROM lash_fleet_format t",
    ),
];

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn sqlite_residue_digest(path: &Path, session_id: &SessionId) -> ResidueDigest {
    let connection = rusqlite::Connection::open(path).expect("open SQLite residue reader");
    connection
        .busy_timeout(Duration::from_secs(15))
        .expect("configure SQLite residue reader busy timeout");
    let mut tables = BTreeMap::new();
    for (table, sql) in SQLITE_RESIDUE_QUERIES {
        let mut statement = connection
            .prepare(sql)
            .unwrap_or_else(|error| panic!("prepare SQLite residue read for `{table}`: {error}"));
        let column_count = statement.column_count();
        let render = |row: &rusqlite::Row| -> rusqlite::Result<String> {
            let mut rendered = String::new();
            for column in 0..column_count {
                let value: rusqlite::types::Value = row.get(column)?;
                let _ = write!(rendered, "{value:?}|");
            }
            Ok(rendered)
        };
        let mut rows: Vec<String> = if statement.parameter_count() == 0 {
            statement.query_map([], render)
        } else {
            statement.query_map([session_id.as_str()], render)
        }
        .unwrap_or_else(|error| panic!("read SQLite residue rows for `{table}`: {error}"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("collect SQLite residue rows for `{table}`: {error}"));
        rows.sort();
        tables.insert(*table, rows);
    }
    ResidueDigest { tables }
}

pub(super) async fn postgres_residue_digest(
    pool: &PgPool,
    session_id: &SessionId,
) -> ResidueDigest {
    let mut tables = BTreeMap::new();
    for (table, sql) in POSTGRES_RESIDUE_QUERIES {
        // `$1` is the session id; a store-wide singleton query declares no
        // parameter, and Postgres refuses a bind it does not declare.
        let query = sqlx::query_scalar::<_, String>(sql);
        let query = if sql.contains("$1") {
            query.bind(session_id.as_str())
        } else {
            query
        };
        let mut rows: Vec<String> = query
            .fetch_all(pool)
            .await
            .unwrap_or_else(|error| panic!("read Postgres residue rows for `{table}`: {error}"));
        rows.sort();
        tables.insert(*table, rows);
    }
    ResidueDigest { tables }
}

/// SQLite schema source, read back so the coverage gate below cannot drift.
/// Fragment-carried tables live in schema_fragments.rs (FIG-3260).
const SQLITE_SCHEMA_SOURCE: &str = concat!(
    include_str!("../../../lash-sqlite-store/src/schema_fragments.rs"),
    include_str!("../../../lash-sqlite-store/src/schema.rs"),
);

/// Reason strings shared by the tables one other suite owns.
const TURN_CANCELLATION: &str = "turn-cancellation surface: this fixture wires no TurnCancellationAuthority, so no driven \
     operation can write it; owned by the turn_control conformance suite";
const PROCESS_LIFECYCLE: &str = "process-lifecycle surface: this fixture wires no process registry, so no driven operation \
     can write it; owned by the process conformance suites";
const AWAIT_EVENT: &str = "await-event surface, reached through the EffectHost seam rather than the store traits this \
     differential drives; owned by the await-event conformance suite";
const TRIGGERS: &str = "trigger surface: no driven operation subscribes, delivers or occurs; owned by the trigger \
     conformance suite";
const EFFECTS: &str = "effect-replay surface, written through the EffectHost seam rather than the store traits this \
     differential drives";
const ARTIFACTS: &str = "artifact/blob-byte store rather than a session row; the session-reachable blob bytes are \
     already compared by the `checkpoint_blobs` entry, and the blob store itself by \
     attachment_blob_store_differential_agrees";

/// Durable tables this digest deliberately does not read, and who owns them.
///
/// A table in neither the digest nor this list fails
/// [`residue_digest_covers_every_durable_table`]. That is the whole point: a
/// table a driven operation can write but the digest cannot see makes the
/// no-residue law vacuous exactly where a leak would land.
const RESIDUE_TABLE_EXCLUSIONS: &[(&str, &str)] = &[
    ("turn_cancel_closure_authorizations", TURN_CANCELLATION),
    ("turn_cancel_closure_participants", TURN_CANCELLATION),
    ("turn_cancellation_bindings", TURN_CANCELLATION),
    ("turn_cancel_requests", TURN_CANCELLATION),
    ("turn_cancel_retired_scopes", TURN_CANCELLATION),
    ("processes", PROCESS_LIFECYCLE),
    ("process_events", PROCESS_LIFECYCLE),
    ("process_leases", PROCESS_LIFECYCLE),
    ("process_observers", PROCESS_LIFECYCLE),
    ("process_segment_handovers", PROCESS_LIFECYCLE),
    ("process_tombstones", PROCESS_LIFECYCLE),
    ("process_wake_deliveries", PROCESS_LIFECYCLE),
    ("process_artifact_cleanup", PROCESS_LIFECYCLE),
    (
        "parent_end_plans",
        "process-lifecycle surface: every write goes through the process registry's parent-end \
         path, which this fixture does not wire, and the row is keyed by the ended parent scope \
         rather than by a session, so a session-scoped digest query could not read it either; \
         owned by the process conformance suites",
    ),
    (
        "wake_allocation_floors",
        "process-wake surface, and on SQLite it lives in the factory-wide `durable-core.db` \
         catalog rather than the per-session database this digest reads; owned by the process \
         conformance suites",
    ),
    (
        "process_change_clock",
        "a store-wide singleton counter row, not session state; it carries no session id and is \
         shared by every case in the one database this suite runs against",
    ),
    (
        "turn_park_clock",
        "the turn park feed's sequence row: a store-wide singleton counter, not session state; \
         it carries no session id and is shared by every case in the one database this suite \
         runs against; owned by the turn_park_feed laws L1-L6 in lash-conformance",
    ),
    (
        "turn_park_events",
        "the turn park feed's durable ledger: no driven operation records or clears a park, so \
         none can write it, and every feed append rides inside the transaction that changes \
         `turn_parks`, which the digest already reads; owned by the turn_park_feed laws L1-L6 \
         in lash-conformance",
    ),
    (
        "process_park_clock",
        "the process park feed's sequence row: a store-wide singleton counter, not session \
         state; it carries no session id and is shared by every case in the one database this \
         suite runs against; owned by the process_park_feed laws in lash-conformance",
    ),
    (
        "process_park_events",
        "the process park feed's durable ledger: process-lifecycle surface this fixture does not \
         drive, and every feed append rides inside the transaction that changes the park on \
         `processes`; owned by the process_park_feed laws in lash-conformance",
    ),
    ("await_event_meta", AWAIT_EVENT),
    ("await_event_waits", AWAIT_EVENT),
    ("await_event_revoked_sessions", AWAIT_EVENT),
    ("trigger_deliveries", TRIGGERS),
    ("trigger_mutation_receipts", TRIGGERS),
    ("trigger_occurrences", TRIGGERS),
    ("trigger_subscriptions", TRIGGERS),
    ("effect_scope_retirements", EFFECTS),
    ("runtime_effect_group", EFFECTS),
    ("runtime_effect_group_child", EFFECTS),
    ("runtime_effect_replay", EFFECTS),
    ("tool_intent_submissions", EFFECTS),
    ("artifact_owners", ARTIFACTS),
    ("artifact_owner_retirements", ARTIFACTS),
    ("artifact_refs", ARTIFACTS),
    (
        "blobs",
        "content-addressed byte store shared across every session in the one database this suite \
         runs against; the rows this session can reach are compared by `checkpoint_blobs`",
    ),
    (
        "session_ingress",
        "the one session ingress (ADR 0101) is not yet a `RuntimePersistence` component, so no \
         store-trait operation this differential drives writes it; owned by the session-ingress \
         conformance registrations on both backends until the FIG-3540 cutover wires it in",
    ),
    (
        "attachment_blobs",
        "SQLite-only attachment byte store (`SqliteAttachmentStore`): no store-trait operation \
         this differential drives writes it, and a PostgreSQL deployment takes an external \
         attachment backend instead, so there is no counterpart table to compare; owned by the \
         SQLite attachment-store conformance registrations (FIG-3578)",
    ),
    (
        "release_stamp",
        "deployment metadata, not session state: the single row records which lash release wrote \
         the store, it is written by the schema-open path rather than by any driven operation, \
         and it carries no session id, so a session-scoped digest query could not read it \
         either; owned by the release-stamp conformance law (FIG-3092)",
    ),
    (
        "process_definitions",
        "definition-registry surface: this fixture wires no ProcessDefinitionRegistry, so no \
         driven operation can write it, and the row is keyed by its owner scope rather than by a \
         session, so a session-scoped digest query could not read it either; owned by the \
         registry suites (FIG-2995)",
    ),
];

/// Every `CREATE TABLE` name declared by the SQLite schema, deduplicated.
fn declared_sqlite_tables() -> Vec<&'static str> {
    let mut tables = Vec::new();
    for line in SQLITE_SCHEMA_SOURCE.lines() {
        let Some(rest) = line.trim().strip_prefix("CREATE TABLE IF NOT EXISTS ") else {
            continue;
        };
        let name: &str = rest
            .split(|character: char| !(character.is_alphanumeric() || character == '_'))
            .next()
            .unwrap_or_default();
        if !name.is_empty() && !tables.contains(&name) {
            tables.push(name);
        }
    }
    assert!(
        tables.len() > 40,
        "the schema scan found only {} tables; its parser has drifted",
        tables.len()
    );
    tables
}

#[test]
fn residue_digest_covers_every_durable_table() {
    let sqlite_digest_tables: Vec<&str> = SQLITE_RESIDUE_QUERIES
        .iter()
        .map(|(table, _)| *table)
        .collect();
    let postgres_digest_tables: Vec<&str> = POSTGRES_RESIDUE_QUERIES
        .iter()
        .map(|(table, _)| *table)
        .collect();
    assert_eq!(
        sqlite_digest_tables, postgres_digest_tables,
        "the two SQL backends must render the same logical tables in the same order; the \
         mutated-table set crosses the backend boundary and a table read on only one backend \
         would read as a cross-backend divergence"
    );

    let mut missing = Vec::new();
    let mut stale = Vec::new();
    for table in declared_sqlite_tables() {
        let excluded = RESIDUE_TABLE_EXCLUSIONS
            .iter()
            .find(|(name, _)| *name == table);
        let read = sqlite_digest_tables.contains(&table)
            || SQLITE_RESIDUE_QUERIES
                .iter()
                .any(|(_, sql)| sql.contains(&format!(" {table} ")));
        match (read, excluded) {
            (true, None) => {}
            (false, Some((_, reason))) => assert!(
                !reason.trim().is_empty(),
                "the residue exclusion for `{table}` carries no reason"
            ),
            (true, Some(_)) => stale.push(table),
            (false, None) => missing.push(table),
        }
    }
    assert!(
        missing.is_empty(),
        "durable tables are neither read by the residue digest nor excluded with a reason: \
         {missing:?}. A refused operation that wrote one of these would leave residue this law \
         cannot see. Add a query to SQLITE_RESIDUE_QUERIES and POSTGRES_RESIDUE_QUERIES, or add \
         the table to RESIDUE_TABLE_EXCLUSIONS with the suite that owns it."
    );
    assert!(
        stale.is_empty(),
        "these tables are excluded but the digest now reads them; delete their exclusions: \
         {stale:?}"
    );
}
