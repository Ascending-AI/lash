//! PostgreSQL: a finished process's history archive (FIG-7701). The pack
//! moves a process's run facts, tool calls and model calls into one archive
//! on its row, and the reads answer from it once the rows are gone.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{DecodeLimits, GraphRefusal, NodeId};
use lash_core_store::model_calls::ModelCallRecord;
use lash_durable::domain::process_archive::{
    self as archive, ArchiveContents, ArchivedCall, ArchivedFact, ArchivedModelCall, ArchivedNode,
    ArchivedRecord, ModelRead, PROCESS_ARCHIVE_CAP_BYTES, PROCESS_ARCHIVE_MAX_NODES,
    PROCESS_ARCHIVE_MAX_ROWS, ProcessArchive,
};
use lash_durable::domain::{CallOwner, OwnerKey, RunFacts, ToolCallSelection, ToolCalls};
use lash_durable::{ActorKey, CommitLabel, DurableError};
use lash_store_sql::{Dialect, durable::process_history::ProcessHistoryStatements};
use sqlx::{PgConnection, Row as _};

use super::{corrupt, sqlx_failure};

pub(super) static SQL: LazyLock<ProcessHistoryStatements> =
    LazyLock::new(|| ProcessHistoryStatements::render(Dialect::postgres()));
lash_store_sql::statements! {
    lazy
    struct ProcessHistoryPostgresStatements @ "durable_process_history_postgres" {
        /// Actor `?1`'s state, its row locked: the actor before the process.
        lock_actor = "SELECT state FROM actors WHERE actor_key = ?1 FOR UPDATE";

        /// Process `?1`, locked, when it is finished and its history undecided.
        lock_process = "SELECT 1 FROM processes
             WHERE process_id = ?1 AND terminal_event_sequence IS NOT NULL AND history IS NULL
             FOR UPDATE";

        /// Up to `?3` blobs of the closure owner `?1`'s rows and session
        /// `?2`'s model calls reach along every edge: each blob's id, size
        /// and edge count, before any content is read.
        closure = "WITH RECURSIVE roots AS (
                 SELECT record_ref AS id FROM run_records WHERE owner_key = ?1
                 UNION SELECT decision_ref FROM tool_calls WHERE owner_key = ?1 AND decision_ref IS NOT NULL
                 UNION SELECT body_ref FROM tool_calls WHERE owner_key = ?1 AND body_ref IS NOT NULL
                 UNION SELECT record_ref FROM model_call_records WHERE session_id = ?2 AND record_ref IS NOT NULL
             ), reach AS (
                 SELECT id FROM roots
                 UNION
                 SELECT edge.child_id FROM blob_edges AS edge JOIN reach ON edge.parent_id = reach.id
             )
             SELECT bounded.id, octet_length(blobs.content)::BIGINT,
                    (SELECT count(*) FROM blob_edges WHERE parent_id = bounded.id)
             FROM (SELECT id FROM reach LIMIT ?3) AS bounded JOIN blobs ON blobs.id = bounded.id";

        /// The blobs with ids `?1`.
        nodes = "SELECT id, hash, encoding, content FROM blobs WHERE id = ANY(?1)";

        /// Every edge from the blobs with ids `?1`, to its child's address.
        edges = "SELECT edge.parent_id, child.hash FROM blob_edges AS edge
             JOIN blobs AS child ON child.id = edge.child_id
             WHERE edge.parent_id = ANY(?1)";

        /// Restore archived nodes `?1` (raw digests) with bytes `?2` and
        /// encodings `?3` as live blobs, in digest order, unless one is
        /// stored under its address; answers the digests it restored.
        restore_nodes = "INSERT INTO blobs (hash, content, encoding)
             SELECT staged.digest, staged.content, staged.encoding
             FROM UNNEST(?1::bytea[], ?2::bytea[], ?3::smallint[]) AS staged(digest, content, encoding)
             ORDER BY staged.digest
             ON CONFLICT (hash) DO NOTHING
             RETURNING hash";

        /// The stored blobs of `?1` (raw digests), key-share locked in digest
        /// order: a live node a restore keeps is held against a peel until
        /// its commit ends.
        lock_live = "SELECT hash FROM blobs WHERE hash = ANY(?1::bytea[]) ORDER BY hash FOR KEY SHARE";

        /// Restore every stored edge of parents `?1` to children `?2` (raw
        /// digests), all live.
        restore_edges = "INSERT INTO blob_edges (parent_id, child_id)
             SELECT parent.id, child.id
             FROM UNNEST(?1::bytea[], ?2::bytea[]) AS staged(parent, child)
             JOIN blobs AS parent ON parent.hash = staged.parent
             JOIN blobs AS child ON child.hash = staged.child
             ORDER BY 1, 2
             ON CONFLICT (parent_id, child_id) DO NOTHING";
    }
}
static POSTGRES: LazyLock<ProcessHistoryPostgresStatements> =
    LazyLock::new(|| ProcessHistoryPostgresStatements::render(Dialect::postgres()));

