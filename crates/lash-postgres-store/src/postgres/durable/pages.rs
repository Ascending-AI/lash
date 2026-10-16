//! Persistent-heap pages on PostgreSQL (FIG-6191, S6): root leases, demand
//! page reads, a candidate's streamed install beside its owner commit, the
//! semantic admission it publishes, and the bounded retirement steps that
//! peel retired roots off the publication path.
//!
//! # Lock order
//!
//! One global order for every transaction that takes these locks
//! (FIG-6193); each class is taken in ascending digest order:
//!
//! 1. the retirement advisory key (steps only), or the owner's fence row;
//! 2. blob rows a writer names or installs: key-share locks and inserts, a
//!    candidate's batch at a time, then the children its edges name;
//! 3. the root catalog's owning rows the commit's domain writes change, and
//!    the semantic admission row;
//! 4. retirement holds ([`hold`]): one statement key-share locks every node
//!    the transaction holds, then one statement inserts or widens every
//!    hold row, so a holder waits on a peel lock only before it owns any
//!    hold row;
//! 5. a step's peel locks: blob rows `FOR UPDATE SKIP LOCKED`;
//! 6. the step's own frontier rows (`retirement_frontier`).
//!
//! A step takes the advisory key, the only lock it ever waits for. It then
//! takes expired leases and the oldest hold `FOR UPDATE SKIP LOCKED` (4),
//! peel locks (5) and frontier rows (6), and none of those waits: SKIP
//! LOCKED passes over what another transaction holds, and only steps,
//! one at a time, touch the frontier, which carries no foreign key and is
//! no root of the catalog. A step therefore never waits for a publication,
//! a lease release or an inline peel, so no lock cycle runs through it: a
//! publication that meets a hold row a step took, or a node a step
//! peel-locked, waits for a transaction that waits on nothing. Before this
//! order a step put what was left of its frontier back as hold rows after
//! peeling, and could wait on a publication's uncommitted hold row while
//! that publication waited on the hold row the step had taken: a deadlock
//! PostgreSQL broke by aborting one of them
//! (`publications_and_retirement_on_two_nodes_never_deadlock` choreographs
//! it).
//!
//! A lease takes its actor row `FOR SHARE`, the fence a claim's epoch bump
//! waits behind. A candidate install key-share locks every node it names
//! and every stored node it supplies, a batch at a time in global digest
//! order, inserts the absent ones in that order, and only then, once every
//! node row exists, key-share locks each supplied node's children and
//! inserts its edges. Key-share locks never wait on one another; they wait
//! only on a peel, which never waits.
//!
//! # Retirement
//!
//! A commit that retires a root holds it ([`hold`]): a row of the root
//! catalog, inserted after the hold key-share locks the node. Retirement
//! steps run one at a time under a transaction advisory lock: a step reaps
//! expired leases into its frontier, then continues its frontier, oldest
//! first, before it takes the oldest hold. It peels level by level in
//! digest order until its node budget is spent, and puts every node its
//! peel skipped and the frontier it did not reach back on the frontier,
//! all in one transaction. A frontier node gone meanwhile (an inline peel
//! or the orphan audit took it) is passed over.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{
    BlobEncoding, CommitId, GraphRefusal, InstalledGraphMeasurement, NodeId,
};
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::graph_commit::{
    CandidateBuffers, CandidateInstall, CheckedGraph, CheckedNode, EDGE_ROW_LOGICAL_BYTES,
    InstallEntry, blob_row_logical_bytes, check_candidate, encode_owner,
};
use lash_durable::domain::pages::{
    CandidatePublication, CandidateReceipt, LeaseFence, PageStore, RetirementBacklog,
    RetirementBudget, RetirementProgress, RootLease, RootLeaseId, RootLeaseRequest,
    SemanticAdmission, StoredPage,
};
use lash_durable::domain::{DomainWrite, ExecKey};
use lash_durable::{
    ActorKey, ActorTx, CommitLabel, DurableError, DurableInstant, DurableReads, Epoch, Fenced,
};
use lash_store_sql::durable::pages::PageStatements;
use lash_store_sql::{Dialect, Rendered};
use sqlx::PgConnection;

use super::graph::{self, db};
use super::{PostgresDurableStore, Retired, Tx, sqlx_failure};

