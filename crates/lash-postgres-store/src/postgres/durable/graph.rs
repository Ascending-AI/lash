//! The checkpoint graph on PostgreSQL: owner graph installation, hydration
//! and the peel that reclaims a retired root's closure (FIG-5887 §5;
//! FIG-5949; FIG-6191). The DDL is in `schema.sql`.
//!
//! # Locks
//!
//! A commit takes the owner's epoch fence, then the write authority of each
//! session whose history it locks, in session order, then a key-share lock
//! on every stored node it names (the external nodes, then the staged nodes
//! another writer stored), in digest order, then installs blobs in digest
//! order, then edges and root rows. The owner fence precedes the history
//! locks, as in every existing session writer (`lock_session_writes`: actor
//! row, then history).
//!
//! # Reclamation
//!
//! The root catalog (`lash_graph_root_catalog`) is a view of the owning
//! columns, and the graph is a DAG: a node no root reaches is an orphan (no
//! root row, no edge to it) or reached only from such nodes. A transaction
//! that retires a root holds it (`lash_retirement_holds`, a root of the
//! catalog), and bounded retirement steps peel it later
//! (`super::pages`): each level's candidates no other transaction holds
//! are locked `FOR UPDATE SKIP LOCKED` in digest order, then one statement
//! deletes every locked candidate no root names and no edge reaches and
//! returns the children their edges reached, the next level. A peel never
//! waits, so it is never part of a lock cycle: a candidate a writer
//! key-share locked first is one that writer names, and stays; a writer
//! that comes after waits for the peel, finds the node gone, and refuses a
//! missing node or installs its bytes again.
//! The schema's `RESTRICT` keys stay the backstop.
//!
//! # Reads
//!
//! No lock orders a reader against a peel. A hydration is one statement:
//! the owner's root and everything it reaches, in that statement's
//! snapshot, so it reads a whole committed root or none. A read of several
//! statements runs in one `REPEATABLE READ READ ONLY` snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use futures_util::TryStreamExt;

use lash_core_store::checkpoint_graph::node::{immutable_catalog_heads, immutable_closure};
use lash_core_store::checkpoint_graph::{
    BlobEncoding, DecodeDimension, DecodeLimits, GraphRefusal, InstalledGraphMeasurement, NodeId,
    NodeSource, StoredNode, check_envelope,
};
use lash_durable::domain::DomainRefusal;
use lash_durable::domain::graph::{
    EffectAdmissionWrite, HydratedGraph, HydratedNode, ReleasePinWrite, RootOwner,
};
use lash_durable::domain::graph_commit::{
    CheckedGraph, CheckedNode, EDGE_ROW_LOGICAL_BYTES, blob_row_logical_bytes,
};
use lash_durable::{
    CommitCapacity, CommitLabel, DurableError, Epoch, StoreFailure, StoreFailureKind,
};
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::{Dialect, Rendered};
use sqlx::PgConnection;

use super::steps::{self, Entry, Shape, Step, Ty, Value};
use super::{Committing, PostgresDurableStore, Tx, integer, sqlx_failure};

mod reclaim;
pub(crate) use reclaim::{audit_page, graph_counts, peel};