const PACK: CommitLabel = CommitLabel::new("process.history_pack");

/// Process `process`'s history archive, when its history is packed.
pub(super) async fn archive_bytes(
    connection: &mut PgConnection,
    process: &str,
) -> Result<Option<Vec<u8>>, sqlx::Error> {
    crate::observed_sql::query_scalar(SQL.archive.sql())
        .bind(process)
        .fetch_optional(crate::observed_sql::executor(connection))
        .await
}

async fn archive_of(
    connection: &mut PgConnection,
    process: &str,
) -> Result<Option<Vec<u8>>, DurableError> {
    archive_bytes(connection, process)
        .await
        .map_err(sqlx_failure)
}

/// `owner`'s run facts from its process's archive, when its rows are
/// packed: the same rows, order, `max_rows` refusal and graph walk.
pub(super) async fn run_facts(
    connection: &mut PgConnection,
    owner: &OwnerKey,
    run: Option<lash_durable::domain::RunSeq>,
    max_rows: Option<usize>,
    limits: &DecodeLimits,
) -> Result<Option<RunFacts>, DurableError> {
    let Some(process) = archive::fact_process(owner) else {
        return Ok(None);
    };
    let Some(bytes) = archive_of(connection, process.as_str()).await? else {
        return Ok(None);
    };
    let archive = ProcessArchive::open(&bytes)?;
    let run = run
        .map(|run| i64::try_from(run.0))
        .transpose()
        .map_err(|_| corrupt("integer", "run out of range"))?;
    let facts = archive.facts(run, max_rows.map(|max| max.saturating_add(1)))?;
    if let Some(max_rows) = max_rows.filter(|limit| facts.len() > *limit) {
        return Err(DurableError::Graph(
            lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                dimension: lash_kernel_value::tokens::DecodeDimension::Nodes,
                actual: facts.len() as u64,
                limit: max_rows as u64,
            },
        ));
    }
    let rows = facts
        .into_iter()
        .map(|fact| archive::fact_row(owner, fact))
        .collect::<Result<Vec<_>, _>>()?;
    let roots: Vec<_> = rows.iter().map(|row| row.record).collect();
    let nodes = super::graph::walk_archive(
        &archive,
        &roots,
        limits,
        super::graph::Ingest::EncodingFirst,
    )
    .await
    .map_err(DurableError::Graph)?;
    Ok(Some(RunFacts {
        rows,
        nodes,
        read_limits: *limits,
    }))
}

