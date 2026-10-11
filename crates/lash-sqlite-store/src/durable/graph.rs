//! The checkpoint graph on SQLite: the `graph` domain's DDL, the
//! [`GraphStore`] commit, hydration and read pins, and the reachability
//! collector (FIG-5887 §5, §7, §8; lane G, FIG-5890).
//!
//! # One transaction, one lock
//!
//! Every graph write runs in [`SqliteConnection::write_flow`]: the
//! database's write lock (`BEGIN IMMEDIATE`) and the writer fence come
//! first, then the owner's epoch fence, then the commit in the order of
//! [`lash_durable::domain::graph`]'s contracts. SQLite has one writer, so
//! the lock order PostgreSQL spells with its graph gate is the database
//! lock here: a writer, a pin's fetch and the collector never interleave.
//!
//! A hydration is one read transaction that selects the owner's root first,
//! fixing its snapshot, and reads every descendant on it, so a writer that
//! replaces the root or a collector that sweeps it afterwards cannot reach
//! what it reads.
//!
//! # Roots
//!
//! The root catalog (`graph_root_catalog`) is a view of the owning columns.
//! The collector enumerates it one source at a time, records every source
//! exhausted, and marks from those roots alone: a missing root or child, a
//! stored edge set that is not a node's child table, an incomplete source
//! or an exhausted work budget rolls the whole collection back with nothing
//! swept.

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
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::durable::run_records::RunRecordStatements;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use lash_store_sql::durable::turns::TurnStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, SqliteDurableStore, apply_domain, integer};
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

