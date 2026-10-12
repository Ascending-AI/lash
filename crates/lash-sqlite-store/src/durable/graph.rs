//! The checkpoint graph on SQLite: the `graph` domain's DDL, the
//! owner graph installation, hydration, and the peel that reclaims a
//! retired root's closure (FIG-5887 §5, §7; FIG-5949).
//!
//! # One transaction, one lock
//!
//! Every graph write runs in [`SqliteConnection::write_flow`]: the
//! database's write lock (`BEGIN IMMEDIATE`) and the writer fence come
//! first, then the owner's epoch fence, then the commit in the order of
//! [`lash_durable::domain::graph`]'s contracts. SQLite has one writer, so a
//! peel never interleaves with an install.
//!
//! A hydration is one read transaction that selects the owner's root first,
//! fixing its snapshot, and reads every descendant on it, so a writer that
//! replaces the root and peels its closure afterwards cannot reach what it
//! reads.
//!
//! # Reclamation
//!
//! The root catalog (`graph_root_catalog`) is a view of the owning columns.
//! The graph is a DAG, so a node no root reaches is an orphan (no root row,
//! no edge to it) or reached only from such nodes: deleting orphans, whose
//! edges go with them, deletes exactly what nothing reaches. Every
//! transaction that retires a root peels it before it commits: the root
//! goes when nothing else names it, then each child its edges no longer
//! reach, level by level, stopping at a node something still names. The
//! orphan audit ([`audit_page`]) peels whatever a write outside the peel
//! left behind.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::node::immutable_closure;
use lash_core_store::checkpoint_graph::{
    BlobEncoding, DecodeDimension, DecodeLimits, GraphRefusal, InstalledGraphMeasurement, NodeId,
    NodeSource, StoredNode, check_envelope,
};
use lash_durable::domain::DomainRefusal;
use lash_durable::domain::graph::{
    EffectAdmissionWrite, HydratedGraph, HydratedNode, ReleasePinWrite, RootOwner,
};
use lash_durable::domain::graph_commit::{
    CheckedGraph, EDGE_ROW_LOGICAL_BYTES, blob_row_logical_bytes,
};
use lash_durable::{CommitLabel, DurableError, DurableInstant, StoreFailure, StoreFailureKind};
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::durable::run_records::RunRecordStatements;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use lash_store_sql::durable::turns::TurnStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, Retired, SqliteDurableStore, integer};
use crate::conn::{FencedTx, TxOutcome, cached_execute};

/// The graph tables and the root catalog, created after every table the
/// catalog names.
pub(crate) const TABLES: &str = "
-- One row per child of an installed graph node: exactly its envelope's child
-- table. A parent's edges go with it; a child cannot go while an edge names it.
CREATE TABLE IF NOT EXISTS blob_edges (
    parent_hash TEXT NOT NULL REFERENCES blobs(hash) ON DELETE CASCADE,
    child_hash TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY (parent_hash, child_hash)
);
CREATE INDEX IF NOT EXISTS blob_edges_child ON blob_edges (child_hash, parent_hash);

