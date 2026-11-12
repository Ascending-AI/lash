//! Persistent-heap pages on SQLite (FIG-6191, S6): root leases, demand
//! page reads, a candidate's streamed install beside its owner commit, and
//! the bounded retirement steps that peel retired roots off the
//! publication path.
//!
//! # Locks
//!
//! Every write here runs in [`SqliteConnection::write_flow`](crate::conn::SqliteConnection::write_flow),
//! behind the database's one writer: a retirement step never interleaves
//! with an install, a lease or another step, so no lock order is needed
//! here (PostgreSQL's is in its `durable/pages.rs`). A lease's fence is the
//! actor row read under that writer. A page read is one statement that
//! checks its lease and reads the bytes on one snapshot.
//!
//! # Retirement
//!
//! A commit that retires a root holds it ([`hold`]); a step continues its
//! own frontier (`retirement_frontier`), oldest first, before it takes the
//! oldest hold, peels level by level in node id order until its node budget
//! is spent, and puts the frontier it did not reach back on it, in one
//! transaction. Expired leases are reaped first: their roots join the
//! frontier, so what only they kept is peeled by a later step.

use crate::Prepare as _;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{
    BlobEncoding, CommitId, GraphRefusal, InstalledGraphMeasurement, NodeId,
};
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::graph_commit::{
    CandidateBuffers, CandidateInstall, CheckedGraph, EDGE_ROW_LOGICAL_BYTES, InstallEntry,
    blob_row_logical_bytes, check_candidate, encode_owner,
};
use lash_durable::domain::pages::{
    CandidatePublication, CandidateReceipt, LeaseFence, PageStore, RetirementBacklog,
    RetirementBudget, RetirementProgress, RootLease, RootLeaseId, RootLeaseRequest, StoredPage,
};
use lash_durable::domain::{DomainWrite, ExecKey};
use lash_durable::{ActorTx, CommitLabel, DurableError, DurableInstant, DurableReads, Fenced};
use lash_store_sql::durable::pages::PageStatements;
use rusqlite::{Connection, OptionalExtension};

use super::graph::{self, GraphAnswer};
use super::{Answer, SqliteDurableStore, apply_owner, commit, refuse};
use crate::conn::{TxOutcome, cached_execute};

