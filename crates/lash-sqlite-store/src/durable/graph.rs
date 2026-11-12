//! The checkpoint graph on SQLite: the `graph` domain's DDL, the
//! owner graph installation, hydration, and the retirement holds a commit
//! leaves for bounded retirement steps (FIG-5887 §5, §7; FIG-5949;
//! FIG-6191).
//!
//! # One transaction, one lock
//!
//! Every graph write runs in [`SqliteConnection::write_flow`]: the
//! database's write lock (`BEGIN IMMEDIATE`) and the writer fence come
//! first, then the owner's epoch fence, then the commit in the order of
//! [`lash_durable::domain::graph`]'s contracts. SQLite has one writer, so a
//! retirement step never interleaves with an install.
//!
//! A hydration is one read transaction that selects the owner's root first,
//! fixing its snapshot, and reads every descendant on it, so a writer that
//! replaces the root and peels its closure afterwards cannot reach what it
//! reads.
//!
//! # Reclamation
//!
//! The root catalog (`graph_root_catalog`) is a view of the owning columns,
//! root leases and retirement holds among them. The graph is a DAG, so a
//! node no root reaches is an orphan (no root row, no edge to it) or
//! reached only from such nodes: deleting orphans, whose edges go with
//! them, deletes exactly what nothing reaches. A transaction that retires a
//! root moves it to a retirement hold (D-RET) and peels nothing; a bounded
//! retirement step ([`super::pages`]) takes one hold and peels under it.
//! The orphan audit ([`audit_page`]) peels whatever a write outside the
//! holds left behind.

use crate::Prepare as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::BlobEncoding;
use lash_core_store::checkpoint_graph::DecodeLimits;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_core_store::checkpoint_graph::InstalledGraphMeasurement;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeSource;
use lash_core_store::checkpoint_graph::StoredNode;
use lash_core_store::checkpoint_graph::check_envelope;
use lash_core_store::checkpoint_graph::node::is_semantic;
use lash_durable::domain::DomainRefusal;
use lash_durable::domain::graph::{
    AdmissionMember, Delivery, EffectAdmissionRow, EffectAdmissionWrite, EffectKey, HydratedGraph,
    HydratedNode, RootOwner,
};
use lash_durable::domain::graph_commit::{
    CheckedGraph, CheckedNode, EDGE_ROW_LOGICAL_BYTES, blob_row_logical_bytes,
};
use lash_durable::domain::process_archive;
use lash_durable::domain::{ExecKey, OwnerKey, RunSeq};
use lash_durable::{
    CommitLabel, DurableError, DurableInstant, Epoch, StoreFailure, StoreFailureKind,
};
use lash_kernel_value::tokens::DecodeDimension;
use lash_store_sql::durable::graph::GraphStatements;
use lash_store_sql::durable::run_records::RunRecordStatements;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use lash_store_sql::durable::turns::TurnStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, Retired, SqliteDurableStore, integer, node_ref};
use crate::conn::{FencedTx, TxOutcome, cached_execute};