/// `owner`'s tool calls `selection` reads, from its process's archive.
pub(super) async fn tool_calls(
    connection: &mut PgConnection,
    owner: &CallOwner,
    selection: &ToolCallSelection,
    limits: &DecodeLimits,
) -> Result<Option<ToolCalls>, DurableError> {
    let Some(process) = archive::call_process(owner) else {
        return Ok(None);
    };
    let Some(bytes) = archive_of(connection, process.as_str()).await? else {
        return Ok(None);
    };
    let archive = ProcessArchive::open(&bytes)?;
    let rows = archive::select_calls(&archive, owner, selection)?
        .into_iter()
        .map(|call| archive::call_row(owner, call))
        .collect::<Result<Vec<_>, _>>()?;
    let roots: Vec<NodeId> = rows
        .iter()
        .filter_map(|row| {
            row.answer
                .as_ref()
                .and_then(lash_durable::domain::ToolCallAnswer::body)
                .copied()
        })
        .collect();
    let nodes = super::graph::walk_archive(
        &archive,
        &roots,
        limits,
        super::graph::Ingest::EncodingFirst,
    )
    .await
    .map_err(DurableError::from)?;
    Ok(Some(ToolCalls {
        rows,
        graph: RunFacts {
            nodes,
            read_limits: *limits,
            ..Default::default()
        },
    }))
}

/// An archive's bytes with the model-call rows a read selected from it.
type Selected<Row> = (Vec<u8>, Vec<Row>);

/// Session `session`'s model-call rows `read` selects, from its process's
/// archive. A read built here uses PostgreSQL's `LIKE`, which keeps case.
pub(super) async fn model_calls<R: ModelRead + Sync>(
    connection: &mut PgConnection,
    session: &lash_kernel_value::identity::SessionId,
    read: &R,
) -> Result<Option<Selected<R::Row>>, DurableError> {
    let Some(process) = archive::model_process(session) else {
        return Ok(None);
    };
    let Some(bytes) = archive_of(connection, process).await? else {
        return Ok(None);
    };
    let models = ProcessArchive::open(&bytes)?.models(read)?;
    Ok(Some((bytes, models)))
}

/// The sealed records `models` from archive `bytes`, decoded as a read of
/// the same rows decodes them.
pub(super) async fn model_records(
    bytes: &[u8],
    models: Vec<ArchivedRecord>,
    limits: DecodeLimits,
) -> Result<Vec<ModelCallRecord>, DurableError> {
    let archive = ProcessArchive::open(bytes)?;
    let rows = models
        .into_iter()
        .map(archive::record_parts)
        .collect::<Result<Vec<_>, _>>()?;
    let roots: Vec<NodeId> = rows.iter().map(|(_, root, _)| *root).collect();
    let nodes = super::graph::walk_archive(
        &archive,
        &roots,
        &limits,
        super::graph::Ingest::EncodingFirst,
    )
    .await
    .map_err(DurableError::from)?;
    super::model_calls::decode_records(rows, nodes, limits)
}

