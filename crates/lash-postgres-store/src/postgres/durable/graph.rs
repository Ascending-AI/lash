//! The checkpoint graph on PostgreSQL: the [`GraphStore`] commit, hydration
//! and read pins, and the reachability collector (FIG-5887 §5, §8; lane G,
//! FIG-5890). The DDL is in `schema.sql`.
//!
//! # The gate
//!
//! Every guarded transaction takes the graph gate shared as its first lock,
//! in the `BEGIN` prelude ahead of the writer fence
//! ([`crate::guarded_tx`]); a collection takes it exclusive
//! ([`crate::guarded_tx::begin_collecting`]). So a commit, a hydration and a
//! pin's fetch each run whole beside other shared holders and never beside
//! a collection: a root a reader selected cannot be swept until it ends.
//! Inside the gate a commit takes the owner's epoch fence, then the write
//! authority of each session whose history it locks, in session order, then
//! its read pins' rows in pin order, then the external nodes' key-share
//! locks in digest order, then installs blobs in digest order, then edges
//! and root rows. The owner fence precedes the history locks, as in every
//! existing session writer (`lock_session_writes`: actor row, then
//! history); the design's order puts history first (FIG-5887 §8), which
//! would invert the order those writers already share.
//!
//! A hydration selects the owner's root once and reads every descendant in
//! the same transaction: READ COMMITTED is enough because nodes are
//! immutable and the gate keeps the collector out, not because its snapshot
//! is stable. A pin's fetch locks its pin row `FOR UPDATE` before checking
//! it, so its renewal, release and expiry serialize with the fetch.
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

use lash_core_store::checkpoint_graph::{
    BlobEncoding, DecodeDimension, DecodeLimits, GraphRefusal, InstalledGraphMeasurement, NodeId,
    NodeSource, RootSource, StoredNode, check_envelope,
};
use lash_durable::domain::DomainRefusal;
use lash_durable::domain::graph::{
    EffectAdmissionWrite, FencedCommit, GraphCommitBatch, GraphStore, HydratedGraph, HydratedNode,
    PinnedBatch, PublishedGraph, ReadLease, ReadPin, ReadPinId, ReadPinWrite, ReleasePinWrite,
    RootChange, RootOwner,
};
use lash_durable::domain::graph_commit::{
    CheckedBatch, CheckedGraph, CollectionLimits, EDGE_ROW_LOGICAL_BYTES, GraphCollection,
    blob_row_logical_bytes, check_batch, decode_published, encode_published,
    graph_write_logical_bytes,
};
use lash_durable::{CommitLabel, DurableError, DurableInstant, StoreFailure, StoreFailureKind};
use lash_store_sql::Dialect;
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::durable::run_records::RunRecordStatements;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use lash_store_sql::durable::turns::TurnStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, PostgresDurableStore, Tx, apply_domain, integer, sqlx_failure};
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

        /// Read pin `?1`'s row, locked for the rest of the fetch.
        pin_row = "SELECT lease_owner, generation, root_ref, expires_at_ms
             FROM blob_read_pins WHERE pin_id = ?1 FOR UPDATE";

        /// Lock read pins `?1` in pin order for the rest of the commit.
        lock_pins = "SELECT pin_id FROM blob_read_pins WHERE pin_id = ANY(?1::text[])
             ORDER BY pin_id FOR UPDATE";

        /// The encodings of every stored blob in `?1`, key-share locked in
        /// digest order against a targeted reclaim.
        lock_nodes = "SELECT hash, encoding FROM blobs WHERE hash = ANY(?1::text[])
             ORDER BY hash FOR KEY SHARE";

        /// Installs every staged graph node `?1` (digests) with its bytes
        /// `?2`, in digest order, and returns the digests it inserted: a
        /// node already stored is left as it is.
        insert_nodes = "INSERT INTO blobs (hash, content, encoding)
             SELECT staged.hash, staged.content, 1
             FROM UNNEST(?1::text[], ?2::bytea[]) AS staged(hash, content)
             ORDER BY staged.hash
             ON CONFLICT (hash) DO NOTHING
             RETURNING hash";

        /// The bytes and encoding of every stored blob in `?1`.
        stored_nodes = "SELECT hash, content, encoding FROM blobs WHERE hash = ANY(?1::text[])";

        /// Every edge of freshly installed parents `?1` to children `?2`.
        insert_edges = "INSERT INTO blob_edges (parent_hash, child_hash)
             SELECT parent, child FROM UNNEST(?1::text[], ?2::text[]) AS staged(parent, child)";

        /// Every blob root `?1` reaches along stored edges, the root
        /// included, with its encoding and bytes: one read of a whole
        /// graph, whose child tables a walk still checks.
        walk_reach = "WITH RECURSIVE reach AS (
                 SELECT CAST(?1 AS TEXT) AS hash
                 UNION
                 SELECT edge.child_hash FROM blob_edges AS edge
                 JOIN reach ON edge.parent_hash = reach.hash
             )
             SELECT blobs.hash, blobs.encoding, blobs.content
             FROM blobs JOIN reach ON blobs.hash = reach.hash";

        /// [`Self::walk_reach`] without any opaque blob's bytes but the
        /// root's: what a reader of the graph alone reads.
        walk_reach_graph = "WITH RECURSIVE reach AS (
                 SELECT CAST(?1 AS TEXT) AS hash
                 UNION
                 SELECT edge.child_hash FROM blob_edges AS edge
                 JOIN reach ON edge.parent_hash = reach.hash
             )
             SELECT blobs.hash, blobs.encoding,
                    CASE WHEN blobs.encoding = 1 OR blobs.hash = ?1 THEN blobs.content END
             FROM blobs JOIN reach ON blobs.hash = reach.hash";

        /// One level of a walk: the encoding of every blob in `?1`, with the
        /// bytes of each graph node and of root `?2`, never an opaque
        /// child's.
        walk_level = "SELECT hash, encoding,
                    CASE WHEN encoding = 1 OR hash = ?2 THEN content END
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

