//! The page law of a candidate's single read (FIG-6596,
//! CANDIDATE-BODY-READ-ONCE): an install reads and checks each supplied
//! body once, while it checks the nodes, and its edge pass inserts exactly
//! the candidate's edges from its own authenticated log, reading only the
//! child tables it names and never a body again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core_store::checkpoint_graph::CommitId;
use lash_core_store::checkpoint_graph::EncodedNode;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::TypedBody;
use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EDGE_RECORD_BYTES, EdgeLog, ManifestEntry, ManifestSummary, StageFault,
};
use lash_core_store::store_failure::StoreFailureKind;
use lash_kernel_value::tokens::TokenWriter;

use super::super::{LawBroken, LawOutcome, ensure};
use super::{
    Candidate, Cut, Owned, PageTier, ScriptedManifest, children_of, exec_root, owned, page,
    publish, refusal, simple, staging, sweeps_ascend,
};
use crate::DurableError;

/// The candidate's payload leaves, and how many one keyed leaf names.
const LEAVES: usize = 2_048;
const FANOUT: usize = 64;

/// A payload leaf of `tag` carrying about `len` bytes of text in inline
/// tokens.
pub(super) fn sized_leaf(
    nodes: &mut BTreeMap<NodeId, EncodedNode>,
    tag: &str,
    len: usize,
) -> NodeId {
    let chunk = "x".repeat(200);
    let parts = len.div_ceil(chunk.len());
    let mut out = TokenWriter::new();
    out.seq(1 + parts as u64);
    out.text(tag);
    for _ in 0..parts {
        out.text(&chunk);
    }
    let mut staging = staging();
    let id = staging
        .encode(NodeKind::PAYLOAD_LEAF, TypedBody::logical(out.as_bytes()))
        .unwrap_or_else(|error| panic!("a law's leaf encodes: {error}"));
    let node = staging
        .get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("a law's leaf is staged"));
    nodes.insert(id, node);
    id
}