lash_store_sql::statements! {
    lazy
    /// Page statements only SQLite issues: the `json_each` binding of a
    /// node list and its integer holds.
    pub(crate) struct PageSqliteStatements @ "durable_pages_sqlite" {
        /// Hold every stored node of JSON `?1`, its closure when `?2` is 1
        /// or the node alone when 0, queued at `?3`. A node already held
        /// keeps the wider hold and its place in the queue.
        hold = "INSERT INTO retirement_holds (root_ref, closure, queued_at_ms)
             SELECT blobs.id, ?2, ?3 FROM blobs
             WHERE blobs.hash IN (SELECT unhex(value) FROM json_each(?1))
             ON CONFLICT (root_ref) DO UPDATE SET closure = max(closure, excluded.closure)";

        /// [`hold`](Self::hold) of every stored node id of JSON `?1`.
        hold_nodes = "INSERT INTO retirement_holds (root_ref, closure, queued_at_ms)
             SELECT blobs.id, ?2, ?3 FROM blobs
             WHERE blobs.id IN (SELECT value FROM json_each(?1))
             ON CONFLICT (root_ref) DO UPDATE SET closure = max(closure, excluded.closure)";

        /// Mixed closure and node-only holds from one owner commit. A retained
        /// call keeps its roots until logical expiry, which queues them after
        /// deleting the row. SQLite's single writer makes that check atomic.
        hold_set = "WITH RECURSIVE input AS MATERIALIZED (SELECT ?1 AS closure_bytes, ?2 AS node_bytes),
             offsets AS (SELECT 1 AS pos UNION ALL SELECT pos + 32 FROM offsets, input
                 WHERE pos + 32 <= max(length(closure_bytes), length(node_bytes))),
             retired AS (
                 SELECT substr(closure_bytes, pos, 32) AS hash, 1 AS closure FROM input, offsets WHERE pos <= length(closure_bytes)
                 UNION ALL SELECT substr(node_bytes, pos, 32), 0 FROM input, offsets WHERE pos <= length(node_bytes))
             INSERT INTO retirement_holds (root_ref, closure, queued_at_ms)
             SELECT blobs.id, retired.closure, ?3 FROM retired
             JOIN blobs ON blobs.hash = retired.hash
             WHERE NOT EXISTS (SELECT 1 FROM tool_calls WHERE decision_ref = blobs.id)
               AND NOT EXISTS (SELECT 1 FROM tool_calls WHERE body_ref = blobs.id)
             ON CONFLICT (root_ref) DO UPDATE SET closure = excluded.closure
             WHERE retirement_holds.closure < excluded.closure";

        /// Release the complete owner handoff, returning roots for holds.
        release_leases = "DELETE FROM root_leases
            WHERE lease_id IN (SELECT value FROM json_each(?1))
            RETURNING (SELECT hash FROM blobs WHERE id = root_ref) AS root_ref";

        /// End lease `?1` if it expired by `?2`; answers the node id of the
        /// root it held.
        lease_reap = "DELETE FROM root_leases WHERE lease_id = ?1 AND expires_at_ms <= ?2
             RETURNING root_ref";

        /// Take the oldest `?1` nodes of the steps' frontier, in one
        /// statement; answers each node id and whether its closure is peeled.
        frontier_take = "DELETE FROM retirement_frontier
             WHERE node_ref IN (SELECT node_ref FROM retirement_frontier
                 ORDER BY queued_at_ms, node_ref LIMIT ?1)
             RETURNING node_ref, closure";

        /// The oldest hold's node id.
        hold_next = "SELECT root_ref FROM retirement_holds
             ORDER BY queued_at_ms, root_ref LIMIT 1";

        /// Put every node id of JSON `?1` on the steps' frontier, its closure
        /// when `?2` is 1 or the node alone when 0, queued at `?3`. A node
        /// already there keeps the wider peel and its place in the queue.
        frontier_put = "INSERT INTO retirement_frontier (node_ref, closure, queued_at_ms)
             SELECT blobs.id, ?2, ?3 FROM blobs
             WHERE blobs.id IN (SELECT value FROM json_each(?1))
             ON CONFLICT (node_ref) DO UPDATE SET closure = max(closure, excluded.closure)";

        /// Up to `?2` leases expired by `?1`, oldest first.
        leases_expired = "SELECT lease_id FROM root_leases WHERE expires_at_ms <= ?1
             ORDER BY expires_at_ms, lease_id LIMIT ?2";

        /// Whether lease `?1` on root `?2` is live at `?3`, as one row with
        /// no hash; and only when it is, the hash, bytes and encoding of
        /// every blob of JSON `?4`. One statement, so one snapshot answers
        /// the lease and its pages: a release or retirement step committed
        /// meanwhile is seen whole or not at all.
        leased_pages = "WITH lease AS (
                 SELECT EXISTS (SELECT 1 FROM root_leases
                     WHERE lease_id = ?1 AND root_ref = COALESCE((SELECT id FROM blobs WHERE hash = ?2), CASE WHEN ?2 IS NOT NULL THEN -1 END)
                         AND expires_at_ms > ?3) AS live)
             SELECT NULL, NULL, NULL, lease.live FROM lease
             UNION ALL
             SELECT blobs.hash, blobs.content, blobs.encoding, NULL FROM lease JOIN blobs
                 ON lease.live AND blobs.hash IN (SELECT unhex(value) FROM json_each(?4))";

        /// The encoding of every blob of JSON `?1`, a candidate batch's
        /// named nodes, and the encoding and bytes of every blob of JSON
        /// `?2`, the nodes it supplies: a named node is checked by its
        /// encoding, a supplied one byte for byte. A batch names or
        /// supplies each node once.
        candidate_nodes = "SELECT hash, encoding, NULL FROM blobs
             WHERE hash IN (SELECT unhex(value) FROM json_each(?1))
             UNION ALL
             SELECT hash, encoding, content FROM blobs
             WHERE hash IN (SELECT unhex(value) FROM json_each(?2))";

        /// The encoding of every blob of JSON `?1`.
        encodings = "SELECT hash, encoding FROM blobs
             WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";

        /// Every edge of JSON `?1`, `[parent, child]` digest pairs, that is
        /// not stored yet: a supplied node another commit installed already
        /// has its edges.
        insert_edges_once = "INSERT INTO blob_edges (parent_id, child_id)
             SELECT parent.id, child.id FROM json_each(?1) AS edge
               JOIN blobs AS parent ON parent.hash = unhex(edge.value ->> 0)
               JOIN blobs AS child ON child.hash = unhex(edge.value ->> 1)
              WHERE true
             ON CONFLICT (parent_id, child_id) DO NOTHING";
    }
}

