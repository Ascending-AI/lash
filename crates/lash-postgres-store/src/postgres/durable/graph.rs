//! The checkpoint graph on PostgreSQL: owner graph installation, hydration
//! and the reachability collector (FIG-5887 §5, §8; lane G,
//! FIG-5890). The DDL is in `schema.sql`.
//!
//! # The gate
//!
//! Every guarded transaction takes the graph gate shared as its first lock,
//! in the `BEGIN` prelude ahead of the writer fence
//! ([`crate::guarded_tx`]); a collection takes it exclusive
//! ([`crate::guarded_tx::begin_collecting`]). So a commit and a hydration
//! each run whole beside other shared holders and never beside a collection: a root a reader selected cannot be swept until it ends.
//! Inside the gate a commit takes the owner's epoch fence, then the write
//! authority of each session whose history it locks, in session order, then
//! the external nodes' key-share
//! locks in digest order, then installs blobs in digest order, then edges
//! and root rows. The owner fence precedes the history locks, as in every
//! existing session writer (`lock_session_writes`: actor row, then
//! history); the design's order puts history first (FIG-5887 §8), which
//! would invert the order those writers already share.
//!
//! A hydration selects the owner's root once and reads every descendant in
//! the same transaction: READ COMMITTED is enough because nodes are
//! immutable and the gate keeps the collector out, not because its snapshot
//! is stable.
//!
//! # Roots
//!
//! The collector enumerates the root catalog (`lash_graph_root_catalog`)
//! one source at a time, records every source exhausted, and marks from
//! those roots alone: a missing root or child, a stored edge set that is not
//! a node's child table, an incomplete source or an exhausted work budget
//! rolls the whole collection back with nothing swept.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use futures_util::TryStreamExt;

use lash_core_store::checkpoint_graph::node::{immutable_catalog_heads, immutable_closure};
use lash_core_store::checkpoint_graph::{
    BlobEncoding, DecodeDimension, DecodeLimits, GraphRefusal, InstalledGraphMeasurement, NodeId,
    NodeSource, RootSource, StoredNode, check_envelope,
};
use lash_durable::domain::DomainRefusal;
use lash_durable::domain::graph::{
    EffectAdmissionWrite, HydratedGraph, HydratedNode, ReleasePinWrite, RootOwner,
};
use lash_durable::domain::graph_commit::{
    CheckedGraph, CollectionLimits, EDGE_ROW_LOGICAL_BYTES, GraphCollection, blob_row_logical_bytes,
};
use lash_durable::{CommitLabel, DurableError, StoreFailure, StoreFailureKind};
use lash_store_sql::Dialect;
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::durable::run_records::RunRecordStatements;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use lash_store_sql::durable::turns::TurnStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, PostgresDurableStore, Tx, integer, sqlx_failure};
use crate::guarded_tx::begin_collecting;

lash_store_sql::statements! {
    /// Graph statements only PostgreSQL issues: the artifact bridge's
    /// column, the row locks a shared-gate writer takes, and the sweep's
    /// array binding of the mark set.
    pub(crate) struct GraphPostgresStatements @ "durable_graph_postgres" {
        /// Artifact `?2` of namespace `?1`'s graph node: PostgreSQL's bridge
        /// is the artifact row's `graph_ref`, set only for storage kind 1.
        artifact_root = "SELECT graph_ref FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2 AND storage_kind = 1";

        /// The encoding of every stored blob among the nodes `?1` a commit
        /// names and the nodes it supplies (`?2`, raw digests: never one it
        /// names), key-share locked in digest order against a targeted
        /// reclaim, with the bytes of each supplied one: what the commit
        /// compares instead of supplying it again.
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

        /// The bytes and encoding of every stored blob in `?1`.
        stored_nodes = "SELECT hash, content, encoding FROM blobs WHERE hash = ANY(?1::text[])";

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

        /// Every hash roots `?1` reach along stored edges, the roots
        /// included: an owner-scoped delete's reclaim candidates, in one
        /// statement.
        reach_from = "WITH RECURSIVE reach AS (
                 SELECT UNNEST(?1::text[]) AS hash
                 UNION
                 SELECT edge.child_hash FROM blob_edges AS edge
                 JOIN reach ON edge.parent_hash = reach.hash
             )
             SELECT hash FROM reach";

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

        /// How many edges lead from a parent in `?1` to a child not in it:
        /// none after a complete closure.
        dangling_edges = "SELECT count(*) FROM blob_edges
             WHERE parent_hash = ANY(?1::text[]) AND NOT (child_hash = ANY(?1::text[]))";

        /// Sweep every edge whose parent is not marked in `?1`.
        sweep_edges = "DELETE FROM blob_edges WHERE NOT (parent_hash = ANY(?1::text[]))";

        /// Sweep every blob not marked in `?1`.
        sweep_blobs = "DELETE FROM blobs WHERE NOT (hash = ANY(?1::text[]))";
    }
}