/// CANDIDATE-BODY-READ-ONCE: a candidate of N supplied bodies and E edges,
/// several install batches long, is read from its manifest in one ascending
/// sweep that answers each body once; its edge pass reads exactly the child
/// table of each manifest page with children, once, and no other byte; and
/// the commit stores exactly its edges. The commit's own execution root,
/// which the manifest supplies too, is one node with its edge once.
///
/// A damaged edge pass writes nothing: a log record whose tag changed, a
/// child table that changed after it was checked, a log that lost bytes,
/// and a lost direct reference the manifest never named. A publication cut
/// once its nodes are inserted and retried over the cut install's log is
/// refused, and retried exactly it commits; repeated after its answer was
/// lost, it writes nothing and its execution row resolves it.
///
/// # Errors
///
/// The first rule broken.
pub async fn candidate_edge_pass_does_not_reread_fresh_bodies(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-read-once").await?;
    let exec = owner.exec("read-once");
    // Committed pages the candidate's keyed leaves name directly.
    let mut base = BTreeMap::new();
    let named: Vec<NodeId> = (0..8)
        .map(|at| {
            page(
                &mut base,
                NodeKind::PAYLOAD_LEAF,
                &format!("read-once named {at}"),
                &[],
            )
        })
        .collect();
    let base_root = page(
        &mut base,
        NodeKind::CHECKPOINT_ROOT,
        "read-once base",
        &named,
    );
    publish(
        tier,
        &owner,
        &simple(&owner.exec("read-once-base"), None),
        Box::new(
            Candidate {
                root: base_root,
                fresh: base,
            }
            .manifest(None, CommitId::mint()),
        ),
    )
    .await?;
    let mut fresh = BTreeMap::new();
    let leaves: Vec<NodeId> = (0..LEAVES)
        .map(|leaf| sized_leaf(&mut fresh, &format!("read-once leaf {leaf}"), 2_000))
        .collect();
    let keyed: Vec<NodeId> = leaves
        .chunks(FANOUT)
        .enumerate()
        .map(|(at, chunk)| {
            let mut children = chunk.to_vec();
            children.push(named[at % named.len()]);
            page(
                &mut fresh,
                NodeKind::KEYED_LEAF,
                &format!("read-once keyed {at}"),
                &children,
            )
        })
        .collect();
    let index = page(&mut fresh, NodeKind::KEYED_INDEX, "read-once index", &keyed);
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "read-once root",
        &[index],
    );
    let exec_node = exec_root(root);
    fresh.insert(exec_node.id, exec_node.clone());
    let candidate = Candidate { root, fresh };
    let manifest = candidate.manifest(None, CommitId::mint());
    let fresh_bytes = manifest.summary.fresh_bytes;
    let reads = manifest.reads();
    let probe = Probe::new(manifest, Damage::None);
    let ranges = Arc::clone(&probe.ranges);
    let edges_before = tier.edge_rows().await;
    publish(tier, &owner, &simple(&exec, None), Box::new(probe)).await?;
    let mut all: Vec<NodeId> = candidate
        .fresh
        .keys()
        .copied()
        .chain(candidate.external())
        .collect();
    all.sort();
    let reads = reads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let (sweeps, complete) = sweeps_ascend(&reads, &all)?;
    ensure!(
        sweeps == 1 && complete == 1,
        "the install swept the manifest {sweeps} times, {complete} of them whole, not once"
    );
    let answered: u64 = reads
        .iter()
        .flat_map(|read| &read.answered)
        .filter_map(|id| candidate.fresh.get(id))
        .map(EncodedNode::len)
        .sum();
    ensure!(
        answered == fresh_bytes,
        "the install read {answered} body bytes of a candidate of {fresh_bytes}"
    );
    let ranges = lock(&ranges).clone();
    let logged: BTreeSet<NodeId> = candidate
        .fresh
        .values()
        .filter(|node| node.id != exec_node.id && !children_of(node).is_empty())
        .map(|node| node.id)
        .collect();
    let read: BTreeSet<NodeId> = ranges.iter().map(|(id, ..)| *id).collect();
    ensure!(
        ranges.len() == logged.len() && read == logged,
        "the edge pass read {} child tables, not each of the manifest's {} pages with children once",
        ranges.len(),
        logged.len()
    );
    for (id, offset, len) in &ranges {
        let node = &candidate.fresh[id];
        let table: Vec<u8> = children_of(node).iter().flat_map(|child| child.0).collect();
        let range = usize::try_from(*offset)
            .ok()
            .and_then(|start| node.bytes.get(start..start.checked_add(*len)?));
        ensure!(
            range == Some(&table[..]),
            "the edge pass read bytes of {id} beyond its child table"
        );
    }
    ensure!(
        installed_edges(tier, &edges_before).await == edges_of(&candidate),
        "the install did not store exactly the candidate's edges"
    );
    let stored = tier.stored().await;
    ensure!(
        candidate.fresh.keys().all(|id| stored.contains(id)),
        "a supplied page is missing"
    );
    damaged_edge_passes_write_nothing(tier, &owner, &named).await?;
    a_retry_never_trusts_an_earlier_install(tier, &owner, named[0]).await
}

/// Every edge of `candidate`'s pages.
fn edges_of(candidate: &Candidate) -> BTreeSet<(NodeId, NodeId)> {
    candidate
        .fresh
        .values()
        .flat_map(|node| {
            children_of(node)
                .into_iter()
                .map(move |child| (node.id, child))
        })
        .collect()
}

async fn installed_edges(
    tier: &dyn PageTier,
    before: &BTreeSet<(NodeId, NodeId)>,
) -> BTreeSet<(NodeId, NodeId)> {
    tier.edge_rows().await.difference(before).copied().collect()
}