-- A streaming reader's pin on one root. Every row roots, expired or not,
-- until its explicit retirement.
CREATE TABLE IF NOT EXISTS blob_read_pins (
    pin_id TEXT PRIMARY KEY,
    lease_owner TEXT NOT NULL,
    generation INTEGER NOT NULL CONSTRAINT ck_blob_read_pins_generation CHECK (generation >= 1),
    root_ref TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_blob_read_pins_root_ref ON blob_read_pins (root_ref);

-- A deliberately installed runnable release manifest.
CREATE TABLE IF NOT EXISTS graph_release_pins (
    release_key TEXT PRIMARY KEY,
    root_ref TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_graph_release_pins_root_ref ON graph_release_pins (root_ref);

-- A commit's retry identity and the result it answered. It owns no root and
-- authorizes no hydration.
CREATE TABLE IF NOT EXISTS graph_commit_receipts (
    owner_key TEXT NOT NULL,
    commit_id BLOB NOT NULL CONSTRAINT ck_graph_commit_receipts_commit_id
        CHECK (length(commit_id) = 32),
    batch_digest BLOB NOT NULL CONSTRAINT ck_graph_commit_receipts_batch_digest
        CHECK (length(batch_digest) = 32),
    result_bytes BLOB NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (owner_key, commit_id)
);

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
    UNION ALL SELECT 'read_pin', root_ref FROM blob_read_pins
    UNION ALL SELECT 'artifact', blob_ref FROM artifact_refs
    UNION ALL SELECT 'release', root_ref FROM graph_release_pins
    UNION ALL SELECT 'terminal_window', terminal_window_checkpoint_ref FROM session_runs
        WHERE terminal_window_checkpoint_ref IS NOT NULL;
";

lash_store_sql::statements! {
    /// Graph statements only SQLite issues: the artifact bridge's pointer
    /// table, the unlocked pin read (the write lock holds it), and the
    /// sweep's `json_each` binding of the mark set.
    pub(crate) struct GraphSqliteStatements @ "durable_graph_sqlite" {
        /// Artifact `?2` of namespace `?1`'s blob: SQLite's bridge is its
        /// pointer table.
        artifact_root = "SELECT blob_ref FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2";

        /// Read pin `?1`'s row.
        pin_row = "SELECT lease_owner, generation, root_ref, expires_at_ms
             FROM blob_read_pins WHERE pin_id = ?1";

        /// How many edges lead from a marked parent in JSON `?1` to an
        /// unmarked child: none after a complete closure.
        dangling_edges = "SELECT count(*) FROM blob_edges
             WHERE parent_hash IN (SELECT value FROM json_each(?1))
               AND child_hash NOT IN (SELECT value FROM json_each(?1))";

        /// Sweep every edge whose parent is not marked in JSON `?1`.
        sweep_edges = "DELETE FROM blob_edges
             WHERE parent_hash NOT IN (SELECT value FROM json_each(?1))";

        /// Sweep every blob not marked in JSON `?1`.
        sweep_blobs = "DELETE FROM blobs WHERE hash NOT IN (SELECT value FROM json_each(?1))";

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

/// A read pin's acquisition, fetch, renewal or retirement.
const GRAPH_READ_PIN: CommitLabel = CommitLabel::new("graph.read_pin");

/// A reachability collection.
const GRAPH_COLLECT: CommitLabel = CommitLabel::new("graph.collect");

/// A lease's time to live as a span of the store's clock.
fn ttl(lease: &ReadLease) -> i64 {
    i64::try_from(lease.ttl_ms).unwrap_or(i64::MAX)
}

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

fn stored_edges(tx: &Connection, hash: &str) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.graph.edges_of.sql())?
        .query_map([hash], |row| row.get::<_, String>(0))?
        .collect()
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
        RootOwner::ReadPin(pin) => one(tx, SQL.graph.pin_root.sql(), [pin.0.as_str()])?,
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
    walk_within(tx, root, limits, None)
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
    Ok(walk_within(tx, root, limits, Some(&mut opaque))?
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

fn walk_within(
    tx: &Connection,
    root: NodeId,
    limits: &DecodeLimits,
    mut skipped: Option<&mut BTreeSet<NodeId>>,
) -> GraphAnswer<BTreeMap<NodeId, HydratedNode>> {
    let mut nodes = BTreeMap::new();
    let mut stack = vec![root];
    let mut total = 0_u64;
    while let Some(id) = stack.pop() {
        if nodes.contains_key(&id) {
            continue;
        }
        let cached = crate::conn::catalog::get(tx, id)?;
        // A reader of the graph alone records an opaque blob as opaque and
        // reads nothing of it.
        if let Some(skipped) = skipped.as_deref_mut()
            && id != root
            && cached.is_none()
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
        let (bytes, encoding) = match cached {
            Some(node) => (node.bytes, BlobEncoding::Graph),
            None => {
                let Some((bytes, encoding)) = (match stored_blob(tx, &id.to_hex())? {
                    Ok(found) => found,
                    Err(refusal) => return Ok(Err(refusal)),
                }) else {
                    return Ok(Err(GraphRefusal::MissingNode(id)));
                };
                (std::sync::Arc::<[u8]>::from(bytes), encoding)
            }
        };
        let count = nodes.len() as u64 + 1;
        total = total.saturating_add(bytes.len() as u64);
        if count > limits.nodes {
            return Ok(Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::Nodes,
                actual: count,
                limit: limits.nodes,
            }));
        }
        if total > limits.total_bytes {
            return Ok(Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::TotalBytes,
                actual: total,
                limit: limits.total_bytes,
            }));
        }
        if encoding == BlobEncoding::Graph {
            match check_envelope(&bytes, id, limits) {
                Ok(envelope) => {
                    stack.extend(envelope.children.iter());
                    crate::conn::catalog::remember(tx, id, &bytes)?;
                }
                Err(refusal) => return Ok(Err(refusal)),
            }
        }
        nodes.insert(id, HydratedNode { encoding, bytes });
    }
    Ok(Ok(nodes))
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

/// Every blob reachable from `roots` along stored edges, the roots included:
/// what an owner-scoped delete may reclaim once its roots are gone.
pub(crate) fn reachable_from(
    tx: &Connection,
    roots: &BTreeSet<String>,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = roots.iter().cloned().collect();
    while let Some(hash) = stack.pop() {
        if seen.insert(hash.clone()) {
            stack.extend(stored_edges(tx, &hash)?);
        }
    }
    Ok(seen)
}

/// Deletes every blob of `candidates` nothing roots or references any more,
/// parents first: a parent's edges go with it, so its children become
/// deletable in a later round. Answers how many it deleted and how many it
/// kept.
pub(crate) fn reclaim_candidates(
    tx: &Connection,
    candidates: BTreeSet<String>,
    reclaim: &str,
) -> rusqlite::Result<(usize, usize)> {
    let mut remaining = candidates;
    let mut deleted = 0;
    loop {
        let mut kept = BTreeSet::new();
        let before = deleted;
        for hash in remaining {
            match cached_execute(tx, reclaim, [&hash])? {
                0 => {
                    kept.insert(hash);
                }
                count => deleted += count,
            }
        }
        remaining = kept;
        if deleted == before || remaining.is_empty() {
            return Ok((deleted, remaining.len()));
        }
    }
}

/// Whether every node in `wanted` is reachable from `root` along stored
/// edges, which a commit installs exactly as each node's child table.
fn reachable(tx: &Connection, root: &NodeId, wanted: &[NodeId]) -> rusqlite::Result<bool> {
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
        stack.extend(stored_edges(tx, &hash)?);
    }
    Ok(missing.is_empty())
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
        }
    }
    Ok(Ok(()))
}

