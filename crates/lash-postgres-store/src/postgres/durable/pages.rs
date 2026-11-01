//! Persistent-heap pages on PostgreSQL (FIG-6191, S6): root leases, demand
//! page reads, a candidate's streamed install beside its owner commit, and
//! the bounded retirement steps that peel retired roots off the
//! publication path.
//!
//! # Lock order
//!
//! One global order for every transaction that takes these locks
//! (FIG-6193); each class is taken in ascending digest order:
//!
//! 1. the retirement advisory key (steps only), or the owner's fence row;
//! 2. blob rows a writer names or installs: key-share locks and inserts, a
//!    candidate's batch at a time, then the children its edges name;
//! 3. the root catalog's owning rows the commit's domain writes change;
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
//! first, before it takes the oldest hold. Each transaction peels one
//! bounded candidate set in digest order and puts every node its peel
//! skipped and the frontier it did not reach back on the frontier,
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
    RetirementBudget, RetirementProgress, RootLease, RootLeaseId, RootLeaseRequest, StoredPage,
};
use lash_durable::domain::{DomainWrite, ExecKey};
use lash_durable::{
    ActorKey, ActorTx, CommitLabel, DurableError, DurableInstant, DurableReads, Epoch, Fenced,
};
use lash_store_sql::durable::pages::PageStatements;
use lash_store_sql::{Dialect, Rendered};
use sqlx::PgConnection;

use super::graph::{self, db};
use super::steps::{Collect, Entry, Shape, Step, Ty, Value};
use super::{PostgresDurableStore, Retired, Tx, sqlx_failure};