struct Sql {
    graph: GraphStatements,
    facts: RunRecordStatements,
    snapshots: SnapshotStatements,
    turns: TurnStatements,
    postgres: GraphPostgresStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    Sql {
        graph: GraphStatements::render(dialect),
        facts: RunRecordStatements::render(dialect),
        snapshots: SnapshotStatements::render(dialect),
        turns: TurnStatements::render(dialect),
        postgres: GraphPostgresStatements::render(dialect),
    }
});

/// A hydration or a graph row read: a guarded transaction that writes
/// nothing, for the gate it holds.
pub(super) const GRAPH_READ: CommitLabel = CommitLabel::new("graph.read");

/// A graph answer: a value or a typed refusal that rolls the transaction
/// back.
type Answer<T> = Result<T, GraphRefusal>;

/// What a graph transaction's body leaves: commit it, or roll it back and
/// still answer (a receipt's replay, a read).
pub(super) enum Settled<T> {
    Commit(T),
    Rollback(T),
}

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

fn db(error: sqlx::Error) -> GraphRefusal {
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

fn node_of(stored: &str) -> Answer<NodeId> {
    NodeId::from_hex(stored).ok_or_else(|| corrupt("node reference", stored))
}

fn encoding_of(stored: i16) -> Answer<BlobEncoding> {
    match stored {
        0 => Ok(BlobEncoding::Opaque),
        1 => Ok(BlobEncoding::Graph),
        other => Err(corrupt("blob encoding", &other.to_string())),
    }
}

/// One stored blob: its bytes and encoding.
async fn stored_blob(tx: &mut PgConnection, hash: &str) -> Answer<Option<(Vec<u8>, BlobEncoding)>> {
    let row: Option<(Vec<u8>, i16)> = crate::observed_sql::query_as(SQL.graph.blob.sql())
        .bind(hash)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    row.map(|(bytes, encoding)| Ok((bytes, encoding_of(encoding)?)))
        .transpose()
}

async fn stored_edges(tx: &mut PgConnection, hash: &str) -> Answer<Vec<String>> {
    crate::observed_sql::query_scalar(SQL.graph.edges_of.sql())
        .bind(hash)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)
}

/// One nullable text column of the row `sql` selects, or none.
async fn one_text(
    tx: &mut PgConnection,
    query: crate::observed_sql::Query<'_>,
) -> Answer<Option<String>> {
    let row = query
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    Ok(match row {
        None => None,
        Some(row) => row.try_get::<Option<String>, _>(0).map_err(db)?,
    })
}

/// `owner`'s current root, or none when its row is gone or holds none.
async fn root_of(tx: &mut PgConnection, owner: &RootOwner) -> Answer<Option<NodeId>> {
    let stored = match owner {
        RootOwner::Exec(exec) => {
            let row = crate::observed_sql::query(SQL.snapshots.root.sql())
                .bind(exec.stored())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(db)?;
            match row {
                None => None,
                Some(row) => row.try_get::<Option<String>, _>(1).map_err(db)?,
            }
        }
        RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.facts.root.sql())
                    .bind(owner.stored())
                    .bind(number::<i64>(run.0)?)
                    .bind(number::<i64>(ordinal.0)?),
            )
            .await?
        }
        RootOwner::EffectAdmission { exec, effect } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.graph.admission_root.sql())
                    .bind(exec.stored())
                    .bind(effect.0.to_vec()),
            )
            .await?
        }
        RootOwner::TurnPhase { session, run } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.turns.phase_root.sql())
                    .bind(session.as_str())
                    .bind(run.as_str()),
            )
            .await?
        }
        RootOwner::WorkingExecution { session, run } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.turns.working_root.sql())
                    .bind(session.as_str())
                    .bind(run.as_str()),
            )
            .await?
        }
        RootOwner::TurnNamespace {
            session,
            run,
            plugin,
        } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.turns.namespace_root.sql())
                    .bind(session.as_str())
                    .bind(run.as_str())
                    .bind(plugin.as_str()),
            )
            .await?
        }
        RootOwner::SessionRevision { session, revision } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.graph.revision_root.sql())
                    .bind(session.as_str())
                    .bind(number::<i64>(*revision)?),
            )
            .await?
        }
        RootOwner::SessionAdmissionBase { session } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.graph.admission_base_root.sql())
                    .bind(session.as_str()),
            )
            .await?
        }
        RootOwner::ArtifactBridge {
            namespace,
            artifact,
        } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.postgres.artifact_root.sql())
                    .bind(namespace.as_str())
                    .bind(artifact.as_str()),
            )
            .await?
        }
        RootOwner::ReleasePin { release } => {
            one_text(
                tx,
                crate::observed_sql::query(SQL.graph.release_root.sql()).bind(release.as_str()),
            )
            .await?
        }
    };
    stored.as_deref().map(node_of).transpose()
}