lash_store_sql::statements! {
    /// Page statements only PostgreSQL issues: the array binding of a node
    /// list, its boolean holds and the row locks a lease and a step take.
    pub(crate) struct PagePostgresStatements @ "durable_pages_postgres" {
        /// Key-share lock every stored node of `?1` and `?2`, in digest
        /// order, and answer them: what a hold may name, locked against a
        /// peel before any hold row is written.
        hold_lock = "SELECT ARRAY(SELECT hash FROM blobs
             WHERE hash = ANY(?1::text[]) OR hash = ANY(?2::text[])
             ORDER BY hash FOR KEY SHARE)";

        /// Hold every node of `?4` (locked by [`Self::hold_lock`]) that
        /// `?1` names as a closure or `?2` as a node alone, queued at `?3`,
        /// in digest order. A node already held keeps the wider hold and
        /// its place in the queue.
        hold = "INSERT INTO retirement_holds AS held (root_ref, closure, queued_at_ms)
             SELECT node.hash, bool_or(node.closure), ?3::bigint
             FROM (SELECT UNNEST(?1::text[]) AS hash, TRUE AS closure
                   UNION ALL SELECT UNNEST(?2::text[]), FALSE) AS node
             WHERE node.hash = ANY(?4::text[])
             GROUP BY node.hash
             ORDER BY node.hash
             ON CONFLICT (root_ref) DO UPDATE SET closure = held.closure OR excluded.closure";

        /// The oldest hold no other transaction holds, locked for its step.
        hold_next = "SELECT root_ref FROM retirement_holds
             ORDER BY queued_at_ms, root_ref LIMIT 1 FOR UPDATE SKIP LOCKED";

        /// Put every node of `?1` on the step's frontier, its closure when
        /// `?2`, queued at `?3`. A node already there keeps the wider
        /// peel and its place in the queue.
        frontier_put = "INSERT INTO retirement_frontier AS queued (node_ref, closure, queued_at_ms)
             SELECT node.hash, ?2::boolean, ?3::bigint
             FROM UNNEST(?1::text[]) AS node(hash)
             ORDER BY node.hash
             ON CONFLICT (node_ref) DO UPDATE SET closure = queued.closure OR excluded.closure";

        /// Up to `?2` leases expired by `?1` no other transaction holds,
        /// oldest first.
        leases_expired = "SELECT lease_id FROM root_leases WHERE expires_at_ms <= ?1
             ORDER BY expires_at_ms, lease_id LIMIT ?2 FOR UPDATE SKIP LOCKED";

        /// Actor `?1`'s epoch and state, share-locked: a lease's fence.
        actor_fence = "SELECT epoch, state FROM actors WHERE actor_key = ?1 FOR SHARE";

        /// The bytes and encoding of every blob of `?1`.
        pages = "SELECT hash, content, encoding FROM blobs WHERE hash = ANY(?1::text[])";

        /// Every stored blob of `?1` with its encoding, key-share locked
        /// in digest order: a candidate's children, held against a peel.
        lock_children = "SELECT hash, encoding FROM blobs WHERE hash = ANY(?1::text[])
             ORDER BY hash FOR KEY SHARE";

        /// Lock for its delete every blob of a step's level `?1` that no
        /// other transaction holds, in digest order, beside every blob of
        /// `?1` still stored: the step's peel lock, which never waits.
        peel_claim = "SELECT ARRAY(SELECT hash FROM blobs WHERE hash = ANY(?1::text[])
                 ORDER BY hash FOR UPDATE SKIP LOCKED),
             ARRAY(SELECT hash FROM blobs WHERE hash = ANY(?1::text[]))";

        /// Every edge of parents `?1` to children `?2` (raw digests) not
        /// stored yet: a supplied node another commit installed already has
        /// its edges.
        insert_edges_once = "INSERT INTO blob_edges (parent_hash, child_hash)
             SELECT encode(staged.parent, 'hex'), encode(staged.child, 'hex')
             FROM UNNEST(?1::bytea[], ?2::bytea[]) AS staged(parent, child)
             ORDER BY 1, 2
             ON CONFLICT (parent_hash, child_hash) DO NOTHING";
    }
}

struct Sql {
    pages: PageStatements,
    postgres: PagePostgresStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    Sql {
        pages: PageStatements::render(dialect),
        postgres: PagePostgresStatements::render(dialect),
    }
});

