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
    BlobEncoding, CommitId, DecodeDimension, DecodeLimits, GraphRefusal, InstalledGraphMeasurement,
    NodeId, RootSource, check_envelope,
};
use lash_durable::domain::graph::{
    EffectAdmissionWrite, ExecRootRow, ExecRootWrite, FencedCommit, GraphCommitBatch,
    GraphDomainWrite, GraphStore, HydratedGraph, HydratedNode, PinnedBatch, PublishedGraph,
    ReadLease, ReadPin, ReadPinId, ReadPinWrite, ReleasePinWrite, RootChange, RootOwner,
    RunFactKind, RunFactRow, RunFactWrite, TurnGraphWrite,
};
use lash_durable::domain::graph_commit::{
    CheckedBatch, CollectionLimits, EDGE_ROW_LOGICAL_BYTES, GraphCollection,
    LegacyManifestComponents, blob_row_logical_bytes, check_batch, decode_published,
    encode_published, graph_write_logical_bytes,
};
use lash_durable::domain::{ExecKey, Ordinal, OwnerKey, RunSeq, SnapshotRev};
use lash_durable::{
    CommitLabel, DurableError, DurableInstant, Epoch, StoreFailure, StoreFailureKind,
};
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::durable::run_records::RunRecordStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Committing, SqliteDurableStore, apply_domain, integer};
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
    UNION ALL SELECT 'release', root_ref FROM graph_release_pins;
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

        /// Sweep every legacy manifest edge whose manifest is not marked in
        /// JSON `?1`. Transitional (FIG-5890): goes with the table.
        sweep_legacy_edges = "DELETE FROM checkpoint_blob_refs
             WHERE checkpoint_ref NOT IN (SELECT value FROM json_each(?1))";

        /// Sweep every blob not marked in JSON `?1`.
        sweep_blobs = "DELETE FROM blobs WHERE hash NOT IN (SELECT value FROM json_each(?1))";
    }
}