/// Every node reachable from `root`, checked, within `limits`: a graph
/// node's children are read from its own child table, never trusted from
/// stored edges, and an opaque blob is a leaf.
pub(super) async fn walk(
    tx: &mut PgConnection,
    root: NodeId,
    limits: &DecodeLimits,
) -> Answer<BTreeMap<NodeId, HydratedNode>> {
    walk_within(tx, &[root], limits, None).await
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
    mut skipped: Option<&mut BTreeSet<NodeId>>,
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
    let member = |id: &NodeId| {
        if roots.contains(id) {
            None
        } else {
            immutable_closure(*id)
        }
    };
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
        if !unread.is_empty() {
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

pub(super) async fn hydrate_in(
    tx: &mut PgConnection,
    owner: RootOwner,
    expected: Option<NodeId>,
    limits: &DecodeLimits,
) -> Answer<HydratedGraph> {
    // The owner's root is selected once; every descendant is read after it
    // under the same gate.
    let Some(root) = root_of(tx, &owner).await? else {
        return Err(GraphRefusal::RootRetired);
    };
    if expected.is_some_and(|expected| expected != root) {
        return Err(GraphRefusal::RevisionConflict);
    }
    let nodes = walk(tx, root, limits).await?;
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

/// Applies an admission write. A second admission of one effect, or the
/// removal of a lookup that is not the expected one, refuses the commit.
pub(super) async fn apply_admission(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &EffectAdmissionWrite,
) -> Result<(), DurableError> {
    let applied = match write {
        EffectAdmissionWrite::Put {
            exec,
            effect,
            member,
            admission,
        } => {
            matched(
                tx,
                crate::observed_sql::query(SQL.graph.admission_put.sql())
                    .bind(exec.stored())
                    .bind(effect.0.to_vec())
                    .bind(member.as_ref().map(|member| member.owner.stored()))
                    .bind(
                        member
                            .as_ref()
                            .map(|member| integer::<i64>(member.run.0))
                            .transpose()?,
                    )
                    .bind(
                        member
                            .as_ref()
                            .map(|member| integer::<i64>(member.member))
                            .transpose()?,
                    )
                    .bind(admission.to_hex())
                    .bind(commit.epoch.0),
            )
            .await
        }
        EffectAdmissionWrite::Remove {
            exec,
            effect,
            expected,
        } => {
            matched(
                tx,
                crate::observed_sql::query(SQL.graph.admission_remove.sql())
                    .bind(exec.stored())
                    .bind(effect.0.to_vec())
                    .bind(expected.to_hex()),
            )
            .await
        }
    }
    .map_err(graph_refused)?;
    if applied {
        Ok(())
    } else {
        Err(graph_refused(GraphRefusal::RevisionConflict))
    }
}

/// Applies a release pin write.
pub(super) async fn apply_release_pin(
    tx: &mut PgConnection,
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
                Ok(())
            } else {
                Err(graph_refused(GraphRefusal::RevisionConflict))
            }
        }
    }
}

/// Installs a checked graph in the caller's guarded transaction. Every node
/// it names without supplying is locked in digest order against a targeted
/// reclaim and must exist (a published root as a graph node); the same
/// statement locks every supplied node already stored and reads its bytes
/// back for comparison, so only the others are bound to the insert and a
/// node the store holds never crosses the wire twice. A graph that names no
/// node offers every supplied node to the insert without that read, and
/// compares those already stored after it. One statement writes the fresh
/// nodes' edges. Supplied digests are bound as their 32 raw bytes.
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
    let digests: Vec<&[u8]> = absent.iter().map(|(node, _)| &node.id.0[..]).collect();
    let contents: Vec<&[u8]> = absent.iter().map(|(node, _)| &node.bytes[..]).collect();
    let inserted: BTreeSet<String> =
        crate::observed_sql::query_scalar(SQL.postgres.insert_nodes.sql())
            .bind(&digests)
            .bind(&contents)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?
            .into_iter()
            .collect();
    // A node stored before the insert that the lock did not read is
    // compared now.
    let held: Vec<&str> = absent
        .iter()
        .map(|(_, hash)| hash.as_str())
        .filter(|hash| !inserted.contains(*hash))
        .collect();
    if !held.is_empty() {
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
        for (node, hash) in &absent {
            if inserted.contains(*hash) {
                continue;
            }
            let Some((bytes, encoding)) = stored.get(*hash) else {
                return Err(GraphRefusal::MissingNode(node.id));
            };
            compare(node.id, &node.bytes, encoding_of(*encoding)?, Some(bytes))?;
        }
    }
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