-- An execution's admitted effect, by its canonical binary identity, to its
-- Admission node, while a live kernel or a released member needs the lookup.
CREATE TABLE IF NOT EXISTS effect_admissions (
    exec_key TEXT NOT NULL,
    effect_id BLOB NOT NULL,
    owner_key TEXT,
    run_seq INTEGER,
    member_ordinal INTEGER,
    admission_ref TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (exec_key, effect_id),
    UNIQUE (owner_key, run_seq, member_ordinal),
    CONSTRAINT ck_effect_admissions_member CHECK (
        (owner_key IS NULL AND run_seq IS NULL AND member_ordinal IS NULL) OR
        (owner_key IS NOT NULL AND run_seq IS NOT NULL AND member_ordinal IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_effect_admissions_admission_ref
    ON effect_admissions (admission_ref);

-- A deliberately installed runnable release manifest.
CREATE TABLE IF NOT EXISTS graph_release_pins (
    release_key TEXT PRIMARY KEY,
    root_ref TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_graph_release_pins_root_ref ON graph_release_pins (root_ref);

-- Every root of every source: a view of the owning columns, never a cache.
CREATE VIEW IF NOT EXISTS graph_root_catalog AS
    SELECT 'exec' AS source, root_ref FROM exec_snapshots WHERE root_ref IS NOT NULL
    UNION ALL SELECT 'run_fact', record_ref FROM run_records WHERE record_ref IS NOT NULL
    UNION ALL SELECT 'admission', admission_ref FROM effect_admissions
    UNION ALL SELECT 'phase', checkpoint_ref FROM turn_phases WHERE checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'working', working_execution_ref FROM turn_phases
        WHERE working_execution_ref IS NOT NULL
    UNION ALL SELECT 'namespace', graph_root_ref FROM turn_namespaces
        WHERE graph_root_ref IS NOT NULL
    UNION ALL SELECT 'revision', checkpoint_ref FROM session_revisions
        WHERE checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'admission_base', admission_base_checkpoint_ref FROM session_meta
        WHERE admission_base_checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'artifact', blob_ref FROM artifact_refs
    UNION ALL SELECT 'release', root_ref FROM graph_release_pins
    UNION ALL SELECT 'terminal_window', terminal_window_checkpoint_ref FROM session_runs
        WHERE terminal_window_checkpoint_ref IS NOT NULL
    UNION ALL SELECT 'revision_config', config_ref FROM session_revisions
    UNION ALL SELECT 'terminal_config', terminal_window_config_ref FROM session_runs
        WHERE terminal_window_config_ref IS NOT NULL;
";

lash_store_sql::statements! {
    /// Graph statements only SQLite issues: the artifact bridge's pointer
    /// table and the
    /// `json_each` binding of a node list.
    pub(crate) struct GraphSqliteStatements @ "durable_graph_sqlite" {
        /// Artifact `?2` of namespace `?1`'s blob: SQLite's bridge is its
        /// pointer table.
        artifact_root = "SELECT blob_ref FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2";

        /// The encoding of every stored blob in JSON `?1`.
        stored_encodings = "SELECT hash, encoding FROM blobs
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// The bytes and encoding of every stored blob in JSON `?1`.
        stored_nodes = "SELECT hash, content, encoding FROM blobs
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// Every edge of JSON `?1`, an array of `[parent, child]` digests
        /// of freshly installed parents.
        insert_edges = "INSERT INTO blob_edges (parent_hash, child_hash)
             SELECT value ->> 0, value ->> 1 FROM json_each(?1)";

        /// Deletable parents in JSON `?1` and their children, before the
        /// delete takes their edges. Retained parents contribute no rows;
        /// a deletable leaf contributes its parent with a NULL child.
        peel_edges = "SELECT candidate.hash, edge.child_hash FROM blobs AS candidate
             LEFT JOIN blob_edges AS edge ON edge.parent_hash = candidate.hash
             WHERE candidate.hash IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM graph_root_catalog AS root WHERE root.root_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM blob_edges AS incoming WHERE incoming.child_hash = candidate.hash
               )";

        /// One level of a peel: delete every blob in JSON `?1` no root names
        /// and no edge reaches, and return each one deleted. Its edges go
        /// with it.
        peel_orphans = "DELETE FROM blobs AS candidate
             WHERE candidate.hash IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM graph_root_catalog AS root WHERE root.root_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM blob_edges AS edge WHERE edge.child_hash = candidate.hash
               )
             RETURNING hash";
    }
}

struct Sql {
    graph: GraphStatements,
    facts: RunRecordStatements,
    snapshots: SnapshotStatements,
    turns: TurnStatements,
    sqlite: GraphSqliteStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    Sql {
        graph: GraphStatements::render(dialect),
        facts: RunRecordStatements::render(dialect),
        snapshots: SnapshotStatements::render(dialect),
        turns: TurnStatements::render(dialect),
        sqlite: GraphSqliteStatements::render(dialect),
    }
});

/// One page of the orphan audit.
const GRAPH_AUDIT: CommitLabel = CommitLabel::new("graph.audit");

/// The orphans one audit page reads.
pub(crate) const AUDIT_PAGE: usize = 256;

/// A statement's answer inside a graph transaction: a value, or a typed
/// refusal that rolls the transaction back.
pub(crate) type GraphAnswer<T> = rusqlite::Result<Result<T, GraphRefusal>>;

fn corrupt(what: &str, value: &str) -> GraphRefusal {
    GraphRefusal::Backend(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("stored {what} `{value}` does not decode"),
    })
}

