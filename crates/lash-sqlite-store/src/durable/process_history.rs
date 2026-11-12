//! SQLite: a finished process's history archive (FIG-7701). The pack moves
//! a process's run facts, tool calls and model calls into one archive on its
//! row, and the reads answer from it once the rows are gone.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{DecodeLimits, NodeId};
use lash_core_store::model_calls::ModelCallRecord;
use lash_durable::domain::process_archive::{
    self as archive, ArchiveContents, ArchivedCall, ArchivedFact, ArchivedModelCall, ArchivedNode,
    ModelSelection, PROCESS_ARCHIVE_CAP_BYTES, PROCESS_ARCHIVE_MAX_NODES, PROCESS_ARCHIVE_MAX_ROWS,
    ProcessArchive,
};
use lash_durable::domain::{CallOwner, OwnerKey, RunFacts, ToolCallSelection, ToolCalls};
use lash_durable::{ActorKey, CommitLabel, DurableError};
use lash_store_sql::durable::process_history::ProcessHistoryStatements;
use rusqlite::{Connection, OptionalExtension as _};

use super::{Answer, corrupt};
use crate::Prepare as _;
use crate::conn::TxOutcome;

pub(super) static SQL: LazyLock<ProcessHistoryStatements> =
    LazyLock::new(|| ProcessHistoryStatements::render(crate::schema_layout::MAIN));
lash_store_sql::statements! {
    lazy
    struct ProcessHistorySqliteStatements @ "durable_process_history_sqlite" {
        /// Actor `?1`'s state.
        actor_state = "SELECT state FROM actors WHERE actor_key = ?1";

        /// Up to `?3` blobs of the closure owner `?1`'s rows and session
        /// `?2`'s model calls reach along every edge: each blob's id, size
        /// and edge count, before any content is read. The limit stops the
        /// recursion itself.
        closure = "WITH RECURSIVE roots AS (
                 SELECT record_ref AS id FROM run_records WHERE owner_key = ?1
                 UNION SELECT decision_ref FROM tool_calls WHERE owner_key = ?1 AND decision_ref IS NOT NULL
                 UNION SELECT body_ref FROM tool_calls WHERE owner_key = ?1 AND body_ref IS NOT NULL
                 UNION SELECT record_ref FROM model_call_records WHERE session_id = ?2 AND record_ref IS NOT NULL
             ), reach AS (
                 SELECT id FROM roots
                 UNION
                 SELECT edge.child_id FROM blob_edges AS edge JOIN reach ON edge.parent_id = reach.id
                 LIMIT ?3
             )
             SELECT reach.id, length(CAST(blobs.content AS BLOB)),
                    (SELECT count(*) FROM blob_edges WHERE parent_id = reach.id)
             FROM reach JOIN blobs ON blobs.id = reach.id";

        /// The blobs with ids in JSON array `?1`.
        nodes = "SELECT id, hash, encoding, content FROM blobs
             WHERE id IN (SELECT value FROM json_each(?1))";

        /// Every edge from the blobs with ids in JSON array `?1`, to its
        /// child's address.
        edges = "SELECT edge.parent_id, child.hash FROM blob_edges AS edge
             JOIN blobs AS child ON child.id = edge.child_id
             WHERE edge.parent_id IN (SELECT value FROM json_each(?1))";

        /// Restore archived node `?1` with bytes `?2` and encoding `?3` as a
        /// live blob, unless one is stored under its address.
        restore_node = "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO NOTHING";

        /// Restore the stored edge from node `?1` to node `?2`, both live.
        restore_edge = "INSERT INTO blob_edges (parent_id, child_id)
             SELECT parent.id, child.id FROM blobs AS parent, blobs AS child
             WHERE parent.hash = ?1 AND child.hash = ?2
             ON CONFLICT (parent_id, child_id) DO NOTHING";
    }
}
static SQLITE: LazyLock<ProcessHistorySqliteStatements> =
    LazyLock::new(|| ProcessHistorySqliteStatements::render(crate::schema_layout::MAIN));