/// Make root `root` of packed history archive `bytes` live again, in the
/// caller's transaction, before a lease on it is issued: every node its
/// closure reaches along stored edges that no live blob holds is restored
/// with its exact bytes, encoding and edges. A node stored live already
/// keeps its stored copy, key-share locked until the commit so no peel
/// removes it first, and the closure under it is live with it. The lease
/// then holds what it restored as it holds any live root, and its release
/// or reaping retires it as it retires any other.
pub(super) async fn restore(
    tx: &mut PgConnection,
    bytes: &[u8],
    root: NodeId,
) -> Result<(), GraphRefusal> {
    let refused = |error: archive::ArchiveCorrupt| super::graph::from_durable(error.into());
    let archive = ProcessArchive::open(bytes).map_err(refused)?;
    let mut seen = BTreeSet::from([root]);
    let mut level = vec![root];
    let mut edges: Vec<(NodeId, NodeId)> = Vec::new();
    while !level.is_empty() {
        let mut nodes = Vec::with_capacity(level.len());
        for id in &level {
            nodes.push(
                archive
                    .node(id)
                    .map_err(refused)?
                    .ok_or(GraphRefusal::MissingNode(*id))?,
            );
        }
        let digests: Vec<&[u8]> = nodes.iter().map(|node| &node.id.0[..]).collect();
        let contents: Vec<&[u8]> = nodes.iter().map(|node| node.bytes).collect();
        let encodings: Vec<i16> = nodes.iter().map(|node| i16::from(node.encoding)).collect();
        let restored: BTreeSet<Vec<u8>> =
            crate::observed_sql::query_scalar(POSTGRES.restore_nodes.sql())
                .bind(&digests)
                .bind(&contents)
                .bind(&encodings)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(super::graph::db)?
                .into_iter()
                .collect();
        let live: Vec<&[u8]> = digests
            .iter()
            .copied()
            .filter(|digest| !restored.contains(*digest))
            .collect();
        let locked: BTreeSet<Vec<u8>> = if live.is_empty() {
            BTreeSet::new()
        } else {
            crate::observed_sql::query_scalar(POSTGRES.lock_live.sql())
                .bind(&live)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(super::graph::db)?
                .into_iter()
                .collect()
        };
        let mut next = Vec::new();
        for node in &nodes {
            if restored.contains(&node.id.0[..]) {
                for child in archive.children(node).map_err(refused)? {
                    edges.push((node.id, child.id));
                    if seen.insert(child.id) {
                        next.push(child.id);
                    }
                }
            } else if !locked.contains(&node.id.0[..]) {
                // Peeled between the insert and the lock: restore it again.
                next.push(node.id);
            }
        }
        level = next;
    }
    for chunk in edges.chunks(1024) {
        let parents: Vec<&[u8]> = chunk.iter().map(|(parent, _)| &parent.0[..]).collect();
        let children: Vec<&[u8]> = chunk.iter().map(|(_, child)| &child.0[..]).collect();
        crate::observed_sql::query(POSTGRES.restore_edges.sql())
            .bind(&parents)
            .bind(&children)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(super::graph::db)?;
    }
    Ok(())
}

/// What one pack did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Packed {
    /// The history moved into an archive of this many bytes.
    Archived(usize),
    /// The history keeps its rows: over the cap, or unsupported.
    Kept,
    /// Nothing to decide: not finished, its actor not ended, already
    /// decided, or pruned.
    Skipped,
}