/// The hold statements the one-call commit runs: the lock, then the rows.
pub(super) fn hold_statements() -> (&'static Rendered, &'static Rendered) {
    (&SQL.postgres.hold_lock, &SQL.postgres.hold)
}

/// A lease's acquisition, renewal and release.
const PAGES_LEASE: CommitLabel = CommitLabel::new("pages.lease");

/// One retirement step.
const PAGES_RETIRE: CommitLabel = CommitLabel::new("pages.retire");

/// The advisory key retirement steps serialize on.
const RETIREMENT_KEY: &str = "lash.pages.retirement";

/// Edge pairs one statement inserts.
const EDGE_BATCH: usize = 4096;

/// The nodes one peel statement of a retirement step examines.
const PEEL_LEVEL: usize = 8;

/// The frontier digests a retirement step keeps in memory; the rest are
/// held at once, for later steps.
const FRONTIER_CAP: usize = 4096;

/// The most manifest bytes the last candidate install held at once: what
/// the bounded-memory law reads.
#[cfg(any(test, feature = "testing"))]
static CANDIDATE_PEAK_BUFFERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Arm or disarm a named fault at the hold boundary for conformance laws.
#[cfg(feature = "testing")]
pub(crate) async fn cut_retirement_holds(
    pool: &sqlx::PgPool,
    trigger: &str,
    armed: bool,
) -> sqlx::Result<()> {
    let function = format!("lash_{trigger}").replace('"', "\"\"");
    let trigger = trigger.replace('"', "\"\"");
    if armed {
        sqlx::query(&format!(
            "CREATE OR REPLACE FUNCTION \"{function}\"() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected retirement hold failure'; END $$"
        ))
        .execute(pool)
        .await?;
        sqlx::query(&format!(
            "CREATE TRIGGER \"{trigger}\" BEFORE INSERT ON lash_retirement_holds \
             FOR EACH ROW EXECUTE FUNCTION \"{function}\"()"
        ))
        .execute(pool)
        .await?;
    } else {
        sqlx::query(&format!(
            "DROP TRIGGER IF EXISTS \"{trigger}\" ON lash_retirement_holds"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Holds every stored node of `closures`, the closure under each, and of
/// `alone`, each node alone, queued at `now`: the publication's half of
/// D-RET, and where a lease release leaves its root. Every node is
/// key-share locked before any hold row is written (lock order 4). Answers
/// how many stored nodes it held.
async fn hold(
    tx: &mut PgConnection,
    closures: &BTreeSet<String>,
    alone: &BTreeSet<String>,
    now: DurableInstant,
) -> Result<usize, sqlx::Error> {
    if closures.is_empty() && alone.is_empty() {
        return Ok(0);
    }
    let closures: Vec<&str> = closures.iter().map(String::as_str).collect();
    let alone: Vec<&str> = alone.iter().map(String::as_str).collect();
    let locked: Vec<String> = crate::observed_sql::query_scalar(SQL.postgres.hold_lock.sql())
        .bind(&closures)
        .bind(&alone)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await?;
    if locked.is_empty() {
        return Ok(0);
    }
    crate::observed_sql::query(SQL.postgres.hold.sql())
        .bind(&closures)
        .bind(&alone)
        .bind(now.0)
        .bind(&locked)
        .execute(crate::observed_sql::executor(tx))
        .await?;
    Ok(locked.len())
}

/// Holds what `retired` recorded, in its committing transaction: each
/// retired root's closure, and each ended turn's PhaseRoot alone.
pub(crate) async fn hold_retired(
    tx: &mut PgConnection,
    retired: Retired,
    now: DurableInstant,
) -> Result<(), sqlx::Error> {
    let (closures, phase_ends) = retired.into_parts();
    hold(tx, &closures, &phase_ends, now).await.map(drop)
}

/// Holds the closure of each stored root of `roots`, the roots the
/// caller's transaction retired outside an owner commit (a session's
/// delete, a released revision), queued at `now`. Nothing is peeled here:
/// however large the closures, the transaction writes one hold row per
/// root and never meets a node another transaction peel-locked, so nothing
/// is skipped and left to the orphan audit; retirement steps delete what
/// nothing else reaches later, in bounded transactions. The caller holds
/// once, after every other lock it takes (lock order 4). Answers how many
/// stored roots it held.
pub(crate) async fn retire_roots(
    tx: &mut PgConnection,
    roots: &BTreeSet<String>,
    now: DurableInstant,
) -> Result<usize, sqlx::Error> {
    hold(tx, roots, &BTreeSet::new(), now).await
}

/// Puts `nodes` on the step's frontier, the closure under each when
/// `closure`, queued at `now`. Only a step, under the advisory key, writes
/// the frontier, so this never waits (lock order 6).
async fn frontier_put(
    tx: &mut PgConnection,
    nodes: &BTreeSet<String>,
    closure: bool,
    now: DurableInstant,
) -> Result<(), sqlx::Error> {
    if nodes.is_empty() {
        return Ok(());
    }
    let nodes: Vec<&str> = nodes.iter().map(String::as_str).collect();
    crate::observed_sql::query(SQL.postgres.frontier_put.sql())
        .bind(&nodes)
        .bind(closure)
        .bind(now.0)
        .execute(crate::observed_sql::executor(tx))
        .await?;
    Ok(())
}

/// Records `admission` with its root, in the publishing transaction.
pub(super) async fn admit(
    tx: &mut PgConnection,
    admission: &SemanticAdmission,
) -> Result<(), GraphRefusal> {
    let hash = admission.root.to_hex();
    let stored: Vec<(String, i16)> =
        crate::observed_sql::query_as(SQL.postgres.lock_children.sql())
            .bind(vec![hash.clone()])
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
    match stored.first() {
        None => return Err(GraphRefusal::MissingNode(admission.root)),
        Some((_, encoding)) => {
            if graph::encoding_of(*encoding)? != BlobEncoding::Graph {
                return Err(GraphRefusal::EncodingConflict(admission.root));
            }
        }
    }
    crate::observed_sql::query(SQL.pages.admission_put.sql())
        .bind(hash)
        .bind(i64::from(admission.kernel))
        .bind(i64::from(admission.page_format))
        .bind(admission.validator.as_str())
        .bind(&admission.commit.0[..])
        .execute(crate::observed_sql::executor(tx))
        .await
        .map_err(db)?;
    Ok(())
}

/// Installs `candidate` with the owner commit's own `checked` graph, in the
/// caller's fenced transaction: every node the stream names is checked
/// present and every node it supplies is compared or inserted, in global
/// digest order a batch at a time; then every supplied node's edges once
/// all node rows exist, each child checked stored. Nothing a named node
/// reaches is read.
pub(super) async fn install_candidate(
    tx: &mut PgConnection,
    writes: &[DomainWrite],
    checked: &CheckedGraph,
    candidate: &mut CandidatePublication,
) -> Result<(), GraphRefusal> {
    let summary = candidate.manifest.summary();
    check_candidate(
        writes,
        checked,
        &candidate.owner,
        &summary,
        &candidate.admission,
    )?;
    let mut install =
        CandidateInstall::new(checked, &mut *candidate.manifest, CandidateBuffers::DEFAULT);
    let mut installed = InstalledGraphMeasurement::default();
    while let Some(batch) = install.next_batch()? {
        install_nodes(tx, &batch, &mut installed).await?;
    }
    install.restart();
    while let Some(batch) = install.next_batch()? {
        install_edges(tx, &batch, &mut installed).await?;
    }
    #[cfg(any(test, feature = "testing"))]
    CANDIDATE_PEAK_BUFFERED.store(install.peak_buffered(), std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
        window.installed_graph(installed.newly_installed_canonical_bytes);
    }
    Ok(())
}

/// One batch's nodes: each named node key-share locked and checked stored
/// (a published root as a graph node), each supplied node locked and
/// compared with what is stored under its digest, or inserted.
async fn install_nodes(
    tx: &mut PgConnection,
    batch: &[InstallEntry],
    installed: &mut InstalledGraphMeasurement,
) -> Result<(), GraphRefusal> {
    let mut named = Vec::new();
    let mut supplied: Vec<&CheckedNode> = Vec::new();
    for entry in batch {
        match entry {
            InstallEntry::Named { id, .. } => named.push(id.to_hex()),
            InstallEntry::Supplied(node) => supplied.push(node),
        }
    }
    let digests: Vec<&[u8]> = supplied.iter().map(|node| &node.id.0[..]).collect();
    let rows: Vec<(String, i16, Option<Vec<u8>>)> =
        crate::observed_sql::query_as(graph::lock_nodes().sql())
            .bind(&named)
            .bind(&digests)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
    let mut present = BTreeMap::new();
    for (hash, encoding, bytes) in rows {
        present.insert(hash, (graph::encoding_of(encoding)?, bytes));
    }
    for entry in batch {
        if let InstallEntry::Named { id, root } = entry {
            match present.get(&id.to_hex()) {
                None => return Err(GraphRefusal::MissingNode(*id)),
                Some((BlobEncoding::Opaque, _)) if *root => {
                    return Err(GraphRefusal::EncodingConflict(*id));
                }
                Some(_) => {}
            }
        }
    }
    let hashes: Vec<String> = supplied.iter().map(|node| node.id.to_hex()).collect();
    let mut absent = Vec::new();
    for (node, hash) in supplied.iter().zip(&hashes) {
        match present.get(hash) {
            None => absent.push((*node, hash)),
            Some((encoding, bytes)) => {
                graph::compare(node.id, &node.bytes, *encoding, bytes.as_deref())?;
            }
        }
    }
    if absent.is_empty() {
        return Ok(());
    }
    let inserted = graph::insert_absent(tx, &absent).await?;
    for (node, hash) in &absent {
        if inserted.contains(*hash) {
            installed.newly_installed_canonical_bytes += node.bytes.len() as u64;
            installed.new_nodes += 1;
            installed.sql_logical_bytes += blob_row_logical_bytes(node.bytes.len() as u64);
        }
    }
    Ok(())
}

/// One batch's edges: every child a supplied node's envelope names is
/// key-share locked and must be stored by now, then each `(parent, child)`
/// row not yet stored is inserted.
async fn install_edges(
    tx: &mut PgConnection,
    batch: &[InstallEntry],
    installed: &mut InstalledGraphMeasurement,
) -> Result<(), GraphRefusal> {
    let mut pairs: Vec<(NodeId, NodeId)> = Vec::new();
    for entry in batch {
        if let InstallEntry::Supplied(node) = entry {
            pairs.extend(node.children.iter().map(|child| (node.id, *child)));
        }
    }
    for chunk in pairs.chunks(EDGE_BATCH) {
        let children: BTreeSet<NodeId> = chunk.iter().map(|(_, child)| *child).collect();
        let wanted: Vec<String> = children.iter().map(NodeId::to_hex).collect();
        let present: BTreeSet<String> = crate::observed_sql::query_as::<
            sqlx::Postgres,
            (String, i16),
        >(SQL.postgres.lock_children.sql())
        .bind(&wanted)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .into_iter()
        .map(|(hash, _)| hash)
        .collect();
        if let Some(missing) = children
            .iter()
            .find(|child| !present.contains(&child.to_hex()))
        {
            return Err(GraphRefusal::MissingNode(*missing));
        }
        let parents: Vec<&[u8]> = chunk.iter().map(|(parent, _)| &parent.0[..]).collect();
        let children: Vec<&[u8]> = chunk.iter().map(|(_, child)| &child.0[..]).collect();
        let inserted = crate::observed_sql::query(SQL.postgres.insert_edges_once.sql())
            .bind(&parents)
            .bind(&children)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?
            .rows_affected();
        installed.new_edges += inserted;
        installed.sql_logical_bytes += inserted * EDGE_ROW_LOGICAL_BYTES;
    }
    Ok(())
}

fn owner_bytes(owner: &RootOwner) -> Vec<u8> {
    let mut out = lash_core_store::checkpoint_graph::TokenWriter::new();
    encode_owner(&mut out, owner);
    out.into_bytes()
}

/// Whether `actor` is owned at `epoch`, share-locked for the caller's
/// transaction; when not, the refusal.
async fn fence(
    tx: &mut PgConnection,
    actor: &ActorKey,
    epoch: Epoch,
) -> Result<Result<(), DurableError>, DurableError> {
    let row: Option<(i64, String)> = crate::observed_sql::query_as(SQL.postgres.actor_fence.sql())
        .bind(actor.as_str())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(match row {
        Some((current, state)) if current == epoch.0 && state == "owned" => Ok(()),
        row => Err(DurableError::OwnershipLost(Fenced {
            actor: actor.clone(),
            held: epoch,
            current: row.map(|(current, _)| Epoch(current)),
        })),
    })
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

/// One level of a step's peel: locks what of `level` no other transaction
/// holds, then deletes every locked node nothing names or reaches. Answers
/// how many it deleted, the children their edges reached, and the stored
/// nodes it could not lock; a node gone already is neither.
async fn peel(
    tx: &mut PgConnection,
    level: &BTreeSet<String>,
) -> Result<(u64, BTreeSet<String>, BTreeSet<String>), sqlx::Error> {
    let wanted: Vec<&str> = level.iter().map(String::as_str).collect();
    let (locked, stored): (Vec<String>, Vec<String>) =
        crate::observed_sql::query_as(SQL.postgres.peel_claim.sql())
            .bind(&wanted)
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await?;
    let skipped: BTreeSet<String> = stored
        .into_iter()
        .filter(|node| !locked.contains(node))
        .collect();
    if locked.is_empty() {
        return Ok((0, BTreeSet::new(), skipped));
    }
    let (gone, children): (i64, Vec<String>) =
        crate::observed_sql::query_as(graph::peel_level().sql())
            .bind(&locked)
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await?;
    Ok((
        u64::try_from(gone).unwrap_or_default(),
        children.into_iter().collect(),
        skipped,
    ))
}

/// The next node a step peels from: its own frontier first, oldest first,
/// then the oldest hold no other transaction holds. Answers the node and
/// whether its closure is peeled.
async fn next_retired(tx: &mut PgConnection) -> Result<Option<(String, bool)>, sqlx::Error> {
    let continued: Option<(String, bool)> =
        crate::observed_sql::query_as(SQL.pages.frontier_take.sql())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await?;
    if continued.is_some() {
        return Ok(continued);
    }
    let root: Option<String> = crate::observed_sql::query_scalar(SQL.postgres.hold_next.sql())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await?;
    let Some(root) = root else {
        return Ok(None);
    };
    let closure: bool = crate::observed_sql::query_scalar(SQL.pages.hold_take.sql())
        .bind(&root)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await?;
    Ok(Some((root, closure)))
}

/// One retirement step in the caller's transaction (see the module docs).
/// After the advisory key it waits on nothing.
async fn retire_step_in(
    tx: &mut PgConnection,
    budget: RetirementBudget,
    now: DurableInstant,
) -> Result<RetirementProgress, sqlx::Error> {
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(RETIREMENT_KEY)
    .execute(crate::observed_sql::executor(&mut *tx))
    .await?;
    let mut progress = RetirementProgress::default();
    let limit = budget.nodes.max(1);
    let expired: Vec<i64> = crate::observed_sql::query_scalar(SQL.postgres.leases_expired.sql())
        .bind(now.0)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    let mut reaped = BTreeSet::new();
    for lease in expired {
        let root: Option<String> = crate::observed_sql::query_scalar(SQL.pages.lease_reap.sql())
            .bind(lease)
            .bind(now.0)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await?;
        if let Some(root) = root {
            reaped.insert(root);
            progress.leases_reaped += 1;
        }
    }
    frontier_put(tx, &reaped, true, now).await?;
    let mut remaining = limit;
    // A step examines at most a few times its deletion budget: a frontier
    // something else still names costs reads, not deletions.
    let mut examinable = remaining
        .saturating_mul(4)
        .saturating_add(PEEL_LEVEL as u64);
    let mut frontier = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    while remaining > 0 && examinable > 0 {
        if frontier.is_empty() {
            // The next retired node: a closure joins the frontier, an
            // ended phase's root is peeled alone.
            let Some((node, closure)) = next_retired(tx).await? else {
                break;
            };
            progress.holds_advanced += 1;
            if !closure {
                examinable -= 1;
                let (gone, _, missed) = peel(tx, &BTreeSet::from([node])).await?;
                progress.nodes_deleted += gone;
                remaining = remaining.saturating_sub(gone);
                frontier_put(tx, &missed, false, now).await?;
                continue;
            }
            frontier.insert(node);
        }
        // The next nodes in digest order, within the budget.
        let take = usize::try_from(remaining.min(examinable))
            .unwrap_or(usize::MAX)
            .min(PEEL_LEVEL);
        let level: BTreeSet<String> = frontier.iter().take(take).cloned().collect();
        for node in &level {
            frontier.remove(node);
        }
        examinable -= level.len() as u64;
        let (gone, children, missed) = peel(tx, &level).await?;
        progress.nodes_deleted += gone;
        remaining = remaining.saturating_sub(gone);
        skipped.extend(missed);
        frontier.extend(children);
        if let Some(first_excess) = frontier.iter().nth(FRONTIER_CAP).cloned() {
            let excess = frontier.split_off(&first_excess);
            frontier_put(tx, &excess, true, now).await?;
        }
    }
    // What another transaction held is retired by a later step.
    frontier.extend(skipped);
    frontier_put(tx, &frontier, true, now).await?;
    Ok(progress)
}

impl PostgresDurableStore {
    /// The most manifest bytes the last candidate install held at once.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn candidate_peak_buffered_for_testing() -> u64 {
        CANDIDATE_PEAK_BUFFERED.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Runs `body` in one guarded transaction on `label`'s capacity: its
    /// answer commits it, a refusal rolls it back.
    async fn page_write<T>(
        &self,
        label: CommitLabel,
        body: impl for<'t> FnOnce(
            &'t mut Tx,
            DurableInstant,
        )
            -> futures_util::future::BoxFuture<'t, Result<T, DurableError>>,
    ) -> Result<T, DurableError> {
        self.within(label.capacity(), async {
            let (mut tx, now) = self.open(label).await?;
            match body(&mut tx, now).await {
                Ok(value) => {
                    tx.commit().await.map_err(sqlx_failure)?;
                    Ok(value)
                }
                Err(error) => {
                    // A failed transaction is gone either way: rolled back
                    // here, or by the server when its connection closes.
                    let _ = tx.rollback().await;
                    Err(error)
                }
            }
        })
        .await
    }
}

#[async_trait::async_trait]
impl PageStore for PostgresDurableStore {
    async fn acquire_root_lease(
        &self,
        request: RootLeaseRequest,
    ) -> Result<RootLease, DurableError> {
        if request.ttl_millis <= 0 {
            return Err(DurableError::Graph(GraphRefusal::LeaseLost));
        }
        self.page_write(PAGES_LEASE, move |tx, now| {
            Box::pin(async move {
                if let LeaseFence::Actor { actor, epoch } = &request.fence {
                    fence(tx, actor, *epoch).await??;
                }
                match graph::root_of(tx, &request.owner).await {
                    Ok(Some(root)) if root == request.expected => {}
                    Ok(Some(_)) => return Err(DurableError::Graph(GraphRefusal::RevisionConflict)),
                    Ok(None) => return Err(DurableError::Graph(GraphRefusal::RootRetired)),
                    Err(refusal) => return Err(DurableError::Graph(refusal)),
                }
                let id: i64 = crate::observed_sql::query_scalar(SQL.pages.lease_insert.sql())
                    .bind(owner_bytes(&request.owner))
                    .bind(request.expected.to_hex())
                    .bind(fence_actor(&request.fence))
                    .bind(fence_epoch(&request.fence))
                    .bind(request.ttl_millis)
                    .bind(now.0)
                    .fetch_one(crate::observed_sql::executor(&mut ***tx))
                    .await
                    .map_err(sqlx_failure)?;
                Ok(RootLease {
                    id: RootLeaseId(id.cast_unsigned()),
                    expires_at: DurableInstant(now.0 + request.ttl_millis),
                    owner: request.owner,
                    root: request.expected,
                    fence: request.fence,
                })
            })
        })
        .await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        let lease = lease.clone();
        self.page_write(PAGES_LEASE, move |tx, now| {
            Box::pin(async move {
                if let LeaseFence::Actor { actor, epoch } = &lease.fence {
                    fence(tx, actor, *epoch).await??;
                }
                let renewed: Option<i64> =
                    crate::observed_sql::query_scalar(SQL.pages.lease_renew.sql())
                        .bind(lease.id.0.cast_signed())
                        .bind(lease.root.to_hex())
                        .bind(fence_epoch(&lease.fence))
                        .bind(now.0)
                        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
                        .await
                        .map_err(sqlx_failure)?;
                match renewed {
                    Some(expires) => Ok(RootLease {
                        expires_at: DurableInstant(expires),
                        ..lease
                    }),
                    None => Err(DurableError::Graph(GraphRefusal::LeaseLost)),
                }
            })
        })
        .await
    }

    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError> {
        self.page_write(PAGES_LEASE, move |tx, now| {
            Box::pin(async move {
                let root: Option<String> =
                    crate::observed_sql::query_scalar(SQL.pages.lease_release.sql())
                        .bind(lease.0.cast_signed())
                        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
                        .await
                        .map_err(sqlx_failure)?;
                // What only the lease kept is peeled by a later step.
                if let Some(root) = root {
                    hold(tx, &BTreeSet::from([root]), &BTreeSet::new(), now)
                        .await
                        .map_err(sqlx_failure)?;
                }
                Ok(())
            })
        })
        .await
    }

    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        let (id, root) = (lease.id, lease.root);
        self.read(|connection| {
            Box::pin(async move {
                let mut snapshot = super::snapshot(connection).await?;
                let now = self.instant_on(&mut snapshot).await?;
                let live: Option<String> =
                    crate::observed_sql::query_scalar(SQL.pages.lease_live.sql())
                        .bind(id.0.cast_signed())
                        .bind(root.to_hex())
                        .bind(now.0)
                        .fetch_optional(crate::observed_sql::executor(&mut *snapshot))
                        .await
                        .map_err(sqlx_failure)?;
                if live.is_none() {
                    return Err(DurableError::Graph(GraphRefusal::LeaseLost));
                }
                let wanted: Vec<String> = pages.iter().map(NodeId::to_hex).collect();
                let rows: Vec<(String, Vec<u8>, i16)> =
                    crate::observed_sql::query_as(SQL.postgres.pages.sql())
                        .bind(&wanted)
                        .fetch_all(crate::observed_sql::executor(&mut *snapshot))
                        .await
                        .map_err(sqlx_failure)?;
                snapshot.rollback().await.map_err(sqlx_failure)?;
                let mut found = BTreeMap::new();
                for (hash, bytes, encoding) in rows {
                    let node = graph::node_of(&hash).map_err(DurableError::Graph)?;
                    match graph::encoding_of(encoding).map_err(DurableError::Graph)? {
                        BlobEncoding::Graph => {}
                        BlobEncoding::Opaque => {
                            return Err(DurableError::Graph(GraphRefusal::EncodingConflict(node)));
                        }
                    }
                    found.insert(node, std::sync::Arc::<[u8]>::from(bytes));
                }
                pages
                    .iter()
                    .map(|page| match found.get(page) {
                        Some(bytes) => Ok(StoredPage {
                            id: *page,
                            bytes: bytes.clone(),
                        }),
                        None => Err(DurableError::Graph(GraphRefusal::MissingNode(*page))),
                    })
                    .collect()
            })
        })
        .await
    }

    async fn commit_candidate(
        &self,
        tx: ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
    ) -> Result<CandidateReceipt, DurableError> {
        let summary = candidate.manifest.summary();
        let candidate = tokio::sync::Mutex::new(candidate);
        let actor = self.commit_owner_with(tx, label, Some(&candidate)).await?;
        Ok(CandidateReceipt {
            actor,
            root: summary.root,
            commit: summary.commit,
        })
    }

    async fn admission(&self, root: NodeId) -> Result<Option<SemanticAdmission>, DurableError> {
        let row: Option<(i64, i64, String, Vec<u8>)> = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query_as(SQL.pages.admission_get.sql())
                        .bind(root.to_hex())
                        .fetch_optional(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some((kernel, page_format, validator, commit_id)) = row else {
            return Ok(None);
        };
        match (
            u32::try_from(kernel),
            u32::try_from(page_format),
            <[u8; 32]>::try_from(commit_id.as_slice()),
        ) {
            (Ok(kernel), Ok(page_format), Ok(commit)) => Ok(Some(SemanticAdmission {
                root,
                kernel,
                page_format,
                validator,
                commit: CommitId(commit),
            })),
            _ => Err(super::corrupt("semantic admission", &root.to_hex())),
        }
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
        self.page_write(PAGES_RETIRE, move |tx, now| {
            Box::pin(async move { retire_step_in(tx, budget, now).await.map_err(sqlx_failure) })
        })
        .await
    }

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError> {
        let (holds, oldest): (i64, Option<i64>) = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query_as(SQL.pages.backlog.sql())
                        .fetch_one(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        Ok(RetirementBacklog {
            holds: holds.cast_unsigned(),
            oldest_at: oldest.map(DurableInstant),
        })
    }
}
