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
//! actor row read under that writer. A page read is one read transaction
//! that checks its lease and reads the bytes on one snapshot.
//!
//! # Retirement
//!
//! A commit that retires a root holds it ([`hold`]); a step continues its
//! own frontier (`retirement_frontier`), oldest first, before it takes the
//! oldest hold, peels level by level in digest order until its node budget
//! is spent, and puts the frontier it did not reach back on it, in one
//! transaction. Expired leases are reaped first: their roots join the
//! frontier, so what only they kept is peeled by a later step.

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
    /// Page statements only SQLite issues: the `json_each` binding of a
    /// node list and its integer holds.
    pub(crate) struct PageSqliteStatements @ "durable_pages_sqlite" {
        /// Hold every stored node of JSON `?1`, its closure when `?2` is 1
        /// or the node alone when 0, queued at `?3`. A node already held
        /// keeps the wider hold and its place in the queue.
        hold = "INSERT INTO retirement_holds (root_ref, closure, queued_at_ms)
             SELECT blobs.hash, ?2, ?3 FROM blobs
             WHERE blobs.hash IN (SELECT value FROM json_each(?1))
             ON CONFLICT (root_ref) DO UPDATE SET closure = max(closure, excluded.closure)";

        /// The oldest hold.
        hold_next = "SELECT root_ref FROM retirement_holds
             ORDER BY queued_at_ms, root_ref LIMIT 1";

        /// Put every node of JSON `?1` on the steps' frontier, its closure
        /// when `?2` is 1 or the node alone when 0, queued at `?3`. A node
        /// already there keeps the wider peel and its place in the queue.
        frontier_put = "INSERT INTO retirement_frontier (node_ref, closure, queued_at_ms)
             SELECT value, ?2, ?3 FROM json_each(?1) WHERE true
             ON CONFLICT (node_ref) DO UPDATE SET closure = max(closure, excluded.closure)";

        /// Up to `?2` leases expired by `?1`, oldest first.
        leases_expired = "SELECT lease_id FROM root_leases WHERE expires_at_ms <= ?1
             ORDER BY expires_at_ms, lease_id LIMIT ?2";

        /// The bytes and encoding of every blob of JSON `?1`.
        pages = "SELECT hash, content, encoding FROM blobs
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// The encoding of every blob of JSON `?1`.
        encodings = "SELECT hash, encoding FROM blobs
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// Every edge of JSON `?1`, `[parent, child]` digest pairs, that is
        /// not stored yet: a supplied node another commit installed already
        /// has its edges.
        insert_edges_once = "INSERT INTO blob_edges (parent_hash, child_hash)
             SELECT value ->> 0, value ->> 1 FROM json_each(?1) WHERE true
             ON CONFLICT (parent_hash, child_hash) DO NOTHING";
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
    install.restart();
    while let Some(batch) = answer!(install.next_batch()) {
        answer!(install_edges(tx, &batch, &mut installed)?);
    }
    #[cfg(any(test, feature = "testing"))]
    CANDIDATE_PEAK_BUFFERED.store(install.peak_buffered(), std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
        window.installed_graph(installed.newly_installed_canonical_bytes);
    }
    Ok(Ok(()))
}

/// One batch's nodes: each named node checked stored (a published root as
/// a graph node), each supplied node compared with what is stored under
/// its digest or inserted.
fn install_nodes(
    tx: &Connection,
    batch: &[InstallEntry],
    installed: &mut InstalledGraphMeasurement,
) -> GraphAnswer<()> {
    let mut stored = std::collections::BTreeMap::new();
    {
        let mut statement = tx.prepare_cached(SQL.sqlite.pages.sql())?;
        let mut rows = statement.query([graph::digest_array(
            batch
                .iter()
                .map(InstallEntry::id)
                .collect::<Vec<_>>()
                .iter(),
        )])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let encoding = match graph::encoding_of(row.get(2)?) {
                Ok(encoding) => encoding,
                Err(refusal) => return Ok(Err(refusal)),
            };
            stored.insert(hash, (encoding, row.get::<_, Vec<u8>>(1)?));
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
                if bytes[..] != node.bytes[..] {
                    return Ok(Err(GraphRefusal::ContentConflict(node.id)));
                }
            }
            (InstallEntry::Supplied(node), None) => fresh.push(node),
        }
    }
    let mut hash = String::with_capacity(64);
    for chunk in fresh.chunks(graph::BLOB_BATCH_ROWS) {
        let mut insert = tx.prepare_cached(graph::blob_batch_sql(chunk.len()))?;
        for (index, node) in chunk.iter().enumerate() {
            hash.clear();
            node.id.write_hex(&mut hash);
            insert.raw_bind_parameter(index * 2 + 1, hash.as_str())?;
            insert.raw_bind_parameter(index * 2 + 2, &node.bytes[..])?;
            installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
            installed.new_nodes += 1;
            installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        }
        insert.raw_execute()?;
    }
    Ok(Ok(()))
}