lash_store_sql::statements! {
    /// Page statements only PostgreSQL issues: the array binding of a node
    /// list, its boolean holds and the row locks a lease and a step take.
    pub(crate) struct PagePostgresStatements @ "durable_pages_postgres" {
        /// Lease execution `?7`'s root `?2`, owned as `?1`, to actor `?3`
        /// at epoch `?4` for `?5` ms from `?6`, when `?2` is the root the
        /// execution holds: answers the lease's id. A one-call owner
        /// commit's lease of the root it publishes (FIG-7585).
        lease_exec_root = "INSERT INTO root_leases (owner_key, root_ref, actor_key, epoch, ttl_ms, expires_at_ms)
             SELECT ?1, snapshot.root_ref, ?3, ?4, ?5, ?6 + ?5 FROM exec_snapshots AS snapshot
             WHERE snapshot.exec_key = ?7
               AND snapshot.root_ref = (SELECT id FROM blobs WHERE hash = ?2)
             RETURNING lease_id";

        /// Key-share lock every stored node of `?1` and `?2`, in digest
        /// order, and answer them: what a hold may name, locked against a
        /// peel before any hold row is written.
        hold_lock = "SELECT ARRAY(SELECT hash FROM blobs
             WHERE hash = ANY(?1::bytea[]) OR hash = ANY(?2::bytea[])
             ORDER BY hash FOR KEY SHARE)";

        /// Hold every node of `?4` (locked by [`Self::hold_lock`]) that
        /// `?1` names as a closure or `?2` as a node alone, queued at `?3`,
        /// in digest order. A node already held keeps the wider hold and
        /// its place in the queue.
        hold = "INSERT INTO retirement_holds AS held (root_ref, closure, queued_at_ms)
             SELECT stored.id, bool_or(node.closure), ?3::bigint
             FROM (SELECT UNNEST(?1::bytea[]) AS hash, TRUE AS closure
                   UNION ALL SELECT UNNEST(?2::bytea[]), FALSE) AS node
             JOIN blobs AS stored ON stored.hash = node.hash
             WHERE node.hash = ANY(?4::bytea[])
             GROUP BY stored.id
             ORDER BY stored.id
             ON CONFLICT (root_ref) DO UPDATE SET closure = held.closure OR excluded.closure";

        /// The oldest hold no other transaction holds, locked for its step:
        /// its node id and digest.
        hold_next = "SELECT held.root_ref, root.hash FROM retirement_holds AS held
             JOIN blobs AS root ON root.id = held.root_ref
             ORDER BY held.queued_at_ms, held.root_ref LIMIT 1 FOR UPDATE OF held SKIP LOCKED";

        /// End lease `?1` if it expired by `?2`; answers the digest of the
        /// root it held.
        lease_reap = "DELETE FROM root_leases WHERE lease_id = ?1 AND expires_at_ms <= ?2
             RETURNING (SELECT hash FROM blobs WHERE id = root_ref) AS root_ref";

        /// Take the oldest `?1` nodes of the steps' frontier, in one
        /// statement; answers each one still stored, by digest, and whether
        /// its closure is peeled.
        frontier_take = "WITH taken AS (DELETE FROM retirement_frontier
                 WHERE node_ref IN (SELECT node_ref FROM retirement_frontier
                     ORDER BY queued_at_ms, node_ref LIMIT ?1)
                 RETURNING node_ref, closure)
             SELECT node.hash, taken.closure FROM taken JOIN blobs AS node ON node.id = taken.node_ref";

        /// Put every node of `?1` on the step's frontier, its closure when
        /// `?2`, queued at `?3`. A node already there keeps the wider
        /// peel and its place in the queue.
        frontier_put = "INSERT INTO retirement_frontier AS queued (node_ref, closure, queued_at_ms)
             SELECT stored.id, ?2::boolean, ?3::bigint
             FROM UNNEST(?1::bytea[]) AS node(hash) JOIN blobs AS stored ON stored.hash = node.hash
             ORDER BY stored.id
             ON CONFLICT (node_ref) DO UPDATE SET closure = queued.closure OR excluded.closure";

        /// Up to `?2` leases expired by `?1` no other transaction holds,
        /// oldest first.
        leases_expired = "SELECT lease_id FROM root_leases WHERE expires_at_ms <= ?1
             ORDER BY expires_at_ms, lease_id LIMIT ?2 FOR UPDATE SKIP LOCKED";

        /// Actor `?1`'s epoch and state, share-locked: a lease's fence.
        actor_fence = "SELECT epoch, state FROM actors WHERE actor_key = ?1 FOR SHARE";

        /// Whether lease `?1` on root `?2` is live at `?3`, or at the
        /// database's clock (`clock_timestamp()`, read once) when `?3` is
        /// null, as one row with no hash; and only when it is, the hash,
        /// bytes and encoding of every blob of `?4`. One statement, so one
        /// snapshot answers the lease and its pages: a release or
        /// retirement step committed meanwhile is seen whole or not at all.
        leased_pages = "WITH clock AS MATERIALIZED (
                 SELECT COALESCE(?3::bigint,
                     (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT) AS now_ms),
             lease AS MATERIALIZED (
                 SELECT EXISTS (SELECT 1 FROM root_leases, clock
                     WHERE lease_id = ?1 AND root_ref = COALESCE((SELECT id FROM blobs WHERE hash = ?2), CASE WHEN ?2 IS NOT NULL THEN -1 END)
                         AND expires_at_ms > clock.now_ms)
                     AS live)
             SELECT NULL::bytea, NULL::bytea, NULL::smallint, lease.live FROM lease
             UNION ALL
             SELECT blobs.hash, blobs.content, blobs.encoding, NULL FROM lease JOIN blobs
                 ON lease.live AND blobs.hash = ANY(?4::bytea[])";

        /// Every stored blob of `?1` with its encoding, key-share locked
        /// in digest order: a candidate's children, held against a peel.
        lock_children = "SELECT hash, encoding FROM blobs WHERE hash = ANY(?1::bytea[])
             ORDER BY hash FOR KEY SHARE";

        /// Every edge of parents `?1` to children `?2` (raw digests) not
        /// stored yet: a supplied node another commit installed already has
        /// its edges.
        insert_edges_once = "INSERT INTO blob_edges (parent_id, child_id)
             SELECT parent.id, child.id
             FROM UNNEST(?1::bytea[], ?2::bytea[]) AS staged(parent, child)
             JOIN blobs AS parent ON parent.hash = staged.parent
             JOIN blobs AS child ON child.hash = staged.child
             ORDER BY 1, 2
             ON CONFLICT (parent_id, child_id) DO NOTHING";
    }
}

struct Sql {
    pages: PageStatements,
    postgres: PagePostgresStatements,
}