lash_store_sql::statements! {
    /// Graph statements only PostgreSQL issues: the artifact bridge's
    /// column, the row locks a writer and a peel take, the array binding of
    /// a node list, and the one-statement hydration of each root source.
    pub(crate) struct GraphPostgresStatements @ "durable_graph_postgres" {
        /// Artifact `?3` of namespace `?2`'s graph node: PostgreSQL's bridge
        /// is the artifact row's `graph_ref`, set only for storage kind 1.
        artifact_root = "SELECT graph_ref FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2 AND storage_kind = 1";

        /// The encoding of every stored blob among the nodes `?1` a commit
        /// names and the nodes it supplies (`?2`, raw digests: never one it
        /// names), key-share locked in digest order against a peel, with
        /// the bytes of each supplied one: what the commit compares instead
        /// of supplying it again.
        lock_nodes = "SELECT hash, encoding, CASE WHEN hash = ANY(?1::text[]) THEN NULL ELSE content END
             FROM blobs
             WHERE hash = ANY(?1::text[])
                OR hash = ANY(ARRAY(SELECT encode(digest, 'hex') FROM UNNEST(?2::bytea[]) AS digest))
             ORDER BY hash FOR KEY SHARE";

        /// Installs every graph node `?1` (raw digests) with its bytes `?2`,
        /// in digest order, and returns the digests it inserted: a node
        /// already stored is left as it is.
        insert_nodes = "INSERT INTO blobs (hash, content, encoding)
             SELECT encode(staged.digest, 'hex'), staged.content, 1
             FROM UNNEST(?1::bytea[], ?2::bytea[]) AS staged(digest, content)
             ORDER BY staged.digest
             ON CONFLICT (hash) DO NOTHING
             RETURNING hash";

        /// The bytes and encoding of every stored blob in `?1`, key-share
        /// locked in digest order: a staged node another writer stored is
        /// held against a peel until the commit ends.
        stored_nodes = "SELECT hash, content, encoding FROM blobs WHERE hash = ANY(?1::text[])
             ORDER BY hash FOR KEY SHARE";

        /// Every edge of freshly installed parents `?1` to children `?2`
        /// (raw digests).
        insert_edges = "INSERT INTO blob_edges (parent_hash, child_hash)
             SELECT encode(parent, 'hex'), encode(child, 'hex')
             FROM UNNEST(?1::bytea[], ?2::bytea[]) AS staged(parent, child)";

        /// Every blob the roots `?1` reach along stored edges, the roots
        /// included, with its encoding and bytes, short of the build
        /// catalog's heads `?2`: one read of a whole graph, whose child
        /// tables a walk still checks. Nothing a head reaches is read.
        walk_reach = "WITH RECURSIVE reach AS (
                 SELECT UNNEST(?1::text[]) AS hash
                 UNION
                 SELECT edge.child_hash FROM blob_edges AS edge
                 JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?2::text[]))
                   AND NOT (edge.child_hash = ANY(?2::text[]))
             )
             SELECT blobs.hash, blobs.encoding, blobs.content
             FROM blobs JOIN reach ON blobs.hash = reach.hash";

        /// [`Self::walk_reach`] without any opaque blob's bytes but the
        /// roots' bytes: what a reader of the graph alone reads.
        walk_reach_graph = "WITH RECURSIVE reach AS (
                 SELECT UNNEST(?1::text[]) AS hash
                 UNION
                 SELECT edge.child_hash FROM blob_edges AS edge
                 JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?2::text[]))
                   AND NOT (edge.child_hash = ANY(?2::text[]))
             )
             SELECT blobs.hash, blobs.encoding,
                    CASE WHEN blobs.encoding = 1 OR blobs.hash = ANY(?1::text[]) THEN blobs.content END
             FROM blobs JOIN reach ON blobs.hash = reach.hash";

        /// One level of a walk: every blob in `?1`. A graph-only walk
        /// (`?3`) omits opaque children's bytes, but includes roots `?2`.
        walk_level = "SELECT hash, encoding,
                    CASE WHEN NOT ?3::boolean OR encoding = 1 OR hash = ANY(?2::text[]) THEN content END
             FROM blobs WHERE hash = ANY(?1::text[])";

        /// Lock for its delete every candidate of a peel's level `?1` that
        /// no other transaction holds, in digest order, and answer them as
        /// one list. A peel never waits: a node a writer holds key-share
        /// locked is one it is about to name, and one a rolled-back writer
        /// leaves is the orphan audit's.
        peel_lock = "SELECT ARRAY(SELECT hash FROM blobs WHERE hash = ANY(?1::text[])
             ORDER BY hash FOR UPDATE SKIP LOCKED)";

        /// One level of a peel: delete every blob in `?1` no root names and
        /// no edge reaches, and answer how many it deleted with the
        /// children their edges reached (read before the delete took them),
        /// as one row.
        peel_level = "WITH dead AS (
                 DELETE FROM blobs AS candidate
                 WHERE candidate.hash = ANY(?1::text[])
                   AND NOT EXISTS (
                       SELECT 1 FROM graph_root_catalog AS root
                       WHERE root.root_ref = candidate.hash
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM blob_edges AS edge WHERE edge.child_hash = candidate.hash
                   )
                 RETURNING candidate.hash
             )
             SELECT (SELECT count(*) FROM dead),
                 ARRAY(SELECT DISTINCT edge.child_hash
                     FROM dead JOIN blob_edges AS edge ON edge.parent_hash = dead.hash)";

        /// Execution `?2`'s root, with every blob it reaches along stored
        /// edges and their bytes, in one statement: the root column (no row
        /// when the owner is gone), then each node, short of the build
        /// catalog's heads `?1` (nothing a head reaches is read). Each
        /// `hydrate_*` statement is this one over its own root source.
        hydrate_exec = "WITH RECURSIVE root AS (SELECT root_ref AS hash FROM exec_snapshots WHERE exec_key = ?2),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Owner `?2`'s run `?3` fact `?4`'s node, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_run_fact = "WITH RECURSIVE root AS (SELECT record_ref AS hash FROM run_records WHERE owner_key = ?2 AND run_seq = ?3 AND ordinal = ?4),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Effect `?3` of execution `?2`'s Admission node, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_admission = "WITH RECURSIVE root AS (SELECT admission_ref AS hash FROM effect_admissions WHERE exec_key = ?2 AND effect_id = ?3),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Run `?3` of session `?2`'s PhaseRoot, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_phase = "WITH RECURSIVE root AS (SELECT checkpoint_ref AS hash FROM turn_phases WHERE session_id = ?2 AND run = ?3),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Run `?3` of session `?2`'s working execution, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_working = "WITH RECURSIVE root AS (SELECT working_execution_ref AS hash FROM turn_phases WHERE session_id = ?2 AND run = ?3),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Plugin `?4`'s namespace values for run `?3` of session `?2`, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_namespace = "WITH RECURSIVE root AS (SELECT graph_root_ref AS hash FROM turn_namespaces WHERE session_id = ?2 AND run = ?3 AND plugin = ?4),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Session `?2`'s revision `?3`'s checkpoint, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_revision = "WITH RECURSIVE root AS (SELECT checkpoint_ref AS hash FROM session_revisions WHERE session_id = ?2 AND head_revision = ?3),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Session `?2`'s admission base, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_admission_base = "WITH RECURSIVE root AS (SELECT admission_base_checkpoint_ref AS hash FROM session_meta WHERE session_id = ?2),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Artifact `?3` of namespace `?2`'s graph node, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_artifact = "WITH RECURSIVE root AS (SELECT graph_ref AS hash FROM lash_vm_artifacts WHERE namespace = ?2 AND artifact_ref = ?3 AND storage_kind = 1),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
        /// Release `?2`'s manifest, as [`Self::hydrate_exec`] reads an execution's.
        hydrate_release = "WITH RECURSIVE root AS (SELECT root_ref AS hash FROM graph_release_pins WHERE release_key = ?2),
             reach AS (SELECT root.hash FROM root WHERE root.hash IS NOT NULL UNION
                 SELECT edge.child_hash FROM blob_edges AS edge JOIN reach ON edge.parent_hash = reach.hash
                 WHERE NOT (reach.hash = ANY(?1::text[])) AND NOT (edge.child_hash = ANY(?1::text[])))
             SELECT root.hash, blob.hash, blob.encoding, blob.content
             FROM root LEFT JOIN (reach JOIN blobs AS blob ON blob.hash = reach.hash) ON TRUE";
    }
}

struct Sql {
    graph: GraphStatements,
    postgres: GraphPostgresStatements,
    snapshots: lash_store_sql::durable::snapshots::SnapshotStatements,
    facts: lash_store_sql::durable::run_records::RunRecordStatements,
    turns: lash_store_sql::durable::turns::TurnStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    Sql {
        graph: GraphStatements::render(dialect),
        postgres: GraphPostgresStatements::render(dialect),
        snapshots: lash_store_sql::durable::snapshots::SnapshotStatements::render(dialect),
        facts: lash_store_sql::durable::run_records::RunRecordStatements::render(dialect),
        turns: lash_store_sql::durable::turns::TurnStatements::render(dialect),
    }
});

/// The install's statements the one-call commit runs.
pub(super) fn lock_nodes() -> &'static Rendered {
    &SQL.postgres.lock_nodes
}

pub(super) fn insert_nodes() -> &'static Rendered {
    &SQL.postgres.insert_nodes
}

pub(super) fn insert_edges() -> &'static Rendered {
    &SQL.postgres.insert_edges
}