/// One batch's edges: every child a supplied node's envelope names must be
/// stored by now, then each `(parent, child)` row not yet stored is
/// inserted.
fn install_edges(
    tx: &Connection,
    batch: &[InstallEntry],
    installed: &mut InstalledGraphMeasurement,
) -> GraphAnswer<()> {
    let mut pairs: Vec<(NodeId, NodeId)> = Vec::new();
    for entry in batch {
        if let InstallEntry::Supplied(node) = entry {
            pairs.extend(node.children.iter().map(|child| (node.id, *child)));
        }
    }
    for chunk in pairs.chunks(EDGE_BATCH) {
        let children: BTreeSet<NodeId> = chunk.iter().map(|(_, child)| *child).collect();
        let present: BTreeSet<String> = tx
            .prepare_cached(SQL.sqlite.encodings.sql())?
            .query_map([graph::digest_array(children.iter())], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
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
        .prepare_cached(SQL.actor.epoch_of.sql())?
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
        .prepare_cached(SQL.actor.epoch_of.sql())?
        .query_row([actor.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    Ok(row.is_some_and(|(current, state)| current == epoch.0 && state == "owned"))
}

fn owner_bytes(owner: &RootOwner) -> Vec<u8> {
    let mut out = lash_core_store::checkpoint_graph::TokenWriter::new();
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
        .prepare_cached(SQL.sqlite.leases_expired.sql())?
        .query_map(
            rusqlite::params![now.0, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut reaped = BTreeSet::new();
    for lease in expired {
        if let Some(root) = tx
            .prepare_cached(SQL.pages.lease_reap.sql())?
            .query_row(rusqlite::params![lease, now.0], |row| {
                row.get::<_, String>(0)
            })
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
            // The next retired node: a closure joins the frontier, an
            // ended phase's root is peeled alone.
            let Some((root, closure)) = next_retired(tx)? else {
                break;
            };
            progress.holds_advanced += 1;
            if !closure {
                examinable -= 1;
                let (gone, _) = graph::peel_level(tx, &BTreeSet::from([root]))?;
                progress.nodes_deleted += gone as u64;
                remaining = remaining.saturating_sub(gone);
                continue;
            }
            frontier.insert(root);
        }
        // The next nodes in digest order, within the budget.
        let take = PEEL_LEVEL.min(remaining).min(examinable);
        let level: BTreeSet<String> = frontier.iter().take(take).cloned().collect();
        for node in &level {
            frontier.remove(node);
        }
        examinable -= level.len();
        let (gone, children) = graph::peel_level(tx, &level)?;
        progress.nodes_deleted += gone as u64;
        remaining = remaining.saturating_sub(gone);
        frontier.extend(children);
        if let Some(first_excess) = frontier.iter().nth(FRONTIER_CAP).cloned() {
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
    nodes: &BTreeSet<String>,
    closure: bool,
    now: DurableInstant,
) -> rusqlite::Result<()> {
    let nodes: Vec<&String> = nodes.iter().collect();
    for chunk in nodes.chunks(HOLD_BATCH) {
        cached_execute(
            tx,
            SQL.sqlite.frontier_put.sql(),
            rusqlite::params![graph::json_array(&chunk), i64::from(closure), now.0],
        )?;
    }
    Ok(())
}

/// The next node a step peels from: its own frontier first, oldest first,
/// then the oldest hold. Answers the node and whether its closure is
/// peeled.
fn next_retired(tx: &Connection) -> rusqlite::Result<Option<(String, bool)>> {
    let continued = tx
        .prepare_cached(SQL.pages.frontier_take.sql())?
        .query_row([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .optional()?;
    if continued.is_some() {
        return Ok(continued);
    }
    let Some(root) = tx
        .prepare_cached(SQL.sqlite.hold_next.sql())?
        .query_row([], |row| row.get::<_, String>(0))
        .optional()?
    else {
        return Ok(None);
    };
    let closure: bool = tx
        .prepare_cached(SQL.pages.hold_take.sql())?
        .query_row([&root], |row| row.get(0))?;
    Ok(Some((root, closure)))
}

impl SqliteDurableStore {
    /// The most manifest bytes the last candidate install held at once.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn candidate_peak_buffered_for_testing() -> u64 {
        CANDIDATE_PEAK_BUFFERED.load(std::sync::atomic::Ordering::SeqCst)
    }
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
            match graph::root_of(tx, &request.owner)? {
                Ok(Some(root)) if root == request.expected => {}
                Ok(Some(_)) => return refuse(DurableError::Graph(GraphRefusal::RevisionConflict)),
                Ok(None) => return refuse(DurableError::Graph(GraphRefusal::RootRetired)),
                Err(refusal) => return refuse(DurableError::Graph(refusal)),
            }
            let id: i64 = tx.prepare_cached(SQL.pages.lease_insert.sql())?.query_row(
                rusqlite::params![
                    owner_bytes(&request.owner),
                    request.expected.to_hex(),
                    fence_actor(&request.fence),
                    fence_epoch(&request.fence),
                    request.ttl_millis,
                    now.0
                ],
                |row| row.get(0),
            )?;
            commit(RootLease {
                id: RootLeaseId(id.cast_unsigned()),
                owner: request.owner,
                root: request.expected,
                fence: request.fence,
                expires_at: DurableInstant(now.0 + request.ttl_millis),
            })
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
                .prepare_cached(SQL.pages.lease_renew.sql())?
                .query_row(
                    rusqlite::params![
                        lease.id.0.cast_signed(),
                        lease.root.to_hex(),
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
            let root = tx
                .prepare_cached(SQL.pages.lease_release.sql())?
                .query_row([lease.0.cast_signed()], |row| row.get::<_, String>(0))
                .optional()?;
            // What only the lease kept is peeled by a later step.
            if let Some(root) = root {
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
        self.read(move |tx| -> Answer<Vec<StoredPage>> {
            let live = tx
                .prepare_cached(SQL.pages.lease_live.sql())?
                .query_row(
                    rusqlite::params![id.0.cast_signed(), root.to_hex(), now.0],
                    |_| Ok(()),
                )
                .optional()?;
            if live.is_none() {
                return Ok(Err(DurableError::Graph(GraphRefusal::LeaseLost)));
            }
            let mut found = std::collections::BTreeMap::new();
            {
                let mut statement = tx.prepare_cached(SQL.sqlite.pages.sql())?;
                let mut rows = statement.query([graph::digest_array(pages.iter())])?;
                while let Some(row) = rows.next()? {
                    let hash: String = row.get(0)?;
                    match graph::encoding_of(row.get(2)?) {
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
                    found.insert(
                        hash,
                        std::sync::Arc::<[u8]>::from(row.get::<_, Vec<u8>>(1)?),
                    );
                }
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
        let blob_profile = self.blob_profile;
        let summary = candidate.manifest.summary();
        let mut candidate = candidate;
        let actor = lash_core_execution::facade_support::sql::with_owner(
            tx.actor().as_str().to_owned(),
            tx.epoch().0,
            tx.revision().0,
            self.write(label, move |connection, now| {
                apply_owner(connection, tx, now, blob_profile, Some(&mut candidate))
            }),
        )
        .await?;
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
        #[cfg(feature = "perf-witness")]
        lash_core_execution::perf_witness::record_retirement_batch();
        self.write(PAGES_RETIRE, move |tx, now| {
            Ok(TxOutcome::Commit(Ok(retire_step_in(tx, budget, now)?)))
        })
        .await
    }

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError> {
        self.read(move |tx| -> Answer<RetirementBacklog> {
            let (holds, oldest) = tx
                .prepare_cached(SQL.pages.backlog.sql())?
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