/// One row of [`PagePostgresStatements::leased_pages`]: the lease's row
/// (no hash, its liveness) or a page's hash, bytes and encoding.
type LeasedPageRow = (
    Option<crate::keys::Key>,
    Option<Vec<u8>>,
    Option<i16>,
    Option<bool>,
);

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

/// A one-call owner commit's lease of an execution root it publishes: one
/// row, the lease's id, or the call is refused (FIG-7585).
pub(super) static LEASE_ROOT: Entry = Entry::new(
    || &SQL.postgres.lease_exec_root,
    &[
        Ty::Bytes,
        Ty::Key,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Now,
        Ty::Text,
    ],
    Shape::Returning,
)
.collecting(Collect::Lease("lease_id"));

/// A one-call owner commit's release of a lease: the root it held is
/// retired once nothing else holds it. A lease already gone releases
/// nothing.
pub(super) static RELEASE_LEASE: Entry =
    Entry::new(|| &SQL.pages.lease_release, &[Ty::Int], Shape::Returning).retiring("root_ref");

/// The steps that lease the roots `tx` publishes, in request order; none
/// when a request is not an owner's lease of an execution root, which the
/// explicit form takes.
pub(super) fn lease_steps(tx: &lash_durable::ActorTx) -> Option<Vec<Step>> {
    tx.root_leases()
        .iter()
        .map(|request| {
            let RootOwner::Exec(exec) = &request.owner else {
                return None;
            };
            let LeaseFence::Actor { actor, epoch } = &request.fence else {
                return None;
            };
            if request.ttl_millis <= 0 || actor != tx.actor() || *epoch != tx.epoch() {
                return None;
            }
            Some(Step::one(
                &LEASE_ROOT,
                vec![
                    Value::Bytes(Some(owner_bytes(&request.owner))),
                    Value::key(request.expected.to_hex()),
                    Value::text(actor.as_str()),
                    Value::int(epoch.0),
                    Value::int(request.ttl_millis),
                    Value::Now,
                    Value::text(exec.stored()),
                ],
            ))
        })
        .collect()
}

/// The steps that end the leases `tx` releases.
pub(super) fn release_steps(tx: &lash_durable::ActorTx) -> Vec<Step> {
    tx.released_root_leases()
        .iter()
        .map(|lease| {
            Step::new(
                &RELEASE_LEASE,
                vec![Value::int(lease.0.cast_signed())],
                None,
            )
        })
        .collect()
}

/// The leases a one-call commit took for `requests`, by the ids it
/// answered, each from the commit's clock `now`.
pub(super) fn leased(
    requests: &[RootLeaseRequest],
    ids: &[i64],
    now: DurableInstant,
) -> Option<Vec<RootLease>> {
    (requests.len() == ids.len()).then(|| {
        requests
            .iter()
            .zip(ids)
            .map(|(request, id)| RootLease {
                id: RootLeaseId(id.cast_unsigned()),
                expires_at: DurableInstant(now.0 + request.ttl_millis),
                owner: request.owner.clone(),
                root: request.expected,
                fence: request.fence.clone(),
            })
            .collect()
    })
}

/// A lease's acquisition, renewal and release.
const PAGES_LEASE: CommitLabel = CommitLabel::new("pages.lease");

/// One retirement step.
const PAGES_RETIRE: CommitLabel = CommitLabel::new("pages.retire");

/// The advisory key retirement steps serialize on.
const RETIREMENT_KEY: &str = "lash.pages.retirement";

/// Edge pairs one statement inserts.
const EDGE_BATCH: usize = 4096;

/// The retirement frontier limit also bounds one transaction's candidate set.
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
    let locked: Vec<String> = crate::keys::query_scalar(SQL.postgres.hold_lock.sql())
        .bind(crate::keys::keys(&closures))
        .bind(crate::keys::keys(&alone))
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await?;
    if locked.is_empty() {
        return Ok(0);
    }
    crate::observed_sql::query(SQL.postgres.hold.sql())
        .bind(crate::keys::keys(&closures))
        .bind(crate::keys::keys(&alone))
        .bind(now.0)
        .bind(crate::keys::keys(&locked))
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
        .bind(crate::keys::keys(&nodes))
        .bind(closure)
        .bind(now.0)
        .execute(crate::observed_sql::executor(tx))
        .await?;
    Ok(())
}