/// A durable port refusal as a graph refusal: a lost fence stays typed, a
/// second outcome is a duplicate, another domain write's refusal is a
/// conflict, and a store failure keeps its kind.
fn from_durable(error: DurableError) -> GraphRefusal {
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

fn node_of(stored: &str) -> Result<NodeId, GraphRefusal> {
    NodeId::from_hex(stored).ok_or_else(|| corrupt("node reference", stored))
}

fn encoding_of(stored: i64) -> Result<BlobEncoding, GraphRefusal> {
    match stored {
        0 => Ok(BlobEncoding::Opaque),
        1 => Ok(BlobEncoding::Graph),
        other => Err(corrupt("blob encoding", &other.to_string())),
    }
}

/// One stored blob: its bytes and encoding.
fn stored_blob(tx: &Connection, hash: &str) -> GraphAnswer<Option<(Vec<u8>, BlobEncoding)>> {
    let row = tx
        .prepare_cached(SQL.graph.blob.sql())?
        .query_row([hash], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?;
    Ok(match row {
        None => Ok(None),
        Some((bytes, encoding)) => encoding_of(encoding).map(|encoding| Some((bytes, encoding))),
    })
}

fn stored_encoding(tx: &Connection, node: &NodeId) -> GraphAnswer<Option<BlobEncoding>> {
    let row = tx
        .prepare_cached(SQL.graph.blob_encoding.sql())?
        .query_row([node.to_hex()], |row| row.get::<_, i64>(0))
        .optional()?;
    Ok(row.map(encoding_of).transpose())
}

/// `owner`'s current root, or none when its row is gone or holds none.
fn root_of(tx: &Connection, owner: &RootOwner) -> GraphAnswer<Option<NodeId>> {
    fn one(
        tx: &Connection,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> rusqlite::Result<Option<String>> {
        Ok(tx
            .prepare_cached(sql)?
            .query_row(params, |row| row.get::<_, Option<String>>(0))
            .optional()?
            .flatten())
    }
    let stored = match owner {
        RootOwner::Exec(exec) => tx
            .prepare_cached(SQL.snapshots.root.sql())?
            .query_row([exec.stored()], |row| row.get::<_, Option<String>>(1))
            .optional()?
            .flatten(),
        RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => one(
            tx,
            SQL.facts.root.sql(),
            rusqlite::params![
                owner.stored(),
                integer::<i64>(run.0)?,
                integer::<i64>(ordinal.0)?
            ],
        )?,
        RootOwner::EffectAdmission { exec, effect } => one(
            tx,
            SQL.graph.admission_root.sql(),
            rusqlite::params![exec.stored(), &effect.0[..]],
        )?,
        RootOwner::TurnPhase { session, run } => one(
            tx,
            SQL.turns.phase_root.sql(),
            [session.as_str(), run.as_str()],
        )?,
        RootOwner::WorkingExecution { session, run } => one(
            tx,
            SQL.turns.working_root.sql(),
            [session.as_str(), run.as_str()],
        )?,
        RootOwner::TurnNamespace {
            session,
            run,
            plugin,
        } => one(
            tx,
            SQL.turns.namespace_root.sql(),
            [session.as_str(), run.as_str(), plugin.as_str()],
        )?,
        RootOwner::SessionRevision { session, revision } => one(
            tx,
            SQL.graph.revision_root.sql(),
            rusqlite::params![session.as_str(), integer::<i64>(*revision)?],
        )?,
        RootOwner::SessionAdmissionBase { session } => {
            one(tx, SQL.graph.admission_base_root.sql(), [session.as_str()])?
        }
        RootOwner::ArtifactBridge {
            namespace,
            artifact,
        } => one(
            tx,
            SQL.sqlite.artifact_root.sql(),
            [namespace.as_str(), artifact.as_str()],
        )?,
        RootOwner::ReleasePin { release } => {
            one(tx, SQL.graph.release_root.sql(), [release.as_str()])?
        }
    };
    Ok(stored.as_deref().map(node_of).transpose())
}

/// Every node reachable from `root`, checked, within `limits`: a graph
/// node's children are read from its own child table, never trusted from
/// stored edges, and an opaque blob is a leaf.
pub(super) fn walk(
    tx: &Connection,
    root: NodeId,
    limits: &DecodeLimits,
) -> GraphAnswer<BTreeMap<NodeId, HydratedNode>> {
    walk_roots(tx, &[root], limits)
}

/// All selected roots share one visited set and one decode budget.
pub(super) fn walk_roots(
    tx: &Connection,
    roots: &[NodeId],
    limits: &DecodeLimits,
) -> GraphAnswer<BTreeMap<NodeId, HydratedNode>> {
    walk_within(tx, roots, limits, None)
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

/// [`GraphClosure`] of `root`, read in the caller's transaction.
pub(crate) fn graph_closure(
    tx: &Connection,
    root: NodeId,
    limits: &DecodeLimits,
) -> GraphAnswer<GraphClosure> {
    let mut opaque = BTreeSet::new();
    Ok(walk_within(tx, &[root], limits, Some(&mut opaque))?
        .map(|nodes| GraphClosure { nodes, opaque }))
}

/// Installs the staged `nodes` in the caller's transaction (the session
/// save path): every node they reference without supplying must be stored.
pub(crate) fn install_staged(
    tx: &Connection,
    nodes: &BTreeMap<NodeId, lash_core_store::checkpoint_graph::EncodedNode>,
) -> GraphAnswer<()> {
    let checked =
        match lash_durable::domain::graph_commit::check_graph(nodes, &BTreeSet::new(), &[]) {
            Ok(checked) => checked,
            Err(refusal) => return Ok(Err(refusal)),
        };
    Ok(install(tx, &checked)?.map(|_| ()))
}

#[cfg(test)]
thread_local! {
    pub(super) static WALK_COUNTS: std::cell::RefCell<(u64, BTreeMap<NodeId, u64>)> = const { std::cell::RefCell::new((0, BTreeMap::new())) };
}

fn walk_within(
    tx: &Connection,
    roots: &[NodeId],
    limits: &DecodeLimits,
    mut skipped: Option<&mut BTreeSet<NodeId>>,
) -> GraphAnswer<BTreeMap<NodeId, HydratedNode>> {
    #[cfg(test)]
    WALK_COUNTS.with_borrow_mut(|counts| counts.0 += 1);
    let mut nodes = BTreeMap::new();
    // Visit roots in fact order, as the individual walks did.
    let mut stack: Vec<_> = roots.iter().rev().copied().collect();
    let mut visited = BTreeSet::new();
    let mut total = 0_u64;
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        #[cfg(test)]
        WALK_COUNTS.with_borrow_mut(|counts| *counts.1.entry(id).or_default() += 1);
        // A stored node's edges keep every child it names stored, and the
        // child's own edges keep its children: a member of the build's
        // catalog a stored node reaches is stored with all it reaches. Its
        // bytes are the build's under their digests, so none is read back.
        if !roots.contains(&id)
            && let Some(closure) = immutable_closure(id)
        {
            for node in closure.nodes() {
                if nodes.contains_key(&node.id) {
                    continue;
                }
                if let Err(refusal) = count(&nodes, &mut total, node.bytes.len(), limits) {
                    return Ok(Err(refusal));
                }
                let len = node.bytes.len() as u64;
                if len > limits.node_bytes {
                    return Ok(Err(GraphRefusal::DecodeLimit {
                        dimension: DecodeDimension::NodeBytes,
                        actual: len,
                        limit: limits.node_bytes,
                    }));
                }
                visited.insert(node.id);
                #[cfg(test)]
                if node.id != id {
                    WALK_COUNTS
                        .with_borrow_mut(|counts| *counts.1.entry(node.id).or_default() += 1);
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
        // A reader of the graph alone records an opaque blob as opaque and
        // reads nothing of it.
        if let Some(skipped) = skipped.as_deref_mut()
            && !roots.contains(&id)
        {
            match stored_encoding(tx, &id)? {
                Ok(Some(BlobEncoding::Opaque)) => {
                    skipped.insert(id);
                    continue;
                }
                Ok(_) => {}
                Err(refusal) => return Ok(Err(refusal)),
            }
        }
        let Some((bytes, encoding)) = (match stored_blob(tx, &id.to_hex())? {
            Ok(found) => found,
            Err(refusal) => return Ok(Err(refusal)),
        }) else {
            return Ok(Err(GraphRefusal::MissingNode(id)));
        };
        if let Err(refusal) = count(&nodes, &mut total, bytes.len(), limits) {
            return Ok(Err(refusal));
        }
        if encoding == BlobEncoding::Graph {
            match check_envelope(&bytes, id, limits) {
                Ok(envelope) => stack.extend(envelope.children.iter()),
                Err(refusal) => return Ok(Err(refusal)),
            }
        }
        nodes.insert(
            id,
            HydratedNode {
                encoding,
                bytes: bytes.into(),
            },
        );
    }
    Ok(Ok(nodes))
}

/// Counts one more node of `bytes` against `limits`, beside `nodes`.
fn count(
    nodes: &BTreeMap<NodeId, HydratedNode>,
    total: &mut u64,
    bytes: usize,
    limits: &DecodeLimits,
) -> Result<(), GraphRefusal> {
    let count = nodes.len() as u64 + 1;
    *total = total.saturating_add(bytes as u64);
    if count > limits.nodes {
        return Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::Nodes,
            actual: count,
            limit: limits.nodes,
        });
    }
    if *total > limits.total_bytes {
        return Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::TotalBytes,
            actual: *total,
            limit: limits.total_bytes,
        });
    }
    Ok(())
}

pub(super) fn hydrate_in(
    tx: &Connection,
    owner: RootOwner,
    expected: Option<NodeId>,
    limits: &DecodeLimits,
) -> GraphAnswer<HydratedGraph> {
    // The owner's row is the read's first statement: it fixes the snapshot
    // every descendant is read on.
    let root = match root_of(tx, &owner)? {
        Ok(Some(root)) => root,
        Ok(None) => return Ok(Err(GraphRefusal::RootRetired)),
        Err(refusal) => return Ok(Err(refusal)),
    };
    if expected.is_some_and(|expected| expected != root) {
        return Ok(Err(GraphRefusal::RevisionConflict));
    }
    Ok(walk(tx, root, limits)?.map(|nodes| HydratedGraph { owner, root, nodes }))
}

/// What a peel did: the candidates it examined (the retired roots and each
/// child of a node it deleted), how many it deleted, and how many something
/// still names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Peeled {
    pub(crate) examined: usize,
    pub(crate) deleted: usize,
}

impl Peeled {
    /// The candidates something still names.
    pub(crate) fn retained(self) -> usize {
        self.examined - self.deleted
    }
}

/// Peels `frontier`, the roots the caller's transaction retired: deletes
/// each one nothing names or reaches any more, then each child the deleted
/// nodes' edges reached, level by level, stopping wherever something still
/// names a node. Its work is the newly unreachable nodes and their edges,
/// never a closure something still roots.
pub(crate) fn peel(tx: &Connection, frontier: BTreeSet<String>) -> rusqlite::Result<Peeled> {
    let mut examined = frontier.clone();
    let mut frontier = frontier;
    let mut deleted = 0;
    while !frontier.is_empty() {
        let (gone, children) = peel_level(tx, &frontier)?;
        deleted += gone;
        examined.extend(children.iter().cloned());
        frontier = children;
    }
    Ok(Peeled {
        examined: examined.len(),
        deleted,
    })
}

/// `values` as one JSON array, the binding a `json_each` statement reads.
fn json_array(values: &impl serde::Serialize) -> String {
    #[expect(clippy::expect_used, reason = "strings always encode as JSON")]
    serde_json::to_string(values).expect("a list of digests encodes")
}

/// One level of [`peel`]: deletes every node of `frontier` nothing names or
/// reaches, and answers how many with the children their edges reached.
fn peel_level(
    tx: &Connection,
    frontier: &BTreeSet<String>,
) -> rusqlite::Result<(usize, BTreeSet<String>)> {
    let list = json_array(frontier);
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut statement = tx.prepare_cached(SQL.sqlite.peel_edges.sql())?;
        let mut rows = statement.query([&list])?;
        while let Some(row) = rows.next()? {
            let children = edges.entry(row.get(0)?).or_default();
            if let Some(child) = row.get::<_, Option<String>>(1)? {
                children.push(child);
            }
        }
    }
    if edges.is_empty() {
        return Ok((0, BTreeSet::new()));
    }
    // Delete exactly the parents selected before this level changes any
    // edges. A frontier may contain both a parent and its child: the child
    // becomes deletable only after the parent goes, on the next level.
    let list = json_array(&edges.keys().collect::<Vec<_>>());
    let gone = tx
        .prepare_cached(SQL.sqlite.peel_orphans.sql())?
        .query_map([&list], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let children = gone
        .iter()
        .filter_map(|hash| edges.remove(hash))
        .flatten()
        .collect();
    Ok((gone.len(), children))
}

/// Peels what `retired` recorded, before its transaction commits: each
/// retired root's closure, and each ended turn's PhaseRoot alone. Answers
/// how many blobs it deleted.
pub(super) fn reclaim(tx: &Connection, retired: Retired) -> rusqlite::Result<usize> {
    let (closures, phase_ends) = retired.into_parts();
    let mut deleted = peel(tx, closures)?.deleted;
    if !phase_ends.is_empty() {
        deleted += peel_level(tx, &phase_ends)?.0;
    }
    Ok(deleted)
}

/// How many roots the catalog names and how many blobs are stored.
pub(crate) fn graph_counts(tx: &Connection) -> rusqlite::Result<(usize, usize)> {
    let count = |sql: &str| -> rusqlite::Result<usize> {
        let count: i64 = tx.prepare_cached(sql)?.query_row([], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    };
    Ok((
        count(SQL.graph.catalog_count.sql())?,
        count(SQL.graph.blob_count.sql())?,
    ))
}

/// One page of the orphan audit: peels up to `limit` blobs after `after`,
/// in digest order, that no root names and no edge reaches. Answers the
/// last orphan the page read (none when it read none) and how many blobs
/// the peel deleted.
pub(crate) fn audit_page(
    tx: &Connection,
    after: &str,
    limit: usize,
) -> rusqlite::Result<(Option<String>, usize)> {
    let orphans = tx
        .prepare_cached(SQL.graph.orphans_after.sql())?
        .query_map(
            rusqlite::params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let last = orphans.last().cloned();
    Ok((last, peel(tx, orphans)?.deleted))
}

fn graph_refused<T>(refusal: GraphRefusal) -> Answer<T> {
    Ok(Err(DurableError::Graph(refusal)))
}

/// Applies an admission write. A second admission of one effect, or the
/// removal of a lookup that is not the expected one, refuses the commit.
pub(super) fn apply_admission(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &EffectAdmissionWrite,
) -> Answer<()> {
    match write {
        EffectAdmissionWrite::Put {
            exec,
            effect,
            member,
            admission,
        } => {
            let inserted = tx
                .prepare_cached(SQL.graph.admission_put.sql())?
                .query_row(
                    rusqlite::params![
                        exec.stored(),
                        &effect.0[..],
                        member.as_ref().map(|member| member.owner.stored()),
                        member
                            .as_ref()
                            .map(|member| integer::<i64>(member.run.0))
                            .transpose()?,
                        member
                            .as_ref()
                            .map(|member| integer::<i64>(member.member))
                            .transpose()?,
                        admission.to_hex(),
                        commit.epoch.0
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if inserted.is_none() {
                return graph_refused(GraphRefusal::RevisionConflict);
            }
        }
        EffectAdmissionWrite::Remove {
            exec,
            effect,
            expected,
        } => {
            let removed = tx
                .prepare_cached(SQL.graph.admission_remove.sql())?
                .query_row(
                    rusqlite::params![exec.stored(), &effect.0[..], expected.to_hex()],
                    |_| Ok(()),
                )
                .optional()?;
            if removed.is_none() {
                return graph_refused(GraphRefusal::RevisionConflict);
            }
            commit.retired.closure(expected);
        }
    }
    Ok(Ok(()))
}

/// Drop every admission of the execution stored as `exec_key`, answering
/// the Admission nodes they held: the caller's to peel.
pub(super) fn clear_admissions(tx: &Connection, exec_key: &str) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.graph.admissions_clear.sql())?
        .query_map([exec_key], |row| row.get(0))?
        .collect()
}

/// Drop every admission whose execution key starts with `prefix`,
/// answering the Admission nodes they held: the caller's to peel.
pub(super) fn clear_admissions_under(
    tx: &Connection,
    prefix: &str,
) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.graph.admissions_clear_under.sql())?
        .query_map([prefix], |row| row.get(0))?
        .collect()
}

/// Applies a release pin write.
pub(super) fn apply_release_pin(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &ReleasePinWrite,
) -> Answer<()> {
    match write {
        ReleasePinWrite::Install { release, root } => {
            let installed = cached_execute(
                tx,
                SQL.graph.release_install.sql(),
                [release.as_str(), root.to_hex().as_str()],
            )?;
            if installed == 0 {
                let pinned = tx
                    .prepare_cached(SQL.graph.release_root.sql())?
                    .query_row([release.as_str()], |row| row.get::<_, String>(0))?;
                if pinned != root.to_hex() {
                    return graph_refused(GraphRefusal::RevisionConflict);
                }
            }
        }
        ReleasePinWrite::Retire { release, expected } => {
            let retired = tx
                .prepare_cached(SQL.graph.release_retire.sql())?
                .query_row([release.as_str(), expected.to_hex().as_str()], |_| Ok(()))
                .optional()?;
            if retired.is_none() {
                return graph_refused(GraphRefusal::RevisionConflict);
            }
            commit.retired.closure(expected);
        }
    }
    Ok(Ok(()))
}

/// Installs a checked graph inside the caller's fenced transaction: every
/// node it references without supplying must be stored (a published root
/// as a graph node), then each supplied blob in digest order, compared on
/// conflict, then every new node's edges once every blob exists.
pub(super) fn install(
    tx: &Connection,
    graph: &CheckedGraph,
) -> GraphAnswer<InstalledGraphMeasurement> {
    macro_rules! answer {
        ($answer:expr) => {
            match $answer? {
                Ok(value) => value,
                Err(refusal) => return Ok(Err(refusal)),
            }
        };
    }
    // One statement checks every node referenced without being supplied,
    // one installs every supplied node, one reads back those already stored
    // and one writes the fresh nodes' edges, whatever their number.
    let named: BTreeSet<&NodeId> = graph
        .external
        .iter()
        .chain(graph.external_roots.iter())
        .collect();
    if !named.is_empty() {
        let mut present = BTreeMap::new();
        let mut statement = tx.prepare_cached(SQL.sqlite.stored_encodings.sql())?;
        let rows = statement.query_map([digest_array(named)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (hash, encoding) = row?;
            present.insert(
                hash,
                answer!(Ok::<_, rusqlite::Error>(encoding_of(encoding))),
            );
        }
        for node in &graph.external {
            if !present.contains_key(&node.to_hex()) {
                return Ok(Err(GraphRefusal::MissingNode(*node)));
            }
        }
        for root in &graph.external_roots {
            match present.get(&root.to_hex()) {
                None => return Ok(Err(GraphRefusal::MissingNode(*root))),
                Some(BlobEncoding::Opaque) => {
                    return Ok(Err(GraphRefusal::EncodingConflict(*root)));
                }
                Some(BlobEncoding::Graph) => {}
            }
        }
    }
    let mut installed = InstalledGraphMeasurement::default();
    if graph.nodes.is_empty() {
        return Ok(Ok(installed));
    }
    // One statement reads back every staged node already stored, each row
    // compared as it streams; each node not stored is inserted alone, its
    // bytes bound as they are. An unchanged graph costs the same statements
    // at any size. A writer holds the database, so what is not stored now
    // is inserted.
    let mut present = BTreeSet::new();
    let staged: BTreeMap<NodeId, _> = graph.nodes.iter().map(|node| (node.id, node)).collect();
    {
        let mut statement = tx.prepare_cached(SQL.sqlite.stored_nodes.sql())?;
        let mut rows = statement.query([digest_array(staged.keys())])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let Some(node) = NodeId::from_hex(&hash).and_then(|id| staged.get(&id)) else {
                continue;
            };
            if answer!(Ok::<_, rusqlite::Error>(encoding_of(row.get(2)?))) != BlobEncoding::Graph {
                return Ok(Err(GraphRefusal::EncodingConflict(node.id)));
            }
            if row.get_ref(1)?.as_blob()? != &node.bytes[..] {
                return Ok(Err(GraphRefusal::ContentConflict(node.id)));
            }
            present.insert(node.id);
        }
    }
    let mut insert = tx.prepare_cached(SQL.graph.insert_blob.sql())?;
    let mut hash = String::with_capacity(64);
    let mut edges = String::from("[");
    for node in staged.values() {
        if present.contains(&node.id) {
            continue;
        }
        hash.clear();
        node.id.write_hex(&mut hash);
        insert.execute(rusqlite::params![
            hash.as_str(),
            &node.bytes[..],
            BlobEncoding::Graph as i64
        ])?;
        installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
        installed.new_nodes += 1;
        installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        for child in &node.children {
            if edges.len() > 1 {
                edges.push(',');
            }
            edges.push_str("[\"");
            edges.push_str(&hash);
            edges.push_str("\",\"");
            child.write_hex(&mut edges);
            edges.push_str("\"]");
            installed.new_edges += 1;
            installed.sql_logical_bytes += EDGE_ROW_LOGICAL_BYTES;
        }
    }
    if edges.len() > 1 {
        edges.push(']');
        cached_execute(tx, SQL.sqlite.insert_edges.sql(), [edges])?;
    }
    if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
        window.installed_graph(installed.newly_installed_canonical_bytes);
    }
    Ok(Ok(installed))
}