struct Sql {
    pages: PageStatements,
    sqlite: PageSqliteStatements,
    actor: lash_store_sql::durable::ActorStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    Sql {
        pages: PageStatements::render(dialect),
        sqlite: PageSqliteStatements::render(dialect),
        actor: lash_store_sql::durable::ActorStatements::render(dialect),
    }
});

/// A lease's acquisition, renewal and release.
const PAGES_LEASE: CommitLabel = CommitLabel::new("pages.lease");

/// One retirement step.
const PAGES_RETIRE: CommitLabel = CommitLabel::new("pages.retire");

/// Edge pairs one statement inserts.
const EDGE_BATCH: usize = 4096;

/// Holds one statement installs.
const HOLD_BATCH: usize = 4096;

/// The nodes one peel statement of a retirement step examines: a level's
/// children stay a bounded read.
pub(crate) const PEEL_LEVEL: usize = 8;

/// The frontier digests a retirement step keeps in memory; the rest are
/// held at once, for later steps.
pub(crate) const FRONTIER_CAP: usize = 4096;

/// The most manifest bytes the last candidate install on this thread held
/// at once: what the bounded-memory law reads.
#[cfg(any(test, feature = "testing"))]
pub(crate) static CANDIDATE_PEAK_BUFFERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Install a named fault at the hold boundary for external conformance laws.
#[cfg(feature = "testing")]
pub(crate) fn cut_retirement_holds(tx: &Connection, trigger: &str) -> rusqlite::Result<()> {
    tx.execute_batch(&format!(
        "CREATE TRIGGER \"{}\" BEFORE INSERT ON retirement_holds BEGIN \
         SELECT RAISE(ABORT, 'injected retirement hold failure'); END;",
        trigger.replace('"', "\"\"")
    ))
}

/// Holds every stored node of `roots`, the closure under each when
/// `closure`, queued at `now`: the publication's half of D-RET. Answers
/// how many stored nodes it held.
pub(super) fn hold(
    tx: &Connection,
    roots: &BTreeSet<String>,
    closure: bool,
    now: DurableInstant,
) -> rusqlite::Result<usize> {
    let roots: Vec<&String> = roots.iter().collect();
    let mut held = 0;
    for chunk in roots.chunks(HOLD_BATCH) {
        held += cached_execute(
            tx,
            SQL.sqlite.hold.sql(),
            rusqlite::params![graph::json_array(&chunk), i64::from(closure), now.0],
        )?;
    }
    Ok(held)
}

/// [`hold`] of node ids: what a peel left of a closure it did not finish.
pub(super) fn hold_nodes(
    tx: &Connection,
    nodes: &BTreeSet<i64>,
    closure: bool,
    now: DurableInstant,
) -> rusqlite::Result<usize> {
    let nodes: Vec<i64> = nodes.iter().copied().collect();
    let mut held = 0;
    for chunk in nodes.chunks(HOLD_BATCH) {
        held += cached_execute(
            tx,
            SQL.sqlite.hold_nodes.sql(),
            rusqlite::params![graph::json_array(&chunk), i64::from(closure), now.0],
        )?;
    }
    Ok(held)
}

/// Fixed-width native digests keep the bounded hold batch out of JSON/hex.
fn packed_roots(roots: &[String]) -> rusqlite::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(roots.len() * 32);
    for root in roots {
        let node = NodeId::from_hex(root).ok_or_else(|| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(super::corrupt(
                "retirement root",
                root,
            )))
        })?;
        bytes.extend_from_slice(&node.0);
    }
    Ok(bytes)
}

pub(super) fn hold_retired(
    tx: &Connection,
    retired: super::Retired,
    now: DurableInstant,
) -> rusqlite::Result<()> {
    let (closures, phase_ends) = retired.into_parts();
    let mut closures = closures.into_iter();
    let mut phase_ends = phase_ends.into_iter();
    loop {
        let closures: Vec<_> = closures.by_ref().take(HOLD_BATCH).collect();
        let phases: Vec<_> = phase_ends
            .by_ref()
            .take(HOLD_BATCH - closures.len())
            .collect();
        if closures.is_empty() && phases.is_empty() {
            return Ok(());
        }
        cached_execute(
            tx,
            SQL.sqlite.hold_set.sql(),
            rusqlite::params![packed_roots(&closures)?, packed_roots(&phases)?, now.0],
        )?;
    }
}