const PACK: CommitLabel = CommitLabel::new("process.history_pack");

/// Process `process`'s history archive, when its history discriminator says
/// it is packed: what every history read consults before any row.
pub(super) fn archive_bytes(tx: &Connection, process: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    tx.statement(SQL.archive.sql())?
        .query_row([process], |row| row.get(0))
        .optional()
}

/// `owner`'s run facts from its process's archive, when its history is
/// packed (none while its rows hold it): the same rows, order, `max_rows`
/// refusal and graph walk.
pub(super) fn run_facts(
    tx: &Connection,
    owner: &OwnerKey,
    run: Option<lash_durable::domain::RunSeq>,
    max_rows: Option<usize>,
    limits: &DecodeLimits,
) -> Answer<Option<RunFacts>> {
    let Some(process) = archive::fact_process(owner) else {
        return Ok(Ok(None));
    };
    let Some(bytes) = archive_bytes(tx, process.as_str())? else {
        return Ok(Ok(None));
    };
    Ok((|| {
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
        let nodes =
            super::graph::walk_archive(&archive, &roots, limits).map_err(DurableError::from)?;
        Ok(Some(RunFacts {
            rows,
            nodes,
            read_limits: *limits,
        }))
    })())
}

/// `owner`'s tool calls `selection` reads, from its process's archive when
/// its history is packed (none while its rows hold it).
pub(super) fn tool_calls(
    tx: &Connection,
    owner: &CallOwner,
    selection: &ToolCallSelection,
    limits: &DecodeLimits,
) -> Answer<Option<ToolCalls>> {
    let Some(process) = archive::call_process(owner) else {
        return Ok(Ok(None));
    };
    let Some(bytes) = archive_bytes(tx, process.as_str())? else {
        return Ok(Ok(None));
    };
    Ok((|| {
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
        let nodes =
            super::graph::walk_archive(&archive, &roots, limits).map_err(DurableError::from)?;
        Ok(Some(ToolCalls {
            rows,
            graph: RunFacts {
                nodes,
                read_limits: *limits,
                ..Default::default()
            },
        }))
    })())
}

/// Session `session`'s model calls `selection` reads for `owner`, from its
/// process's archive, with the archive's bytes, when its history is packed
/// (none while its rows hold it).
pub(super) fn model_calls(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    owner: &str,
    selection: ModelSelection<'_>,
) -> Answer<Option<(Vec<u8>, Vec<ArchivedModelCall>)>> {
    let Some(process) = archive::model_process(session) else {
        return Ok(Ok(None));
    };
    let Some(bytes) = archive_bytes(tx, process)? else {
        return Ok(Ok(None));
    };
    // SQLite's `LIKE` folds ASCII case, as the row read's owner match does.
    let models = ProcessArchive::open(&bytes)
        .and_then(|view| archive::select_models(&view, owner, selection, true));
    Ok(match models {
        Ok(models) => Ok(Some((bytes, models))),
        Err(error) => Err(error.into()),
    })
}

/// The sealed records `models` from archive `bytes`, decoded as a read of
/// the same rows decodes them.
pub(super) fn model_records(
    bytes: &[u8],
    models: Vec<ArchivedModelCall>,
    limits: DecodeLimits,
) -> Result<Vec<ModelCallRecord>, DurableError> {
    let archive = ProcessArchive::open(bytes)?;
    let rows = models
        .into_iter()
        .map(archive::record_parts)
        .collect::<Result<Vec<_>, _>>()?;
    let roots: Vec<NodeId> = rows.iter().map(|(_, root, _)| *root).collect();
    let nodes =
        super::graph::walk_archive(&archive, &roots, &limits).map_err(DurableError::from)?;
    super::model_calls::decode_records(rows, nodes, limits)
}