/// The key-share lock and read of the staged nodes another writer stored.
pub(super) fn stored_nodes() -> &'static Rendered {
    &SQL.postgres.stored_nodes
}

/// A peel level's lock, and its delete.
pub(super) fn peel_lock() -> &'static Rendered {
    &SQL.postgres.peel_lock
}

pub(super) fn peel_level() -> &'static Rendered {
    &SQL.postgres.peel_level
}

/// One page of the orphan audit.
const GRAPH_AUDIT: CommitLabel = CommitLabel::new("graph.audit");

/// The orphans one audit page reads.
pub(crate) const AUDIT_PAGE: i64 = 256;

/// A graph answer: a value or a typed refusal that rolls the transaction
/// back.
type Answer<T> = Result<T, GraphRefusal>;

/// A durable port refusal as a graph refusal: a lost fence stays typed, a
/// second outcome is a duplicate, another domain write's refusal is a
/// conflict, and a store failure keeps its kind.
pub(super) fn from_durable(error: DurableError) -> GraphRefusal {
    match error {
        DurableError::Store(failure) => GraphRefusal::Backend(failure),
        DurableError::OwnershipLost(_) => GraphRefusal::FenceLost,
        DurableError::Graph(refusal) => refusal,
        DurableError::Domain(DomainRefusal::OutcomeExists { call, .. }) => {
            GraphRefusal::DuplicateOutcome(call)
        }
        DurableError::Domain(_) => GraphRefusal::RevisionConflict,
        other => GraphRefusal::Backend(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message: other.to_string(),
        }),
    }
}

pub(super) fn db(error: sqlx::Error) -> GraphRefusal {
    from_durable(sqlx_failure(error))
}

fn number<T>(value: impl TryInto<T, Error: std::fmt::Display>) -> Answer<T> {
    integer(value).map_err(from_durable)
}

fn corrupt(what: &str, value: &str) -> GraphRefusal {
    GraphRefusal::Backend(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("stored {what} `{value}` does not decode"),
    })
}

pub(super) fn node_of(stored: &str) -> Answer<NodeId> {
    NodeId::from_hex(stored).ok_or_else(|| corrupt("node reference", stored))
}

pub(super) fn encoding_of(stored: i16) -> Answer<BlobEncoding> {
    match stored {
        0 => Ok(BlobEncoding::Opaque),
        1 => Ok(BlobEncoding::Graph),
        other => Err(corrupt("blob encoding", &other.to_string())),
    }
}

/// The graph nodes reachable from `root`, without reading any opaque blob
/// they reference: what a reader of the graph alone (a session manifest, a
/// namespace value) decodes from. An opaque blob a node references is known
/// by its encoding alone, which is what a decoder asks of it: it reads an
/// opaque child's reference, never its bytes.
pub(crate) struct GraphClosure {
    nodes: BTreeMap<NodeId, HydratedNode>,
    opaque: BTreeSet<NodeId>,
}

impl NodeSource for GraphClosure {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        match self.nodes.get(id) {
            Some(node) => Some(StoredNode {
                encoding: node.encoding,
                bytes: &node.bytes,
            }),
            None => self.opaque.contains(id).then_some(StoredNode {
                encoding: BlobEncoding::Opaque,
                bytes: &[],
            }),
        }
    }
}

/// [`GraphClosure`] of `root`, read on the caller's connection.
pub(crate) async fn graph_closure(
    tx: &mut PgConnection,
    root: NodeId,
    limits: &DecodeLimits,
) -> Answer<GraphClosure> {
    let mut opaque = BTreeSet::new();
    let nodes = walk_within(tx, &[root], limits, Some(&mut opaque)).await?;
    Ok(GraphClosure { nodes, opaque })
}

async fn walk_within(
    tx: &mut PgConnection,
    roots: &[NodeId],
    limits: &DecodeLimits,
    skipped: Option<&mut BTreeSet<NodeId>>,
) -> Answer<BTreeMap<NodeId, HydratedNode>> {
    if roots.is_empty() {
        return Ok(BTreeMap::new());
    }
    // One recursive query seeds every selected fact root. Bound the
    // retained closure as rows arrive, before buffering its material.
    // A stored node's edges keep every child it names stored, and the
    // child's own edges keep its children: a member of the build's catalog
    // a stored node reaches is stored with all it reaches, under the
    // build's bytes. The query stops at the catalog's heads, and the walk
    // takes each member it meets, and all it reaches, from the build.
    let roots_hex: Vec<_> = roots.iter().map(NodeId::to_hex).collect();
    let heads: Vec<_> = immutable_catalog_heads()
        .iter()
        .map(NodeId::to_hex)
        .collect();
    let reach = if skipped.is_some() {
        SQL.postgres.walk_reach_graph.sql()
    } else {
        SQL.postgres.walk_reach.sql()
    };
    let mut found = BTreeMap::new();
    let mut total = 0_u64;
    let mut count = 0_u64;
    {
        let mut rows = crate::observed_sql::query_as::<_, (String, i16, Option<Vec<u8>>)>(reach)
            .bind(&roots_hex)
            .bind(&heads)
            .fetch(crate::observed_sql::executor(&mut *tx));
        while let Some((hash, encoding, bytes)) = rows.try_next().await.map_err(db)? {
            let id = node_of(&hash)?;
            let encoding = encoding_of(encoding)?;
            if let Some(bytes) = &bytes {
                charge_node(limits, &mut count, &mut total, bytes.len())?;
            }
            found.insert(id, (encoding, bytes));
        }
    }
    walk_found(
        Some(tx),
        roots,
        found,
        Budget { count, total },
        limits,
        skipped,
    )
    .await
}

/// What a walk has charged against its limits so far.
struct Budget {
    count: u64,
    total: u64,
}

/// What a walk has read: each node's encoding and, unless skipped, bytes.
type Found = BTreeMap<NodeId, (BlobEncoding, Option<Vec<u8>>)>;