/// A small candidate: a leaf and the committed page `named` under a keyed
/// leaf, under its CheckpointRoot.
fn small(tag: &str, named: NodeId) -> Candidate {
    let mut fresh = BTreeMap::new();
    let leaf = page(
        &mut fresh,
        NodeKind::PAYLOAD_LEAF,
        &format!("{tag} leaf"),
        &[],
    );
    let keyed = page(
        &mut fresh,
        NodeKind::KEYED_LEAF,
        &format!("{tag} keyed"),
        &[leaf, named],
    );
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        &format!("{tag} root"),
        &[keyed],
    );
    Candidate { root, fresh }
}

async fn damaged_edge_passes_write_nothing(
    tier: &dyn PageTier,
    owner: &Owned,
    named: &[NodeId],
) -> LawOutcome {
    let exec = owner.exec("read-once-damaged");
    let before = rows(tier).await;
    for (damage, kind) in [
        (Damage::Tag, StoreFailureKind::Corrupt),
        (Damage::Table, StoreFailureKind::Corrupt),
        (Damage::Tail, StoreFailureKind::Unavailable),
    ] {
        let candidate = small(&format!("read-once {damage:?}"), named[0]);
        let manifest = Probe::new(candidate.manifest(None, CommitId::mint()), damage);
        let refused = failure(publish(tier, owner, &simple(&exec, None), Box::new(manifest)).await);
        ensure!(
            refused == Some(kind),
            "an edge pass with {damage:?} answered {refused:?}"
        );
        ensure!(
            rows(tier).await == before,
            "an edge pass with {damage:?} left rows behind"
        );
    }
    // A direct reference the store lost and the manifest never named: the
    // edge pass's own check refuses it.
    tier.drop_blob(&named[1]).await;
    let before = rows(tier).await;
    let candidate = small("read-once missing", named[1]);
    let manifest = Probe::new(
        candidate.manifest(None, CommitId::mint()),
        Damage::Externals,
    );
    let refused = refusal(publish(tier, owner, &simple(&exec, None), Box::new(manifest)).await);
    ensure!(
        refused == Some(GraphRefusal::MissingNode(named[1])),
        "an unnamed lost reference answered {refused:?}"
    );
    ensure!(
        rows(tier).await == before,
        "an unnamed lost reference left rows behind"
    );
    Ok(())
}

async fn a_retry_never_trusts_an_earlier_install(
    tier: &dyn PageTier,
    owner: &Owned,
    named: NodeId,
) -> LawOutcome {
    let exec = owner.exec("read-once-retried");
    let publication = simple(&exec, None);
    let candidate = small("read-once retried", named);
    let commit = CommitId::mint();
    let before = rows(tier).await;
    // Cut once every node row exists: the log is whole, the commit is not.
    let first = Probe::new(
        candidate.manifest(None, commit).cut(Cut::Edges),
        Damage::None,
    );
    let log = Arc::clone(&first.log);
    ensure!(
        publish(tier, owner, &publication, Box::new(first))
            .await
            .is_err(),
        "a cut publication committed"
    );
    ensure!(
        rows(tier).await == before && !lock(&log).is_empty(),
        "a cut publication left rows behind or logged nothing"
    );
    let mut replay = Probe::new(candidate.manifest(None, commit), Damage::Replay);
    replay.log = log;
    let refused = failure(publish(tier, owner, &publication, Box::new(replay)).await);
    ensure!(
        refused == Some(StoreFailureKind::Corrupt),
        "a retry over an earlier install's log answered {refused:?}"
    );
    ensure!(
        rows(tier).await == before,
        "a retry over an earlier install's log left rows behind"
    );
    let exact = || Box::new(Probe::new(candidate.manifest(None, commit), Damage::None));
    publish(tier, owner, &publication, exact()).await?;
    let exec_root = exec_root(candidate.root).id;
    let mut expected = edges_of(&candidate);
    expected.insert((exec_root, candidate.root));
    ensure!(
        installed_edges(tier, &before.2).await == expected,
        "the retried install did not store exactly its edges"
    );
    // Its answer lost, the publication repeated writes nothing, and the
    // execution row resolves it.
    let committed = rows(tier).await;
    ensure!(
        publish(tier, owner, &publication, exact()).await.is_err(),
        "a committed candidate was applied twice"
    );
    ensure!(
        rows(tier).await == committed,
        "a repeated candidate changed rows"
    );
    ensure!(
        tier.pages().candidate_state(&exec).await? == Some((exec_root, commit)),
        "the execution row does not resolve the committed candidate"
    );
    Ok(())
}