/// Make root `root` of packed history archive `bytes` live again, in the
/// caller's write transaction, before a lease on it is issued: every node
/// its closure reaches along stored edges that no live blob holds is
/// restored with its exact bytes, encoding and edges. A node stored live
/// already keeps its stored copy, and the closure under it is live with
/// it. The lease then holds what it restored as it holds any live root,
/// and its release or reaping retires it as it retires any other.
pub(super) fn restore(tx: &Connection, bytes: &[u8], root: NodeId) -> Answer<()> {
    let archive = match ProcessArchive::open(bytes) {
        Ok(archive) => archive,
        Err(error) => return Ok(Err(error.into())),
    };
    let mut seen = BTreeSet::from([root]);
    let mut level = vec![root];
    let mut edges = Vec::new();
    while !level.is_empty() {
        let mut next = Vec::new();
        for id in level {
            let node = match archive.node(&id) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    return Ok(Err(DurableError::Graph(
                        lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(id),
                    )));
                }
                Err(error) => return Ok(Err(error.into())),
            };
            let restored = tx
                .statement(SQLITE.restore_node.sql())?
                .execute(rusqlite::params![
                    &id.0[..],
                    node.bytes,
                    i64::from(node.encoding)
                ])?;
            if restored == 0 {
                continue;
            }
            let children = match archive.children(&node) {
                Ok(children) => children,
                Err(error) => return Ok(Err(error.into())),
            };
            for child in children {
                edges.push((id, child.id));
                if seen.insert(child.id) {
                    next.push(child.id);
                }
            }
        }
        level = next;
    }
    for (parent, child) in edges {
        tx.statement(SQLITE.restore_edge.sql())?
            .execute(rusqlite::params![&parent.0[..], &child.0[..]])?;
    }
    Ok(Ok(()))
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

/// Pack `process`'s history in write transaction `tx`, which excludes
/// every other writer: after its actor ended, with every check made before
/// any write. Over the cap or unsupported, it keeps its rows, and is not
/// retried. The rows are read with bounded counts and the closure is sized
/// before its content is read, so the work stays within the cap.
pub(super) fn pack(
    tx: &Connection,
    process: &lash_vocabulary::identity::ProcessId,
    limits: &DecodeLimits,
) -> Answer<Packed> {
    let Ok(actor) = ActorKey::process(process.as_str()) else {
        return Ok(Err(corrupt("process id", process.as_str())));
    };
    let state: Option<String> = tx
        .statement(SQLITE.actor_state.sql())?
        .query_row([actor.as_str()], |row| row.get(0))
        .optional()?;
    if state.is_some_and(|state| state != "terminal") {
        return Ok(Ok(Packed::Skipped));
    }
    let undecided: Option<i64> = tx
        .statement(SQL.undecided.sql())?
        .query_row([process.as_str()], |row| row.get(0))
        .optional()?;
    if undecided.is_none() {
        return Ok(Ok(Packed::Skipped));
    }
    match contents(tx, process, limits)? {
        Some((contents, bytes)) => {
            if let Err(error) = write(tx, process, &contents, &bytes)? {
                return Ok(Err(error));
            }
            Ok(Ok(Packed::Archived(bytes.len())))
        }
        None => {
            tx.statement(SQL.keep_rows.sql())?
                .execute([process.as_str()])?;
            Ok(Ok(Packed::Kept))
        }
    }
}