/// A supplied node against the blob stored under its digest: a graph node
/// of exactly its bytes, or a conflict.
fn compare(
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

/// Every blob reachable from `roots` along stored edges, the roots included:
/// what an owner-scoped delete may reclaim once its roots are gone. One
/// recursive statement, whatever the graph's size.
pub(crate) async fn reachable_from(
    tx: &mut PgConnection,
    roots: &BTreeSet<String>,
) -> Answer<BTreeSet<String>> {
    if roots.is_empty() {
        return Ok(BTreeSet::new());
    }
    let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    let reached: Vec<String> = crate::observed_sql::query_scalar(SQL.postgres.reach_from.sql())
        .bind(&roots)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    Ok(reached.into_iter().collect())
}

/// Marks every blob reachable from the complete root catalog and sweeps the
/// rest, in the caller's collecting transaction (the graph gate held
/// exclusive).
pub(crate) async fn collect_in(
    tx: &mut PgConnection,
    limits: CollectionLimits,
) -> Answer<GraphCollection> {
    let mut report = GraphCollection::default();
    let mut stack: Vec<String> = Vec::new();
    for source in RootSource::ALL {
        let rows: Vec<String> = crate::observed_sql::query_scalar(SQL.graph.catalog_source.sql())
            .bind(source.catalog_name())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(|_| GraphRefusal::RootSourceIncomplete(source))?;
        report.roots.insert(source, rows.len() as u64);
        stack.extend(rows);
    }
    let mut marked = BTreeSet::new();
    let mut spent = 0_u64;
    let envelope_limits = DecodeLimits::within_total(u64::MAX);
    while let Some(hash) = stack.pop() {
        if marked.contains(&hash) {
            continue;
        }
        if marked.len() as u64 >= limits.nodes {
            return Err(GraphRefusal::CollectionIncomplete);
        }
        let Some((bytes, encoding)) = stored_blob(tx, &hash).await? else {
            return Err(match NodeId::from_hex(&hash) {
                Some(id) => GraphRefusal::MissingNode(id),
                None => corrupt("blob reference", &hash),
            });
        };
        spent = spent.saturating_add(bytes.len() as u64);
        if spent > limits.bytes {
            return Err(GraphRefusal::CollectionIncomplete);
        }
        let edges = stored_edges(tx, &hash).await?;
        match encoding {
            BlobEncoding::Graph => {
                let id = NodeId::from_hex(&hash)
                    .ok_or_else(|| corrupt("graph node reference", &hash))?;
                let envelope = check_envelope(&bytes, id, &envelope_limits)?;
                let children: Vec<String> = envelope.children.iter().map(|c| c.to_hex()).collect();
                if children != edges {
                    return Err(GraphRefusal::EdgeProjectionMismatch(id));
                }
                stack.extend(children);
            }
            BlobEncoding::Opaque => {
                if !edges.is_empty() {
                    return Err(match NodeId::from_hex(&hash) {
                        Some(id) => GraphRefusal::EdgeProjectionMismatch(id),
                        None => corrupt("opaque blob edges", &hash),
                    });
                }
            }
        }
        marked.insert(hash);
    }
    report.marked = marked.len() as u64;
    let marks: Vec<String> = marked.into_iter().collect();
    let dangling: i64 = crate::observed_sql::query_scalar(SQL.postgres.dangling_edges.sql())
        .bind(&marks)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    if dangling != 0 {
        return Err(GraphRefusal::CollectionIncomplete);
    }
    report.swept_edges = sweep(tx, SQL.postgres.sweep_edges.sql(), &marks).await?;
    report.swept_blobs = sweep(tx, SQL.postgres.sweep_blobs.sql(), &marks).await?;
    Ok(report)
}

/// Runs sweep `sql` over the mark set `marks`: how many rows it deleted.
async fn sweep(tx: &mut PgConnection, sql: &str, marks: &[String]) -> Answer<u64> {
    Ok(crate::observed_sql::query(sql)
        .bind(marks)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .rows_affected())
}

impl PostgresDurableStore {
    /// End a graph transaction as its body settled it: a refusal rolls it
    /// back.
    pub(super) async fn settle_graph<T>(tx: Tx, outcome: Answer<Settled<T>>) -> Answer<T> {
        match outcome {
            Ok(Settled::Commit(value)) => {
                tx.commit().await.map_err(db)?;
                Ok(value)
            }
            Ok(Settled::Rollback(value)) => {
                tx.rollback().await.map_err(db)?;
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

    /// `owner`'s root graph, read under the graph gate in one guarded
    /// transaction that writes nothing.
    pub(super) async fn hydrate_owner(
        &self,
        owner: RootOwner,
        expected: Option<NodeId>,
        limits: DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        let label = GRAPH_READ;
        self.within(label.capacity(), async {
            let (mut tx, _) = self.open(label).await?;
            let outcome = hydrate_in(&mut tx, owner, expected, &limits)
                .await
                .map(Settled::Rollback);
            Ok(Self::settle_graph(tx, outcome)
                .await
                .map_err(DurableError::Graph))
        })
        .await?
    }

    /// Every run fact of `owner` and every node they reach, read under the
    /// graph gate in one guarded transaction that writes nothing.
    pub(super) async fn read_run_facts(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        let label = GRAPH_READ;
        self.within(label.capacity(), async {
            let (mut tx, _) = self.open(label).await?;
            let rows = match super::run_records::read(&mut tx, owner, run).await {
                Ok(rows) => rows,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error);
                }
            };
            let outcome = async {
                let roots: Vec<_> = rows.iter().map(|row| row.record).collect();
                let nodes = walk_within(
                    &mut tx,
                    &roots,
                    &lash_durable::domain::graph::OWNER_READ_LIMITS,
                    None,
                )
                .await?;
                Ok(Settled::Rollback(nodes))
            }
            .await;
            let nodes = Self::settle_graph(tx, outcome)
                .await
                .map_err(DurableError::Graph)?;
            Ok(lash_durable::domain::RunFacts { rows, nodes })
        })
        .await
    }

    /// Collects every blob no root reaches, within `limits`. The collection
    /// takes the graph gate exclusive; nothing is swept unless every source
    /// enumerated completely and every reached node checked.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::RootSourceIncomplete`], [`GraphRefusal::MissingNode`],
    /// [`GraphRefusal::EdgeProjectionMismatch`], an envelope refusal,
    /// [`GraphRefusal::CollectionIncomplete`], or a store failure.
    pub async fn collect_graph(
        &self,
        limits: CollectionLimits,
    ) -> Result<GraphCollection, GraphRefusal> {
        let capacity = GRAPH_READ.capacity();
        self.within(capacity, async {
            let route = self.route(capacity);
            let mut tx = begin_collecting(route.pool, &self.fence)
                .await
                .map_err(super::store_failure)?;
            let outcome = collect_in(&mut tx, limits).await.map(Settled::Commit);
            Ok(Self::settle_graph(tx, outcome).await)
        })
        .await
        .map_err(from_durable)?
    }
}

/// The owner row lock a law probes while a commit waits on the gate: the
/// gate is the commit's first lock, so the row is free.
#[cfg(test)]
pub(crate) const OWNER_ROW_NOWAIT: &str =
    "SELECT 1 FROM lash_actors WHERE actor_key = $1 FOR UPDATE NOWAIT";

#[cfg(test)]
#[path = "../graph_store_tests.rs"]
mod graph_store_tests;

/// Drop every admission of the execution stored as `key`.
pub(super) async fn clear_admissions(tx: &mut PgConnection, key: &str) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.graph.admissions_clear.sql())
        .bind(key)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map(drop)
}

/// Drop every admission whose execution key starts with `key`.
pub(super) async fn clear_admissions_under(
    tx: &mut PgConnection,
    key: &str,
) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.graph.admissions_clear_under.sql())
        .bind(key)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map(drop)
}

/// The Admission node of every admission whose execution key starts with
/// `prefix`.
pub(super) async fn admission_roots_under(
    tx: &mut PgConnection,
    prefix: &str,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query_scalar(SQL.graph.admission_roots_under.sql())
        .bind(prefix)
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
        for (run, ordinal, root) in roots {
            sqlx::query(SQL.facts.append.sql())
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
        let nodes = walk(&mut connection, root, &limits)
            .await
            .expect("envelope child is hydrated");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[&opaque].bytes.as_ref(), opaque_bytes);
        limits.nodes = 1;
        assert!(matches!(
            walk(&mut connection, root, &limits).await,
            Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::Nodes,
                actual: 2,
                limit: 1
            })
        ));
    }
}