struct Sql {
    graph: GraphStatements,
    facts: RunRecordStatements,
    sqlite: GraphSqliteStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    Sql {
        graph: GraphStatements::render(dialect),
        facts: RunRecordStatements::render(dialect),
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
/// domain write's refusal is a conflict, and a store failure keeps its kind.
fn from_durable(error: DurableError) -> GraphRefusal {
    match error {
        DurableError::Store(failure) => GraphRefusal::Backend(failure),
        DurableError::OwnershipLost(_) => GraphRefusal::FenceLost,
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

fn hex(node: Option<&NodeId>) -> Option<String> {
    node.map(NodeId::to_hex)
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
            .prepare_cached(SQL.graph.exec_root.sql())?
            .query_row([exec.stored()], |row| row.get::<_, Option<String>>(1))
            .optional()?
            .flatten(),
        RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => one(
            tx,
            SQL.graph.fact_root.sql(),
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
            SQL.graph.phase_root.sql(),
            [session.as_str(), run.as_str()],
        )?,
        RootOwner::WorkingExecution { session, run } => one(
            tx,
            SQL.graph.working_root.sql(),
            [session.as_str(), run.as_str()],
        )?,
        RootOwner::TurnNamespace {
            session,
            run,
            plugin,
        } => one(
            tx,
            SQL.graph.namespace_root.sql(),
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
fn walk(
    tx: &Connection,
    root: NodeId,
    limits: &DecodeLimits,
) -> GraphAnswer<BTreeMap<NodeId, HydratedNode>> {
    let mut nodes = BTreeMap::new();
    let mut stack = vec![root];
    let mut total = 0_u64;
    while let Some(id) = stack.pop() {
        if nodes.contains_key(&id) {
            continue;
        }
        let Some((bytes, encoding)) = (match stored_blob(tx, &id.to_hex())? {
            Ok(found) => found,
            Err(refusal) => return Ok(Err(refusal)),
        }) else {
            return Ok(Err(GraphRefusal::MissingNode(id)));
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

fn hydrate_in(
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

/// Applies one graph-reference write. A conflict with the row it expects
/// refuses the whole commit.
fn apply_graph_write(tx: &Connection, epoch: Epoch, write: &GraphDomainWrite) -> GraphAnswer<()> {
    let conflict = || Ok(Err(GraphRefusal::RevisionConflict));
    match write {
        GraphDomainWrite::ExecRoot(ExecRootWrite::Put {
            exec,
            expected,
            expected_root,
            root,
            commit_id,
        }) => {
            let key = exec.stored();
            let found = tx
                .prepare_cached(SQL.graph.exec_root.sql())?
                .query_row([&key], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .optional()?;
            let found_rev = found
                .as_ref()
                .map(|(rev, _)| integer::<u64>(*rev).map(SnapshotRev))
                .transpose()?;
            let found_root = match found.and_then(|(_, root)| root).as_deref().map(node_of) {
                Some(Ok(root)) => Some(root),
                Some(Err(refusal)) => return Ok(Err(refusal)),
                None => None,
            };
            if found_rev != *expected || found_root != *expected_root {
                return conflict();
            }
            match expected {
                None => {
                    cached_execute(
                        tx,
                        SQL.graph.exec_insert.sql(),
                        rusqlite::params![key, root.to_hex(), &commit_id.0[..], epoch.0],
                    )?;
                }
                Some(rev) => {
                    let replaced = tx
                        .prepare_cached(SQL.graph.exec_replace.sql())?
                        .query_row(
                            rusqlite::params![
                                key,
                                integer::<i64>(rev.0)?,
                                root.to_hex(),
                                &commit_id.0[..],
                                epoch.0
                            ],
                            |_| Ok(()),
                        )
                        .optional()?;
                    if replaced.is_none() {
                        return conflict();
                    }
                }
            }
        }
        GraphDomainWrite::ExecRoot(ExecRootWrite::Retire { exec, expected }) => {
            let retired = tx
                .prepare_cached(SQL.graph.exec_retire.sql())?
                .query_row([exec.stored(), expected.to_hex()], |_| Ok(()))
                .optional()?;
            if retired.is_none() {
                return conflict();
            }
        }
        GraphDomainWrite::RunFact(RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        }) => {
            let key = owner.stored();
            let run_seq = integer::<i64>(run.0)?;
            let taken = |ordinal: u64| -> rusqlite::Result<bool> {
                Ok(tx
                    .prepare_cached(SQL.facts.ordinal_taken.sql())?
                    .query_row(
                        rusqlite::params![key, run_seq, integer::<i64>(ordinal)?],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            };
            if taken(ordinal.0)? {
                return conflict();
            }
            if let Some(previous) = ordinal.0.checked_sub(1)
                && !taken(previous)?
            {
                return conflict();
            }
            if *kind == RunFactKind::XOutcome
                && let Some(call) = call
            {
                let exists = tx
                    .prepare_cached(SQL.facts.outcome_exists.sql())?
                    .query_row(rusqlite::params![key, run_seq, call.as_str()], |_| Ok(()))
                    .optional()?;
                if exists.is_some() {
                    return Ok(Err(GraphRefusal::DuplicateOutcome(call.clone())));
                }
            }
            cached_execute(
                tx,
                SQL.graph.fact_append.sql(),
                rusqlite::params![
                    key,
                    run_seq,
                    integer::<i64>(ordinal.0)?,
                    kind.as_str(),
                    call.as_ref().map(|call| call.as_str()),
                    record.to_hex(),
                    epoch.0
                ],
            )?;
        }
        GraphDomainWrite::RunFact(RunFactWrite::Prune { owner, before }) => {
            cached_execute(
                tx,
                SQL.facts.prune.sql(),
                rusqlite::params![owner.stored(), integer::<i64>(before.0)?],
            )?;
        }
        GraphDomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec,
            effect,
            member,
            admission,
        }) => {
            // A second admission of one effect is refused as a conflict: the
            // effect identity's typed form is the kernel's to decode.
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
                        epoch.0
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if inserted.is_none() {
                return conflict();
            }
        }
        GraphDomainWrite::EffectAdmission(EffectAdmissionWrite::Remove {
            exec,
            effect,
            expected,
        }) => {
            let removed = tx
                .prepare_cached(SQL.graph.admission_remove.sql())?
                .query_row(
                    rusqlite::params![exec.stored(), &effect.0[..], expected.to_hex()],
                    |_| Ok(()),
                )
                .optional()?;
            if removed.is_none() {
                return conflict();
            }
        }
        GraphDomainWrite::Turn(write) => {
            let (sql, params): (&str, Vec<Option<String>>) = match write {
                TurnGraphWrite::Phase {
                    session,
                    run,
                    expected,
                    checkpoint,
                } => (
                    SQL.graph.phase_set.sql(),
                    vec![
                        Some(session.as_str().to_owned()),
                        Some(run.as_str().to_owned()),
                        hex(checkpoint.as_ref()),
                        hex(expected.as_ref()),
                    ],
                ),
                TurnGraphWrite::Working {
                    session,
                    run,
                    expected,
                    working,
                } => (
                    SQL.graph.working_set.sql(),
                    vec![
                        Some(session.as_str().to_owned()),
                        Some(run.as_str().to_owned()),
                        hex(working.as_ref()),
                        hex(expected.as_ref()),
                    ],
                ),
                TurnGraphWrite::Namespace {
                    session,
                    run,
                    plugin,
                    expected,
                    graph_root,
                } => (
                    SQL.graph.namespace_set.sql(),
                    vec![
                        Some(session.as_str().to_owned()),
                        Some(run.as_str().to_owned()),
                        Some(plugin.clone()),
                        hex(graph_root.as_ref()),
                        hex(expected.as_ref()),
                    ],
                ),
            };
            let set = tx
                .prepare_cached(sql)?
                .query_row(rusqlite::params_from_iter(params), |_| Ok(()))
                .optional()?;
            if set.is_none() {
                return conflict();
            }
        }
        GraphDomainWrite::ReadPin(ReadPinWrite::Acquire { pin }) => {
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
        GraphDomainWrite::ReadPin(ReadPinWrite::Retire { pin }) => {
            if !retire_pin(tx, pin)? {
                return Ok(Err(GraphRefusal::ReadPinMismatch));
            }
        }
        GraphDomainWrite::ReleasePin(ReleasePinWrite::Install { release, root }) => {
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
                    return conflict();
                }
            }
        }
        GraphDomainWrite::ReleasePin(ReleasePinWrite::Retire { release, expected }) => {
            let retired = tx
                .prepare_cached(SQL.graph.release_retire.sql())?
                .query_row([release.as_str(), expected.to_hex().as_str()], |_| Ok(()))
                .optional()?;
            if retired.is_none() {
                return conflict();
            }
        }
    }
    Ok(Ok(()))
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
                .prepare_cached(SQL.graph.exec_root.sql())?
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
    for node in &checked.external {
        if answer!(stored_encoding(tx, node)).is_none() {
            return refuse(GraphRefusal::MissingNode(*node));
        }
    }
    for root in &checked.external_roots {
        match answer!(stored_encoding(tx, root)) {
            None => return refuse(GraphRefusal::MissingNode(*root)),
            Some(BlobEncoding::Opaque) => return refuse(GraphRefusal::EncodingConflict(*root)),
            Some(BlobEncoding::Graph) => {}
        }
    }
    // Blobs in digest order, each compared on conflict; then every new
    // node's edges, once every blob exists.
    let mut installed = InstalledGraphMeasurement::default();
    let mut fresh = Vec::new();
    for node in &checked.nodes {
        let hash = node.id.to_hex();
        let inserted = cached_execute(
            tx,
            SQL.graph.insert_blob.sql(),
            rusqlite::params![hash, &node.bytes[..], BlobEncoding::Graph as i64],
        )?;
        if inserted == 1 {
            installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
            installed.new_nodes += 1;
            installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
            fresh.push(node);
            continue;
        }
        let Some((bytes, encoding)) = answer!(stored_blob(tx, &hash)) else {
            return refuse(GraphRefusal::MissingNode(node.id));
        };
        if encoding != BlobEncoding::Graph {
            return refuse(GraphRefusal::EncodingConflict(node.id));
        }
        if bytes[..] != node.bytes[..] {
            return refuse(GraphRefusal::ContentConflict(node.id));
        }
    }
    for node in fresh {
        let parent = node.id.to_hex();
        for child in &node.children {
            cached_execute(
                tx,
                SQL.graph.insert_edge.sql(),
                [parent.as_str(), child.to_hex().as_str()],
            )?;
            installed.new_edges += 1;
            installed.sql_logical_bytes += EDGE_ROW_LOGICAL_BYTES;
        }
    }
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
    }
    for write in &batch.graph_writes {
        answer!(apply_graph_write(tx, fence.epoch, write));
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
/// rest, in the caller's write transaction. `legacy` lists the component
/// references of an opaque session manifest (transitional, FIG-5890: the
/// integrator deletes it once session manifests are graph nodes).
pub(crate) fn collect_in(
    tx: &Connection,
    limits: CollectionLimits,
    legacy: &LegacyManifestComponents,
) -> GraphAnswer<GraphCollection> {
    let mut report = GraphCollection::default();
    let mut stack: Vec<(String, Option<RootSource>)> = Vec::new();
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
        stack.extend(rows.into_iter().map(|root| (root, Some(source))));
    }
    let mut marked = BTreeSet::new();
    let mut spent = 0_u64;
    let envelope_limits = DecodeLimits::within_total(u64::MAX);
    while let Some((hash, source)) = stack.pop() {
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
                stack.extend(children.into_iter().map(|child| (child, None)));
            }
            BlobEncoding::Opaque => {
                if !edges.is_empty() {
                    return Ok(Err(match NodeId::from_hex(&hash) {
                        Some(id) => GraphRefusal::EdgeProjectionMismatch(id),
                        None => corrupt("opaque blob edges", &hash),
                    }));
                }
                if matches!(
                    source,
                    Some(RootSource::SessionRevision | RootSource::SessionAdmissionBase)
                ) {
                    match legacy(&bytes) {
                        Ok(components) => {
                            stack.extend(components.into_iter().map(|blob| (blob, None)));
                        }
                        Err(refusal) => return Ok(Err(refusal)),
                    }
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
    cached_execute(tx, SQL.sqlite.sweep_legacy_edges.sql(), [&marks])?;
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
            let fleet = tx.fleet();
            let legacy = move |bytes: &[u8]| crate::graph::legacy_manifest_components(bytes, fleet);
            Ok(match collect_in(tx, limits, &legacy)? {
                Ok(report) => TxOutcome::Commit(Ok(report)),
                Err(refusal) => TxOutcome::Rollback(Err(refusal)),
            })
        })
        .await
    }

    /// Execution `exec`'s graph root row.
    ///
    /// # Errors
    ///
    /// A store failure, or a row that does not decode.
    pub async fn graph_exec_root(
        &self,
        exec: &ExecKey,
    ) -> Result<Option<ExecRootRow>, GraphRefusal> {
        let exec = exec.clone();
        self.read(move |tx| {
            let row = tx
                .prepare_cached(SQL.graph.exec_row.sql())?
                .query_row([exec.stored()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .optional()?;
            Ok(Ok(match row {
                None => Ok(None),
                Some((rev, root, epoch, commit)) => node_of(&root).and_then(|root| {
                    let commit_id = <[u8; 32]>::try_from(commit.as_slice())
                        .map_err(|_| corrupt("commit id", &root.to_hex()))?;
                    Ok(Some(ExecRootRow {
                        exec,
                        rev: SnapshotRev(
                            u64::try_from(rev)
                                .map_err(|_| corrupt("revision", &rev.to_string()))?,
                        ),
                        root,
                        written_epoch: Epoch(epoch),
                        commit_id: CommitId(commit_id),
                    }))
                }),
            }))
        })
        .await
        .map_err(backend)?
    }

    /// Every graph run fact of `owner`, by run and ordinal.
    ///
    /// # Errors
    ///
    /// A store failure, or a row that does not decode.
    pub async fn graph_run_facts(&self, owner: &OwnerKey) -> Result<Vec<RunFactRow>, GraphRefusal> {
        let owner = owner.clone();
        self.read(move |tx| {
            let rows = tx
                .prepare_cached(SQL.graph.facts.sql())?
                .query_map([owner.stored()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let decoded = rows
                .into_iter()
                .map(|(run, ordinal, kind, call, record, epoch)| {
                    Ok(RunFactRow {
                        owner: owner.clone(),
                        run: RunSeq(
                            integer::<u64>(run).map_err(|_| corrupt("run", &run.to_string()))?,
                        ),
                        ordinal: Ordinal(
                            integer::<u64>(ordinal)
                                .map_err(|_| corrupt("ordinal", &ordinal.to_string()))?,
                        ),
                        kind: RunFactKind::parse(&kind)
                            .ok_or_else(|| corrupt("fact kind", &kind))?,
                        call: call
                            .map(|call| {
                                lash_sansio::ToolCallId::parse(&call)
                                    .map_err(|_| corrupt("call id", &call))
                            })
                            .transpose()?,
                        record: node_of(&record)?,
                        written_epoch: Epoch(epoch),
                    })
                })
                .collect::<Result<Vec<_>, GraphRefusal>>();
            Ok(Ok(decoded))
        })
        .await
        .map_err(backend)?
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

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<NodeId>,
        limits: DecodeLimits,
    ) -> Result<HydratedGraph, GraphRefusal> {
        self.read(move |tx| Ok(Ok(hydrate_in(tx, owner, expected, &limits)?)))
            .await
            .map_err(backend)?
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