/// `process`'s archive and what it holds, or `None` when its history keeps
/// its rows.
fn contents(
    tx: &Connection,
    process: &lash_vocabulary::identity::ProcessId,
    limits: &DecodeLimits,
) -> rusqlite::Result<Option<(ArchiveContents, Vec<u8>)>> {
    let owner = OwnerKey::Process(process.clone()).stored();
    let session = archive::model_session(process);
    let foreign: Option<i64> = tx
        .statement(SQL.foreign_calls.sql())?
        .query_row([&owner], |row| row.get(0))
        .optional()?;
    if foreign.is_some() {
        return Ok(None);
    }
    let bound = i64::try_from(PROCESS_ARCHIVE_MAX_ROWS + 1).unwrap_or(i64::MAX);
    let Some(mut contents) = rows(tx, &owner, session.as_str(), bound)? else {
        return Ok(None);
    };
    let sized: Vec<(i64, i64, i64)> = tx
        .statement(SQLITE.closure.sql())?
        .query_map(
            rusqlite::params![
                owner,
                session.as_str(),
                i64::try_from(PROCESS_ARCHIVE_MAX_NODES + 1).unwrap_or(i64::MAX)
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
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
    let ids =
        serde_json::Value::from(sized.iter().map(|(id, ..)| *id).collect::<Vec<_>>()).to_string();
    let mut nodes: BTreeMap<i64, ArchivedNode> = BTreeMap::new();
    let stored: Vec<(i64, Vec<u8>, i64, Vec<u8>)> = tx
        .statement(SQLITE.nodes.sql())?
        .query_map([&ids], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, hash, encoding, bytes) in stored {
        let Some(address) = address(hash) else {
            return Ok(None);
        };
        nodes.insert(
            id,
            ArchivedNode {
                id: address,
                encoding: u8::try_from(encoding).unwrap_or(u8::MAX),
                bytes,
                children: Vec::new(),
            },
        );
    }
    let edge_rows: Vec<(i64, Vec<u8>)> = tx
        .statement(SQLITE.edges.sql())?
        .query_map([&ids], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
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
    let Ok(view) = ProcessArchive::open(&bytes) else {
        return Ok(None);
    };
    for root in contents.roots() {
        if super::graph::walk_archive(&view, &[root], limits).is_err() {
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
fn rows(
    tx: &Connection,
    owner: &str,
    session: &str,
    bound: i64,
) -> rusqlite::Result<Option<ArchiveContents>> {
    let limit = usize::try_from(bound).unwrap_or(usize::MAX) - 1;
    let node = |row: &rusqlite::Row<'_>, at: usize| -> rusqlite::Result<Option<Option<NodeId>>> {
        let stored: Option<Vec<u8>> = row.get(at)?;
        Ok(match stored {
            None => Some(None),
            Some(stored) => address(stored).map(Some),
        })
    };
    let facts: Vec<Option<ArchivedFact>> = tx
        .statement(SQL.facts.sql())?
        .query_map(rusqlite::params![owner, bound], |row| {
            let Some(Some(record)) = node(row, 4)? else {
                return Ok(None);
            };
            Ok(Some(ArchivedFact {
                run_seq: row.get(0)?,
                ordinal: row.get(1)?,
                kind: row.get(2)?,
                call_id: row.get(3)?,
                record,
                written_epoch: row.get(5)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let calls: Vec<Option<ArchivedCall>> = tx
        .statement(SQL.calls.sql())?
        .query_map(rusqlite::params![owner, bound], |row| {
            let (Some(decision_ref), Some(body_ref)) = (node(row, 15)?, node(row, 17)?) else {
                return Ok(None);
            };
            Ok(Some(ArchivedCall {
                owner_key: row.get(0)?,
                call_id: row.get(1)?,
                execution_owner: row.get(2)?,
                run_seq: row.get(3)?,
                member: row.get(4)?,
                admission_seq: row.get(5)?,
                counted: row.get(6)?,
                admitted_count: row.get(7)?,
                cancellable: row.get(8)?,
                session_id: row.get(9)?,
                turn_id: row.get(10)?,
                request_id: row.get(11)?,
                origin: row.get(12)?,
                reason: row.get(13)?,
                decision: row.get(14)?,
                decision_ref,
                answer_outcome: row.get(16)?,
                body_ref,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let models: Vec<Option<ArchivedModelCall>> = tx
        .statement(SQL.models.sql())?
        .query_map(rusqlite::params![session, bound], |row| {
            let Some(record_ref) = node(row, 8)? else {
                return Ok(None);
            };
            Ok(Some(ArchivedModelCall {
                run: row.get(0)?,
                call: row.get(1)?,
                call_id: row.get(2)?,
                attempt: row.get(3)?,
                answered: row.get(4)?,
                enclosing_owner: row.get(5)?,
                enclosing_run: row.get(6)?,
                enclosing_call: row.get(7)?,
                record_ref,
                ended_at_ms: row.get(9)?,
                written_epoch: row.get(10)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if facts.len() > limit || calls.len() > limit || models.len() > limit {
        return Ok(None);
    }
    let (Some(facts), Some(calls), Some(models)) = (
        facts.into_iter().collect::<Option<Vec<_>>>(),
        calls.into_iter().collect::<Option<Vec<_>>>(),
        models.into_iter().collect::<Option<Vec<_>>>(),
    ) else {
        return Ok(None);
    };
    Ok(Some(ArchiveContents {
        facts,
        calls,
        models,
        nodes: Vec::new(),
    }))
}

/// Store the archive, then delete exactly the rows it holds and peel the
/// roots they held: one transaction, so a reader sees rows or the archive.
fn write(
    tx: &Connection,
    process: &lash_vocabulary::identity::ProcessId,
    contents: &ArchiveContents,
    bytes: &[u8],
) -> Answer<()> {
    let packed = tx
        .statement(SQL.pack.sql())?
        .execute(rusqlite::params![process.as_str(), bytes])?;
    if packed != 1 {
        return Ok(Err(corrupt("process history", "the row moved")));
    }
    let mut roots: BTreeSet<String> = BTreeSet::new();
    roots.extend(super::run_records::delete_owner(
        tx,
        &OwnerKey::Process(process.clone()).stored(),
    )?);
    roots.extend(super::tool_calls::delete_owner(
        tx,
        &CallOwner::Process(process.clone()),
    )?);
    roots.extend(super::model_calls::delete_session(
        tx,
        &archive::model_session(process),
    )?);
    let held: BTreeSet<String> = contents.roots().iter().map(NodeId::to_hex).collect();
    if roots != held {
        return Ok(Err(corrupt(
            "process history",
            "the rows moved while packing",
        )));
    }
    super::graph::peel(tx, roots)?;
    Ok(Ok(()))
}

impl super::SqliteDurableStore {
    /// Pack the history of up to `limit` finished processes whose actors
    /// ended, one transaction each, keeping the rows of any over the cap or
    /// unsupported. Answers how many it archived.
    pub(super) async fn pack_due_histories(&self, limit: usize) -> Result<usize, DurableError> {
        let limits = self.graph_reads.owner();
        let mut after = String::new();
        let mut archived = 0;
        let mut seen = 0;
        while seen < limit {
            let page = i64::try_from((limit - seen).min(64)).unwrap_or(64);
            let from = after.clone();
            let due: Vec<String> = self
                .query(move |tx| {
                    Ok(Ok(tx
                        .statement(SQL.due.sql())?
                        .query_map(rusqlite::params![from, page], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<String>>>()?))
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
                if let Packed::Archived(_) = self.pack_one(process, limits).await? {
                    archived += 1;
                }
            }
        }
        Ok(archived)
    }

    /// Pack `process`'s history in a write transaction of its own.
    pub(super) async fn pack_one(
        &self,
        process: lash_vocabulary::identity::ProcessId,
        limits: DecodeLimits,
    ) -> Result<Packed, DurableError> {
        self.write(PACK, move |tx, _| {
            Ok(match pack(tx, &process, &limits)? {
                Ok(packed) => TxOutcome::Commit(Ok(packed)),
                Err(error) => TxOutcome::Rollback(Err(error)),
            })
        })
        .await
    }
}