/// The nodes `roots` reach, checked within `limits`, from the rows `found`
/// already read and charged to `spent`; a member of the build's catalog,
/// and all it reaches, is taken from the build. A node they lack is read on
/// `tx`; with none, the rows were the whole read, and a node they lack is
/// missing.
async fn walk_found(
    mut tx: Option<&mut PgConnection>,
    roots: &[NodeId],
    mut found: Found,
    spent: Budget,
    limits: &DecodeLimits,
    mut skipped: Option<&mut BTreeSet<NodeId>>,
) -> Answer<BTreeMap<NodeId, HydratedNode>> {
    let Budget {
        mut count,
        mut total,
    } = spent;
    let roots_hex: Vec<_> = roots.iter().map(NodeId::to_hex).collect();
    let member = |id: &NodeId| {
        if roots.contains(id) {
            None
        } else {
            immutable_closure(*id)
        }
    };
    let mut nodes = BTreeMap::new();
    let mut level = BTreeSet::new();
    let mut pending_roots = roots.iter();
    let mut visited = BTreeSet::new();
    loop {
        if level.is_empty() {
            // Preserve fact-order refusal precedence while sharing the
            // closure, visited set and materialization budget across roots.
            let Some(root) = pending_roots.find(|id| !visited.contains(*id)) else {
                break;
            };
            level.insert(*root);
        }
        let unread: Vec<_> = level
            .iter()
            .filter(|id| !found.contains_key(id) && member(id).is_none())
            .map(NodeId::to_hex)
            .collect();
        if !unread.is_empty()
            && let Some(tx) = tx.as_deref_mut()
        {
            // A malformed edge table must not hide a child named in the
            // checked envelope. Read the missing level within the same budget.
            let mut rows = crate::observed_sql::query_as::<_, (String, i16, Option<Vec<u8>>)>(
                SQL.postgres.walk_level.sql(),
            )
            .bind(&unread)
            .bind(&roots_hex)
            .bind(skipped.is_some())
            .fetch(crate::observed_sql::executor(&mut *tx));
            while let Some((hash, encoding, bytes)) = rows.try_next().await.map_err(db)? {
                let id = node_of(&hash)?;
                let encoding = encoding_of(encoding)?;
                if let Some(bytes) = &bytes {
                    charge_node(limits, &mut count, &mut total, bytes.len())?;
                }
                found.insert(id, (encoding, bytes));
            }
        }
        let mut next = BTreeSet::new();
        for id in level {
            if let Some(closure) = member(&id) {
                for node in closure.nodes() {
                    if !visited.insert(node.id) {
                        continue;
                    }
                    // A row the query read for it was charged as it arrived.
                    if !matches!(found.remove(&node.id), Some((_, Some(_)))) {
                        charge_node(limits, &mut count, &mut total, node.bytes.len())?;
                    }
                    let len = node.bytes.len() as u64;
                    if len > limits.node_bytes {
                        return Err(GraphRefusal::DecodeLimit {
                            dimension: DecodeDimension::NodeBytes,
                            actual: len,
                            limit: limits.node_bytes,
                        });
                    }
                    nodes.insert(
                        node.id,
                        HydratedNode {
                            encoding: BlobEncoding::Graph,
                            bytes: std::sync::Arc::clone(&node.bytes),
                        },
                    );
                }
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            let Some((encoding, bytes)) = found.remove(&id) else {
                return Err(GraphRefusal::MissingNode(id));
            };
            if encoding == BlobEncoding::Opaque
                && !roots.contains(&id)
                && let Some(skipped) = skipped.as_deref_mut()
            {
                skipped.insert(id);
                continue;
            }
            let bytes = bytes.ok_or(GraphRefusal::MissingNode(id))?;
            if encoding == BlobEncoding::Graph {
                next.extend(check_envelope(&bytes, id, limits)?.children.iter());
            }
            nodes.insert(
                id,
                HydratedNode {
                    encoding,
                    bytes: bytes.into(),
                },
            );
        }
        next.retain(|id| !visited.contains(id));
        level = next;
    }
    Ok(nodes)
}

fn charge_node(
    limits: &DecodeLimits,
    count: &mut u64,
    total: &mut u64,
    bytes: usize,
) -> Answer<()> {
    *count = count.saturating_add(1);
    *total = total.saturating_add(bytes as u64);
    for (dimension, actual, limit) in [
        (DecodeDimension::Nodes, *count, limits.nodes),
        (DecodeDimension::TotalBytes, *total, limits.total_bytes),
    ] {
        if actual > limit {
            return Err(GraphRefusal::DecodeLimit {
                dimension,
                actual,
                limit,
            });
        }
    }
    Ok(())
}

/// One row of a hydration: the owner's root column, then one blob it
/// reaches (none when it reaches none).
type HydrateRow = (Option<String>, Option<String>, Option<i16>, Option<Vec<u8>>);

/// The one statement that hydrates `owner`'s root on `connection`: its root
/// column and every blob it reaches.
async fn hydrate_rows(connection: &mut PgConnection, owner: &RootOwner) -> Answer<Vec<HydrateRow>> {
    let sql = &SQL.postgres;
    let heads: Vec<String> = immutable_catalog_heads()
        .iter()
        .map(NodeId::to_hex)
        .collect();
    let query = |statement| {
        crate::observed_sql::query_as::<sqlx::Postgres, HydrateRow>(statement).bind(heads.clone())
    };
    let bound = match owner {
        RootOwner::Exec(exec) => query(sql.hydrate_exec.sql()).bind(exec.stored()),
        RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => query(sql.hydrate_run_fact.sql())
            .bind(owner.stored())
            .bind(number::<i64>(run.0)?)
            .bind(number::<i64>(ordinal.0)?),
        RootOwner::EffectAdmission { exec, effect } => query(sql.hydrate_admission.sql())
            .bind(exec.stored())
            .bind(effect.0.to_vec()),
        RootOwner::TurnPhase { session, run } => query(sql.hydrate_phase.sql())
            .bind(session.as_str())
            .bind(run.as_str()),
        RootOwner::WorkingExecution { session, run } => query(sql.hydrate_working.sql())
            .bind(session.as_str())
            .bind(run.as_str()),
        RootOwner::TurnNamespace {
            session,
            run,
            plugin,
        } => query(sql.hydrate_namespace.sql())
            .bind(session.as_str())
            .bind(run.as_str())
            .bind(plugin.as_str()),
        RootOwner::SessionRevision { session, revision } => query(sql.hydrate_revision.sql())
            .bind(session.as_str())
            .bind(number::<i64>(*revision)?),
        RootOwner::SessionAdmissionBase { session } => {
            query(sql.hydrate_admission_base.sql()).bind(session.as_str())
        }
        RootOwner::ArtifactBridge {
            namespace,
            artifact,
        } => query(sql.hydrate_artifact.sql())
            .bind(namespace.as_str())
            .bind(artifact.as_str()),
        RootOwner::ReleasePin { release } => {
            query(sql.hydrate_release.sql()).bind(release.as_str())
        }
    };
    bound
        .fetch_all(crate::observed_sql::executor(connection))
        .await
        .map_err(db)
}

/// The root `owner` names now, read on the caller's connection: none when
/// the owner is gone or holds no root.
pub(super) async fn root_of(
    connection: &mut PgConnection,
    owner: &RootOwner,
) -> Answer<Option<NodeId>> {
    let sql = &*SQL;
    let query =
        |statement| crate::observed_sql::query_scalar::<sqlx::Postgres, Option<String>>(statement);
    let bound = match owner {
        RootOwner::Exec(exec) => {
            let row: Option<(i64, Option<String>)> =
                crate::observed_sql::query_as(sql.snapshots.root.sql())
                    .bind(exec.stored())
                    .fetch_optional(crate::observed_sql::executor(&mut *connection))
                    .await
                    .map_err(db)?;
            return row
                .and_then(|(_, root)| root)
                .as_deref()
                .map(node_of)
                .transpose();
        }
        RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => query(sql.facts.root.sql())
            .bind(owner.stored())
            .bind(number::<i64>(run.0)?)
            .bind(number::<i64>(ordinal.0)?),
        RootOwner::EffectAdmission { exec, effect } => query(sql.graph.admission_root.sql())
            .bind(exec.stored())
            .bind(effect.0.to_vec()),
        RootOwner::TurnPhase { session, run } => query(sql.turns.phase_root.sql())
            .bind(session.as_str())
            .bind(run.as_str()),
        RootOwner::WorkingExecution { session, run } => query(sql.turns.working_root.sql())
            .bind(session.as_str())
            .bind(run.as_str()),
        RootOwner::TurnNamespace {
            session,
            run,
            plugin,
        } => query(sql.turns.namespace_root.sql())
            .bind(session.as_str())
            .bind(run.as_str())
            .bind(plugin.as_str()),
        RootOwner::SessionRevision { session, revision } => query(sql.graph.revision_root.sql())
            .bind(session.as_str())
            .bind(number::<i64>(*revision)?),
        RootOwner::SessionAdmissionBase { session } => {
            query(sql.graph.admission_base_root.sql()).bind(session.as_str())
        }
        RootOwner::ArtifactBridge {
            namespace,
            artifact,
        } => query(sql.postgres.artifact_root.sql())
            .bind(namespace.as_str())
            .bind(artifact.as_str()),
        RootOwner::ReleasePin { release } => {
            query(sql.graph.release_root.sql()).bind(release.as_str())
        }
    };
    bound
        .fetch_optional(crate::observed_sql::executor(connection))
        .await
        .map_err(db)?
        .flatten()
        .as_deref()
        .map(node_of)
        .transpose()
}

/// `owner`'s root graph, hydrated in one statement on `connection`: its
/// snapshot holds the root and everything it reaches, so a root a peel
/// retires after it began is read whole, and one retired before is gone.
pub(super) async fn hydrate_in(
    connection: &mut PgConnection,
    owner: RootOwner,
    expected: Option<NodeId>,
    limits: &DecodeLimits,
) -> Answer<HydratedGraph> {
    let rows = hydrate_rows(connection, &owner).await?;
    let Some(root) = rows.first().and_then(|(root, ..)| root.as_deref()) else {
        return Err(GraphRefusal::RootRetired);
    };
    let root = node_of(root)?;
    if expected.is_some_and(|expected| expected != root) {
        return Err(GraphRefusal::RevisionConflict);
    }
    let mut found = Found::new();
    let (mut count, mut total) = (0, 0);
    for (_, hash, encoding, bytes) in rows {
        if let (Some(hash), Some(encoding)) = (hash, encoding) {
            if let Some(bytes) = &bytes {
                charge_node(limits, &mut count, &mut total, bytes.len())?;
            }
            found.insert(node_of(&hash)?, (encoding_of(encoding)?, bytes));
        }
    }
    let spent = Budget { count, total };
    let nodes = walk_found(None, &[root], found, spent, limits, None).await?;
    Ok(HydratedGraph { owner, root, nodes })
}

/// Whether `query`, a conditional write that returns its row, matched one.
async fn matched(tx: &mut PgConnection, query: crate::observed_sql::Query<'_>) -> Answer<bool> {
    Ok(query
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .is_some())
}

fn graph_refused(refusal: GraphRefusal) -> DurableError {
    DurableError::Graph(refusal)
}

/// The effect-admission statements an owner commit runs as steps.
pub(super) static ADMISSION_PUT: Entry = Entry::new(
    || &SQL.graph.admission_put,
    &[
        Ty::Text,
        Ty::Bytes,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Text,
        Ty::Int,
    ],
    Shape::Returning,
);
pub(super) static ADMISSION_REMOVE: Entry = Entry::new(
    || &SQL.graph.admission_remove,
    &[Ty::Text, Ty::Bytes, Ty::Text],
    Shape::Returning,
);

/// `write`'s step program at `epoch`: an admission's insert, which must be
/// the effect's first, or its lookup's guarded removal.
pub(super) fn admission_steps(
    epoch: Epoch,
    write: &EffectAdmissionWrite,
) -> Result<Vec<Step>, DurableError> {
    Ok(match write {
        EffectAdmissionWrite::Put {
            exec,
            effect,
            member,
            admission,
        } => vec![Step::one(
            &ADMISSION_PUT,
            vec![
                Value::text(exec.stored()),
                Value::Bytes(Some(effect.0.to_vec())),
                Value::Text(member.as_ref().map(|member| member.owner.stored())),
                Value::Int(
                    member
                        .as_ref()
                        .map(|member| integer::<i64>(member.run.0))
                        .transpose()?,
                ),
                Value::Int(
                    member
                        .as_ref()
                        .map(|member| integer::<i64>(member.member))
                        .transpose()?,
                ),
                Value::text(admission.to_hex()),
                Value::int(epoch.0),
            ],
        )],
        EffectAdmissionWrite::Remove {
            exec,
            effect,
            expected,
        } => vec![
            Step::one(
                &ADMISSION_REMOVE,
                vec![
                    Value::text(exec.stored()),
                    Value::Bytes(Some(effect.0.to_vec())),
                    Value::text(expected.to_hex()),
                ],
            )
            .retiring([expected.to_hex()]),
        ],
    })
}

/// Every admission of the executions under a key: a terminal's cells.
pub(super) static ADMISSIONS_UNDER: Entry = Entry::new(
    || &SQL.graph.admissions_clear_under,
    &[Ty::Text],
    Shape::Returning,
)
.retiring("admission_ref");
pub(super) static CLEAR_ADMISSIONS: Entry = Entry::new(
    || &SQL.graph.admissions_clear,
    &[Ty::Text],
    Shape::Returning,
)
.retiring("admission_ref");

/// Applies an admission write. A second admission of one effect, or the
/// removal of a lookup that is not the expected one, refuses the commit; a
/// removal retires its Admission node.
pub(super) async fn apply_admission(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &EffectAdmissionWrite,
) -> Result<(), DurableError> {
    let program = admission_steps(commit.epoch, write)?;
    match steps::run(tx, &program, &commit.steps()).await? {
        None => Ok(()),
        Some(_) => Err(graph_refused(GraphRefusal::RevisionConflict)),
    }
}

/// Applies a release pin write.
pub(super) async fn apply_release_pin(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ReleasePinWrite,
) -> Result<(), DurableError> {
    match write {
        ReleasePinWrite::Install { release, root } => {
            let installed = crate::observed_sql::query(SQL.graph.release_install.sql())
                .bind(release.as_str())
                .bind(root.to_hex())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?
                .rows_affected();
            if installed == 0 {
                let pinned: String =
                    crate::observed_sql::query_scalar(SQL.graph.release_root.sql())
                        .bind(release.as_str())
                        .fetch_one(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                if pinned != root.to_hex() {
                    return Err(graph_refused(GraphRefusal::RevisionConflict));
                }
            }
            Ok(())
        }
        ReleasePinWrite::Retire { release, expected } => {
            let retired = matched(
                tx,
                crate::observed_sql::query(SQL.graph.release_retire.sql())
                    .bind(release.as_str())
                    .bind(expected.to_hex()),
            )
            .await
            .map_err(graph_refused)?;
            if retired {
                commit.retired.closure(expected);
                Ok(())
            } else {
                Err(graph_refused(GraphRefusal::RevisionConflict))
            }
        }
    }
}

/// Installs a checked graph in the caller's guarded transaction. Every node
/// it names without supplying is key-share locked in digest order against a
/// peel and must exist (a published root as a graph node); the same
/// statement locks every supplied node already stored and reads its bytes
/// back for comparison, so only the others are bound to the insert and a
/// node the store holds never crosses the wire twice. A graph that names no
/// node offers every supplied node to the insert without that read, and
/// locks and compares those already stored after it, installing again any a
/// peel took in between. One statement writes the fresh nodes' edges. Supplied digests are bound as their 32 raw bytes.
pub(crate) async fn install(
    tx: &mut PgConnection,
    graph: &CheckedGraph,
) -> Answer<InstalledGraphMeasurement> {
    let named: Vec<String> = graph
        .external
        .iter()
        .chain(graph.external_roots.iter())
        .collect::<BTreeSet<&NodeId>>()
        .into_iter()
        .map(NodeId::to_hex)
        .collect();
    let supplied: Vec<String> = graph.nodes.iter().map(|node| node.id.to_hex()).collect();
    let mut installed = InstalledGraphMeasurement::default();
    let mut present = BTreeMap::new();
    if !named.is_empty() {
        let digests: Vec<&[u8]> = graph.nodes.iter().map(|node| &node.id.0[..]).collect();
        let rows: Vec<(String, i16, Option<Vec<u8>>)> =
            crate::observed_sql::query_as(SQL.postgres.lock_nodes.sql())
                .bind(&named)
                .bind(&digests)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(db)?;
        for (hash, encoding, bytes) in rows {
            present.insert(hash, (encoding_of(encoding)?, bytes));
        }
    }
    for node in &graph.external {
        if !present.contains_key(&node.to_hex()) {
            return Err(GraphRefusal::MissingNode(*node));
        }
    }
    for root in &graph.external_roots {
        match present.get(&root.to_hex()) {
            None => return Err(GraphRefusal::MissingNode(*root)),
            Some((BlobEncoding::Opaque, _)) => return Err(GraphRefusal::EncodingConflict(*root)),
            Some((BlobEncoding::Graph, _)) => {}
        }
    }
    let mut absent = Vec::new();
    for (node, hash) in graph.nodes.iter().zip(&supplied) {
        match present.get(hash) {
            None => absent.push((node, hash)),
            Some((encoding, bytes)) => compare(node.id, &node.bytes, *encoding, bytes.as_deref())?,
        }
    }
    if absent.is_empty() {
        return Ok(installed);
    }
    let inserted = insert_absent(tx, &absent).await?;
    let mut parents = Vec::new();
    let mut children = Vec::new();
    for (node, hash) in &absent {
        if !inserted.contains(*hash) {
            continue;
        }
        installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
        installed.new_nodes += 1;
        installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        for child in &node.children {
            parents.push(&node.id.0[..]);
            children.push(&child.0[..]);
            installed.new_edges += 1;
            installed.sql_logical_bytes += EDGE_ROW_LOGICAL_BYTES;
        }
    }
    if !parents.is_empty() {
        crate::observed_sql::query(SQL.postgres.insert_edges.sql())
            .bind(&parents)
            .bind(&children)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
    }
    if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
        window.installed_graph(installed.newly_installed_canonical_bytes);
    }
    Ok(installed)
}

/// Inserts `absent`, the supplied nodes a lock found unstored, in digest
/// order, and answers the digests it inserted. One another writer stored
/// first is key-share locked and compared; one a peel deleted between the
/// insert's conflict and that lock is installed again from the bytes the
/// commit carries (FIG-5949); each round only shrinks.
pub(super) async fn insert_absent(
    tx: &mut PgConnection,
    absent: &[(&CheckedNode, &String)],
) -> Answer<BTreeSet<String>> {
    let digests: Vec<&[u8]> = absent.iter().map(|(node, _)| &node.id.0[..]).collect();
    let contents: Vec<&[u8]> = absent.iter().map(|(node, _)| &node.bytes[..]).collect();
    let mut inserted: BTreeSet<String> =
        crate::observed_sql::query_scalar(SQL.postgres.insert_nodes.sql())
            .bind(&digests)
            .bind(&contents)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?
            .into_iter()
            .collect();
    let mut held: Vec<&str> = absent
        .iter()
        .map(|(_, hash)| hash.as_str())
        .filter(|hash| !inserted.contains(*hash))
        .collect();
    while !held.is_empty() {
        let rows: Vec<(String, Vec<u8>, i16)> =
            crate::observed_sql::query_as(SQL.postgres.stored_nodes.sql())
                .bind(&held)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(db)?;
        let stored: BTreeMap<String, (Vec<u8>, i16)> = rows
            .into_iter()
            .map(|(hash, bytes, encoding)| (hash, (bytes, encoding)))
            .collect();
        let mut peeled = Vec::new();
        for (node, hash) in absent {
            if !held.contains(&hash.as_str()) {
                continue;
            }
            match stored.get(*hash) {
                Some((bytes, encoding)) => {
                    compare(node.id, &node.bytes, encoding_of(*encoding)?, Some(bytes))?;
                }
                None => peeled.push((*node, hash.as_str())),
            }
        }
        if peeled.is_empty() {
            break;
        }
        let digests: Vec<&[u8]> = peeled.iter().map(|(node, _)| &node.id.0[..]).collect();
        let contents: Vec<&[u8]> = peeled.iter().map(|(node, _)| &node.bytes[..]).collect();
        let fresh: BTreeSet<String> =
            crate::observed_sql::query_scalar(SQL.postgres.insert_nodes.sql())
                .bind(&digests)
                .bind(&contents)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(db)?
                .into_iter()
                .collect();
        held = peeled
            .iter()
            .map(|(_, hash)| *hash)
            .filter(|hash| !fresh.contains(*hash))
            .collect();
        inserted.extend(fresh);
    }
    Ok(inserted)
}

/// A supplied node against the blob stored under its digest: a graph node
/// of exactly its bytes, or a conflict.
pub(super) fn compare(
    id: NodeId,
    supplied: &[u8],
    encoding: BlobEncoding,
    stored: Option<&[u8]>,
) -> Answer<()> {
    if encoding != BlobEncoding::Graph {
        return Err(GraphRefusal::EncodingConflict(id));
    }
    if stored != Some(supplied) {
        return Err(GraphRefusal::ContentConflict(id));
    }
    Ok(())
}

/// Installs the staged `nodes` in the caller's guarded transaction (the
/// session save path): every node they reference without supplying must be
/// stored.
pub(crate) async fn install_staged(
    tx: &mut PgConnection,
    nodes: &BTreeMap<NodeId, lash_core_store::checkpoint_graph::EncodedNode>,
) -> Answer<()> {
    let checked = lash_durable::domain::graph_commit::check_graph(nodes, &BTreeSet::new(), &[])?;
    install(tx, &checked).await.map(|_| ())
}

impl PostgresDurableStore {
    /// End a graph transaction as its body answered: an answer commits it,
    /// a refusal rolls it back.
    pub(super) async fn settle_graph<T>(tx: Tx, outcome: Answer<T>) -> Answer<T> {
        match outcome {
            Ok(value) => {
                tx.commit().await.map_err(db)?;
                Ok(value)
            }
            Err(refusal) => {
                // A failed transaction is gone either way: rolled back here,
                // or by the server when its connection closes.
                let _ = tx.rollback().await;
                Err(refusal)
            }
        }
    }

    /// `owner`'s root graph, in one statement on a reader connection: no
    /// transaction, no lock, no write.
    pub(super) async fn hydrate_owner(
        &self,
        owner: RootOwner,
        expected: Option<NodeId>,
        limits: DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut reader = self.reader().await?;
            hydrate_in(&mut reader, owner, expected, &limits)
                .await
                .map_err(DurableError::Graph)
        })
        .await
    }

    /// Every run fact of `owner` and every node they reach, read in one
    /// `REPEATABLE READ READ ONLY` snapshot.
    pub(super) async fn read_run_facts(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        self.read_run_facts_within(owner, run, None, self.pools.graph_reads.owner())
            .await
    }

    pub(super) async fn read_run_facts_within(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
        max_rows: Option<usize>,
        limits: DecodeLimits,
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        let policy = self.pools.graph_reads.owner();
        let requested = limits.kernel_data;
        let configured = policy.kernel_data;
        let kernel_data = lash_core_store::checkpoint_graph::KernelDataLimits::new(
            requested.document_depth().min(configured.document_depth()),
            requested
                .document_json_depth()
                .min(configured.document_json_depth()),
            requested.decode_depth().min(configured.decode_depth()),
            requested
                .state_json_depth()
                .min(configured.state_json_depth()),
            requested.value_depth().min(configured.value_depth()),
        )
        .and_then(|limits| {
            limits.with_inline_depth(requested.inline_depth().min(configured.inline_depth()))
        })
        .ok_or_else(|| super::corrupt("diagnostic read limits", "invalid kernel bounds"))?;
        let limits = DecodeLimits {
            kernel_data,
            node_bytes: limits.node_bytes.min(policy.node_bytes),
            total_bytes: limits.total_bytes.min(policy.total_bytes),
            nodes: limits.nodes.min(policy.nodes),
            logical_bytes: limits.logical_bytes.min(policy.logical_bytes),
            depth: limits.depth.min(policy.depth),
        };
        self.read(|connection| {
            Box::pin(async move {
                let mut snapshot = super::snapshot(connection).await?;
                let rows =
                    super::run_records::read_limited(&mut snapshot, owner, run, max_rows).await?;
                let roots: Vec<_> = rows.iter().map(|row| row.record).collect();
                let nodes = walk_within(&mut snapshot, &roots, &limits, None)
                    .await
                    .map_err(DurableError::Graph)?;
                snapshot.rollback().await.map_err(sqlx_failure)?;
                Ok(lash_durable::domain::RunFacts {
                    rows,
                    nodes,
                    read_limits: limits,
                })
            })
        })
        .await
    }

    /// Runs the orphan audit to its end, a page per guarded transaction:
    /// peels every blob no root names and no edge reaches, with what only
    /// it reached. It holds no lock beyond the rows of one page, so commits
    /// beside it progress. Answers how many blobs it deleted.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn audit_orphans(&self) -> Result<u64, GraphRefusal> {
        let mut after = String::new();
        let mut deleted = 0_u64;
        loop {
            let label = GRAPH_AUDIT;
            let from = after.clone();
            let (last, gone) = self
                .within(label.capacity(), async {
                    let (mut tx, _) = self.open(label).await?;
                    let outcome = audit_page(&mut tx, &from, AUDIT_PAGE).await;
                    Ok(Self::settle_graph(tx, outcome).await)
                })
                .await
                .map_err(from_durable)??;
            deleted += gone as u64;
            match last {
                Some(last) => after = last,
                None => return Ok(deleted),
            }
        }
    }
}