/// The graph tables and the root catalog, created after every table the
/// catalog names.
pub(crate) const TABLES: &str = "
-- One row per child of an installed graph node, by node id and keyed by the
-- parent: its envelope's child table. New nodes take the highest ids, so an
-- install's edges land on the rightmost pages. Neither end can go while an
-- edge names it: a peel deletes a parent's edges before the parent.
CREATE TABLE IF NOT EXISTS blob_edges (
    parent_id INTEGER NOT NULL REFERENCES blobs(id),
    child_id INTEGER NOT NULL REFERENCES blobs(id),
    PRIMARY KEY (parent_id, child_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_blob_edges_child ON blob_edges(child_id);

-- An execution's admitted effect, by its canonical binary identity, to its
-- Admission node, while a live kernel or a released member needs the lookup.
CREATE TABLE IF NOT EXISTS effect_admissions (
    exec_key TEXT NOT NULL,
    effect_id BLOB NOT NULL,
    owner_key TEXT,
    run_seq INTEGER,
    member_ordinal INTEGER,
    admission_ref INTEGER NOT NULL REFERENCES blobs(id),
    written_epoch INTEGER NOT NULL,
    -- Where the effect's outcome was delivered to its in-turn machine: the
    -- execution's journal, which a replay from the cell's start follows.
    delivered_batch INTEGER CONSTRAINT ck_effect_admissions_batch CHECK (delivered_batch >= 0),
    delivered_position INTEGER CONSTRAINT ck_effect_admissions_position
        CHECK (delivered_position >= 0),
    -- A checkpoint of the execution covers the admission: its journal no
    -- longer holds it, and a resume starts from that checkpoint.
    covered INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_effect_admissions_covered
        CHECK (covered IN (0, 1)),
    PRIMARY KEY (exec_key, effect_id),
    UNIQUE (owner_key, run_seq, member_ordinal),
    CONSTRAINT ck_effect_admissions_member CHECK (
        (owner_key IS NULL AND run_seq IS NULL AND member_ordinal IS NULL) OR
        (owner_key IS NOT NULL AND run_seq IS NOT NULL AND member_ordinal IS NOT NULL)),
    CONSTRAINT ck_effect_admissions_delivered CHECK (
        (delivered_batch IS NULL) = (delivered_position IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_effect_admissions_admission_ref
    ON effect_admissions (admission_ref);

-- A root lease (FIG-6191, D-LEASE): an activation's hold on the exact
-- committed root it reads on demand, taken under its actor's fence or, for
-- a reader outside every actor, under none. A root
-- of the catalog until released, or reaped by a retirement step once the
-- store's clock passes its expiry; reads and renewals stop at expiry.
CREATE TABLE IF NOT EXISTS root_leases (
    lease_id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_key BLOB NOT NULL,
    root_ref INTEGER NOT NULL REFERENCES blobs(id),
    actor_key TEXT,
    epoch INTEGER,
    ttl_ms INTEGER NOT NULL CONSTRAINT ck_root_leases_ttl CHECK (ttl_ms > 0),
    expires_at_ms INTEGER NOT NULL,
    -- An actor's lease names its actor and epoch; a reader's names
    -- neither.
    CONSTRAINT ck_root_leases_fence CHECK ((actor_key IS NULL) = (epoch IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_root_leases_root_ref ON root_leases (root_ref);
CREATE INDEX IF NOT EXISTS idx_root_leases_expiry ON root_leases (expires_at_ms);

-- A retirement hold (FIG-6191, D-RET): a root a publication retired or a
-- lease released. A root of the catalog until a step takes it; `closure` 0
-- peels the node alone.
CREATE TABLE IF NOT EXISTS retirement_holds (
    root_ref INTEGER PRIMARY KEY REFERENCES blobs(id),
    closure INTEGER NOT NULL CONSTRAINT ck_retirement_holds_closure CHECK (closure IN (0, 1)),
    queued_at_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_retirement_holds_queue
    ON retirement_holds (queued_at_ms, root_ref);

-- A retirement step's unfinished frontier (FIG-6193): a node under a
-- retired root, or a reaped lease's root, that a bounded step has not peeled
-- yet. Only retirement steps write or take it: no root of the catalog and no
-- foreign key; a node gone meanwhile is passed over. A row is a hint, never
-- an authority: a reused id it names is checked like any other candidate.
CREATE TABLE IF NOT EXISTS retirement_frontier (
    node_ref INTEGER PRIMARY KEY,
    closure INTEGER NOT NULL CONSTRAINT ck_retirement_frontier_closure CHECK (closure IN (0, 1)),
    queued_at_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_retirement_frontier_queue
    ON retirement_frontier (queued_at_ms, node_ref);



-- Permanent lifetime fences. manifest_ref deliberately survives graph deletion.
CREATE TABLE IF NOT EXISTS environment_owners (
    owner_id TEXT PRIMARY KEY,
    version_id TEXT NOT NULL,
    state TEXT NOT NULL CONSTRAINT ck_environment_owners_state CHECK (state IN ('preparing', 'published', 'retired')),
    actor_key TEXT NOT NULL REFERENCES actors(actor_key),
    preparing_epoch INTEGER NOT NULL,
    body_json TEXT,
    manifest_ref BLOB NOT NULL UNIQUE,
    CONSTRAINT ck_environment_owners_body CHECK ((state = 'retired') = (body_json IS NULL))
);
CREATE TABLE IF NOT EXISTS environment_heads (
    session_id TEXT PRIMARY KEY,
    root_ref INTEGER NOT NULL REFERENCES blobs(id),
    written_epoch INTEGER NOT NULL
);

-- Every root of every source: a view of the owning columns, never a cache.
CREATE VIEW IF NOT EXISTS graph_root_catalog AS
    SELECT 'environment_head' AS source, root_ref FROM environment_heads
    UNION ALL SELECT 'exec', root_ref FROM exec_snapshots WHERE root_ref IS NOT NULL
    UNION ALL SELECT 'run_fact', record_ref FROM run_records WHERE record_ref IS NOT NULL
    UNION ALL SELECT 'model_call', record_ref FROM model_call_records WHERE record_ref IS NOT NULL
    UNION ALL SELECT 'tool_call_decision', decision_ref FROM tool_calls WHERE decision_ref IS NOT NULL
    UNION ALL SELECT 'tool_call_answer', body_ref FROM tool_calls WHERE body_ref IS NOT NULL
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
    UNION ALL SELECT 'revision_config', config_ref FROM session_revisions
    UNION ALL SELECT 'answer', answer_ref FROM turn_phases WHERE answer_ref IS NOT NULL
    UNION ALL SELECT 'lease', root_ref FROM root_leases
    UNION ALL SELECT 'retirement', root_ref FROM retirement_holds
    UNION ALL SELECT 'terminal_transcript', terminal_transcript_ref FROM session_runs
        WHERE terminal_transcript_ref IS NOT NULL
    UNION ALL SELECT 'prompt_snapshot', text_ref FROM prompt_snapshot_texts WHERE part = 'snapshot'
    UNION ALL SELECT 'prompt_request', text_ref FROM prompt_snapshot_texts WHERE part = 'request'
    UNION ALL SELECT 'prompt_prefix', root_ref FROM prompt_prefixes;
";

lash_store_sql::statements! {
    lazy
    /// Graph statements only SQLite issues: the artifact bridge's pointer
    /// table and the
    /// `json_each` binding of a node list.
    pub(crate) struct GraphSqliteStatements @ "durable_graph_sqlite" {
        /// Artifact `?2` of namespace `?1`'s blob: SQLite's bridge is its
        /// pointer table.
        artifact_root = "SELECT (SELECT hash FROM blobs WHERE id = blob_ref) AS blob_ref FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2";

        /// The encoding of every stored blob in JSON `?1`.
        stored_encodings = "SELECT hash, encoding FROM blobs
             WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";

        /// A cached configuration's graph root must exist in this snapshot.
        config_root_visible = "SELECT EXISTS (SELECT 1 FROM blobs WHERE hash = ?1 AND encoding = 1)";

        /// The bytes and encoding of every stored blob in JSON `?1`.
        stored_nodes = "SELECT hash, content, encoding FROM blobs
             WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";

        /// Discover all absent staged paths in one snapshot. A stored node
        /// stops descent, so re-supplying a shared closure reads only its
        /// frontier. The keyed child map is accessed only for absent parents.
        install_frontier = "WITH RECURSIVE reached AS (
            SELECT value AS hash FROM json_each(?1)
            UNION
            SELECT child.value FROM reached,
                 json_each(json_extract(?2, '$.' || reached.hash)) AS child
            WHERE NOT EXISTS (SELECT 1 FROM blobs WHERE hash = unhex(reached.hash))
        ) SELECT unhex(reached.hash),
                 CASE WHEN json_type(?2, '$.' || reached.hash) = 'array'
                      THEN stored.content ELSE NULL END,
                 stored.encoding
          FROM reached LEFT JOIN blobs AS stored ON stored.hash = unhex(reached.hash)
          ORDER BY reached.hash";

        /// One bounded frontier of `[hash, read_opaque_body]` requests.
        /// Graph-only reads retain opaque identities without their payloads.
        hydration_nodes = "SELECT stored.hash,
             CASE WHEN json_extract(wanted.value, '$[1]') OR stored.encoding <> 0
                  THEN stored.content ELSE x'' END,
             stored.encoding
             FROM json_each(?1) AS wanted JOIN blobs AS stored
               ON stored.hash = unhex(json_extract(wanted.value, '$[0]'))";

        /// The one-row template for bounded graph-body VALUES batches.
        /// Graph encoding is fixed at 1; only digests and bytes are bound.
        insert_blobs = "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1)
             ON CONFLICT (hash) DO NOTHING";

        /// Checked graph bodies packed as digest followed by canonical bytes
        /// in blob `?1`; JSON blob `?2` holds `[one_based_offset, body_length]`.
        /// Two bindings install the whole changed frontier without a variable
        /// limit or a statement per fixed-size VALUES chunk.
        insert_graph = "INSERT INTO blobs (hash, content, encoding)
             SELECT substr(packed.body, value ->> 0, 32),
                    substr(packed.body, (value ->> 0) + 32, value ->> 1), 1
               FROM (SELECT ?1 AS body) AS packed, json_each(CAST(?2 AS TEXT)) WHERE true
             ON CONFLICT (hash) DO NOTHING";

        /// Every edge of JSON `?1`, an array of `[parent, child]` digests
        /// of freshly installed parents.
        insert_edges = "INSERT INTO blob_edges (parent_id, child_id)
             SELECT parent.id, child.id FROM json_each(?1) AS edge
               JOIN blobs AS parent ON parent.hash = unhex(edge.value ->> 0)
               JOIN blobs AS child ON child.hash = unhex(edge.value ->> 1)";

        /// Discover one bounded changed-path set, stopping at live/shared
        /// boundaries. Outside edges and catalog roots seed protection. Cast
        /// the integer limit so SQLite does not expire this large cached
        /// program when the batch rebinds its limit for execution. The
        /// frontier is JSON node ids `?1` or JSON hex digests `?3`, so a
        /// retirement that starts from digests enters the id-keyed graph
        /// in this statement; a frontier node the limit left unexamined
        /// answers a NULL verdict.
        retirement_candidates = "WITH RECURSIVE frontier AS MATERIALIZED (
             SELECT value AS id FROM json_each(?1)
             UNION
             SELECT id FROM blobs WHERE hash IN (SELECT unhex(value) FROM json_each(?3))
         ), candidates AS (
             SELECT id FROM frontier
             UNION
             SELECT edge.child_id AS id FROM candidates
               JOIN blob_edges AS edge ON edge.parent_id = candidates.id
              WHERE NOT EXISTS (SELECT 1 FROM graph_root_catalog WHERE root_ref = candidates.id)
                AND (candidates.id NOT IN (SELECT id FROM frontier)
                     OR NOT EXISTS (SELECT 1 FROM blob_edges WHERE child_id = candidates.id))
                AND (SELECT count(*) FROM (SELECT 1 FROM blob_edges WHERE child_id = candidates.id LIMIT 2)) <= 1
             LIMIT CAST(?2 AS INTEGER)
         ), protected AS (
             SELECT id FROM candidates
              WHERE EXISTS (SELECT 1 FROM graph_root_catalog WHERE root_ref = candidates.id)
                 OR EXISTS (SELECT 1 FROM blob_edges AS edge WHERE edge.child_id = candidates.id
                     AND edge.parent_id NOT IN (SELECT id FROM candidates))
             UNION
             SELECT edge.child_id AS id FROM protected JOIN blob_edges AS edge ON edge.parent_id = protected.id
              WHERE edge.child_id IN (SELECT id FROM candidates)
         ) SELECT candidates.id, candidates.id NOT IN (SELECT id FROM protected) FROM candidates
           UNION ALL
           SELECT id, NULL FROM frontier WHERE id NOT IN (SELECT id FROM candidates)";

        /// Outgoing edges of the proven unprotected set, removed together.
        retire_edges = "DELETE FROM blob_edges WHERE parent_id IN (SELECT value FROM json_each(?1)) RETURNING child_id";

        /// Delete exactly the set proved under the same writer transaction.
        retire_nodes = "DELETE FROM blobs WHERE id IN (SELECT value FROM json_each(?1)) RETURNING id";

        /// One level of a peel: every node id of JSON `?1` no root names
        /// and no edge reaches.
        peel_orphans = "SELECT candidate.id FROM blobs AS candidate
             WHERE candidate.id IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM graph_root_catalog AS root WHERE root.root_ref = candidate.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM blob_edges AS edge WHERE edge.child_id = candidate.id
               )";
    }
}

/// 64 bound parameters per full statement, below even SQLite's historical
/// 999-parameter limit (the bundled build allows 32,766). The first-cell
/// measurements select 32 rows to amortize statement entry without making
/// small graphs allocate a full graph-sized parameter list.
pub(super) const BLOB_BATCH_ROWS: usize = 32;

struct Sql {
    blob_batches: [String; BLOB_BATCH_ROWS],
    graph: GraphStatements,
    facts: RunRecordStatements,
    snapshots: SnapshotStatements,
    turns: TurnStatements,
    sqlite: GraphSqliteStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    let sqlite = GraphSqliteStatements::render(dialect);
    let blob_batches = std::array::from_fn(|index| {
        let values = (0..=index)
            .map(|row| format!("(?{}, ?{}, 1)", row * 2 + 1, row * 2 + 2))
            .collect::<Vec<_>>()
            .join(", ");
        sqlite
            .insert_blobs
            .sql()
            .replacen("(?1, ?2, 1)", &values, 1)
    });
    Sql {
        blob_batches,
        graph: GraphStatements::render(dialect),
        facts: RunRecordStatements::render(dialect),
        snapshots: SnapshotStatements::render(dialect),
        turns: TurnStatements::render(dialect),
        sqlite,
    }
});

/// The multi-row blob insert for `rows` rows (1 to [`BLOB_BATCH_ROWS`]),
/// `?1, ?2` the first row's digest and bytes.
pub(super) fn blob_batch_sql(rows: usize) -> &'static str {
    &SQL.blob_batches[rows - 1]
}

/// One page of the orphan audit.
const GRAPH_AUDIT: CommitLabel = CommitLabel::new("graph.audit");

/// The orphans one audit page reads.
pub(crate) const AUDIT_PAGE: usize = 256;

/// A statement's answer inside a graph transaction: a value, or a typed
/// refusal that rolls the transaction back.
pub(crate) type GraphAnswer<T> = rusqlite::Result<Result<T, GraphRefusal>>;

fn corrupt(what: &str, value: &str) -> GraphRefusal {
    GraphRefusal::Backend(StoreFailure {
        code: None,
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
            code: None,
            kind: StoreFailureKind::Corrupt,
            message: other.to_string(),
        }),
    }
}

pub(super) fn node_of(stored: &str) -> Result<NodeId, GraphRefusal> {
    NodeId::from_hex(stored).ok_or_else(|| corrupt("node reference", stored))
}

pub(super) fn encoding_of(stored: i64) -> Result<BlobEncoding, GraphRefusal> {
    match stored {
        0 => Ok(BlobEncoding::Opaque),
        1 => Ok(BlobEncoding::Graph),
        other => Err(corrupt("blob encoding", &other.to_string())),
    }
}

/// `owner`'s current root, with the process history archive that names it
/// when its row was packed.
pub(super) fn root_in(
    tx: &Connection,
    owner: &RootOwner,
) -> GraphAnswer<Option<(NodeId, Option<Vec<u8>>)>> {
    fn one(
        tx: &Connection,
        sql: &str,
        params: impl crate::statement::Values,
    ) -> rusqlite::Result<Option<String>> {
        Ok(tx
            .statement(sql)?
            .query_row(params, |row| crate::codec::optional_hash_column(row, 0))
            .optional()?
            .flatten())
    }
    let stored = match owner {
        RootOwner::Exec(exec) => tx
            .statement(SQL.snapshots.root.sql())?
            .query_row([exec.stored()], |row| {
                crate::codec::optional_hash_column(row, 1)
            })
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
        RootOwner::ToolCall { owner, call, part } => {
            super::tool_calls::root(tx, owner, call, *part)?
        }
        RootOwner::ModelCall(key) => one(
            tx,
            super::model_calls::SQL.root.sql(),
            [
                key.session.as_str(),
                &key.call.owner_column(),
                &key.call.call_column(),
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
        RootOwner::ModelAnswer { session, run } => one(
            tx,
            SQL.turns.answer_root.sql(),
            [session.as_str(), run.as_str()],
        )?,
        RootOwner::RunTranscript { session, run } => one(
            tx,
            SQL.turns.transcript_root.sql(),
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
    };
    match stored.as_deref().map(node_of).transpose() {
        Ok(Some(root)) => return Ok(Ok(Some((root, None)))),
        Ok(None) => {}
        Err(refusal) => return Ok(Err(refusal)),
    }
    // A finished process's packed history answers from its archive, read
    // after its row was found gone: packing only removes rows.
    let Some(process) = process_archive::root_process(owner) else {
        return Ok(Ok(None));
    };
    let Some(bytes) = super::process_history::archive_bytes(tx, &process)? else {
        return Ok(Ok(None));
    };
    Ok(archived_root(&bytes, owner).map(|root| root.map(|root| (root, Some(bytes)))))
}

fn archive_refused(error: process_archive::ArchiveCorrupt) -> GraphRefusal {
    from_durable(error.into())
}

fn archived_root(bytes: &[u8], owner: &RootOwner) -> Result<Option<NodeId>, GraphRefusal> {
    let archive = process_archive::ProcessArchive::open(bytes).map_err(archive_refused)?;
    process_archive::archived_root(&archive, owner).map_err(archive_refused)
}

/// The nodes `roots` reach in a process history archive, checked within
/// `limits` in the order a walk of the same stored rows checks them: a
/// graph node's children come from its own child table, and the archive is
/// the whole read, so a node it lacks is missing.
pub(super) fn walk_archive(
    archive: &process_archive::ProcessArchive<'_>,
    roots: &[NodeId],
    limits: &DecodeLimits,
) -> Result<BTreeMap<NodeId, HydratedNode>, GraphRefusal> {
    let mut nodes = BTreeMap::new();
    let mut stack: Vec<_> = roots.iter().rev().copied().collect();
    let mut visited = BTreeSet::new();
    let mut total = 0_u64;
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let node = archive
            .node(&id)
            .map_err(archive_refused)?
            .ok_or(GraphRefusal::MissingNode(id))?;
        let encoding = encoding_of(i64::from(node.encoding))?;
        let children = match encoding {
            BlobEncoding::Opaque => Vec::new(),
            BlobEncoding::Graph => {
                let envelope = check_envelope(node.bytes, id, limits)?;
                // A persistent-heap page is read as a leaf: its header,
                // never what it reaches (FIG-6193).
                if !is_semantic(envelope.kind) {
                    envelope.children.iter().collect()
                } else if roots.contains(&id) {
                    return Err(GraphRefusal::SemanticPage(id));
                } else {
                    Vec::new()
                }
            }
        };
        count(&nodes, &mut total, node.bytes.len(), limits)?;
        stack.extend(children.into_iter().rev());
        nodes.insert(
            id,
            HydratedNode {
                encoding,
                bytes: node.bytes.to_vec().into(),
            },
        );
    }
    Ok(nodes)
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

/// Prove snapshot membership without hydrating an already validated closure.
pub(crate) fn config_root_visible(tx: &Connection, root: NodeId) -> rusqlite::Result<bool> {
    tx.statement(SQL.sqlite.config_root_visible.sql())?
        .query_row([root.0.as_slice()], |row| row.get(0))
}

/// Verify acknowledged references by their prepared bytes, without ever
/// inserting an absent node from those bytes.
pub(super) fn validate_exact_reads(
    tx: &Connection,
    expected: &BTreeMap<
        NodeId,
        std::sync::Arc<lash_core_store::checkpoint_graph::node::VerifiedNode>,
    >,
) -> GraphAnswer<()> {
    if expected.is_empty() {
        return Ok(Ok(()));
    }
    let mut present = BTreeSet::new();
    let mut statement = tx.statement(SQL.sqlite.stored_nodes.sql())?;
    let mut rows = statement.query([digest_array(expected.keys())])?;
    while let Some(row) = rows.next()? {
        let hash = crate::codec::hash_column(row, 0)?;
        let Some(id) = NodeId::from_hex(&hash) else {
            continue;
        };
        let Some(node) = expected.get(&id) else {
            continue;
        };
        match encoding_of(row.get(2)?) {
            Ok(BlobEncoding::Graph) => {}
            Ok(BlobEncoding::Opaque) => return Ok(Err(GraphRefusal::EncodingConflict(id))),
            Err(error) => return Ok(Err(error)),
        }
        if row.get_ref(1)?.as_blob()? != &node.bytes[..] {
            return Ok(Err(GraphRefusal::ContentConflict(id)));
        }
        present.insert(id);
    }
    if let Some(id) = expected.keys().find(|id| !present.contains(id)) {
        return Ok(Err(GraphRefusal::MissingNode(*id)));
    }
    Ok(Ok(()))
}

/// Installs the staged nodes after validating every external reference.
pub(crate) fn install_staged(
    tx: &Connection,
    nodes: &BTreeMap<NodeId, lash_core_store::checkpoint_graph::EncodedNode>,
) -> GraphAnswer<()> {
    let checked =
        match lash_durable::domain::graph_commit::check_graph(nodes, &BTreeSet::new(), &[]) {
            Ok(checked) => checked,
            Err(refusal) => return Ok(Err(refusal)),
        };
    let environment = lash_durable::environment::supplied_versions(&checked);
    if let Err(error) =
        super::environment::validate_graph(tx, &checked, &environment, &BTreeSet::new())?
    {
        return Ok(Err(from_durable(error)));
    }
    Ok(install(tx, &checked)?.map(|_| ()))
}

/// A fixed-size read uses one JSON bind, independent of SQLite's variable
/// limit. Each body statement requests at most this many identities.
const READ_BATCH_ROWS: usize = 128;

struct ReadNode {
    encoding: BlobEncoding,
    bytes: Vec<u8>,
    children: Result<Vec<NodeId>, GraphRefusal>,
}

/// Read-local frontier buffers. Fetching discovers the next frontier, but a
/// node's result is consumed only at its depth-first validation position.
/// In particular, SQL row order cannot choose the first refusal, and a
/// prefetched envelope refusal cannot overtake the visited-node budget.
struct FrontierReader {
    roots: BTreeSet<NodeId>,
    pending: BTreeMap<usize, std::collections::VecDeque<NodeId>>,
    discovered: BTreeSet<NodeId>,
    fetched: BTreeSet<NodeId>,
    bodies: BTreeMap<NodeId, GraphAnswer<ReadNode>>,
    graph_only: bool,
    read_nodes: u64,
    read_bytes: u64,
}

impl FrontierReader {
    fn new(roots: &[NodeId], graph_only: bool) -> Self {
        let roots_set: BTreeSet<_> = roots.iter().copied().collect();
        Self {
            roots: roots_set.clone(),
            pending: BTreeMap::from([(0, roots.iter().copied().collect())]),
            discovered: roots_set,
            fetched: BTreeSet::new(),
            bodies: BTreeMap::new(),
            graph_only,
            read_nodes: 0,
            read_bytes: 0,
        }
    }

    fn take(
        &mut self,
        tx: &Connection,
        id: NodeId,
        depth: usize,
        visited: &BTreeSet<NodeId>,
        limits: &DecodeLimits,
    ) -> GraphAnswer<ReadNode> {
        while !self.fetched.contains(&id) {
            // Complete earlier frontiers before descending, so an uneven
            // forest cannot pay one partial batch per depth per root batch.
            // Read-ahead admits only the remaining decode budget. Once it
            // is spent, read only the node validation needs: the validator
            // still chooses the refusal, in its original traversal order.
            let remaining_nodes = limits.nodes.saturating_sub(self.read_nodes);
            let remaining_bytes = limits.total_bytes.saturating_sub(self.read_bytes);
            let level = if remaining_nodes > 0 && remaining_bytes > 0 {
                self.pending
                    .first_key_value()
                    .map_or(depth, |(&level, _)| level.min(depth))
            } else {
                depth
            };
            let batch_rows = if remaining_bytes == 0 {
                1
            } else {
                usize::try_from(remaining_nodes)
                    .unwrap_or(usize::MAX)
                    .clamp(1, READ_BATCH_ROWS)
            };
            let mut batch = Vec::with_capacity(batch_rows);
            // A shared node can first have been discovered at another depth.
            if level == depth {
                self.fetched.insert(id);
                batch.push(id);
            }
            if let Some(frontier) = self.pending.get_mut(&level) {
                while batch.len() < batch_rows {
                    let Some(next) = frontier.pop_front() else {
                        break;
                    };
                    if !visited.contains(&next) && self.fetched.insert(next) {
                        batch.push(next);
                    }
                }
                if frontier.is_empty() {
                    self.pending.remove(&level);
                }
            }
            if batch.is_empty() {
                continue;
            }
            self.read_nodes = self.read_nodes.saturating_add(batch.len() as u64);
            let wanted: Vec<_> = batch
                .iter()
                .map(|node| (node.to_hex(), !self.graph_only || self.roots.contains(node)))
                .collect();
            let mut statement = tx.statement(SQL.sqlite.hydration_nodes.sql())?;
            let mut rows = statement.query([json_array(&wanted)])?;
            while let Some(row) = rows.next()? {
                let hash = crate::codec::hash_column(row, 0)?;
                let Some(node) = NodeId::from_hex(&hash) else {
                    continue;
                };
                // Conversion failures, invalid encodings and envelope refusals
                // belong to this node, not to whichever row SQL returns first.
                let answer = (|| -> GraphAnswer<ReadNode> {
                    let graph_child = self.graph_only && !self.roots.contains(&node);
                    // Graph-only readers used to inspect a child's encoding
                    // before reading its body. Keep that refusal order too.
                    let child_encoding = if graph_child {
                        match encoding_of(row.get(2)?) {
                            Ok(encoding) => Some(encoding),
                            Err(refusal) => return Ok(Err(refusal)),
                        }
                    } else {
                        None
                    };
                    let bytes = row.get::<_, Vec<u8>>(1)?;
                    let encoding = match child_encoding {
                        Some(encoding) => encoding,
                        None => match encoding_of(row.get(2)?) {
                            Ok(encoding) => encoding,
                            Err(refusal) => return Ok(Err(refusal)),
                        },
                    };
                    let children = match encoding {
                        BlobEncoding::Opaque => Ok(Vec::new()),
                        BlobEncoding::Graph => match check_envelope(&bytes, node, limits) {
                            // A persistent-heap page is read as a leaf: its
                            // header, never what it reaches (FIG-6193).
                            Ok(envelope) if is_semantic(envelope.kind) => {
                                if self.roots.contains(&node) {
                                    Err(GraphRefusal::SemanticPage(node))
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            Ok(envelope) => Ok(envelope.children.iter().collect()),
                            Err(refusal) => Err(refusal.into()),
                        },
                    };
                    Ok(Ok(ReadNode {
                        encoding,
                        bytes,
                        children,
                    }))
                })();
                if let Ok(Ok(body)) = &answer {
                    self.read_bytes = self.read_bytes.saturating_add(body.bytes.len() as u64);
                    if self.graph_only
                        && !self.roots.contains(&node)
                        && body.encoding == BlobEncoding::Opaque
                    {
                        self.read_nodes = self.read_nodes.saturating_sub(1);
                    }
                    if let Ok(children) = &body.children {
                        for child in children.iter().rev() {
                            if self.discovered.insert(*child) {
                                self.pending.entry(level + 1).or_default().push_back(*child);
                            }
                        }
                    }
                }
                self.bodies.insert(node, answer);
            }
        }
        self.bodies
            .remove(&id)
            .unwrap_or(Ok(Err(GraphRefusal::MissingNode(id))))
    }
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
    let mut stack: Vec<_> = roots.iter().rev().map(|id| (*id, 0)).collect();
    let mut reader = FrontierReader::new(roots, skipped.is_some());
    let mut visited = BTreeSet::new();
    let mut total = 0_u64;
    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        #[cfg(test)]
        WALK_COUNTS.with_borrow_mut(|counts| *counts.1.entry(id).or_default() += 1);
        let ReadNode {
            encoding,
            bytes,
            children,
        } = match reader.take(tx, id, depth, &visited, limits)? {
            Ok(body) => body,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if encoding == BlobEncoding::Opaque
            && !roots.contains(&id)
            && let Some(skipped) = skipped.as_deref_mut()
        {
            skipped.insert(id);
            continue;
        }
        if let Err(refusal) = count(&nodes, &mut total, bytes.len(), limits) {
            return Ok(Err(refusal));
        }
        match children {
            Ok(children) => stack.extend(children.into_iter().map(|child| (child, depth + 1))),
            Err(refusal) => return Ok(Err(refusal)),
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
    let (root, archived) = match root_in(tx, &owner)? {
        Ok(Some(root)) => root,
        Ok(None) => return Ok(Err(GraphRefusal::RootRetired)),
        Err(refusal) => return Ok(Err(refusal)),
    };
    if expected.is_some_and(|expected| expected != root) {
        return Ok(Err(GraphRefusal::RevisionConflict));
    }
    // A root only the archive names is walked in the archive: its rows are
    // gone, and with them its blobs.
    if let Some(bytes) = archived {
        let archive = match process_archive::ProcessArchive::open(&bytes) {
            Ok(archive) => archive,
            Err(error) => return Ok(Err(archive_refused(error))),
        };
        return Ok(
            walk_archive(&archive, &[root], limits).map(|nodes| HydratedGraph {
                owner,
                root,
                nodes,
            }),
        );
    }
    Ok(walk(tx, root, limits)?.map(|nodes| HydratedGraph { owner, root, nodes }))
}

/// Peels `roots`, digests the caller's transaction retired: deletes each
/// one nothing names or reaches any more, then each child the deleted
/// nodes' edges reached, level by level, stopping wherever something still
/// names a node. Its work is the newly unreachable nodes and their edges,
/// never a closure something still roots. Answers how many it deleted.
pub(crate) fn peel(tx: &Connection, roots: BTreeSet<String>) -> rusqlite::Result<usize> {
    let (_, mut deleted, mut frontier) = peel_batch(tx, Frontier::Digests(&roots), 4096)?;
    while !frontier.is_empty() {
        let (_, gone, children) = peel_batch(tx, Frontier::Ids(&frontier), 4096)?;
        deleted += gone;
        frontier = children;
    }
    Ok(deleted)
}

/// Where a retirement batch starts: stored node ids, or the hex digests a
/// release answered, resolved inside the batch's first statement.
#[derive(Clone, Copy)]
pub(super) enum Frontier<'a> {
    Ids(&'a BTreeSet<i64>),
    Digests(&'a BTreeSet<String>),
}

/// `values` as one JSON array, the binding a `json_each` statement reads.
pub(super) fn json_array(values: &impl serde::Serialize) -> String {
    #[expect(clippy::expect_used, reason = "strings always encode as JSON")]
    serde_json::to_string(values).expect("a list of digests encodes")
}

#[cfg(test)]
pub(super) fn retirement_sql_for_testing() -> &'static str {
    SQL.sqlite.retirement_candidates.sql()
}

/// Set-based closure retirement over node ids. Candidate hints never
/// authorize deletion: live roots and all outside incoming edges are checked
/// under this writer.
pub(super) fn peel_batch(
    tx: &Connection,
    frontier: Frontier<'_>,
    limit: usize,
) -> rusqlite::Result<(usize, usize, BTreeSet<i64>)> {
    let none = BTreeSet::<i64>::new();
    let (ids, digests) = match frontier {
        Frontier::Ids(ids) if ids.is_empty() => return Ok((0, 0, BTreeSet::new())),
        Frontier::Digests(digests) if digests.is_empty() => return Ok((0, 0, BTreeSet::new())),
        Frontier::Ids(ids) => (json_array(ids), json_array(&none)),
        Frontier::Digests(digests) => (json_array(&none), json_array(digests)),
    };
    let rows = tx
        .statement(SQL.sqlite.retirement_candidates.sql())?
        .query_map(
            rusqlite::params![
                ids,
                i64::try_from(limit.max(1)).unwrap_or(i64::MAX),
                digests
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<bool>>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut examined = BTreeSet::new();
    let mut gone = BTreeSet::new();
    let mut next = BTreeSet::new();
    for (id, verdict) in rows {
        match verdict {
            Some(unprotected) => {
                examined.insert(id);
                if unprotected {
                    gone.insert(id);
                }
            }
            None => {
                next.insert(id);
            }
        }
    }
    if gone.is_empty() {
        return Ok((examined.len(), 0, next));
    }
    let (deleted, children) = delete_nodes(tx, &gone)?;
    next.extend(children.difference(&examined));
    Ok((examined.len(), deleted, next))
}

/// Deletes the proven set `gone` with its outgoing edges, answering how
/// many nodes went and the children those edges reached.
fn delete_nodes(tx: &Connection, gone: &BTreeSet<i64>) -> rusqlite::Result<(usize, BTreeSet<i64>)> {
    super::environment::peeled(tx, gone)?;
    let batch = json_array(gone);
    let children = tx
        .statement(SQL.sqlite.retire_edges.sql())?
        .query_map([&batch], |row| row.get(0))?
        .collect::<rusqlite::Result<BTreeSet<i64>>>()?;
    let deleted = tx
        .statement(SQL.sqlite.retire_nodes.sql())?
        .query_map([&batch], |_| Ok(()))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .len();
    Ok((deleted, children))
}

/// One level of [`peel`]: deletes every node of `frontier` nothing names or
/// reaches, and answers how many with the children their edges reached.
pub(super) fn peel_level(
    tx: &Connection,
    frontier: &BTreeSet<i64>,
) -> rusqlite::Result<(usize, BTreeSet<i64>)> {
    // The level's deletable set is read before any of its edges goes. A
    // frontier may contain both a parent and its child: the child becomes
    // deletable only after the parent's edge goes, on the next level.
    let gone = tx
        .statement(SQL.sqlite.peel_orphans.sql())?
        .query_map([json_array(frontier)], |row| row.get(0))?
        .collect::<rusqlite::Result<BTreeSet<i64>>>()?;
    if gone.is_empty() {
        return Ok((0, BTreeSet::new()));
    }
    delete_nodes(tx, &gone)
}

/// Moves what `retired` recorded to retirement holds, before its
/// transaction commits (D-RET): each retired root's closure, and each ended
/// turn's PhaseRoot alone. Nothing is peeled here; a retirement step does
/// that later in bounded transactions.
pub(super) fn hold_retired(
    tx: &Connection,
    retired: Retired,
    now: DurableInstant,
) -> rusqlite::Result<()> {
    super::pages::hold_retired(tx, retired, now)
}

/// Holds the closure of each stored root of `roots`, the roots the
/// caller's transaction retired outside an owner commit (a session's
/// delete, a released revision), queued at `now`. Nothing is peeled here:
/// however large the closures, the transaction writes one hold row per
/// root, and retirement steps delete what nothing else reaches later, in
/// bounded transactions. Answers how many stored roots it held.
pub(crate) fn retire_roots(
    tx: &Connection,
    roots: &BTreeSet<String>,
    now: DurableInstant,
) -> rusqlite::Result<usize> {
    super::pages::hold(tx, roots, true, now)
}

/// How many roots the catalog names and how many blobs are stored.
pub(crate) fn graph_counts(tx: &Connection) -> rusqlite::Result<(usize, usize)> {
    let count = |sql: &str| -> rusqlite::Result<usize> {
        let count: i64 = tx.statement(sql)?.query_row([], |row| row.get(0))?;
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
        .statement(SQL.graph.orphans_after.sql())?
        .query_map(
            rusqlite::params![
                if after.is_empty() {
                    Vec::new()
                } else {
                    lash_core_store::checkpoint_graph::NodeId::from_hex(after)
                        .ok_or_else(|| {
                            crate::sqlite_conversion_error(crate::stored_data_corrupt(
                                "content address cursor",
                                after,
                            ))
                        })?
                        .0
                        .to_vec()
                },
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| crate::codec::hash_column(row, 0),
        )?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let last = orphans.last().cloned();
    Ok((last, peel(tx, orphans)?))
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
                .statement(SQL.graph.admission_put.sql())?
                .query_row(
                    rusqlite::params![
                        exec.stored(),
                        &effect.0[..],
                        member
                            .as_ref()
                            .map(|member| member.owner.stored())
                            .filter(|owner| *owner != exec.stored()),
                        member
                            .as_ref()
                            .map(|member| integer::<i64>(member.run.0))
                            .transpose()?,
                        member
                            .as_ref()
                            .map(|member| integer::<i64>(member.member))
                            .transpose()?,
                        admission.0.as_slice(),
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
                .statement(SQL.graph.admission_remove.sql())?
                .query_row(
                    rusqlite::params![exec.stored(), &effect.0[..], expected.0.as_slice()],
                    |_| Ok(()),
                )
                .optional()?;
            if removed.is_none() {
                return graph_refused(GraphRefusal::RevisionConflict);
            }
            commit.retired.closure(expected);
        }
        EffectAdmissionWrite::Deliver {
            exec,
            effect,
            delivery,
        } => {
            let recorded = tx
                .prepare_cached(SQL.graph.admission_deliver.sql())?
                .query_row(
                    rusqlite::params![
                        exec.stored(),
                        &effect.0[..],
                        integer::<i64>(delivery.batch)?,
                        i64::from(delivery.position)
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if recorded.is_none() {
                return graph_refused(GraphRefusal::RevisionConflict);
            }
        }
        EffectAdmissionWrite::Cover { exec } => {
            tx.prepare_cached(SQL.graph.admissions_cover.sql())?
                .execute([exec.stored()])?;
        }
    }
    Ok(Ok(()))
}

/// Every admission of execution `exec`, by effect identity, with where its
/// outcome was delivered.
pub(super) fn admissions(tx: &Connection, exec: &ExecKey) -> Answer<Vec<EffectAdmissionRow>> {
    let rows = tx
        .prepare_cached(SQL.graph.admissions.sql())?
        .query_map([exec.stored()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                crate::codec::hash_column(row, 4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, i64>(8)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (effect, owner, run, member, admission, epoch, batch, position, covered) in rows {
        let admission = match node_ref(&admission) {
            Ok(admission) => admission,
            Err(error) => return Ok(Err(error)),
        };
        let member = match (owner, run, member) {
            (owner, Some(run), Some(member)) => {
                let owner = match owner {
                    Some(owner) => match OwnerKey::parse(&owner) {
                        Ok(owner) => owner,
                        Err(error) => {
                            return Ok(Err(
                                corrupt("effect admission owner", &error.to_string()).into()
                            ));
                        }
                    },
                    None => exec.owner(),
                };
                Some(AdmissionMember {
                    owner,
                    run: RunSeq(integer(run)?),
                    member: integer(member)?,
                })
            }
            _ => None,
        };
        let delivered = match (batch, position) {
            (Some(batch), Some(position)) => Some(Delivery {
                batch: integer(batch)?,
                position: integer(position)?,
            }),
            _ => None,
        };
        out.push(EffectAdmissionRow {
            exec: exec.clone(),
            effect: EffectKey(effect.into()),
            member,
            admission,
            written_epoch: Epoch(epoch),
            delivered,
            covered,
        });
    }
    Ok(Ok(out))
}

/// Drop every admission of the execution stored as `exec_key`, answering
/// the Admission nodes they held: the caller's to peel.
pub(super) fn clear_admissions(tx: &Connection, exec_key: &str) -> rusqlite::Result<Vec<String>> {
    tx.statement(SQL.graph.admissions_clear.sql())?
        .query_map([exec_key], |row| crate::codec::hash_column(row, 0))?
        .collect()
}

/// Drop every admission whose execution key starts with `prefix`,
/// answering the Admission nodes they held: the caller's to peel.
pub(super) fn clear_admissions_under(
    tx: &Connection,
    prefix: &str,
) -> rusqlite::Result<Vec<String>> {
    tx.statement(SQL.graph.admissions_clear_under.sql())?
        .query_map([prefix], |row| crate::codec::hash_column(row, 0))?
        .collect()
}

/// Installs a checked graph inside the caller's fenced transaction: every
/// node it references without supplying must be stored (a published root
/// as a graph node), then the supplied blobs are read from the supplied
/// DAG's roots down, each stored one compared and what the commit supplies
/// only beneath stored ones left as stored; the rest are inserted in digest
/// order, then their edges once every blob exists.
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
    let named: BTreeSet<&NodeId> = graph
        .external
        .iter()
        .chain(graph.external_roots.iter())
        .collect();
    let mut present = BTreeMap::new();
    let mut installed = InstalledGraphMeasurement::default();
    if graph.nodes.is_empty() {
        if !named.is_empty() {
            let mut statement = tx.statement(SQL.sqlite.stored_encodings.sql())?;
            let rows = statement.query_map([digest_array(named)], |row| {
                Ok((crate::codec::node_column(row, 0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (id, encoding) = row?;
                present.insert(id, answer!(Ok::<_, rusqlite::Error>(encoding_of(encoding))));
            }
        }
        answer!(Ok::<_, rusqlite::Error>(check_external(graph, &present)));
        return Ok(Ok(installed));
    }
    // The staged nodes are read from the roots of the supplied DAG down by
    // one recursive statement, each stored row compared as it streams. A
    // stored node's children are stored (an edge keeps its child), so what
    // the commit supplies only beneath stored nodes is neither read nor
    // inserted again: validation is bounded by the changed frontier, not
    // the size of the graph it shares (FIG-7366). A writer holds the
    // database, so what is not stored now is inserted.
    // check_graph already supplies digest order. Reuse its slice rather than
    // rebuilding a tree of the same nodes; each slot records what was read.
    let staged = &graph.nodes;
    let slot = |id: &NodeId| staged.binary_search_by(|node| node.id.cmp(id)).ok();
    let mut state = vec![Staged::Unread; staged.len()];
    let mut followed = vec![false; staged.len()];
    for node in staged {
        for child in &node.children {
            if let Some(index) = slot(child) {
                followed[index] = true;
            }
        }
    }
    let roots = digest_array(
        staged
            .iter()
            .enumerate()
            .filter(|(index, _)| !followed[*index])
            .map(|(_, node)| &node.id)
            .chain(named.iter().copied()),
    );
    let mut absent = 0;
    let mut staged_refusal = None;
    let children = staged_children(staged);
    let mut statement = tx.statement(SQL.sqlite.install_frontier.sql())?;
    let _bindings = crate::observed_sql::text_bindings(
        SQL.sqlite.install_frontier.sql(),
        &[&roots, &children, &children],
    );
    let mut rows = statement.query(rusqlite::params![roots, children])?;
    while let Some(row) = rows.next()? {
        let id = crate::codec::node_column(row, 0)?;
        let encoding = row.get::<_, Option<i64>>(2)?;
        if named.contains(&id)
            && let Some(encoding) = encoding
        {
            present.insert(id, answer!(Ok::<_, rusqlite::Error>(encoding_of(encoding))));
        }
        let Some(index) = slot(&id) else { continue };
        let node = &staged[index];
        match encoding {
            None => {
                state[index] = Staged::Absent;
                absent += 1;
            }
            Some(encoding) => {
                let checked = encoding_of(encoding).and_then(|encoding| {
                    if encoding != BlobEncoding::Graph {
                        Err(GraphRefusal::EncodingConflict(node.id))
                    } else {
                        Ok(())
                    }
                });
                let refusal = match checked {
                    Err(refusal) => Some(refusal),
                    Ok(()) if row.get_ref(1)?.as_blob()? != &node.bytes[..] => {
                        Some(GraphRefusal::ContentConflict(node.id))
                    }
                    Ok(()) => None,
                };
                if staged_refusal.is_none() {
                    staged_refusal = refusal;
                }
                state[index] = Staged::Present;
            }
        }
    }
    drop(rows);
    drop(statement);
    answer!(Ok::<_, rusqlite::Error>(check_external(graph, &present)));
    if let Some(refusal) = staged_refusal {
        return Ok(Err(refusal));
    }
    if absent == 0 {
        return Ok(Ok(installed));
    }
    let mut bodies = Vec::new();
    let mut offsets = Vec::with_capacity(absent);
    let mut edges = String::from("[");
    for (node, state) in staged.iter().zip(state) {
        if state != Staged::Absent {
            continue;
        }
        offsets.push((bodies.len() + 1, node.bytes.len()));
        bodies.extend_from_slice(&node.id.0);
        bodies.extend_from_slice(&node.bytes);
        if let Some(window) = lash_core_ids::operational_metrics::sql::Window::current()
            .filter(|w| w.turn_context().is_some())
            && let Ok(envelope) =
                lash_core_store::checkpoint_graph::parse_envelope(&node.bytes, Some(node.id))
        {
            let kind = envelope.kind.spec().map_or("unknown", |s| s.name);
            lash_core_ids::operational_metrics::turn::installed_kind(
                Some(&window),
                kind,
                node.bytes.len() as u64,
            );
        }
        installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
        installed.new_nodes += 1;
        installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        for child in &node.children {
            if edges.len() > 1 {
                edges.push(',');
            }
            edges.push_str("[\"");
            node.id.write_hex(&mut edges);
            edges.push_str("\",\"");
            child.write_hex(&mut edges);
            edges.push_str("\"]");
            installed.new_edges += 1;
            installed.sql_logical_bytes += EDGE_ROW_LOGICAL_BYTES;
        }
    }
    let offsets = serde_json::to_vec(&offsets)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    crate::observed_sql::with_blob_bindings(
        SQL.sqlite.insert_graph.sql(),
        [bodies.len(), offsets.len()],
        || {
            cached_execute(
                tx,
                SQL.sqlite.insert_graph.sql(),
                rusqlite::params![bodies, offsets],
            )
        },
    )?;
    if edges.len() > 1 {
        edges.push(']');
        let _bindings =
            crate::observed_sql::text_bindings(SQL.sqlite.insert_edges.sql(), &[&edges]);
        cached_execute(tx, SQL.sqlite.insert_edges.sql(), [edges])?;
    }
    if let Some(window) = lash_core_ids::operational_metrics::sql::Window::current() {
        window.installed_graph(installed.newly_installed_canonical_bytes);
    }
    Ok(Ok(installed))
}

/// What an install knows of one staged node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Staged {
    /// Beneath stored nodes only, or not reached yet.
    Unread,
    /// Read and not stored: inserted with its edges.
    Absent,
    /// Stored, and compared byte for byte.
    Present,
}

/// `ids` as one JSON array of digests, the binding a `json_each`
/// statement reads.
pub(super) fn digest_array<'a>(ids: impl IntoIterator<Item = &'a NodeId>) -> String {
    let mut array = String::new();
    append_digests(&mut array, ids);
    array
}

fn append_digests<'a>(array: &mut String, ids: impl IntoIterator<Item = &'a NodeId>) {
    array.push('[');
    for (index, id) in ids.into_iter().enumerate() {
        if index != 0 {
            array.push(',');
        }
        array.push('"');
        id.write_hex(array);
        array.push('"');
    }
    array.push(']');
}

fn staged_children(nodes: &[CheckedNode]) -> String {
    let mut children = String::from("{");
    for (index, node) in nodes.iter().enumerate() {
        if index != 0 {
            children.push(',');
        }
        children.push('"');
        node.id.write_hex(&mut children);
        children.push_str("\":");
        append_digests(&mut children, &node.children);
    }
    children.push('}');
    children
}

fn check_external(
    graph: &CheckedGraph,
    present: &BTreeMap<NodeId, BlobEncoding>,
) -> Result<(), GraphRefusal> {
    for id in &graph.external {
        if !present.contains_key(id) {
            return Err(GraphRefusal::MissingNode(*id));
        }
    }
    for root in &graph.external_roots {
        match present.get(root) {
            None => return Err(GraphRefusal::MissingNode(*root)),
            Some(BlobEncoding::Opaque) => return Err(GraphRefusal::EncodingConflict(*root)),
            Some(BlobEncoding::Graph) => {}
        }
    }
    Ok(())
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

#[cfg(test)]
#[path = "graph_frontier_tests.rs"]
mod tests;