/// Pack `process`'s history in `tx`: after its actor ended, under the
/// actor's lock then the process's, with every check made before any
/// write. A history is packed only when its actor is terminal and every
/// call has its outcome and every model call its record, so nothing can
/// write it again; otherwise, over the cap or unsupported, it keeps its
/// rows, and is not retried. The rows' text is sized and counted before any
/// row is read and the closure is sized before its content is read, so the
/// work stays within the cap.
pub(super) async fn pack(
    tx: &mut PgConnection,
    process: &lash_vocabulary::identity::ProcessId,
    limits: &DecodeLimits,
) -> Result<Packed, DurableError> {
    let actor =
        ActorKey::process(process.as_str()).map_err(|_| corrupt("process id", process.as_str()))?;
    let state: Option<String> = crate::observed_sql::query_scalar(POSTGRES.lock_actor.sql())
        .bind(actor.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if state.as_deref().is_some_and(|state| state != "terminal") {
        return Ok(Packed::Skipped);
    }
    let undecided: Option<i32> = crate::observed_sql::query_scalar(POSTGRES.lock_process.sql())
        .bind(process.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if undecided.is_none() {
        return Ok(Packed::Skipped);
    }
    // No actor ever ended this process, so nothing shows its history is
    // finished: it keeps its rows.
    let packable = if state.is_some() {
        contents(tx, process, limits).await?
    } else {
        None
    };
    match packable {
        Some((contents, bytes)) => {
            write(tx, process, &contents, &bytes).await?;
            Ok(Packed::Archived(bytes.len()))
        }
        None => {
            crate::observed_sql::query(SQL.keep_rows.sql())
                .bind(process.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(Packed::Kept)
        }
    }
}

/// `process`'s archive and what it holds, or `None` when its history keeps
/// its rows.
async fn contents(
    tx: &mut PgConnection,
    process: &lash_vocabulary::identity::ProcessId,
    limits: &DecodeLimits,
) -> Result<Option<(ArchiveContents, Vec<u8>)>, DurableError> {
    let owner = OwnerKey::Process(process.clone()).stored();
    let session = archive::model_session(process);
    let foreign: Option<i32> = crate::observed_sql::query_scalar(SQL.foreign_calls.sql())
        .bind(&owner)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if foreign.is_some() {
        return Ok(None);
    }
    let bound = i64::try_from(PROCESS_ARCHIVE_MAX_ROWS + 1).unwrap_or(i64::MAX);
    let (unsettled, payload): (i32, i64) = crate::observed_sql::query_as(SQL.preflight.sql())
        .bind(&owner)
        .bind(session.as_str())
        .bind(bound)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if unsettled != 0
        || usize::try_from(payload).map_or(true, |payload| payload > PROCESS_ARCHIVE_CAP_BYTES)
    {
        return Ok(None);
    }
    let Some(mut contents) = rows(tx, &owner, session.as_str(), bound).await? else {
        return Ok(None);
    };
    let sized: Vec<(i64, i64, i64)> = crate::observed_sql::query_as(POSTGRES.closure.sql())
        .bind(&owner)
        .bind(session.as_str())
        .bind(i64::try_from(PROCESS_ARCHIVE_MAX_NODES + 1).unwrap_or(i64::MAX))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if sized.len() > PROCESS_ARCHIVE_MAX_NODES {
        return Ok(None);
    }
    let content: i64 = sized.iter().map(|(_, bytes, _)| *bytes).sum();
    let edges: i64 = sized.iter().map(|(_, _, edges)| *edges).sum();
    let size = contents.size_with(
        sized.len(),
        usize::try_from(content).unwrap_or(usize::MAX),
        usize::try_from(edges).unwrap_or(usize::MAX),
    );
    if size > PROCESS_ARCHIVE_CAP_BYTES {
        return Ok(None);
    }
    let ids: Vec<i64> = sized.iter().map(|(id, ..)| *id).collect();
    let mut nodes: BTreeMap<i64, ArchivedNode> = BTreeMap::new();
    for row in crate::observed_sql::query(POSTGRES.nodes.sql())
        .bind(&ids)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    {
        let id: i64 = row.try_get(0).map_err(sqlx_failure)?;
        let Some(address) = address(row.try_get(1).map_err(sqlx_failure)?) else {
            return Ok(None);
        };
        let encoding: i16 = row.try_get(2).map_err(sqlx_failure)?;
        nodes.insert(
            id,
            ArchivedNode {
                id: address,
                encoding: u8::try_from(encoding).unwrap_or(u8::MAX),
                bytes: row.try_get(3).map_err(sqlx_failure)?,
                children: Vec::new(),
            },
        );
    }
    let edge_rows: Vec<(i64, Vec<u8>)> = crate::observed_sql::query_as(POSTGRES.edges.sql())
        .bind(&ids)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    for (parent, child) in edge_rows {
        let (Some(child), Some(parent)) = (address(child), nodes.get_mut(&parent)) else {
            return Ok(None);
        };
        parent.children.push(child);
    }
    contents.nodes = nodes.into_values().collect();
    if contents.check_supported(process).is_err() {
        return Ok(None);
    }
    let Ok(bytes) = contents.encode() else {
        return Ok(None);
    };
    // Every root must read from the archive alone exactly as from its rows:
    // a closure the stored edges do not cover keeps its rows.
    let view = ProcessArchive::open(&bytes)?;
    for root in contents.roots() {
        if super::graph::walk_archive(&view, &[root], limits, super::graph::Ingest::EncodingFirst)
            .await
            .is_err()
        {
            return Ok(None);
        }
    }
    Ok(Some((contents, bytes)))
}

fn address(stored: Vec<u8>) -> Option<NodeId> {
    Some(NodeId(stored.try_into().ok()?))
}

/// `owner`'s facts and calls and `session`'s model calls, at most `bound`
/// rows each; `None` when a table holds more than the cap admits or a row
/// names no stored node.
async fn rows(
    tx: &mut PgConnection,
    owner: &str,
    session: &str,
    bound: i64,
) -> Result<Option<ArchiveContents>, DurableError> {
    let fetch = async |sql: &str, key: &str, tx: &mut PgConnection| {
        crate::observed_sql::query(sql)
            .bind(key.to_owned())
            .bind(bound)
            .fetch_all(crate::observed_sql::executor(tx))
            .await
            .map_err(sqlx_failure)
    };
    let limit = usize::try_from(bound).unwrap_or(usize::MAX) - 1;
    let mut contents = ArchiveContents::default();
    let facts = fetch(SQL.facts.sql(), owner, tx).await?;
    let calls = fetch(SQL.calls.sql(), owner, tx).await?;
    let models = fetch(SQL.models.sql(), session, tx).await?;
    if facts.len() > limit || calls.len() > limit || models.len() > limit {
        return Ok(None);
    }
    let node =
        |row: &sqlx::postgres::PgRow, at: usize| -> Result<Option<Option<NodeId>>, DurableError> {
            let stored: Option<Vec<u8>> = row.try_get(at).map_err(sqlx_failure)?;
            Ok(match stored {
                None => Some(None),
                Some(stored) => address(stored).map(Some),
            })
        };
    for row in &facts {
        let Some(Some(record)) = node(row, 4)? else {
            return Ok(None);
        };
        contents.facts.push(ArchivedFact {
            run_seq: row.try_get(0).map_err(sqlx_failure)?,
            ordinal: row.try_get(1).map_err(sqlx_failure)?,
            kind: row.try_get(2).map_err(sqlx_failure)?,
            call_id: row.try_get(3).map_err(sqlx_failure)?,
            record,
            written_epoch: row.try_get(5).map_err(sqlx_failure)?,
        });
    }
    for row in &calls {
        let (Some(decision_ref), Some(body_ref)) = (node(row, 15)?, node(row, 17)?) else {
            return Ok(None);
        };
        contents.calls.push(ArchivedCall {
            owner_key: row.try_get(0).map_err(sqlx_failure)?,
            call_id: row.try_get(1).map_err(sqlx_failure)?,
            execution_owner: row.try_get(2).map_err(sqlx_failure)?,
            run_seq: row.try_get(3).map_err(sqlx_failure)?,
            member: row.try_get(4).map_err(sqlx_failure)?,
            admission_seq: row.try_get(5).map_err(sqlx_failure)?,
            counted: row.try_get(6).map_err(sqlx_failure)?,
            admitted_count: row.try_get(7).map_err(sqlx_failure)?,
            cancellable: row.try_get(8).map_err(sqlx_failure)?,
            session_id: row.try_get(9).map_err(sqlx_failure)?,
            turn_id: row.try_get(10).map_err(sqlx_failure)?,
            request_id: row.try_get(11).map_err(sqlx_failure)?,
            origin: row.try_get(12).map_err(sqlx_failure)?,
            reason: row.try_get(13).map_err(sqlx_failure)?,
            decision: row.try_get(14).map_err(sqlx_failure)?,
            decision_ref,
            answer_outcome: row.try_get(16).map_err(sqlx_failure)?,
            body_ref,
        });
    }
    for row in &models {
        let Some(record_ref) = node(row, 8)? else {
            return Ok(None);
        };
        contents.models.push(ArchivedModelCall {
            run: row.try_get(0).map_err(sqlx_failure)?,
            call: row.try_get(1).map_err(sqlx_failure)?,
            call_id: row.try_get(2).map_err(sqlx_failure)?,
            attempt: row.try_get(3).map_err(sqlx_failure)?,
            answered: row.try_get(4).map_err(sqlx_failure)?,
            enclosing_owner: row.try_get(5).map_err(sqlx_failure)?,
            enclosing_run: row.try_get(6).map_err(sqlx_failure)?,
            enclosing_call: row.try_get(7).map_err(sqlx_failure)?,
            record_ref,
            ended_at_ms: row.try_get(9).map_err(sqlx_failure)?,
            written_epoch: row.try_get(10).map_err(sqlx_failure)?,
        });
    }
    Ok(Some(contents))
}

/// Store the archive, then delete exactly the rows it holds and peel the
/// roots they held: one transaction, so a reader sees rows or the archive.
async fn write(
    tx: &mut PgConnection,
    process: &lash_vocabulary::identity::ProcessId,
    contents: &ArchiveContents,
    bytes: &[u8],
) -> Result<(), DurableError> {
    let packed = crate::observed_sql::query(SQL.pack.sql())
        .bind(process.as_str())
        .bind(bytes)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if packed.rows_affected() != 1 {
        return Err(corrupt("process history", "the locked row moved"));
    }
    let mut roots: BTreeSet<String> = BTreeSet::new();
    roots.extend(
        super::run_records::delete_owner(&mut *tx, &OwnerKey::Process(process.clone()).stored())
            .await
            .map_err(sqlx_failure)?,
    );
    roots.extend(
        super::tool_calls::delete_owner(&mut *tx, &CallOwner::Process(process.clone()))
            .await
            .map_err(sqlx_failure)?,
    );
    roots.extend(
        super::model_calls::delete_session(&mut *tx, &archive::model_session(process))
            .await
            .map_err(sqlx_failure)?,
    );
    let held: BTreeSet<String> = contents.roots().iter().map(NodeId::to_hex).collect();
    if roots != held {
        return Err(corrupt("process history", "the rows moved while packing"));
    }
    super::peel(tx, roots).await.map_err(DurableError::Graph)?;
    Ok(())
}

impl super::PostgresDurableStore {
    /// Pack the history of up to `limit` finished processes whose actors
    /// ended, one transaction each, keeping the rows of any over the cap or
    /// unsupported. Answers how many it archived.
    pub(super) async fn pack_due_histories(&self, limit: usize) -> Result<usize, DurableError> {
        let limits = self.pools.graph_reads.owner();
        let mut after = String::new();
        let mut archived = 0;
        let mut seen = 0;
        while seen < limit {
            let page = i64::try_from((limit - seen).min(64)).unwrap_or(64);
            let from = after.clone();
            let due: Vec<String> = self
                .read(|connection| {
                    let from = from.clone();
                    Box::pin(async move {
                        crate::observed_sql::query_scalar(SQL.due.sql())
                            .bind(from)
                            .bind(page)
                            .fetch_all(crate::observed_sql::executor(&mut **connection))
                            .await
                            .map_err(sqlx_failure)
                    })
                })
                .await?;
            let Some(last) = due.last() else {
                break;
            };
            after = last.clone();
            seen += due.len();
            for process in due {
                let process = lash_vocabulary::identity::ProcessId::parse(&process)
                    .map_err(|_| corrupt("process id", &process))?;
                if let Packed::Archived(_) = self.pack_one(&process, &limits).await? {
                    archived += 1;
                }
            }
        }
        Ok(archived)
    }

    /// Pack `process`'s history in a transaction of its own.
    pub(super) async fn pack_one(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
        limits: &DecodeLimits,
    ) -> Result<Packed, DurableError> {
        self.within(PACK.capacity(), async {
            let (mut tx, _) = self.open(PACK).await?;
            match pack(&mut tx, process, limits).await {
                Ok(packed) => {
                    tx.commit().await.map_err(sqlx_failure)?;
                    Ok(packed)
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(error)
                }
            }
        })
        .await
    }
}