/// Owner `tx`'s `candidate` as the commit's own graph, when the commit's
/// graph and the candidate together are one install batch of
/// [`CandidateBuffers::DEFAULT`] and their edges one edge batch (FIG-7576):
/// that batch rides in the commit, which installs it with its graph, so
/// the commit is one round trip. A node and its edges commit in one
/// transaction (a stored node has its edges; a child must be stored), and
/// the edge pass follows every node, so a candidate of more batches cannot
/// split across the commit: it installs ahead ([`install_owned_candidate`])
/// and the commit names its root. `None` for those, which the manifest's
/// summary predicts, so a large candidate is still swept once; a stream
/// that outgrows its summary is installed ahead too.
pub(super) fn riding_candidate(
    owner: &ActorTx,
    candidate: &mut CandidatePublication,
) -> Result<Option<CheckedGraph>, GraphRefusal> {
    let buffers = CandidateBuffers::DEFAULT;
    let publications: Vec<_> = owner
        .domain()
        .iter()
        .flat_map(DomainWrite::publications)
        .collect();
    let checked = lash_durable::domain::graph_commit::check_graph(
        owner.graph(),
        owner.reads(),
        &publications,
    )?;
    let summary = candidate.manifest.summary();
    check_candidate(owner.domain(), &checked, &candidate.owner, &summary)?;
    let own_entries = checked.nodes.len() + checked.external.len() + checked.external_roots.len();
    let own_bytes: u64 = checked
        .nodes
        .iter()
        .map(|node| node.bytes.len() as u64)
        .sum();
    let own_edges: usize = checked.nodes.iter().map(|node| node.children.len()).sum();
    let predicted = summary.fresh_nodes + summary.external + own_entries as u64
        <= buffers.entries as u64
        && summary.fresh_bytes + own_bytes < buffers.bytes
        && summary.fresh_edges + own_edges as u64 <= buffers.edges as u64;
    if !predicted {
        return Ok(None);
    }
    let mut install = CandidateInstall::new(&checked, &mut *candidate.manifest, buffers);
    let batch = install.next_batch()?.unwrap_or_default();
    if install.next_batch()?.is_some() {
        return Ok(None);
    }
    // The edge pass checks the manifest's edge log as an install ahead
    // does, so a damaged or cut stream is refused alike.
    let mut pass = install.edges()?;
    if pass.next_batch()?.is_some() && pass.next_batch()?.is_some() {
        return Ok(None);
    }
    #[cfg(any(test, feature = "testing"))]
    CANDIDATE_PEAK_BUFFERED.store(pass.peak_buffered(), std::sync::atomic::Ordering::SeqCst);
    let mut riding = CheckedGraph::default();
    for entry in batch {
        match entry {
            InstallEntry::Supplied(node) => riding.nodes.push(node),
            InstallEntry::Named { id, root: false } => {
                riding.external.insert(id);
            }
            InstallEntry::Named { id, root: true } => {
                riding.external_roots.insert(id);
            }
        }
    }
    // A child neither supplied nor named is still a reference the store
    // must hold.
    let supplied: BTreeSet<NodeId> = riding.nodes.iter().map(|node| node.id).collect();
    let children: Vec<NodeId> = riding
        .nodes
        .iter()
        .flat_map(|node| node.children.iter().copied())
        .filter(|child| !supplied.contains(child))
        .collect();
    riding.external.extend(children);
    Ok(Some(riding))
}

/// `checked` with a [`riding_candidate`]'s graph: every node either
/// supplies, in digest order, and every reference neither supplies.
pub(super) fn with_riding(mut checked: CheckedGraph, riding: &CheckedGraph) -> CheckedGraph {
    let mut nodes: BTreeMap<NodeId, CheckedNode> = std::mem::take(&mut checked.nodes)
        .into_iter()
        .map(|node| (node.id, node))
        .collect();
    for node in &riding.nodes {
        nodes.entry(node.id).or_insert_with(|| node.clone());
    }
    checked.external.extend(riding.external.iter().copied());
    checked.external.retain(|id| !nodes.contains_key(id));
    checked
        .external_roots
        .extend(riding.external_roots.iter().copied());
    checked.external_roots.retain(|id| !nodes.contains_key(id));
    checked.nodes = nodes.into_values().collect();
    checked
}