/// Installs `candidate` with the owner commit's own `checked` graph, in the
/// caller's fenced transaction: every node the stream names is checked
/// present and every node it supplies is compared or inserted, in global
/// digest order a batch at a time; then every supplied node's edges once
/// all node rows exist, each child checked stored. Nothing a named node
/// reaches is read.
pub(super) fn install_candidate(
    tx: &Connection,
    writes: &[DomainWrite],
    checked: &CheckedGraph,
    candidate: &mut CandidatePublication,
) -> GraphAnswer<()> {
    macro_rules! answer {
        ($answer:expr) => {
            match $answer {
                Ok(value) => value,
                Err(refusal) => return Ok(Err(refusal)),
            }
        };
    }
    let summary = candidate.manifest.summary();
    answer!(check_candidate(writes, checked, &candidate.owner, &summary));
    let mut install =
        CandidateInstall::new(checked, &mut *candidate.manifest, CandidateBuffers::DEFAULT);
    let mut installed = InstalledGraphMeasurement::default();
    while let Some(batch) = answer!(install.next_batch()) {
        answer!(install_nodes(tx, &batch, &mut installed)?);
    }
    let mut edges = answer!(install.edges());
    while let Some(pairs) = answer!(edges.next_batch()) {
        answer!(install_edges(tx, &pairs, &mut installed)?);
    }
    #[cfg(any(test, feature = "testing"))]
    CANDIDATE_PEAK_BUFFERED.store(edges.peak_buffered(), std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = lash_core_ids::operational_metrics::sql::Window::current() {
        window.installed_graph(installed.newly_installed_canonical_bytes);
    }
    Ok(Ok(()))
}

/// One batch's nodes: each named node checked stored (a published root as
/// a graph node) by its encoding alone, each supplied node compared with
/// the bytes stored under its digest or inserted.
fn install_nodes(
    tx: &Connection,
    batch: &[InstallEntry],
    installed: &mut InstalledGraphMeasurement,
) -> GraphAnswer<()> {
    let mut stored = std::collections::BTreeMap::new();
    {
        let named = graph::digest_array(batch.iter().filter_map(|entry| match entry {
            InstallEntry::Named { id, .. } => Some(id),
            InstallEntry::Supplied(_) => None,
        }));
        let supplied = graph::digest_array(batch.iter().filter_map(|entry| match entry {
            InstallEntry::Named { .. } => None,
            InstallEntry::Supplied(node) => Some(&node.id),
        }));
        let mut statement = tx.statement(SQL.sqlite.candidate_nodes.sql())?;
        let _bindings = crate::observed_sql::text_bindings(
            SQL.sqlite.candidate_nodes.sql(),
            &[&named, &supplied],
        );
        let mut rows = statement.query([named, supplied])?;
        while let Some(row) = rows.next()? {
            let hash: String = crate::codec::hash_column(row, 0)?;
            let encoding = match graph::encoding_of(row.get(1)?) {
                Ok(encoding) => encoding,
                Err(refusal) => return Ok(Err(refusal)),
            };
            stored.insert(hash, (encoding, row.get::<_, Option<Vec<u8>>>(2)?));
        }
    }
    let mut fresh = Vec::new();
    for entry in batch {
        let hash = entry.id().to_hex();
        match (entry, stored.get(&hash)) {
            (InstallEntry::Named { id, .. }, None) => {
                return Ok(Err(GraphRefusal::MissingNode(*id)));
            }
            (InstallEntry::Named { id, root: true }, Some((BlobEncoding::Opaque, _))) => {
                return Ok(Err(GraphRefusal::EncodingConflict(*id)));
            }
            (InstallEntry::Named { .. }, Some(_)) => {}
            (InstallEntry::Supplied(node), Some((encoding, bytes))) => {
                if *encoding != BlobEncoding::Graph {
                    return Ok(Err(GraphRefusal::EncodingConflict(node.id)));
                }
                if bytes.as_deref() != Some(&node.bytes[..]) {
                    return Ok(Err(GraphRefusal::ContentConflict(node.id)));
                }
            }
            (InstallEntry::Supplied(node), None) => fresh.push(node),
        }
    }
    for chunk in fresh.chunks(graph::BLOB_BATCH_ROWS) {
        let mut insert = tx.statement(graph::blob_batch_sql(chunk.len()))?;
        for (index, node) in chunk.iter().enumerate() {
            insert.raw_bind_parameter(index * 2 + 1, node.id.0.as_slice())?;
            insert.raw_bind_parameter(index * 2 + 2, &node.bytes[..])?;
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
        }
        crate::observed_sql::with_blob_bindings(
            graph::blob_batch_sql(chunk.len()),
            chunk
                .iter()
                .flat_map(|node| [node.id.0.len(), node.bytes.len()]),
            || insert.raw_execute(),
        )?;
    }
    Ok(Ok(()))
}

/// One batch of supplied nodes' edges: every child must be stored by now,
/// then each `(parent, child)` row not yet stored is inserted.
fn install_edges(
    tx: &Connection,
    pairs: &[(NodeId, NodeId)],
    installed: &mut InstalledGraphMeasurement,
) -> GraphAnswer<()> {
    for chunk in pairs.chunks(EDGE_BATCH) {
        let children: BTreeSet<NodeId> = chunk.iter().map(|(_, child)| *child).collect();
        let present: BTreeSet<String> = {
            let digests = graph::digest_array(children.iter());
            let _bindings =
                crate::observed_sql::text_bindings(SQL.sqlite.encodings.sql(), &[&digests]);
            tx.statement(SQL.sqlite.encodings.sql())?
                .query_map([digests], |row| crate::codec::hash_column(row, 0))?
                .collect::<rusqlite::Result<_>>()?
        };
        if let Some(missing) = children
            .iter()
            .find(|child| !present.contains(&child.to_hex()))
        {
            return Ok(Err(GraphRefusal::MissingNode(*missing)));
        }
        let mut edges = String::from("[");
        for (parent, child) in chunk {
            if edges.len() > 1 {
                edges.push(',');
            }
            edges.push_str("[\"");
            parent.write_hex(&mut edges);
            edges.push_str("\",\"");
            child.write_hex(&mut edges);
            edges.push_str("\"]");
        }
        edges.push(']');
        let _bindings =
            crate::observed_sql::text_bindings(SQL.sqlite.insert_edges_once.sql(), &[&edges]);
        let inserted = cached_execute(tx, SQL.sqlite.insert_edges_once.sql(), [edges])?;
        installed.new_edges += inserted as u64;
        installed.sql_logical_bytes += inserted as u64 * EDGE_ROW_LOGICAL_BYTES;
    }
    Ok(Ok(()))
}

/// The refusal for an actor whose fence a lease did not match.
fn lease_fenced(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    held: lash_durable::Epoch,
) -> rusqlite::Result<DurableError> {
    let current = tx
        .statement(SQL.actor.epoch_of.sql())?
        .query_row([actor.as_str()], |row| row.get::<_, i64>(0))
        .optional()?;
    Ok(DurableError::OwnershipLost(Fenced {
        actor: actor.clone(),
        held,
        current: current.map(lash_durable::Epoch),
    }))
}

/// The refusal an actor's lease meets once its fence moved; a reader's
/// lease is under no fence.
fn fenced(tx: &Connection, fence: &LeaseFence) -> rusqlite::Result<Option<DurableError>> {
    match fence {
        LeaseFence::Actor { actor, epoch } if !fence_holds(tx, actor, *epoch)? => {
            lease_fenced(tx, actor, *epoch).map(Some)
        }
        LeaseFence::Actor { .. } | LeaseFence::Reader => Ok(None),
    }
}

/// A lease row's actor column: none for a reader.
fn fence_actor(fence: &LeaseFence) -> Option<&str> {
    match fence {
        LeaseFence::Actor { actor, .. } => Some(actor.as_str()),
        LeaseFence::Reader => None,
    }
}

/// A lease row's epoch column: none for a reader.
fn fence_epoch(fence: &LeaseFence) -> Option<i64> {
    match fence {
        LeaseFence::Actor { epoch, .. } => Some(epoch.0),
        LeaseFence::Reader => None,
    }
}

/// Whether `actor` is owned at `epoch`, read under the writer.
fn fence_holds(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    epoch: lash_durable::Epoch,
) -> rusqlite::Result<bool> {
    let row = tx
        .statement(SQL.actor.epoch_of.sql())?
        .query_row([actor.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    Ok(row.is_some_and(|(current, state)| current == epoch.0 && state == "owned"))
}

fn owner_bytes(owner: &RootOwner) -> Vec<u8> {
    let mut out = lash_kernel_value::tokens::TokenWriter::new();
    encode_owner(&mut out, owner);
    out.into_bytes()
}

/// One retirement step in the caller's transaction (see the module docs).
pub(crate) fn retire_step_in(
    tx: &Connection,
    budget: RetirementBudget,
    now: DurableInstant,
) -> rusqlite::Result<RetirementProgress> {
    let mut progress = RetirementProgress::default();
    let limit = budget.nodes.max(1);
    let expired: Vec<i64> = tx
        .statement(SQL.sqlite.leases_expired.sql())?
        .query_map(
            rusqlite::params![now.0, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut reaped = BTreeSet::new();
    for lease in expired {
        if let Some(root) = tx
            .statement(SQL.sqlite.lease_reap.sql())?
            .query_row(rusqlite::params![lease, now.0], |row| row.get::<_, i64>(0))
            .optional()?
        {
            reaped.insert(root);
            progress.leases_reaped += 1;
        }
    }
    frontier_put(tx, &reaped, true, now)?;
    let mut remaining = usize::try_from(limit).unwrap_or(usize::MAX);
    // A step examines at most a few times its deletion budget: a frontier
    // something else still names costs reads, not deletions.
    let mut examinable = remaining.saturating_mul(4).saturating_add(PEEL_LEVEL);
    let mut frontier = BTreeSet::new();
    while remaining > 0 && examinable > 0 {
        if frontier.is_empty() {
            // The next retired nodes: a closure joins the frontier, an
            // ended phase's root is peeled alone.
            let next = next_retired(tx, remaining.min(examinable).min(FRONTIER_CAP))?;
            if next.is_empty() {
                break;
            }
            progress.holds_advanced += next.len() as u64;
            let mut alone = BTreeSet::new();
            for (node, closure) in next {
                if closure {
                    frontier.insert(node);
                } else {
                    alone.insert(node);
                }
            }
            if !alone.is_empty() {
                examinable = examinable.saturating_sub(alone.len());
                let (gone, _) = graph::peel_level(tx, &alone)?;
                progress.nodes_deleted += gone as u64;
                remaining = remaining.saturating_sub(gone);
                continue;
            }
        }
        // The next nodes in id order, within the budget.
        let take = PEEL_LEVEL.min(remaining).min(examinable);
        let level: BTreeSet<i64> = frontier.iter().take(take).copied().collect();
        for node in &level {
            frontier.remove(node);
        }
        let (examined, gone, children) =
            graph::peel_batch(tx, graph::Frontier::Ids(&level), remaining.min(examinable))?;
        examinable = examinable.saturating_sub(examined);
        progress.nodes_deleted += gone as u64;
        remaining = remaining.saturating_sub(gone);
        frontier.extend(children);
        if let Some(first_excess) = frontier.iter().nth(FRONTIER_CAP).copied() {
            let excess = frontier.split_off(&first_excess);
            frontier_put(tx, &excess, true, now)?;
        }
    }
    frontier_put(tx, &frontier, true, now)?;
    Ok(progress)
}

/// Puts `nodes` on the steps' frontier, the closure under each when
/// `closure`, queued at `now`.
fn frontier_put(
    tx: &Connection,
    nodes: &BTreeSet<i64>,
    closure: bool,
    now: DurableInstant,
) -> rusqlite::Result<()> {
    let nodes: Vec<i64> = nodes.iter().copied().collect();
    for chunk in nodes.chunks(HOLD_BATCH) {
        cached_execute(
            tx,
            SQL.sqlite.frontier_put.sql(),
            rusqlite::params![graph::json_array(&chunk), i64::from(closure), now.0],
        )?;
    }
    Ok(())
}

/// The next nodes a step peels from: up to `limit` of its own frontier,
/// oldest first, in one statement; else the oldest hold. Answers each node
/// and whether its closure is peeled.
fn next_retired(tx: &Connection, limit: usize) -> rusqlite::Result<Vec<(i64, bool)>> {
    let continued = tx
        .statement(SQL.sqlite.frontier_take.sql())?
        .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !continued.is_empty() {
        return Ok(continued);
    }
    let Some(root) = tx
        .statement(SQL.sqlite.hold_next.sql())?
        .query_row([], |row| row.get::<_, i64>(0))
        .optional()?
    else {
        return Ok(Vec::new());
    };
    let closure: bool = tx
        .statement(SQL.pages.hold_take.sql())?
        .query_row([root], |row| row.get(0))?;
    Ok(vec![(root, closure)])
}

impl SqliteDurableStore {
    /// The most manifest bytes the last candidate install held at once.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn candidate_peak_buffered_for_testing() -> u64 {
        CANDIDATE_PEAK_BUFFERED.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The owner's fence is already held by the enclosing transaction.
pub(super) fn acquire_in(
    tx: &Connection,
    request: RootLeaseRequest,
    now: DurableInstant,
) -> Answer<RootLease> {
    if request.ttl_millis <= 0 {
        return Ok(Err(DurableError::Graph(GraphRefusal::LeaseLost)));
    }
    match graph::root_in(tx, &request.owner)? {
        Ok(Some((root, archive))) if root == request.expected => {
            // A root of a packed history leases a live copy of its closure,
            // restored in this transaction: the page reads, renewals and
            // retirement of the lease are those of any live root.
            if let Some(archive) = archive
                && let Err(error) = super::process_history::restore(tx, &archive, root)?
            {
                return Ok(Err(error));
            }
        }
        Ok(Some(_)) => return Ok(Err(DurableError::Graph(GraphRefusal::RevisionConflict))),
        Ok(None) => return Ok(Err(DurableError::Graph(GraphRefusal::RootRetired))),
        Err(refusal) => return Ok(Err(DurableError::Graph(refusal))),
    }
    let id: i64 = tx.statement(SQL.pages.lease_insert.sql())?.query_row(
        rusqlite::params![
            owner_bytes(&request.owner),
            request.expected.0.as_slice(),
            fence_actor(&request.fence),
            fence_epoch(&request.fence),
            request.ttl_millis,
            now.0
        ],
        |row| row.get(0),
    )?;
    Ok(Ok(RootLease {
        id: RootLeaseId(id.cast_unsigned()),
        owner: request.owner,
        root: request.expected,
        fence: request.fence,
        expires_at: DurableInstant(now.0 + request.ttl_millis),
    }))
}

pub(super) fn release_many_in(
    tx: &Connection,
    leases: &BTreeSet<RootLeaseId>,
) -> rusqlite::Result<Vec<String>> {
    if leases.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<_> = leases.iter().map(|lease| lease.0.cast_signed()).collect();
    tx.statement(SQL.sqlite.release_leases.sql())?
        .query_map([graph::json_array(&ids)], |row| {
            crate::codec::hash_column(row, 0)
        })?
        .collect()
}

pub(super) fn release_in(tx: &Connection, lease: RootLeaseId) -> rusqlite::Result<Option<String>> {
    tx.statement(SQL.pages.lease_release.sql())?
        .query_row([lease.0.cast_signed()], |row| {
            crate::codec::hash_column(row, 0)
        })
        .optional()
}

#[async_trait::async_trait]
impl PageStore for SqliteDurableStore {
    async fn acquire_root_lease(
        &self,
        request: RootLeaseRequest,
    ) -> Result<RootLease, DurableError> {
        self.write(PAGES_LEASE, move |tx, now| {
            if request.ttl_millis <= 0 {
                return refuse(DurableError::Graph(GraphRefusal::LeaseLost));
            }
            if let Some(refusal) = fenced(tx, &request.fence)? {
                return refuse(refusal);
            }
            match acquire_in(tx, request, now)? {
                Ok(lease) => commit(lease),
                Err(error) => refuse(error),
            }
        })
        .await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        let lease = lease.clone();
        self.write(PAGES_LEASE, move |tx, now| {
            if let Some(refusal) = fenced(tx, &lease.fence)? {
                return refuse(refusal);
            }
            let renewed = tx
                .statement(SQL.pages.lease_renew.sql())?
                .query_row(
                    rusqlite::params![
                        lease.id.0.cast_signed(),
                        lease.root.0.as_slice(),
                        fence_epoch(&lease.fence),
                        now.0
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            match renewed {
                Some(expires) => commit(RootLease {
                    expires_at: DurableInstant(expires),
                    ..lease
                }),
                None => refuse(DurableError::Graph(GraphRefusal::LeaseLost)),
            }
        })
        .await
    }

    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError> {
        self.write(PAGES_LEASE, move |tx, now| {
            if let Some(root) = release_in(tx, lease)? {
                hold(tx, &BTreeSet::from([root]), true, now)?;
            }
            commit(())
        })
        .await
    }

    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        let (id, root) = (lease.id, lease.root);
        let pages = pages.to_vec();
        let now = self.instant()?;
        self.query(move |conn| -> Answer<Vec<StoredPage>> {
            let mut live = None;
            let mut returned = Vec::new();
            {
                let mut statement = conn.statement(SQL.sqlite.leased_pages.sql())?;
                let mut rows = statement.query(rusqlite::params![
                    id.0.cast_signed(),
                    root.0.as_slice(),
                    now.0,
                    graph::digest_array(pages.iter()),
                ])?;
                while let Some(row) = rows.next()? {
                    match crate::codec::optional_hash_column(row, 0)? {
                        None => live = Some(row.get::<_, bool>(3)?),
                        Some(hash) => {
                            returned.push((hash, row.get::<_, i64>(2)?, row.get::<_, Vec<u8>>(1)?))
                        }
                    }
                }
            }
            // The lease answers first: a lost one refuses before any page
            // is decoded or missed.
            match live {
                Some(true) => {}
                Some(false) => return Ok(Err(DurableError::Graph(GraphRefusal::LeaseLost))),
                None => return Ok(Err(super::corrupt("page read", "without its lease row"))),
            }
            let mut found = std::collections::BTreeMap::new();
            for (hash, encoding, bytes) in returned {
                match graph::encoding_of(encoding) {
                    Ok(BlobEncoding::Graph) => {}
                    Ok(BlobEncoding::Opaque) => {
                        let id = graph::node_of(&hash).map_err(DurableError::Graph);
                        return Ok(Err(match id {
                            Ok(id) => DurableError::Graph(GraphRefusal::EncodingConflict(id)),
                            Err(error) => error,
                        }));
                    }
                    Err(refusal) => return Ok(Err(DurableError::Graph(refusal))),
                }
                found.insert(hash, std::sync::Arc::<[u8]>::from(bytes));
            }
            let mut answered = Vec::with_capacity(pages.len());
            for page in pages {
                match found.get(&page.to_hex()) {
                    Some(bytes) => answered.push(StoredPage {
                        id: page,
                        bytes: bytes.clone(),
                    }),
                    None => return Ok(Err(DurableError::Graph(GraphRefusal::MissingNode(page)))),
                }
            }
            Ok(Ok(answered))
        })
        .await
    }

    async fn commit_candidate(
        &self,
        tx: ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
    ) -> Result<CandidateReceipt, DurableError> {
        if tx.ack().is_some_and(|through| through > tx.seen()) {
            return Err(DurableError::AckBeyondRead {
                actor: tx.actor().clone(),
            });
        }
        let prepared = super::owner::prepare(tx, true, self.conn.fleet());
        let queued = std::sync::Arc::clone(&prepared);
        let blob_profile = self.blob_profile;
        let summary = candidate.manifest.summary();
        let mut candidate = candidate;
        let actor = lash_core_ids::operational_metrics::sql::with_owner(
            prepared.write.actor().as_str().to_owned(),
            prepared.write.epoch().0,
            prepared.write.revision().0,
            self.write(label, move |connection, now| {
                apply_owner(connection, &queued, now, blob_profile, Some(&mut candidate))
            }),
        )
        .await;
        drop(prepared);
        let actor = actor?;
        Ok(CandidateReceipt {
            actor,
            root: summary.root,
            commit: summary.commit,
        })
    }

    async fn candidate_state(
        &self,
        exec: &ExecKey,
    ) -> Result<Option<(NodeId, CommitId)>, DurableError> {
        Ok(self
            .exec_root(exec)
            .await?
            .map(|row| (row.root, row.commit_id)))
    }

    async fn retire_step(
        &self,
        budget: RetirementBudget,
    ) -> Result<RetirementProgress, DurableError> {
        self.write(PAGES_RETIRE, move |tx, now| {
            Ok(TxOutcome::Commit(Ok(retire_step_in(tx, budget, now)?)))
        })
        .await
    }

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError> {
        self.read(move |tx| -> Answer<RetirementBacklog> {
            let (holds, oldest) = tx
                .statement(SQL.pages.backlog.sql())?
                .query_row([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
                })?;
            Ok(Ok(RetirementBacklog {
                holds: holds.cast_unsigned(),
                oldest_at: oldest.map(DurableInstant),
            }))
        })
        .await
    }
}