/// Each child-table read an install made: the page, offset and length.
type Ranges = Arc<Mutex<Vec<(NodeId, u64, usize)>>>;

/// Damage a [`Probe`] does to an install's edge pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Damage {
    None,
    /// A tag byte of the log's first record flips as it is read back.
    Tag,
    /// The log loses the last byte of every append.
    Tail,
    /// A child table changes after the node pass checked it.
    Table,
    /// The log keeps an earlier install's records ahead of this one's.
    Replay,
    /// The manifest omits the candidate's committed references.
    Externals,
}

/// A scripted manifest that records each child-table read and does its
/// damage, its edge log in `log`.
struct Probe {
    inner: ScriptedManifest,
    ranges: Ranges,
    damage: Damage,
    log: Arc<Mutex<Vec<u8>>>,
}

impl Probe {
    fn new(inner: ScriptedManifest, damage: Damage) -> Self {
        Self {
            inner,
            ranges: Ranges::default(),
            damage,
            log: Arc::default(),
        }
    }
}

impl CandidateManifest for Probe {
    fn summary(&self) -> ManifestSummary {
        self.inner.summary()
    }

    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault> {
        let mut entries = self.inner.entries(after, limit)?;
        if self.damage == Damage::Externals {
            entries.retain(|entry| matches!(entry, ManifestEntry::Fresh(_)));
        }
        Ok(entries)
    }

    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
        lock(&self.ranges).push((*id, offset, len));
        let mut range = self.inner.fresh_range(id, offset, len)?;
        if self.damage == Damage::Table
            && let Some(last) = range.last_mut()
        {
            *last ^= 1;
        }
        Ok(range)
    }

    fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
        if self.damage != Damage::Replay {
            lock(&self.log).clear();
        }
        Ok(Box::new(ProbeLog {
            bytes: Arc::clone(&self.log),
            damage: self.damage,
        }))
    }
}

struct ProbeLog {
    bytes: Arc<Mutex<Vec<u8>>>,
    damage: Damage,
}

impl EdgeLog for ProbeLog {
    fn append(&mut self, bytes: &[u8]) -> Result<(), StageFault> {
        let kept = if self.damage == Damage::Tail {
            &bytes[..bytes.len() - 1]
        } else {
            bytes
        };
        lock(&self.bytes).extend_from_slice(kept);
        Ok(())
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), StageFault> {
        let log = lock(&self.bytes);
        let bytes = usize::try_from(offset)
            .ok()
            .and_then(|start| log.get(start..start.checked_add(out.len())?))
            .ok_or_else(|| StageFault::Io("the edge log ended early".into()))?;
        out.copy_from_slice(bytes);
        if self.damage == Damage::Tag && offset == 0 {
            out[EDGE_RECORD_BYTES as usize - 1] ^= 1;
        }
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The rows a refused install must leave as they were.
async fn rows(tier: &dyn PageTier) -> (BTreeSet<NodeId>, Vec<NodeId>, BTreeSet<(NodeId, NodeId)>) {
    (
        tier.stored().await,
        tier.catalog().await,
        tier.edge_rows().await,
    )
}

/// The kind of store failure `result` answered, whether the dialect
/// reports it as a store failure or as the graph refusal that carries it.
fn failure(result: Result<impl std::fmt::Debug, DurableError>) -> Option<StoreFailureKind> {
    match result {
        Err(DurableError::Store(failure) | DurableError::Graph(GraphRefusal::Backend(failure))) => {
            Some(failure.kind)
        }
        _ => None,
    }
}