/// Applies a read pin write.
pub(super) fn apply_read_pin(tx: &Connection, write: &ReadPinWrite) -> Answer<()> {
    match write {
        ReadPinWrite::Acquire { pin } => {
            cached_execute(
                tx,
                SQL.graph.pin_insert.sql(),
                rusqlite::params![
                    pin.pin_id.0,
                    pin.lease_owner,
                    integer::<i64>(pin.generation)?,
                    pin.root.to_hex(),
                    pin.expires_at.0
                ],
            )?;
        }
        ReadPinWrite::Retire { pin } => {
            if !retire_pin(tx, pin)? {
                return graph_refused(GraphRefusal::ReadPinMismatch);
            }
        }
    }
    Ok(Ok(()))
}

/// Applies a release pin write.
pub(super) fn apply_release_pin(tx: &Connection, write: &ReleasePinWrite) -> Answer<()> {
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
    let named: BTreeSet<String> = graph
        .external
        .iter()
        .chain(graph.external_roots.iter())
        .map(NodeId::to_hex)
        .collect();
    if !named.is_empty() {
        let mut present = BTreeMap::new();
        let mut statement = tx.prepare_cached(SQL.sqlite.stored_encodings.sql())?;
        let rows = statement.query_map([json_array(&named)], |row| {
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
    let mut staged = BTreeMap::new();
    let mut present = BTreeSet::new();
    for node in &graph.nodes {
        if crate::conn::catalog::get(tx, node.id)?.is_some_and(|stored| stored.bytes == node.bytes)
        {
            present.insert(node.id.to_hex());
        } else {
            staged.insert(node.id.to_hex(), node);
        }
    }
    {
        let mut statement = tx.prepare_cached(SQL.sqlite.stored_nodes.sql())?;
        let mut rows = statement.query([json_array(&staged.keys().collect::<Vec<_>>())])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let Some(node) = staged.get(&hash) else {
                continue;
            };
            if answer!(Ok::<_, rusqlite::Error>(encoding_of(row.get(2)?))) != BlobEncoding::Graph {
                return Ok(Err(GraphRefusal::EncodingConflict(node.id)));
            }
            if row.get_ref(1)?.as_blob()? != &node.bytes[..] {
                return Ok(Err(GraphRefusal::ContentConflict(node.id)));
            }
            present.insert(hash);
        }
    }
    let mut inserted = BTreeSet::new();
    for (hash, node) in &staged {
        if present.contains(hash) {
            continue;
        }
        cached_execute(
            tx,
            SQL.graph.insert_blob.sql(),
            rusqlite::params![hash, &node.bytes[..], BlobEncoding::Graph as i64],
        )?;
        inserted.insert(hash.clone());
    }
    let mut edges = Vec::new();
    for node in &graph.nodes {
        let parent = node.id.to_hex();
        if !inserted.contains(&parent) {
            continue;
        }
        installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
        installed.new_nodes += 1;
        installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        for child in &node.children {
            edges.push((parent.clone(), child.to_hex()));
            installed.new_edges += 1;
            installed.sql_logical_bytes += EDGE_ROW_LOGICAL_BYTES;
        }
    }
    if !edges.is_empty() {
        cached_execute(tx, SQL.sqlite.insert_edges.sql(), [json_array(&edges)])?;
    }
    Ok(Ok(installed))
}

/// `values` as one JSON array, the binding a `json_each` statement reads.
fn json_array(values: &impl serde::Serialize) -> String {
    #[expect(
        clippy::expect_used,
        reason = "strings and pairs of strings always encode as JSON"
    )]
    serde_json::to_string(values).expect("a list of digests encodes")
}

