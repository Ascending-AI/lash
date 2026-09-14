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
        "session_execution_leases",
        "SELECT * FROM session_execution_leases WHERE session_id = ?1",
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
        "session_execution_leases",
        "SELECT to_jsonb(t)::text FROM lash_session_execution_leases t WHERE session_id = $1",
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
];

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
        let mut rows: Vec<String> = statement
            .query_map([session_id.as_str()], |row| {
                let mut rendered = String::new();
                for column in 0..column_count {
                    let value: rusqlite::types::Value = row.get(column)?;
                    let _ = write!(rendered, "{value:?}|");
                }
                Ok(rendered)
            })
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
        let mut rows: Vec<String> = sqlx::query_scalar(sql)
            .bind(session_id.as_str())
            .fetch_all(pool)
            .await
            .unwrap_or_else(|error| panic!("read Postgres residue rows for `{table}`: {error}"));
        rows.sort();
        tables.insert(*table, rows);
    }
    ResidueDigest { tables }
}

/// The in-memory reference store holds typed records, not rows: it has no raw
/// column surface to read undecoded. Its digest is therefore projected from the
/// decoded state the harness already compares, which is sound because typed
/// memory records cannot carry the undecodable bytes the corrupt-input cases
/// seed — those cases run on the SQL backends only, for that reason.
pub(super) fn in_memory_residue_digest(state: &RawDurableState) -> ResidueDigest {
    let mut tables: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    tables.insert(
        "session_head",
        vec![format!(
            "{:?}|{:?}",
            state.head_revision, state.leaf_node_id
        )],
    );
    tables.insert("session_meta", vec![format!("{:?}", state.session_meta)]);
    tables.insert(
        "graph_nodes",
        state
            .durable_nodes
            .iter()
            .map(|node| format!("{node:?}"))
            .collect(),
    );
    tables.insert(
        "node_anchors",
        state
            .node_anchors
            .iter()
            .map(|anchor| format!("{anchor:?}"))
            .collect(),
    );
    tables.insert(
        "runtime_turn_commits",
        state
            .runtime_turn_commits
            .iter()
            .map(|receipt| format!("{receipt:?}"))
            .collect(),
    );
    tables.insert(
        "usage_deltas",
        state
            .usage_deltas
            .iter()
            .map(|delta| format!("{delta:?}"))
            .collect(),
    );
    tables.insert(
        "attachment_manifest",
        state
            .attachment_manifest
            .iter()
            .map(|row| format!("{row:?}"))
            .collect(),
    );
    tables.insert(
        "session_execution_leases",
        state
            .session_execution_leases
            .iter()
            .map(|lease| format!("{lease:?}"))
            .collect(),
    );
    tables.insert(
        "pending_turn_inputs",
        state
            .pending_turn_inputs
            .iter()
            .map(|input| format!("{input:?}"))
            .collect(),
    );
    tables.insert(
        "queued_work_batches",
        state
            .queued_work
            .iter()
            .map(|batch| format!("{batch:?}"))
            .collect(),
    );
    tables.insert("checkpoint_blobs", vec![format!("{:?}", state.checkpoint)]);
    ResidueDigest { tables }
}