/// A read pin's acquisition, fetch, renewal or retirement.
const GRAPH_READ_PIN: CommitLabel = CommitLabel::new("graph.read_pin");

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

/// A lease's time to live as a span of the store's clock.
fn ttl(lease: &ReadLease) -> i64 {
    i64::try_from(lease.ttl_ms).unwrap_or(i64::MAX)
}

/// One stored blob: its bytes and encoding.
async fn stored_blob(tx: &mut PgConnection, hash: &str) -> Answer<Option<(Vec<u8>, BlobEncoding)>> {
    let row: Option<(Vec<u8>, i16)> = sqlx::query_as(SQL.graph.blob.sql())
        .bind(hash)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    row.map(|(bytes, encoding)| Ok((bytes, encoding_of(encoding)?)))
        .transpose()
}

async fn stored_edges(tx: &mut PgConnection, hash: &str) -> Answer<Vec<String>> {
    sqlx::query_scalar(SQL.graph.edges_of.sql())
        .bind(hash)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)
}

/// One nullable text column of the row `sql` selects, or none.
async fn one_text(
    tx: &mut PgConnection,
    query: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
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
            let row = sqlx::query(SQL.snapshots.root.sql())
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
                sqlx::query(SQL.facts.root.sql())
                    .bind(owner.stored())
                    .bind(number::<i64>(run.0)?)
                    .bind(number::<i64>(ordinal.0)?),
            )
            .await?
        }
        RootOwner::EffectAdmission { exec, effect } => {
            one_text(
                tx,
                sqlx::query(SQL.graph.admission_root.sql())
                    .bind(exec.stored())
                    .bind(effect.0.to_vec()),
            )
            .await?
        }
        RootOwner::TurnPhase { session, run } => {
            one_text(
                tx,
                sqlx::query(SQL.turns.phase_root.sql())
                    .bind(session.as_str())
                    .bind(run.as_str()),
            )
            .await?
        }
        RootOwner::WorkingExecution { session, run } => {
            one_text(
                tx,
                sqlx::query(SQL.turns.working_root.sql())
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
                sqlx::query(SQL.turns.namespace_root.sql())
                    .bind(session.as_str())
                    .bind(run.as_str())
                    .bind(plugin.as_str()),
            )
            .await?
        }
        RootOwner::SessionRevision { session, revision } => {
            one_text(
                tx,
                sqlx::query(SQL.graph.revision_root.sql())
                    .bind(session.as_str())
                    .bind(number::<i64>(*revision)?),
            )
            .await?
        }
        RootOwner::SessionAdmissionBase { session } => {
            one_text(
                tx,
                sqlx::query(SQL.graph.admission_base_root.sql()).bind(session.as_str()),
            )
            .await?
        }
        RootOwner::ReadPin(pin) => {
            one_text(
                tx,
                sqlx::query(SQL.graph.pin_root.sql()).bind(pin.0.as_str()),
            )
            .await?
        }
        RootOwner::ArtifactBridge {
            namespace,
            artifact,
        } => {
            one_text(
                tx,
                sqlx::query(SQL.postgres.artifact_root.sql())
                    .bind(namespace.as_str())
                    .bind(artifact.as_str()),
            )
            .await?
        }
        RootOwner::ReleasePin { release } => {
            one_text(
                tx,
                sqlx::query(SQL.graph.release_root.sql()).bind(release.as_str()),
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
    walk_within(tx, root, limits, None).await
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
    let nodes = walk_within(tx, root, limits, Some(&mut opaque)).await?;
    Ok(GraphClosure { nodes, opaque })
}

async fn walk_within(
    tx: &mut PgConnection,
    root: NodeId,
    limits: &DecodeLimits,
    mut skipped: Option<&mut BTreeSet<NodeId>>,
) -> Answer<BTreeMap<NodeId, HydratedNode>> {
    // The root's whole reach is read along its stored edges in one
    // statement; every node's children are still taken from its own child
    // table, and a child the edges did not reach is read a level at a time.
    let root_hex = root.to_hex();
    let reach = if skipped.is_some() {
        SQL.postgres.walk_reach_graph.sql()
    } else {
        SQL.postgres.walk_reach.sql()
    };
    let rows: Vec<(String, i16, Option<Vec<u8>>)> = sqlx::query_as(reach)
        .bind(&root_hex)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    let mut found = BTreeMap::new();
    for (hash, encoding, bytes) in rows {
        found.insert(hash, (encoding_of(encoding)?, bytes));
    }
    let mut nodes = BTreeMap::new();
    let mut level = BTreeSet::from([root]);
    let mut total = 0_u64;
    while !level.is_empty() {
        let unread: Vec<String> = level
            .iter()
            .map(NodeId::to_hex)
            .filter(|hash| !found.contains_key(hash))
            .collect();
        if !unread.is_empty() {
            let rows: Vec<(String, i16, Option<Vec<u8>>)> =
                sqlx::query_as(SQL.postgres.walk_level.sql())
                    .bind(&unread)
                    .bind(&root_hex)
                    .fetch_all(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(db)?;
            for (hash, encoding, bytes) in rows {
                found.insert(hash, (encoding_of(encoding)?, bytes));
            }
        }
        let mut next = BTreeSet::new();
        for id in level {
            let hash = id.to_hex();
            let Some((encoding, _)) = found.get(&hash) else {
                return Err(GraphRefusal::MissingNode(id));
            };
            // A reader of the graph alone records an opaque blob as opaque
            // and reads nothing of it; another parent may name it again.
            if *encoding == BlobEncoding::Opaque
                && id != root
                && let Some(skipped) = skipped.as_deref_mut()
            {
                skipped.insert(id);
                continue;
            }
            let Some((encoding, bytes)) = found.remove(&hash) else {
                return Err(GraphRefusal::MissingNode(id));
            };
            let bytes = match bytes {
                Some(bytes) => bytes,
                None => match stored_blob(tx, &id.to_hex()).await? {
                    Some((bytes, _)) => bytes,
                    None => return Err(GraphRefusal::MissingNode(id)),
                },
            };
            let count = nodes.len() as u64 + 1;
            total = total.saturating_add(bytes.len() as u64);
            if count > limits.nodes {
                return Err(GraphRefusal::DecodeLimit {
                    dimension: DecodeDimension::Nodes,
                    actual: count,
                    limit: limits.nodes,
                });
            }
            if total > limits.total_bytes {
                return Err(GraphRefusal::DecodeLimit {
                    dimension: DecodeDimension::TotalBytes,
                    actual: total,
                    limit: limits.total_bytes,
                });
            }
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
        next.retain(|id| !nodes.contains_key(id));
        level = next;
    }
    Ok(nodes)
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

/// Whether every node in `wanted` is reachable from `root` along stored
/// edges, which a commit installs exactly as each node's child table.
async fn reachable(tx: &mut PgConnection, root: &NodeId, wanted: &[NodeId]) -> Answer<bool> {
    let mut missing: BTreeSet<String> = wanted.iter().map(NodeId::to_hex).collect();
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_hex()];
    while let Some(hash) = stack.pop() {
        if missing.is_empty() {
            break;
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        missing.remove(&hash);
        stack.extend(stored_edges(tx, &hash).await?);
    }
    Ok(missing.is_empty())
}

/// Whether `query`, a conditional write that returns its row, matched one.
async fn matched(
    tx: &mut PgConnection,
    query: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Answer<bool> {
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
                sqlx::query(SQL.graph.admission_put.sql())
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
                sqlx::query(SQL.graph.admission_remove.sql())
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

/// Applies a read pin write.
pub(super) async fn apply_read_pin(
    tx: &mut PgConnection,
    write: &ReadPinWrite,
) -> Result<(), DurableError> {
    match write {
        ReadPinWrite::Acquire { pin } => insert_pin(tx, pin).await.map_err(graph_refused),
        ReadPinWrite::Retire { pin } => {
            if retire_pin(tx, pin).await.map_err(graph_refused)? {
                Ok(())
            } else {
                Err(graph_refused(GraphRefusal::ReadPinMismatch))
            }
        }
    }
}

/// Applies a release pin write.
pub(super) async fn apply_release_pin(
    tx: &mut PgConnection,
    write: &ReleasePinWrite,
) -> Result<(), DurableError> {
    match write {
        ReleasePinWrite::Install { release, root } => {
            let installed = sqlx::query(SQL.graph.release_install.sql())
                .bind(release.as_str())
                .bind(root.to_hex())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?
                .rows_affected();
            if installed == 0 {
                let pinned: String = sqlx::query_scalar(SQL.graph.release_root.sql())
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
                sqlx::query(SQL.graph.release_retire.sql())
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

/// Installs a checked graph in the caller's guarded transaction: every node
/// it names without supplying is locked in digest order against a targeted
/// reclaim and must exist (a published root as a graph node), then each
/// supplied blob in digest order, compared on conflict, then every new
/// node's edges once every blob exists.
pub(crate) async fn install(
    tx: &mut PgConnection,
    graph: &CheckedGraph,
) -> Answer<InstalledGraphMeasurement> {
    let named: BTreeSet<&NodeId> = graph
        .external
        .iter()
        .chain(graph.external_roots.iter())
        .collect();
    if !named.is_empty() {
        let rows: Vec<(String, i16)> = sqlx::query_as(SQL.postgres.lock_nodes.sql())
            .bind(named.iter().map(|node| node.to_hex()).collect::<Vec<_>>())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
        let mut present = BTreeMap::new();
        for (hash, encoding) in rows {
            present.insert(hash, encoding_of(encoding)?);
        }
        for node in &graph.external {
            if !present.contains_key(&node.to_hex()) {
                return Err(GraphRefusal::MissingNode(*node));
            }
        }
        for root in &graph.external_roots {
            match present.get(&root.to_hex()) {
                None => return Err(GraphRefusal::MissingNode(*root)),
                Some(BlobEncoding::Opaque) => return Err(GraphRefusal::EncodingConflict(*root)),
                Some(BlobEncoding::Graph) => {}
            }
        }
    }
    let mut installed = InstalledGraphMeasurement::default();
    if graph.nodes.is_empty() {
        return Ok(installed);
    }
    // One statement installs every node, one reads back those already
    // stored and one writes the fresh nodes' edges, whatever their number.
    let hashes: Vec<String> = graph.nodes.iter().map(|node| node.id.to_hex()).collect();
    let contents: Vec<&[u8]> = graph.nodes.iter().map(|node| &node.bytes[..]).collect();
    let inserted: BTreeSet<String> = sqlx::query_scalar(SQL.postgres.insert_nodes.sql())
        .bind(&hashes)
        .bind(&contents)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .into_iter()
        .collect();
    let held: Vec<String> = hashes
        .iter()
        .filter(|hash| !inserted.contains(*hash))
        .cloned()
        .collect();
    if !held.is_empty() {
        let rows: Vec<(String, Vec<u8>, i16)> = sqlx::query_as(SQL.postgres.stored_nodes.sql())
            .bind(&held)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
        let stored: BTreeMap<String, (Vec<u8>, i16)> = rows
            .into_iter()
            .map(|(hash, bytes, encoding)| (hash, (bytes, encoding)))
            .collect();
        for node in &graph.nodes {
            let hash = node.id.to_hex();
            if inserted.contains(&hash) {
                continue;
            }
            let Some((bytes, encoding)) = stored.get(&hash) else {
                return Err(GraphRefusal::MissingNode(node.id));
            };
            if encoding_of(*encoding)? != BlobEncoding::Graph {
                return Err(GraphRefusal::EncodingConflict(node.id));
            }
            if bytes[..] != node.bytes[..] {
                return Err(GraphRefusal::ContentConflict(node.id));
            }
        }
    }
    let mut parents = Vec::new();
    let mut children = Vec::new();
    for node in &graph.nodes {
        let parent = node.id.to_hex();
        if !inserted.contains(&parent) {
            continue;
        }
        installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
        installed.new_nodes += 1;
        installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        for child in &node.children {
            parents.push(parent.clone());
            children.push(child.to_hex());
            installed.new_edges += 1;
            installed.sql_logical_bytes += EDGE_ROW_LOGICAL_BYTES;
        }
    }
    if !parents.is_empty() {
        sqlx::query(SQL.postgres.insert_edges.sql())
            .bind(&parents)
            .bind(&children)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
    }
    Ok(installed)
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
/// what an owner-scoped delete may reclaim once its roots are gone.
pub(crate) async fn reachable_from(
    tx: &mut PgConnection,
    roots: &BTreeSet<String>,
) -> Answer<BTreeSet<String>> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = roots.iter().cloned().collect();
    while let Some(hash) = stack.pop() {
        if seen.insert(hash.clone()) {
            stack.extend(stored_edges(tx, &hash).await?);
        }
    }
    Ok(seen)
}

async fn insert_pin(tx: &mut PgConnection, pin: &ReadPin) -> Answer<()> {
    sqlx::query(SQL.graph.pin_insert.sql())
        .bind(pin.pin_id.0.as_str())
        .bind(pin.lease_owner.as_str())
        .bind(number::<i64>(pin.generation)?)
        .bind(pin.root.to_hex())
        .bind(pin.expires_at.0)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    Ok(())
}

async fn retire_pin(tx: &mut PgConnection, pin: &ReadPin) -> Answer<bool> {
    matched(
        tx,
        sqlx::query(SQL.graph.pin_retire.sql())
            .bind(pin.pin_id.0.as_str())
            .bind(pin.lease_owner.as_str())
            .bind(number::<i64>(pin.generation)?)
            .bind(pin.root.to_hex()),
    )
    .await
}

/// The whole commit, in its guarded transaction.
async fn commit_in(
    tx: &mut Tx,
    fence: &FencedCommit,
    batch: &GraphCommitBatch,
    checked: &CheckedBatch,
    now: DurableInstant,
    checkpoint_ref_chunk: usize,
) -> Answer<Settled<PublishedGraph>> {
    // The owner's epoch fence: the first row lock, under the gate and the
    // writer fence the `BEGIN` took.
    let actor = fence.actor.as_str();
    let fenced = sqlx::query(super::SQL.actor.fence.sql())
        .bind(actor)
        .bind(fence.epoch.0)
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(db)?;
    if fenced.is_none() {
        return Err(GraphRefusal::FenceLost);
    }
    // Then the fence's declared keys: each session's write authority in
    // session order (its actor row, then its history, as every session
    // writer takes them), then the read pins in pin order.
    for session in &fence.history_sessions {
        PostgresDurableStore::lock_session_writes(tx, session)
            .await
            .map_err(|error| from_durable(super::store_failure(error)))?;
    }
    if !fence.read_pins.is_empty() {
        sqlx::query(SQL.postgres.lock_pins.sql())
            .bind(
                fence
                    .read_pins
                    .iter()
                    .map(|pin| pin.0.clone())
                    .collect::<Vec<_>>(),
            )
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(db)?;
    }
    // A retry of an acknowledged commit is answered from its receipt; the
    // fence bump above rolls back with it.
    let receipt: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(SQL.graph.receipt.sql())
        .bind(actor)
        .bind(batch.commit_id.0.to_vec())
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(db)?;
    if let Some((digest, result)) = receipt {
        if digest != checked.encoded.digest.0 {
            return Err(GraphRefusal::CommitIdentityConflict);
        }
        return decode_published(&result)
            .map(Settled::Rollback)
            .map_err(|error| corrupt("commit receipt", &error.to_string()));
    }
    batch.budget.admit(&checked.encoded.measurement)?;
    for (owner, expected) in batch.expected.iter() {
        let found = match owner {
            RootOwner::Exec(exec) => {
                let rev: Option<i64> = sqlx::query_scalar(SQL.snapshots.root.sql())
                    .bind(exec.stored())
                    .fetch_optional(crate::observed_sql::executor(&mut ***tx))
                    .await
                    .map_err(db)?;
                rev.map(number::<u64>).transpose()?
            }
            // Only an execution's row carries a stored revision; every other
            // owner is compared by its expected root.
            _ => None,
        };
        if found != expected {
            return Err(GraphRefusal::RevisionConflict);
        }
    }
    let mut installed = install(tx, &checked.graph).await?;
    let committing = Committing {
        checkpoint_ref_chunk,
        actor: &fence.actor,
        epoch: fence.epoch,
        now,
        fleet: tx.fleet(),
    };
    for write in &batch.domain_writes {
        apply_domain(tx, &committing, write)
            .await
            .map_err(from_durable)?;
        installed.sql_logical_bytes += graph_write_logical_bytes(write);
    }
    let revisions = batch.expected.advanced()?;
    let published = PublishedGraph {
        commit_id: batch.commit_id,
        revisions,
        roots: batch
            .graph
            .publications
            .iter()
            .filter_map(|change| match change {
                RootChange::Put { owner, root, .. } => Some((owner.clone(), *root)),
                RootChange::Retire { .. } => None,
            })
            .collect(),
        installed,
    };
    // The receipt records the result, so its own row is not in the result's
    // SQL bytes.
    sqlx::query(SQL.graph.receipt_insert.sql())
        .bind(actor)
        .bind(batch.commit_id.0.to_vec())
        .bind(checked.encoded.digest.0.to_vec())
        .bind(encode_published(&published))
        .bind(fence.epoch.0)
        .execute(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(db)?;
    Ok(Settled::Commit(published))
}

/// Reads read pin `pin`'s row, locked, and checks it is exactly `pin` and
/// unexpired at `now`.
async fn check_pin(tx: &mut PgConnection, pin: &ReadPin, now: DurableInstant) -> Answer<()> {
    let row: Option<(String, i64, String, i64)> = sqlx::query_as(SQL.postgres.pin_row.sql())
        .bind(pin.pin_id.0.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    let Some((lease_owner, generation, root, expires_at)) = row else {
        return Err(GraphRefusal::ReadPinMismatch);
    };
    if lease_owner != pin.lease_owner
        || number::<u64>(generation)? != pin.generation
        || root != pin.root.to_hex()
    {
        return Err(GraphRefusal::ReadPinMismatch);
    }
    if expires_at < now.0 {
        return Err(GraphRefusal::ReadPinExpired);
    }
    Ok(())
}

async fn fetch_pinned_in(
    tx: &mut PgConnection,
    pin: ReadPin,
    nodes: &[NodeId],
    renew: Option<ReadLease>,
    now: DurableInstant,
) -> Answer<PinnedBatch> {
    check_pin(tx, &pin, now).await?;
    if !reachable(tx, &pin.root, nodes).await? {
        return Err(GraphRefusal::ReadPinMismatch);
    }
    let mut fetched = BTreeMap::new();
    let limits = DecodeLimits::within_total(u64::MAX);
    for id in nodes {
        let Some((bytes, encoding)) = stored_blob(tx, &id.to_hex()).await? else {
            return Err(GraphRefusal::MissingNode(*id));
        };
        if encoding == BlobEncoding::Graph {
            check_envelope(&bytes, *id, &limits)?;
        }
        fetched.insert(
            *id,
            HydratedNode {
                encoding,
                bytes: bytes.into(),
            },
        );
    }
    let pin = match renew {
        None => pin,
        Some(lease) => {
            if lease.lease_owner != pin.lease_owner {
                return Err(GraphRefusal::ReadPinMismatch);
            }
            let expires_at = now.after_millis(ttl(&lease));
            let generation: i64 = sqlx::query_scalar(SQL.graph.pin_renew.sql())
                .bind(pin.pin_id.0.as_str())
                .bind(expires_at.0)
                .fetch_one(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(db)?;
            ReadPin {
                generation: number::<u64>(generation)?,
                expires_at,
                ..pin
            }
        }
    };
    Ok(PinnedBatch {
        pin,
        nodes: fetched,
    })
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
        let rows: Vec<String> = sqlx::query_scalar(SQL.graph.catalog_source.sql())
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
    let dangling: i64 = sqlx::query_scalar(SQL.postgres.dangling_edges.sql())
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
    Ok(sqlx::query(sql)
        .bind(marks)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .rows_affected())
}

/// Retires every read pin expired at `now`, in the caller's guarded
/// transaction: answers how many.
pub(crate) async fn retire_expired_read_pins_in(
    tx: &mut PgConnection,
    now: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(SQL.graph.pins_retire_expired.sql())
        .bind(now)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?
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
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        let label = GRAPH_READ;
        self.within(label.capacity(), async {
            let (mut tx, _) = self.open(label).await?;
            let rows = match super::run_records::read(&mut tx, owner).await {
                Ok(rows) => rows,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error);
                }
            };
            let outcome = async {
                let mut nodes = BTreeMap::new();
                for row in &rows {
                    if !nodes.contains_key(&row.record) {
                        nodes.extend(
                            walk(
                                &mut tx,
                                row.record,
                                &lash_durable::domain::graph::OWNER_READ_LIMITS,
                            )
                            .await?,
                        );
                    }
                }
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

    /// Releases `pin` before its expiry. Refused when it is not exactly
    /// the stored pin.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::ReadPinMismatch`], or a store failure.
    pub async fn release_read_pin(&self, pin: ReadPin) -> Result<(), GraphRefusal> {
        let label = GRAPH_READ_PIN;
        self.within(label.capacity(), async {
            let (mut tx, _) = self.open(label).await?;
            let outcome = match retire_pin(&mut tx, &pin).await {
                Ok(true) => Ok(Settled::Commit(())),
                Ok(false) => Err(GraphRefusal::ReadPinMismatch),
                Err(refusal) => Err(refusal),
            };
            Ok(Self::settle_graph(tx, outcome).await)
        })
        .await
        .map_err(from_durable)?
    }

    /// Retires every read pin expired now, in an ordinary guarded
    /// transaction of its own: expiry alone never removes a root. Answers
    /// how many it retired.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn retire_expired_read_pins(&self) -> Result<u64, GraphRefusal> {
        let label = GRAPH_READ_PIN;
        self.within(label.capacity(), async {
            let (mut tx, now) = self.open(label).await?;
            let outcome = retire_expired_read_pins_in(&mut tx, now.0)
                .await
                .map(Settled::Commit)
                .map_err(db);
            Ok(Self::settle_graph(tx, outcome).await)
        })
        .await
        .map_err(from_durable)?
    }

    /// Collects every blob no root reaches, within `limits`, after retiring
    /// expired read pins in a transaction of their own. The collection
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
        self.retire_expired_read_pins().await?;
        let capacity = GRAPH_READ_PIN.capacity();
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

#[async_trait::async_trait]
impl GraphStore for PostgresDurableStore {
    async fn commit_graph(
        &self,
        fence: FencedCommit,
        batch: GraphCommitBatch,
    ) -> Result<PublishedGraph, GraphRefusal> {
        let checked = check_batch(&batch)?;
        let label = fence.label;
        let chunk = self.pools.maintenance.checkpoint_ref_chunk as usize;
        self.within(label.capacity(), async {
            let (mut tx, now) = self.open(label).await?;
            let outcome = commit_in(&mut tx, &fence, &batch, &checked, now, chunk).await;
            Ok(Self::settle_graph(tx, outcome).await)
        })
        .await
        .map_err(from_durable)?
    }

    async fn acquire_read_pin(
        &self,
        owner: RootOwner,
        lease: ReadLease,
    ) -> Result<ReadPin, GraphRefusal> {
        let label = GRAPH_READ_PIN;
        self.within(label.capacity(), async {
            let (mut tx, now) = self.open(label).await?;
            let outcome = async {
                let Some(root) = root_of(&mut tx, &owner).await? else {
                    return Err(GraphRefusal::RootRetired);
                };
                let pin = ReadPin {
                    pin_id: ReadPinId(uuid::Uuid::new_v4().to_string()),
                    expires_at: now.after_millis(ttl(&lease)),
                    lease_owner: lease.lease_owner,
                    generation: 1,
                    root,
                };
                insert_pin(&mut tx, &pin).await?;
                Ok(Settled::Commit(pin))
            }
            .await;
            Ok(Self::settle_graph(tx, outcome).await)
        })
        .await
        .map_err(from_durable)?
    }

    async fn fetch_pinned(
        &self,
        pin: ReadPin,
        nodes: &[NodeId],
        renew: Option<ReadLease>,
    ) -> Result<PinnedBatch, GraphRefusal> {
        let label = GRAPH_READ_PIN;
        self.within(label.capacity(), async {
            let (mut tx, now) = self.open(label).await?;
            let outcome = fetch_pinned_in(&mut tx, pin, nodes, renew, now)
                .await
                .map(Settled::Commit);
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