#[cfg(test)]
#[path = "../graph_store_tests.rs"]
mod graph_store_tests;

/// Drop every admission whose execution key starts with `key`, answering
/// the Admission nodes they held: the caller's to peel.
pub(super) async fn clear_admissions_under(
    tx: &mut PgConnection,
    key: &str,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query_scalar(SQL.graph.admissions_clear_under.sql())
        .bind(key)
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}

#[cfg(test)]
mod run_fact_tests {
    use super::*;
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    use lash_durable::DurableReads;
    use lash_durable::domain::{OwnerKey, RunSeq};

    /// RUN-READ: the fact walk reads all selected roots and their shared
    /// descendants in one PostgreSQL round trip, for either read shape.
    #[tokio::test]
    async fn a_run_scoped_fact_walk_takes_one_round_trip() {
        let url = crate::postgres_test_support::database_url().expect("hermetic PostgreSQL");
        let database = crate::testing::IsolatedDatabase::create(&url).await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("store");
        let store = storage.durable_store();
        let owner = OwnerKey::Turn("fact-walk-session".into(), "fact-walk-turn".into());
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut body = TokenWriter::new();
        body.text("shared");
        let shared = staging
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
            .expect("shared");
        let mut roots = Vec::new();
        for run in 1..=8 {
            for ordinal in 0..2 {
                let mut body = TokenWriter::new();
                body.seq(2);
                body.text(&format!("round {run} fact {ordinal}"));
                body.child(shared.child_ref());
                let root = staging
                    .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
                    .expect("fact");
                roots.push((run, ordinal, root));
            }
        }
        let mut connection = store.reader().await.expect("connection");
        install_staged(&mut connection, staging.nodes())
            .await
            .expect("install");
        let facts =
            lash_store_sql::durable::run_records::RunRecordStatements::render(Dialect::postgres());
        for (run, ordinal, root) in roots {
            sqlx::query(facts.append.sql())
                .bind(owner.stored())
                .bind(run as i64)
                .bind(ordinal as i64)
                .bind("admit")
                .bind(Option::<String>::None)
                .bind(root.to_hex())
                .bind(1_i64)
                .execute(&mut *connection)
                .await
                .expect("fact row");
        }
        drop(connection);
        for run in [Some(RunSeq(8)), None] {
            let work = async {
                match run {
                    Some(run) => store.run_facts_of(&owner, run).await,
                    None => store.run_facts(&owner).await,
                }
            };
            let (answer, receipt) =
                lash_core_execution::facade_support::sql::collect("fact-walk", "postgres", work)
                    .await;
            let facts = answer.expect("read");
            let expected_rows = if run.is_some() { 2 } else { 16 };
            assert_eq!(facts.rows.len(), expected_rows);
            assert_eq!(facts.nodes.len(), expected_rows + 1);
            let walks: Vec<_> = receipt
                .shapes
                .iter()
                .filter(|(sql, _)| sql.starts_with("with recursive reach"))
                .collect();
            assert_eq!(
                walks.len(),
                1,
                "one recursive fact walk: {:?}",
                receipt.shapes
            );
            assert_eq!(walks[0].1.statements, 1, "one round trip for the fact walk");
            assert_eq!(walks[0].1.rows_returned, (expected_rows + 1) as u64);
            eprintln!(
                "{run:?}: fact-walk round_trips=1 rows={} nodes={}",
                facts.rows.len(),
                facts.nodes.len()
            );
            if let Some(run) = run {
                assert!(facts.rows.iter().all(|row| row.run == run));
                let mut connection = store.reader().await.expect("connection");
                let roots: Vec<_> = facts.rows.iter().map(|row| row.record).collect();
                let total: u64 = facts
                    .nodes
                    .values()
                    .map(|node| node.bytes.len() as u64)
                    .sum();
                let mut limits = DecodeLimits::within_total(total);
                limits.nodes = 3;
                assert_eq!(
                    walk_within(&mut connection, &roots, &limits, None)
                        .await
                        .expect("exact budget"),
                    facts.nodes
                );
                limits.nodes = 2;
                assert!(matches!(
                    walk_within(&mut connection, &roots, &limits, None).await,
                    Err(GraphRefusal::DecodeLimit {
                        dimension: DecodeDimension::Nodes,
                        actual: 3,
                        limit: 2
                    })
                ));
                limits.nodes = 3;
                limits.total_bytes = total - 1;
                assert!(matches!(
                    walk_within(&mut connection, &roots, &limits, None).await,
                    Err(GraphRefusal::DecodeLimit {
                        dimension: DecodeDimension::TotalBytes,
                        ..
                    })
                ));
            }
        }
        // A missing stored edge must not hide an opaque child named by
        // the envelope. Its fallback read shares the materialization budget.
        let opaque = NodeId([0x88; 32]);
        let opaque_bytes = b"opaque child";
        let mut connection = store.reader().await.expect("connection");
        sqlx::query("INSERT INTO lash_blobs (hash, content, encoding) VALUES ($1, $2, 0)")
            .bind(opaque.to_hex())
            .bind(opaque_bytes.as_slice())
            .execute(&mut *connection)
            .await
            .expect("opaque child");
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut body = TokenWriter::new();
        body.child(opaque.child_ref());
        let root = staging
            .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
            .expect("root");
        install_staged(&mut connection, staging.nodes())
            .await
            .expect("install");
        sqlx::query("DELETE FROM lash_blob_edges WHERE parent_hash = $1")
            .bind(root.to_hex())
            .execute(&mut *connection)
            .await
            .expect("missing stored edge");
        let total =
            staging.get(&root).expect("root").bytes.len() as u64 + opaque_bytes.len() as u64;
        let mut limits = DecodeLimits::within_total(total);
        limits.nodes = 2;
        let nodes = walk_within(&mut connection, &[root], &limits, None)
            .await
            .expect("envelope child is hydrated");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[&opaque].bytes.as_ref(), opaque_bytes);
        limits.nodes = 1;
        assert!(matches!(
            walk_within(&mut connection, &[root], &limits, None).await,
            Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::Nodes,
                actual: 2,
                limit: 1
            })
        ));
    }
}