fn retire_pin(tx: &Connection, pin: &ReadPin) -> rusqlite::Result<bool> {
    Ok(tx
        .prepare_cached(SQL.graph.pin_retire.sql())?
        .query_row(
            rusqlite::params![
                pin.pin_id.0,
                pin.lease_owner,
                integer::<i64>(pin.generation)?,
                pin.root.to_hex()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The whole commit, in its fenced transaction.
fn commit_in(
    tx: &FencedTx<'_>,
    fence: &FencedCommit,
    batch: &GraphCommitBatch,
    checked: &CheckedBatch,
    now: DurableInstant,
    blob_profile: crate::BuiltinBlobProfile,
) -> rusqlite::Result<TxOutcome<Result<PublishedGraph, GraphRefusal>>> {
    let refuse = |refusal| Ok(TxOutcome::Rollback(Err(refusal)));
    macro_rules! answer {
        ($answer:expr) => {
            match $answer? {
                Ok(value) => value,
                Err(refusal) => return refuse(refusal),
            }
        };
    }
    // The owner's epoch fence: the transaction's first engine statement.
    let actor = fence.actor.as_str();
    let fenced = tx
        .prepare_cached(super::SQL.actor.fence.sql())?
        .query_row(rusqlite::params![actor, fence.epoch.0], |row| {
            row.get::<_, i64>(0)
        })
        .optional()?;
    if fenced.is_none() {
        return refuse(GraphRefusal::FenceLost);
    }
    // A retry of an acknowledged commit is answered from its receipt; the
    // fence bump above rolls back with it.
    let receipt = tx
        .prepare_cached(SQL.graph.receipt.sql())?
        .query_row(rusqlite::params![actor, &batch.commit_id.0[..]], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .optional()?;
    if let Some((digest, result)) = receipt {
        if digest != checked.encoded.digest.0 {
            return refuse(GraphRefusal::CommitIdentityConflict);
        }
        return Ok(TxOutcome::Rollback(
            decode_published(&result)
                .map_err(|error| corrupt("commit receipt", &error.to_string())),
        ));
    }
    answer!(Ok::<_, rusqlite::Error>(
        batch.budget.admit(&checked.encoded.measurement)
    ));
    for (owner, expected) in batch.expected.iter() {
        let found = match owner {
            RootOwner::Exec(exec) => tx
                .prepare_cached(SQL.snapshots.root.sql())?
                .query_row([exec.stored()], |row| row.get::<_, i64>(0))
                .optional()?
                .map(integer::<u64>)
                .transpose()?,
            // Only an execution's row carries a stored revision; every other
            // owner is compared by its expected root.
            _ => None,
        };
        if found != expected {
            return refuse(GraphRefusal::RevisionConflict);
        }
    }
    let mut installed = answer!(install(tx, &checked.graph));
    let committing = Committing {
        actor: &fence.actor,
        epoch: fence.epoch,
        now,
        fleet: tx.fleet(),
        blob_profile,
    };
    for write in &batch.domain_writes {
        if let Err(error) = apply_domain(tx, &committing, write)? {
            return refuse(from_durable(error));
        }
        installed.sql_logical_bytes += graph_write_logical_bytes(write);
    }
    let revisions = answer!(Ok::<_, rusqlite::Error>(batch.expected.advanced()));
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
    cached_execute(
        tx,
        SQL.graph.receipt_insert.sql(),
        rusqlite::params![
            actor,
            &batch.commit_id.0[..],
            &checked.encoded.digest.0[..],
            encode_published(&published),
            fence.epoch.0
        ],
    )?;
    Ok(TxOutcome::Commit(Ok(published)))
}

/// Reads read pin `pin`'s row under the write lock and checks it is exactly
/// `pin` and unexpired at `now`.
fn check_pin(tx: &Connection, pin: &ReadPin, now: DurableInstant) -> GraphAnswer<()> {
    let row = tx
        .prepare_cached(SQL.sqlite.pin_row.sql())?
        .query_row([pin.pin_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .optional()?;
    let Some((lease_owner, generation, root, expires_at)) = row else {
        return Ok(Err(GraphRefusal::ReadPinMismatch));
    };
    if lease_owner != pin.lease_owner
        || integer::<u64>(generation)? != pin.generation
        || root != pin.root.to_hex()
    {
        return Ok(Err(GraphRefusal::ReadPinMismatch));
    }
    if expires_at < now.0 {
        return Ok(Err(GraphRefusal::ReadPinExpired));
    }
    Ok(Ok(()))
}

fn fetch_pinned_in(
    tx: &Connection,
    pin: ReadPin,
    nodes: &[NodeId],
    renew: Option<ReadLease>,
    now: DurableInstant,
) -> GraphAnswer<PinnedBatch> {
    if let Err(refusal) = check_pin(tx, &pin, now)? {
        return Ok(Err(refusal));
    }
    if !reachable(tx, &pin.root, nodes)? {
        return Ok(Err(GraphRefusal::ReadPinMismatch));
    }
    let mut fetched = BTreeMap::new();
    let limits = DecodeLimits::within_total(u64::MAX);
    for id in nodes {
        let Some((bytes, encoding)) = (match stored_blob(tx, &id.to_hex())? {
            Ok(found) => found,
            Err(refusal) => return Ok(Err(refusal)),
        }) else {
            return Ok(Err(GraphRefusal::MissingNode(*id)));
        };
        if encoding == BlobEncoding::Graph
            && let Err(refusal) = check_envelope(&bytes, *id, &limits)
        {
            return Ok(Err(refusal));
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
                return Ok(Err(GraphRefusal::ReadPinMismatch));
            }
            let expires_at = now.after_millis(ttl(&lease));
            let generation: i64 = tx
                .prepare_cached(SQL.graph.pin_renew.sql())?
                .query_row(rusqlite::params![pin.pin_id.0, expires_at.0], |row| {
                    row.get(0)
                })?;
            ReadPin {
                generation: integer::<u64>(generation)?,
                expires_at,
                ..pin
            }
        }
    };
    Ok(Ok(PinnedBatch {
        pin,
        nodes: fetched,
    }))
}

/// Marks every blob reachable from the complete root catalog and sweeps the
/// rest, in the caller's write transaction.
pub(crate) fn collect_in(
    tx: &Connection,
    limits: CollectionLimits,
) -> GraphAnswer<GraphCollection> {
    let mut report = GraphCollection::default();
    let mut stack: Vec<String> = Vec::new();
    for source in RootSource::ALL {
        let rows = tx
            .prepare_cached(SQL.graph.catalog_source.sql())
            .and_then(|mut statement| {
                statement
                    .query_map([source.catalog_name()], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        let Ok(rows) = rows else {
            return Ok(Err(GraphRefusal::RootSourceIncomplete(source)));
        };
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
            return Ok(Err(GraphRefusal::CollectionIncomplete));
        }
        let Some((bytes, encoding)) = (match stored_blob(tx, &hash)? {
            Ok(found) => found,
            Err(refusal) => return Ok(Err(refusal)),
        }) else {
            return Ok(Err(match NodeId::from_hex(&hash) {
                Some(id) => GraphRefusal::MissingNode(id),
                None => corrupt("blob reference", &hash),
            }));
        };
        spent = spent.saturating_add(bytes.len() as u64);
        if spent > limits.bytes {
            return Ok(Err(GraphRefusal::CollectionIncomplete));
        }
        let edges = stored_edges(tx, &hash)?;
        match encoding {
            BlobEncoding::Graph => {
                let id = match NodeId::from_hex(&hash) {
                    Some(id) => id,
                    None => return Ok(Err(corrupt("graph node reference", &hash))),
                };
                let envelope = match check_envelope(&bytes, id, &envelope_limits) {
                    Ok(envelope) => envelope,
                    Err(refusal) => return Ok(Err(refusal)),
                };
                let children: Vec<String> = envelope.children.iter().map(|c| c.to_hex()).collect();
                if children != edges {
                    return Ok(Err(GraphRefusal::EdgeProjectionMismatch(id)));
                }
                stack.extend(children);
            }
            BlobEncoding::Opaque => {
                if !edges.is_empty() {
                    return Ok(Err(match NodeId::from_hex(&hash) {
                        Some(id) => GraphRefusal::EdgeProjectionMismatch(id),
                        None => corrupt("opaque blob edges", &hash),
                    }));
                }
            }
        }
        marked.insert(hash);
    }
    report.marked = marked.len() as u64;
    let marks = serde_json::Value::from(marked.into_iter().collect::<Vec<_>>()).to_string();
    let dangling: i64 = tx
        .prepare_cached(SQL.sqlite.dangling_edges.sql())?
        .query_row([&marks], |row| row.get(0))?;
    if dangling != 0 {
        return Ok(Err(GraphRefusal::CollectionIncomplete));
    }
    report.swept_edges = cached_execute(tx, SQL.sqlite.sweep_edges.sql(), [&marks])? as u64;
    report.swept_blobs = cached_execute(tx, SQL.sqlite.sweep_blobs.sql(), [&marks])? as u64;
    Ok(Ok(report))
}

/// Retires every read pin expired at `now`, in the caller's transaction.
pub(crate) fn retire_expired_read_pins_in(tx: &Connection, now: i64) -> rusqlite::Result<usize> {
    cached_execute(tx, SQL.graph.pins_retire_expired.sql(), [now])
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

    /// Releases `pin` before its expiry. Refused when it is not exactly
    /// the stored pin.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::ReadPinMismatch`], or a store failure.
    pub async fn release_read_pin(&self, pin: ReadPin) -> Result<(), GraphRefusal> {
        self.graph_write(GRAPH_READ_PIN, move |tx, _| {
            Ok(if retire_pin(tx, &pin)? {
                TxOutcome::Commit(Ok(()))
            } else {
                TxOutcome::Rollback(Err(GraphRefusal::ReadPinMismatch))
            })
        })
        .await
    }

    /// Retires every read pin expired now, in an ordinary transaction of its
    /// own: expiry alone never removes a root. Answers how many it retired.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn retire_expired_read_pins(&self) -> Result<u64, GraphRefusal> {
        self.graph_write(GRAPH_READ_PIN, move |tx, now| {
            let retired = retire_expired_read_pins_in(tx, now.0)?;
            Ok(TxOutcome::Commit(Ok(retired as u64)))
        })
        .await
    }

    /// Collects every blob no root reaches, within `limits`, after retiring
    /// expired read pins in a transaction of their own. Nothing is swept
    /// unless every source enumerated completely and every reached node
    /// checked.
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
        self.graph_write(GRAPH_COLLECT, move |tx, _| {
            Ok(match collect_in(tx, limits)? {
                Ok(report) => TxOutcome::Commit(Ok(report)),
                Err(refusal) => TxOutcome::Rollback(Err(refusal)),
            })
        })
        .await
    }
}

#[async_trait::async_trait]
impl GraphStore for SqliteDurableStore {
    async fn commit_graph(
        &self,
        tx: FencedCommit,
        batch: GraphCommitBatch,
    ) -> Result<PublishedGraph, GraphRefusal> {
        let checked = check_batch(&batch)?;
        let blob_profile = self.blob_profile;
        let label = tx.label;
        self.graph_write(label, move |connection, now| {
            commit_in(connection, &tx, &batch, &checked, now, blob_profile)
        })
        .await
    }

    async fn acquire_read_pin(
        &self,
        owner: RootOwner,
        lease: ReadLease,
    ) -> Result<ReadPin, GraphRefusal> {
        self.graph_write(GRAPH_READ_PIN, move |tx, now| {
            let root = match root_of(tx, &owner)? {
                Ok(Some(root)) => root,
                Ok(None) => return Ok(TxOutcome::Rollback(Err(GraphRefusal::RootRetired))),
                Err(refusal) => return Ok(TxOutcome::Rollback(Err(refusal))),
            };
            let expires_at = now.after_millis(ttl(&lease));
            let pin = ReadPin {
                pin_id: ReadPinId(uuid::Uuid::new_v4().to_string()),
                lease_owner: lease.lease_owner,
                generation: 1,
                root,
                expires_at,
            };
            cached_execute(
                tx,
                SQL.graph.pin_insert.sql(),
                rusqlite::params![
                    pin.pin_id.0,
                    pin.lease_owner,
                    1_i64,
                    root.to_hex(),
                    pin.expires_at.0
                ],
            )?;
            Ok(TxOutcome::Commit(Ok(pin)))
        })
        .await
    }

    async fn fetch_pinned(
        &self,
        pin: ReadPin,
        nodes: &[NodeId],
        renew: Option<ReadLease>,
    ) -> Result<PinnedBatch, GraphRefusal> {
        let nodes = nodes.to_vec();
        self.graph_write(GRAPH_READ_PIN, move |tx, now| {
            Ok(match fetch_pinned_in(tx, pin, &nodes, renew, now)? {
                Ok(batch) => TxOutcome::Commit(Ok(batch)),
                Err(refusal) => TxOutcome::Rollback(Err(refusal)),
            })
        })
        .await
    }
}