/// Installs owner `tx`'s `candidate` ahead of its commit, in the caller's
/// transaction of its own: the owner's actor row share-locked at its epoch
/// first, the fence an ownership transfer's epoch bump waits behind, so a
/// fenced activation installs nothing; then [`install_candidate`].
pub(super) async fn install_owned_candidate(
    tx: &mut PgConnection,
    owner: &ActorTx,
    checked: &CheckedGraph,
    candidate: &mut CandidatePublication,
) -> Result<(), DurableError> {
    fence(tx, owner.actor(), owner.epoch()).await??;
    install_candidate(tx, owner.domain(), checked, candidate)
        .await
        .map_err(DurableError::Graph)
}

/// Installs `candidate` with the owner commit's own `checked` graph, in the
/// caller's transaction: every node the stream names is checked
/// present and every node it supplies is compared or inserted, in global
/// digest order a batch at a time; then every supplied node's edges once
/// all node rows exist, each child checked stored. Nothing a named node
/// reaches is read.
async fn install_candidate(
    tx: &mut PgConnection,
    writes: &[DomainWrite],
    checked: &CheckedGraph,
    candidate: &mut CandidatePublication,
) -> Result<(), GraphRefusal> {
    let summary = candidate.manifest.summary();
    check_candidate(writes, checked, &candidate.owner, &summary)?;
    let mut install =
        CandidateInstall::new(checked, &mut *candidate.manifest, CandidateBuffers::DEFAULT);
    let mut installed = InstalledGraphMeasurement::default();
    while let Some(batch) = install.next_batch()? {
        install_nodes(tx, &batch, &mut installed).await?;
    }
    let mut edges = install.edges()?;
    while let Some(pairs) = edges.next_batch()? {
        install_edges(tx, &pairs, &mut installed).await?;
    }
    #[cfg(any(test, feature = "testing"))]
    CANDIDATE_PEAK_BUFFERED.store(edges.peak_buffered(), std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = lash_core_ids::operational_metrics::sql::Window::current() {
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
    let rows: Vec<(crate::keys::Key, i16, Option<Vec<u8>>)> =
        crate::observed_sql::query_as(graph::lock_nodes().sql())
            .bind(crate::keys::keys(&named))
            .bind(&digests)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
    let mut present = BTreeMap::new();
    for (hash, encoding, bytes) in rows {
        present.insert(hash.0, (graph::encoding_of(encoding)?, bytes));
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
    }
    Ok(())
}

/// One batch of supplied nodes' edges: every child is key-share locked and
/// must be stored by now, then each `(parent, child)` row not yet stored is
/// inserted.
async fn install_edges(
    tx: &mut PgConnection,
    pairs: &[(NodeId, NodeId)],
    installed: &mut InstalledGraphMeasurement,
) -> Result<(), GraphRefusal> {
    for chunk in pairs.chunks(EDGE_BATCH) {
        let children: BTreeSet<NodeId> = chunk.iter().map(|(_, child)| *child).collect();
        let wanted: Vec<String> = children.iter().map(NodeId::to_hex).collect();
        let present: BTreeSet<String> = crate::observed_sql::query_as::<
            sqlx::Postgres,
            (crate::keys::Key, i16),
        >(SQL.postgres.lock_children.sql())
        .bind(crate::keys::keys(&wanted))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .into_iter()
        .map(|(hash, _)| hash.0)
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
    let mut out = lash_kernel_value::tokens::TokenWriter::new();
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

/// The next nodes a step peels from: up to `limit` of its own frontier,
/// oldest first, in one statement; else the oldest hold no other
/// transaction holds. Answers each node and whether its closure is peeled.
async fn next_retired(
    tx: &mut PgConnection,
    limit: u64,
) -> Result<Vec<(String, bool)>, sqlx::Error> {
    let continued: Vec<(crate::keys::Key, bool)> =
        crate::observed_sql::query_as(SQL.postgres.frontier_take.sql())
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await?;
    if !continued.is_empty() {
        return Ok(continued
            .into_iter()
            .map(|(key, closure)| (key.0, closure))
            .collect());
    }
    let held: Option<(i64, crate::keys::Key)> =
        crate::observed_sql::query_as(SQL.postgres.hold_next.sql())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await?;
    let Some((id, crate::keys::Key(root))) = held else {
        return Ok(Vec::new());
    };
    let closure: bool = crate::observed_sql::query_scalar(SQL.pages.hold_take.sql())
        .bind(id)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await?;
    Ok(vec![(root, closure)])
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
        let root: Option<String> = crate::keys::query_scalar(SQL.postgres.lease_reap.sql())
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
    // One bounded peel per transaction. Exclusive graph locks never span
    // another retirement batch or a network round trip to fetch its seeds.
    let take = limit.min(FRONTIER_CAP as u64);
    let next = next_retired(tx, take).await?;
    progress.holds_advanced = next.len() as u64;
    let Some((_, closure)) = next.first() else {
        return Ok(progress);
    };
    let closure = *closure;
    let (selected, later): (Vec<_>, Vec<_>) =
        next.into_iter().partition(|(_, reach)| *reach == closure);
    let later = later.into_iter().map(|(node, _)| node).collect();
    frontier_put(tx, &later, !closure, now).await?;
    let selected = selected.into_iter().map(|(node, _)| node).collect();
    let (_, gone, mut pending, missed) =
        graph::peel_set(tx, &selected, take as usize, closure).await?;
    progress.nodes_deleted = gone as u64;
    if !closure {
        pending.clear();
    }
    pending.extend(missed);
    frontier_put(tx, &pending, closure, now).await?;
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

/// The owner transaction already holds its epoch fence.
pub(super) async fn acquire_in(
    tx: &mut Tx,
    request: RootLeaseRequest,
    now: DurableInstant,
) -> Result<RootLease, DurableError> {
    if request.ttl_millis <= 0 {
        return Err(DurableError::Graph(GraphRefusal::LeaseLost));
    }
    match graph::root_of(tx, &request.owner).await {
        Ok(Some(root)) if root == request.expected => {}
        Ok(Some(_)) => return Err(DurableError::Graph(GraphRefusal::RevisionConflict)),
        Ok(None) => return Err(DurableError::Graph(GraphRefusal::RootRetired)),
        Err(refusal) => return Err(DurableError::Graph(refusal)),
    }
    let id: i64 = crate::observed_sql::query_scalar(SQL.pages.lease_insert.sql())
        .bind(owner_bytes(&request.owner))
        .bind(crate::keys::key(request.expected.to_hex()))
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
}

pub(super) async fn release_in(
    tx: &mut Tx,
    lease: RootLeaseId,
) -> Result<Option<String>, DurableError> {
    crate::keys::query_scalar(SQL.pages.lease_release.sql())
        .bind(lease.0.cast_signed())
        .fetch_optional(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(sqlx_failure)
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
                acquire_in(tx, request, now).await
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
                        .bind(crate::keys::key(lease.root.to_hex()))
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
                if let Some(root) = release_in(tx, lease).await? {
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
        let now = self.injected_instant().transpose()?.map(|now| now.0);
        let wanted: Vec<String> = pages.iter().map(NodeId::to_hex).collect();
        let rows: Vec<LeasedPageRow> = self
            .read(|connection| {
                let wanted = &wanted;
                Box::pin(async move {
                    crate::observed_sql::query_as(SQL.postgres.leased_pages.sql())
                        .bind(id.0.cast_signed())
                        .bind(crate::keys::key(root.to_hex()))
                        .bind(now)
                        .bind(crate::keys::keys(wanted))
                        .fetch_all(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let mut live = None;
        let mut returned = Vec::with_capacity(rows.len());
        for (hash, bytes, encoding, status) in rows {
            match (hash, bytes, encoding) {
                (None, _, _) => live = status,
                (Some(hash), Some(bytes), Some(encoding)) => returned.push((hash, bytes, encoding)),
                (Some(hash), _, _) => return Err(super::corrupt("page row", &hash)),
            }
        }
        // The lease answers first: a lost one refuses before any page is
        // decoded or missed.
        match live {
            Some(true) => {}
            Some(false) => return Err(DurableError::Graph(GraphRefusal::LeaseLost)),
            None => return Err(super::corrupt("page read", "without its lease row")),
        }
        let mut found = BTreeMap::new();
        for (hash, bytes, encoding) in returned {
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