/// `ids` as one JSON array of digests, the binding a `json_each`
/// statement reads.
fn digest_array<'a>(ids: impl IntoIterator<Item = &'a NodeId>) -> String {
    let mut array = String::from("[");
    for id in ids {
        if array.len() > 1 {
            array.push(',');
        }
        array.push('"');
        id.write_hex(&mut array);
        array.push('"');
    }
    array.push(']');
    array
}

fn backend(error: DurableError) -> GraphRefusal {
    from_durable(error)
}

impl SqliteDurableStore {
    /// The store's connection, for laws that write foreign data.
    #[cfg(test)]
    pub(crate) fn connection_for_testing(&self) -> &crate::conn::SqliteConnection {
        &self.conn
    }

    /// A graph transaction's value, or its refusal: a store failure keeps
    /// its kind.
    async fn graph_write<T, F>(&self, label: CommitLabel, body: F) -> Result<T, GraphRefusal>
    where
        T: Send + 'static,
        F: FnOnce(
                &FencedTx<'_>,
                DurableInstant,
            ) -> rusqlite::Result<TxOutcome<Result<T, GraphRefusal>>>
            + Send
            + 'static,
    {
        self.write(label, move |tx, now| {
            Ok(match body(tx, now)? {
                TxOutcome::Commit(value) => TxOutcome::Commit(Ok(value)),
                TxOutcome::Rollback(value) => TxOutcome::Rollback(Ok(value)),
            })
        })
        .await
        .map_err(backend)?
    }

    /// Runs the orphan audit to its end, a page per transaction: peels
    /// every blob no root names and no edge reaches, with what only it
    /// reached. A write beside it waits for one page at most. Answers how
    /// many blobs it deleted.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn audit_orphans(&self) -> Result<u64, GraphRefusal> {
        let mut after = String::new();
        let mut deleted = 0_u64;
        loop {
            let from = after.clone();
            let (last, gone) = self
                .graph_write(GRAPH_AUDIT, move |tx, _| {
                    Ok(TxOutcome::Commit(Ok(audit_page(tx, &from, AUDIT_PAGE)?)))
                })
                .await?;
            deleted += gone as u64;
            match last {
                Some(last) => after = last,
                None => return Ok(deleted),
            }
        }
    }
}
