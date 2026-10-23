//! The persistent-heap page store's laws (FIG-6191, S6; D-LEASE, D-RET,
//! D-SPOOL), written once and run by each dialect over its own store
//! through a [`PageTier`].
//!
//! Candidates are synthetic: a [`ScriptedManifest`] streams pages built here
//! from the semantic kinds, and a [`GeneratedManifest`] derives a large one
//! on demand. The store layer never sees a heap.
//!
//! Each law takes a fresh store. A candidate's CheckpointRoot hangs under a
//! small FrozenValue the owner commit stages itself: the execution row's
//! root, which a root lease names.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core_store::checkpoint_graph::CommitId;
use lash_core_store::checkpoint_graph::DecodeLimits;
use lash_core_store::checkpoint_graph::EncodeBudget;
use lash_core_store::checkpoint_graph::EncodedNode;
use lash_core_store::checkpoint_graph::ExplicitLimit;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::TypedBody;
use lash_core_store::checkpoint_graph::node::VerifiedNode;
use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EdgeLog, ManifestEntry, ManifestSummary, MemoryEdgeLog, StageFault,
    node_range,
};
use lash_vocabulary::tokens::TokenWriter;

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::graph::{EffectAdmissionWrite, EffectKey, ExecRootWrite, RootOwner};
use crate::domain::graph_commit::CandidateBuffers;
use crate::domain::pages::{
    CandidateOwner, CandidatePublication, CandidateReceipt, PageStore, RetirementBudget,
    RootLeaseRequest,
};
use crate::domain::{CellId, ExecKey, SnapshotRev};
use crate::{ActorKey, DomainWrite, DurableError, Epoch, NodeLease};

#[path = "pages_driver.rs"]
mod driver;
#[path = "pages_generated.rs"]
mod generated;
#[path = "pages_install.rs"]
mod install;
#[path = "pages_owners.rs"]
mod owners;
#[path = "pages_races.rs"]
mod races;
#[path = "pages_reads.rs"]
mod reads;
pub use driver::{
    a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free,
    a_node_retires_sealed_pages_on_its_own, backpressure_holds_claims_until_retirement_catches_up,
};
pub use generated::GeneratedManifest;
use generated::Recipe;
pub use install::candidate_edge_pass_does_not_reread_fresh_bodies;
pub use owners::{
    a_host_read_never_descends_into_a_heap_page,
    a_pruned_step_outcome_frees_exactly_its_unshared_pages, a_step_outcome_owns_its_candidate,
    deleting_a_session_retires_its_heap_in_bounded_steps,
};
pub use races::{PageLockProbe, publications_and_retirement_on_two_nodes_never_deadlock};
pub use reads::{
    a_candidate_check_fetches_only_the_bytes_it_compares,
    a_page_read_is_one_statement_that_answers_its_lease_first,
    checkpoint_lease_handoffs_are_atomic,
};

/// One dialect's page store and the raw access the laws need.
#[async_trait::async_trait]
pub trait PageTier: Send + Sync {
    fn pages(&self) -> &dyn PageStore;
    /// Another node's store over the same database; a tier with one store
    /// answers it again.
    fn peer(&self) -> &dyn PageStore {
        self.pages()
    }
    /// Every stored blob.
    async fn stored(&self) -> BTreeSet<NodeId>;
    /// Every root the catalog names, one per row.
    async fn catalog(&self) -> Vec<NodeId>;
    /// Every stored edge row, as `(parent, child)`.
    async fn edge_rows(&self) -> BTreeSet<(NodeId, NodeId)>;
    /// Deletes a stored node and its edges past the schema's keys: foreign
    /// damage to a node the store must never read.
    async fn drop_blob(&self, node: &NodeId);
    /// Moves the store's clock forward.
    fn advance(&self, millis: u64);
    /// The most manifest bytes the last candidate install held at once.
    fn peak_buffered(&self) -> u64;
    /// Opens the store again over the same database: what a process that
    /// died between transactions finds. A memory tier keeps its store.
    async fn reopen(&mut self);
    /// The process's resident high-water mark in KiB, where the platform
    /// reports one.
    fn resident_high_water(&self) -> Option<u64>;
    /// Deletes session `session` through the deployment's session catalog,
    /// as a host's delete does, returning its observed SQL row count.
    async fn delete_session(&self, session: &str) -> u64;
}

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// A page of `kind` naming `tag` and `children`, staged in `nodes`.
fn page(
    nodes: &mut BTreeMap<NodeId, EncodedNode>,
    kind: NodeKind,
    tag: &str,
    children: &[NodeId],
) -> NodeId {
    let node = encoded(kind, tag, children);
    let id = node.id;
    nodes.insert(id, node);
    id
}

/// The one node a page of `kind` naming `tag` and `children` is.
fn encoded(kind: NodeKind, tag: &str, children: &[NodeId]) -> EncodedNode {
    let mut out = TokenWriter::new();
    out.seq(1 + children.len() as u64);
    out.text(tag);
    for child in children {
        out.child(child.child_ref());
    }
    let mut staging = staging();
    let id = staging
        .encode(kind, TypedBody::logical(out.as_bytes()))
        .unwrap_or_else(|error| panic!("a law's page encodes: {error}"));
    staging
        .get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("a law's page is staged"))
}

/// A law's page as a spool hands it on: checked.
fn verified(node: EncodedNode) -> VerifiedNode {
    VerifiedNode::check(node, &DecodeLimits::within_total(u64::MAX))
        .unwrap_or_else(|error| panic!("a law's page checks: {error}"))
}

fn children_of(node: &EncodedNode) -> Vec<NodeId> {
    node.children()
        .unwrap_or_else(|error| panic!("a law's page parses: {error}"))
}

/// A candidate: its CheckpointRoot and the pages it adds; what they name
/// beyond those is committed already.
#[derive(Clone)]
pub struct Candidate {
    pub root: NodeId,
    pub fresh: BTreeMap<NodeId, EncodedNode>,
}

impl Candidate {
    /// The committed pages its fresh pages name directly.
    fn external(&self) -> BTreeSet<NodeId> {
        self.fresh
            .values()
            .flat_map(children_of)
            .filter(|child| !self.fresh.contains_key(child))
            .collect()
    }

    fn manifest(&self, base: Option<NodeId>, commit: CommitId) -> ScriptedManifest {
        ScriptedManifest::new(self, base, commit)
    }
}

/// One `entries` call a store made: the cursor it passed and what it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestRead {
    pub after: Option<NodeId>,
    pub answered: Vec<NodeId>,
}

/// A failure a scripted manifest injects: a cut in its publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    /// None.
    Never,
    /// The stream fails when a store first starts reading it.
    Nodes,
    /// The stream fails at the edge pass's first child-table read, once
    /// every node row exists.
    Edges,
}

/// A candidate's manifest from a list, ascending by digest, that records
/// every read a store makes.
pub struct ScriptedManifest {
    summary: ManifestSummary,
    entries: Vec<ManifestEntry>,
    reads: Arc<Mutex<Vec<ManifestRead>>>,
    cut: Cut,
    sweeps: usize,
}

impl ScriptedManifest {
    fn new(candidate: &Candidate, base: Option<NodeId>, commit: CommitId) -> Self {
        let external = candidate.external();
        let mut entries: Vec<ManifestEntry> = candidate
            .fresh
            .values()
            .cloned()
            .map(|node| ManifestEntry::Fresh(verified(node)))
            .chain(external.iter().copied().map(ManifestEntry::External))
            .collect();
        entries.sort_by_key(entry_id);
        let summary = ManifestSummary {
            root: candidate.root,
            base,
            commit,
            fresh_nodes: candidate.fresh.len() as u64,
            fresh_bytes: candidate.fresh.values().map(EncodedNode::len).sum(),
            fresh_edges: candidate
                .fresh
                .values()
                .map(|node| children_of(node).len() as u64)
                .sum(),
            external: external.len() as u64,
        };
        Self {
            summary,
            entries,
            reads: Arc::default(),
            cut: Cut::Never,
            sweeps: 0,
        }
    }

    fn cut(mut self, cut: Cut) -> Self {
        self.cut = cut;
        self
    }

    fn reads(&self) -> Arc<Mutex<Vec<ManifestRead>>> {
        Arc::clone(&self.reads)
    }
}

fn entry_id(entry: &ManifestEntry) -> NodeId {
    match entry {
        ManifestEntry::External(id) => *id,
        ManifestEntry::Fresh(node) => node.id,
    }
}

impl CandidateManifest for ScriptedManifest {
    fn summary(&self) -> ManifestSummary {
        self.summary
    }

    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault> {
        if after.is_none() {
            if self.cut == Cut::Nodes && self.sweeps == 0 {
                return Err(StageFault::Io("the law cut the publication here".into()));
            }
            self.sweeps += 1;
        }
        let answered: Vec<ManifestEntry> = self
            .entries
            .iter()
            .filter(|entry| after.is_none_or(|after| entry_id(entry) > after))
            .take(limit)
            .cloned()
            .collect();
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ManifestRead {
                after,
                answered: answered.iter().map(entry_id).collect(),
            });
        Ok(answered)
    }

    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
        if self.cut == Cut::Edges {
            return Err(StageFault::Io("the law cut the publication here".into()));
        }
        match self
            .entries
            .binary_search_by_key(id, entry_id)
            .map(|at| &self.entries[at])
        {
            Ok(ManifestEntry::Fresh(node)) => node_range(node, offset, len),
            _ => Err(StageFault::Missing(*id)),
        }
    }

    fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
        Ok(Box::new(MemoryEdgeLog::default()))
    }
}

/// A session actor a law owns, the node that claimed it, and its fence.
pub struct Owned {
    pub actor: ActorKey,
    pub epoch: Epoch,
    pub node: NodeLease,
    session: String,
}

impl Owned {
    fn exec(&self, cell: &str) -> ExecKey {
        ExecKey::Cell(
            lash_vocabulary::identity::SessionId::try_from(self.session.clone())
                .unwrap_or_else(|_| panic!("a constant session id")),
            lash_vocabulary::identity::TurnId::try_from("pages-turn".to_owned())
                .unwrap_or_else(|_| panic!("a constant turn id")),
            CellId::new(cell),
        )
    }
}

async fn owned(tier: &dyn PageTier, id: &str) -> Result<Owned, LawBroken> {
    let store = tier.pages();
    let actor = session(id)?;
    create(store, std::slice::from_ref(&actor)).await?;
    let lease = node(store, &format!("{id}-node")).await?;
    let claimed = store.claim(&lease, 1).await?;
    let epoch = claimed
        .first()
        .map(|claimed| claimed.epoch)
        .ok_or_else(|| LawBroken(format!("{id} was not claimed")))?;
    Ok(Owned {
        actor,
        epoch,
        node: lease,
        session: id.to_owned(),
    })
}

/// The execution root the owner commit stages over a CheckpointRoot.
fn exec_root(checkpoint: NodeId) -> EncodedNode {
    encoded(NodeKind::FROZEN_VALUE, "exec root", &[checkpoint])
}

/// An effect's admission node, staged by the owner commit.
fn admission_node(effect: &str) -> EncodedNode {
    encoded(NodeKind::FROZEN_VALUE, &format!("admission {effect}"), &[])
}

/// What one publication carries beside its candidate.
struct Publication<'a> {
    exec: &'a ExecKey,
    /// The revision and execution root it replaces.
    base: Option<(u64, NodeId)>,
    /// Effects it admits.
    effects: &'a [&'a str],
    /// The commit identity its execution row records, when not the
    /// manifest's.
    row_commit: Option<CommitId>,
    /// The page its execution root wraps, when not the manifest's root.
    wraps: Option<NodeId>,
}

/// Publishes `manifest` under `owner`'s fence at `epoch` with the
/// execution root over its CheckpointRoot and `publication`'s effects.
async fn publish_at(
    store: &dyn PageStore,
    actor: &ActorKey,
    epoch: Epoch,
    publication: &Publication<'_>,
    manifest: Box<dyn CandidateManifest>,
) -> Result<CandidateReceipt, DurableError> {
    let tx = store.begin(actor, epoch).await?;
    publish_in(store, tx, publication, manifest).await
}

async fn publish_in(
    store: &dyn PageStore,
    mut tx: crate::ActorTx,
    publication: &Publication<'_>,
    manifest: Box<dyn CandidateManifest>,
) -> Result<CandidateReceipt, DurableError> {
    let summary = manifest.summary();
    let root = exec_root(publication.wraps.unwrap_or(summary.root));
    tx.stage([root.clone()]);
    for effect in publication.effects {
        let admission = admission_node(effect);
        tx.stage([admission.clone()]);
        tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec: publication.exec.clone(),
            effect: EffectKey(effect.as_bytes().into()),
            member: None,
            admission: admission.id,
        }));
    }
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: publication.exec.clone(),
        expected: publication.base.map(|(rev, _)| SnapshotRev(rev)),
        expected_root: publication.base.map(|(_, root)| root),
        root: root.id,
        commit_id: publication.row_commit.unwrap_or(summary.commit),
    }));
    store
        .commit_candidate(
            tx,
            CandidatePublication {
                owner: CandidateOwner::ExecRootPut(publication.exec.clone()),
                manifest,
            },
            LABEL,
        )
        .await
}

async fn publish(
    tier: &dyn PageTier,
    owner: &Owned,
    publication: &Publication<'_>,
    manifest: Box<dyn CandidateManifest>,
) -> Result<CandidateReceipt, DurableError> {
    publish_at(
        tier.pages(),
        &owner.actor,
        owner.epoch,
        publication,
        manifest,
    )
    .await
}

fn simple(exec: &ExecKey, base: Option<(u64, NodeId)>) -> Publication<'_> {
    Publication {
        exec,
        base,
        effects: &[],
        row_commit: None,
        wraps: None,
    }
}

/// Runs retirement steps of `budget` nodes until nothing is held: how many
/// steps ran and how many nodes they deleted. Each step keeps its budget.
///
/// # Errors
///
/// A refused step, or one past its budget.
pub async fn drain(store: &dyn PageStore, budget: u64) -> Result<(u64, u64), LawBroken> {
    let (mut steps, mut deleted) = (0_u64, 0_u64);
    loop {
        ensure!(steps < 10_000_000, "retirement never drained");
        let progress = store
            .retire_step(RetirementBudget { nodes: budget })
            .await?;
        ensure!(
            progress.nodes_deleted <= budget,
            "a step of budget {budget} deleted {} nodes",
            progress.nodes_deleted
        );
        if progress.holds_advanced == 0 && progress.leases_reaped == 0 {
            let backlog = store.retirement_backlog().await?;
            ensure!(
                backlog.holds == 0,
                "an idle step left {} holds",
                backlog.holds
            );
            return Ok((steps, deleted));
        }
        steps += 1;
        deleted += progress.nodes_deleted;
    }
}

fn lease_request(owner: &Owned, exec: &ExecKey, expected: NodeId, ttl: i64) -> RootLeaseRequest {
    RootLeaseRequest {
        owner: RootOwner::Exec(exec.clone()),
        expected,
        fence: crate::domain::pages::LeaseFence::Actor {
            actor: owner.actor.clone(),
            epoch: owner.epoch,
        },
        ttl_millis: ttl,
    }
}

fn refusal(result: Result<impl std::fmt::Debug, DurableError>) -> Option<GraphRefusal> {
    match result {
        Err(DurableError::Graph(refusal)) => Some(refusal),
        _ => None,
    }
}

/// Every read of a manifest, split into its sweeps: each sweep starts at
/// the beginning and resumes exactly after its last entry, so it ascends.
/// Answers how many sweeps ran and how many of them read the whole stream.
/// A transaction retried for contention may abandon a sweep part way.
fn sweeps_ascend(reads: &[ManifestRead], all: &[NodeId]) -> Result<(usize, usize), LawBroken> {
    let mut sweeps: Vec<Vec<NodeId>> = Vec::new();
    for read in reads {
        if read.after.is_none() {
            sweeps.push(Vec::new());
        }
        let Some(seen) = sweeps.last_mut() else {
            return Err(LawBroken("the first read did not start a sweep".into()));
        };
        ensure!(
            read.after == seen.last().copied(),
            "a read resumed at {:?}, not after its last entry",
            read.after
        );
        seen.extend(&read.answered);
        ensure!(
            all.starts_with(seen),
            "a sweep left the stream's ascending order"
        );
    }
    let complete = sweeps.iter().filter(|seen| *seen == all).count();
    Ok((sweeps.len(), complete))
}

/// S6: a candidate installs exactly its new pages and their edges and
/// checks only the committed pages they name directly: a damaged page deep
/// under a reused one is never read, and a missing direct reference refuses
/// the whole commit.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_candidate_installs_only_new_nodes_and_direct_references(
    tier: &dyn PageTier,
) -> LawOutcome {
    let owner = owned(tier, "pages-direct").await?;
    let exec = owner.exec("direct");
    let mut base = BTreeMap::new();
    let mut deep = page(&mut base, NodeKind::PAYLOAD_LEAF, "deep 5", &[]);
    let mut chain = vec![deep];
    for depth in (1..5).rev() {
        deep = page(
            &mut base,
            NodeKind::KEYED_LEAF,
            &format!("deep {depth}"),
            &[deep],
        );
        chain.push(deep);
    }
    let shared = page(&mut base, NodeKind::KEYED_INDEX, "shared", &[deep]);
    let direct = page(&mut base, NodeKind::PAYLOAD_LEAF, "direct", &[]);
    let first_root = page(
        &mut base,
        NodeKind::CHECKPOINT_ROOT,
        "first",
        &[shared, direct],
    );
    let first = Candidate {
        root: first_root,
        fresh: base,
    };
    let receipt = publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    let first_exec = exec_root(first_root).id;
    ensure!(
        receipt.root == first_root,
        "the receipt names {} for {first_root}",
        receipt.root
    );
    // Damage a page only the reused page's closure reaches.
    let damaged = chain[2];
    tier.drop_blob(&damaged).await;
    let before = tier.stored().await;
    let mut fresh = BTreeMap::new();
    let added = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "added", &[]);
    let second_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "second",
        &[shared, added],
    );
    let second = Candidate {
        root: second_root,
        fresh,
    };
    let manifest = second.manifest(Some(exec_root(first_root).id), CommitId::mint());
    publish(
        tier,
        &owner,
        &simple(&exec, Some((1, first_exec))),
        Box::new(manifest),
    )
    .await
    .map_err(|error| {
        LawBroken(format!(
            "the candidate read past its direct references: {error}"
        ))
    })?;
    let second_exec = exec_root(second_root).id;
    let after = tier.stored().await;
    let installed: BTreeSet<NodeId> = after.difference(&before).copied().collect();
    ensure!(
        installed == BTreeSet::from([added, second_root, second_exec]),
        "the candidate installed {installed:?}"
    );
    ensure!(
        before.difference(&after).next().is_none(),
        "the candidate's commit deleted a node"
    );
    // A direct reference that is missing refuses the whole commit.
    tier.drop_blob(&direct).await;
    let before = tier.stored().await;
    let roots = tier.catalog().await;
    let mut fresh = BTreeMap::new();
    let third_root = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, "third", &[direct]);
    let third = Candidate {
        root: third_root,
        fresh,
    };
    let refused = refusal(
        publish(
            tier,
            &owner,
            &simple(&exec, Some((2, second_exec))),
            Box::new(third.manifest(Some(exec_root(second_root).id), CommitId::mint())),
        )
        .await,
    );
    ensure!(
        refused == Some(GraphRefusal::MissingNode(direct)),
        "a missing direct reference answered {refused:?}"
    );
    ensure!(
        tier.stored().await == before && tier.catalog().await == roots,
        "a refused candidate left rows behind"
    );
    Ok(())
}

/// S6: the execution root and the effect admissions publish in one
/// transaction or not at all.
///
/// # Errors
///
/// The first rule broken.
pub async fn publication_is_atomic_with_its_effect_admissions(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-atomic").await?;
    let exec = owner.exec("atomic");
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "atomic leaf", &[]);
    let first_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "atomic first",
        &[leaf],
    );
    let first = Candidate {
        root: first_root,
        fresh,
    };
    let commit = CommitId::mint();
    let store = tier.pages();
    publish(
        tier,
        &owner,
        &Publication {
            effects: &["effect-a", "effect-b"],
            ..simple(&exec, None)
        },
        Box::new(first.manifest(None, commit)),
    )
    .await?;
    let first_exec = exec_root(first_root).id;
    let roots: BTreeSet<NodeId> = tier.catalog().await.into_iter().collect();
    for effect in ["effect-a", "effect-b"] {
        ensure!(
            roots.contains(&admission_node(effect).id),
            "{effect}'s admission did not commit with its root"
        );
    }
    ensure!(
        store.candidate_state(&exec).await? == Some((first_exec, commit)),
        "the execution row does not hold the candidate"
    );
    // A publication cut once its nodes are inserted writes none of it.
    let mut fresh = BTreeMap::new();
    let other = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "atomic other", &[]);
    let second_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "atomic second",
        &[leaf, other],
    );
    let second = Candidate {
        root: second_root,
        fresh,
    };
    let second_commit = CommitId::mint();
    let blobs = tier.stored().await;
    let roots = tier.catalog().await;
    let with_effect = Publication {
        effects: &["effect-c"],
        ..simple(&exec, Some((1, first_exec)))
    };
    let cut = publish(
        tier,
        &owner,
        &with_effect,
        Box::new(
            second
                .manifest(Some(exec_root(first_root).id), second_commit)
                .cut(Cut::Edges),
        ),
    )
    .await;
    ensure!(cut.is_err(), "a cut publication committed");
    ensure!(
        tier.stored().await == blobs && tier.catalog().await == roots,
        "a cut publication left rows behind"
    );
    // A candidate whose commit identity the execution row would not record
    // is refused before anything is written.
    let mismatched = publish(
        tier,
        &owner,
        &Publication {
            row_commit: Some(CommitId::mint()),
            ..simple(&exec, Some((1, first_exec)))
        },
        Box::new(second.manifest(Some(exec_root(first_root).id), second_commit)),
    )
    .await;
    ensure!(
        refusal(mismatched) == Some(GraphRefusal::PublicationMismatch),
        "a candidate under another commit identity was not refused"
    );
    // One publication identity (FIG-6197 F1): a manifest built against the
    // semantic root rather than the execution root the row holds, and an
    // execution root wrapping another page each disagree with it, and are
    // refused alike.
    let disagreeing = [
        (
            simple(&exec, Some((1, first_exec))),
            second.manifest(Some(first_root), second_commit),
        ),
        (
            Publication {
                wraps: Some(other),
                ..simple(&exec, Some((1, first_exec)))
            },
            second.manifest(Some(first_exec), second_commit),
        ),
    ];
    for (publication, manifest) in disagreeing {
        ensure!(
            refusal(publish(tier, &owner, &publication, Box::new(manifest)).await)
                == Some(GraphRefusal::PublicationMismatch),
            "a candidate that disagrees with its publication was not refused"
        );
    }
    ensure!(
        tier.stored().await == blobs && tier.catalog().await == roots,
        "a mismatched candidate left rows behind"
    );
    publish(
        tier,
        &owner,
        &with_effect,
        Box::new(second.manifest(Some(exec_root(first_root).id), second_commit)),
    )
    .await?;
    ensure!(
        tier.catalog()
            .await
            .contains(&admission_node("effect-c").id),
        "effect-c's admission did not commit with its root"
    );
    Ok(())
}

/// D-LEASE, D-RET: a leased root and everything it reaches survive every
/// retirement step, racing reads included, until the lease is released;
/// then exactly what nothing else keeps is reclaimed.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_leased_root_survives_concurrent_retirement(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-leased").await?;
    let exec = owner.exec("leased");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let shared = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "leased shared", &[]);
    let gone: Vec<NodeId> = (0..20)
        .map(|leaf| {
            page(
                &mut fresh,
                NodeKind::PAYLOAD_LEAF,
                &format!("leased {leaf}"),
                &[],
            )
        })
        .collect();
    let keyed = page(&mut fresh, NodeKind::KEYED_LEAF, "leased keyed", &gone);
    let first_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "leased first",
        &[shared, keyed],
    );
    let first = Candidate {
        root: first_root,
        fresh,
    };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    let first_exec = exec_root(first_root).id;
    let lease = store
        .acquire_root_lease(lease_request(&owner, &exec, first_exec, 600_000))
        .await?;
    let mut leased: Vec<NodeId> = first.fresh.keys().copied().collect();
    leased.push(first_exec);
    let mut fresh = BTreeMap::new();
    let added = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "leased added", &[]);
    let second_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "leased second",
        &[shared, added],
    );
    let second = Candidate {
        root: second_root,
        fresh,
    };
    publish(
        tier,
        &owner,
        &simple(&exec, Some((1, first_exec))),
        Box::new(second.manifest(Some(exec_root(first_root).id), CommitId::mint())),
    )
    .await?;
    let backlog = store.retirement_backlog().await?;
    ensure!(backlog.holds >= 1, "the replaced root was not held");
    // Steps race the lease's reads: every page stays readable.
    let (drained, read) = futures_util::join!(drain(store, 2), store.read_pages(&lease, &leased));
    drained?;
    let read = read?;
    ensure!(
        read.iter().map(|page| page.id).collect::<Vec<_>>() == leased,
        "a leased read answered other pages"
    );
    for page in &read {
        let expected = first
            .fresh
            .get(&page.id)
            .map_or_else(|| exec_root(first_root).bytes, |node| node.bytes.clone());
        ensure!(
            page.bytes == expected,
            "page {} was read with other bytes",
            page.id
        );
    }
    let stored = tier.stored().await;
    ensure!(
        leased.iter().all(|page| stored.contains(page)),
        "retirement took a page under a live lease"
    );
    store.release_root_lease(lease.id).await?;
    drain(store, 2).await?;
    let stored = tier.stored().await;
    for page in gone.iter().chain([&keyed, &first_root, &first_exec]) {
        ensure!(!stored.contains(page), "{page} outlived its last owner");
    }
    for page in [shared, added, second_root, exec_root(second_root).id] {
        ensure!(
            stored.contains(&page),
            "{page} was reclaimed under its live root"
        );
    }
    ensure!(
        refusal(store.read_pages(&lease, &[shared]).await) == Some(GraphRefusal::LeaseLost),
        "a released lease still reads"
    );
    Ok(())
}

/// D-LEASE: a lease is taken only under its actor's fence on the owner's
/// current root; past its expiry it reads and renews nothing, yet keeps
/// its root until a retirement step reaps it.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_expired_lease_reads_nothing(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-expiry").await?;
    let exec = owner.exec("expiry");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "expiry leaf", &[]);
    let first_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "expiry first",
        &[leaf],
    );
    let first = Candidate {
        root: first_root,
        fresh,
    };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    let first_exec = exec_root(first_root).id;
    let stale = RootLeaseRequest {
        fence: crate::domain::pages::LeaseFence::Actor {
            actor: owner.actor.clone(),
            epoch: Epoch(owner.epoch.0 + 7),
        },
        ..lease_request(&owner, &exec, first_exec, 1_000)
    };
    ensure!(
        matches!(
            store.acquire_root_lease(stale).await,
            Err(DurableError::OwnershipLost(_))
        ),
        "a lease under another epoch was granted"
    );
    ensure!(
        refusal(
            store
                .acquire_root_lease(lease_request(&owner, &exec, first_root, 1_000))
                .await
        ) == Some(GraphRefusal::RevisionConflict),
        "a lease on a root the owner does not hold was granted"
    );
    ensure!(
        refusal(
            store
                .acquire_root_lease(lease_request(
                    &owner,
                    &owner.exec("absent"),
                    first_exec,
                    1_000
                ))
                .await
        ) == Some(GraphRefusal::RootRetired),
        "a lease on an owner with no root was granted"
    );
    let lease = store
        .acquire_root_lease(lease_request(&owner, &exec, first_exec, 1_000))
        .await?;
    store.read_pages(&lease, &[leaf]).await?;
    tier.advance(500);
    let renewed = store.renew_root_lease(&lease).await?;
    ensure!(
        renewed.expires_at.0 > lease.expires_at.0,
        "a renewal did not extend the lease"
    );
    tier.advance(1_000);
    ensure!(
        refusal(store.read_pages(&renewed, &[leaf]).await) == Some(GraphRefusal::LeaseLost),
        "an expired lease still reads"
    );
    ensure!(
        refusal(store.renew_root_lease(&renewed).await) == Some(GraphRefusal::LeaseLost),
        "an expired lease renewed"
    );
    // Its row keeps the root until a step reaps it.
    let mut fresh = BTreeMap::new();
    let second_root = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, "expiry second", &[]);
    let second = Candidate {
        root: second_root,
        fresh,
    };
    publish(
        tier,
        &owner,
        &simple(&exec, Some((1, first_exec))),
        Box::new(second.manifest(Some(exec_root(first_root).id), CommitId::mint())),
    )
    .await?;
    // One row for the lease, one for the replaced root's hold.
    let rows = tier
        .catalog()
        .await
        .into_iter()
        .filter(|root| *root == first_exec)
        .count();
    ensure!(rows == 2, "an expired lease's row no longer keeps its root");
    let progress = store.retire_step(RetirementBudget { nodes: 64 }).await?;
    ensure!(
        progress.leases_reaped == 1,
        "the step reaped {} leases",
        progress.leases_reaped
    );
    drain(store, 64).await?;
    let stored = tier.stored().await;
    ensure!(
        !stored.contains(&first_exec) && !stored.contains(&leaf),
        "the expired lease's root outlived its reaping"
    );
    Ok(())
}

/// D-RET: a retired graph is reclaimed by steps that each keep their node
/// budget; a process that dies between steps resumes from the holds, a
/// page re-used mid-retirement is kept, and in the end exactly what no
/// owner reaches is gone and nothing is held.
///
/// # Errors
///
/// The first rule broken.
pub async fn retirement_advances_in_bounded_steps_and_reclaims_everything_unowned(
    tier: &mut dyn PageTier,
    leaves: u32,
) -> LawOutcome {
    const BUDGET: u64 = 1_000;
    let owner = owned(&*tier, "pages-retire").await?;
    let exec = owner.exec("retire");
    let big = GeneratedManifest::new("retire", leaves, 1_000, CommitId::mint());
    let retired = big.nodes();
    let big_root = big.summary.root;
    let indexes: Vec<EncodedNode> = (0..leaves.div_ceil(1_000))
        .map(|index| big.build(Recipe::Index(index)))
        .collect();
    let kept = indexes[0].clone();
    publish(&*tier, &owner, &simple(&exec, None), Box::new(big)).await?;
    let first_exec = exec_root(big_root).id;
    let mut fresh = BTreeMap::new();
    let second_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "retire second",
        &[kept.id],
    );
    let second = Candidate {
        root: second_root,
        fresh,
    };
    publish(
        &*tier,
        &owner,
        &simple(&exec, Some((1, first_exec))),
        Box::new(second.manifest(Some(exec_root(big_root).id), CommitId::mint())),
    )
    .await?;
    let mut steps = 0_u64;
    let mut deleted = 0_u64;
    // About half the retirement runs before the restart.
    for _ in 0..(u64::from(leaves) / BUDGET / 2).max(1) {
        let progress = tier
            .pages()
            .retire_step(RetirementBudget { nodes: BUDGET })
            .await?;
        ensure!(
            progress.nodes_deleted <= BUDGET,
            "a step deleted {} nodes",
            progress.nodes_deleted
        );
        steps += 1;
        deleted += progress.nodes_deleted;
    }
    // The process dies between steps; another resumes from the holds.
    tier.reopen().await;
    let backlog = tier.pages().retirement_backlog().await?;
    ensure!(
        backlog.holds > 0,
        "the frontier did not survive the restart"
    );
    // A later root re-uses a page still awaiting retirement.
    let stored = tier.stored().await;
    let resurrected = indexes
        .iter()
        .skip(1)
        .find(|index| stored.contains(&index.id))
        .cloned();
    let mut survivors: BTreeSet<NodeId> = children_of(&kept).into_iter().collect();
    survivors.insert(kept.id);
    let other = owner.exec("retire-other");
    if let Some(index) = &resurrected {
        let mut fresh = BTreeMap::new();
        let third_root = page(
            &mut fresh,
            NodeKind::CHECKPOINT_ROOT,
            "retire third",
            &[index.id],
        );
        let third = Candidate {
            root: third_root,
            fresh,
        };
        publish(
            &*tier,
            &owner,
            &simple(&other, None),
            Box::new(third.manifest(None, CommitId::mint())),
        )
        .await?;
        survivors.insert(index.id);
        survivors.extend(children_of(index));
    }
    let (more_steps, more) = drain(tier.pages(), BUDGET).await?;
    steps += more_steps;
    deleted += more;
    let stored = tier.stored().await;
    for node in &retired {
        ensure!(
            stored.contains(node) == survivors.contains(node),
            "{node} is {} after retirement",
            if stored.contains(node) {
                "stored"
            } else {
                "gone"
            }
        );
    }
    ensure!(
        !stored.contains(&first_exec),
        "the retired execution root outlived its retirement"
    );
    let expected = retired.len() as u64 + 1 - survivors.len() as u64;
    ensure!(
        deleted == expected,
        "the steps deleted {deleted} nodes, not the {expected} unowned"
    );
    ensure!(
        steps >= expected.div_ceil(BUDGET),
        "{steps} steps of {BUDGET} deleted {expected} nodes"
    );
    eprintln!(
        "retirement: retired_nodes={} survivors={} deleted={deleted} steps={steps} budget={BUDGET} resurrected={}",
        retired.len(),
        survivors.len(),
        resurrected.is_some()
    );
    Ok(())
}

/// D-SPOOL: a publication whose answer was lost is resolved from the
/// execution row by commit identity. One that committed is never applied
/// again, and its effects are admitted though no worker heard the receipt;
/// one that did not is retried exactly and commits.
///
/// # Errors
///
/// The first rule broken.
pub async fn the_lost_receipt_resolves_by_commit_identity(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-receipt").await?;
    let exec = owner.exec("receipt");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let first_root = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, "receipt first", &[]);
    let first = Candidate {
        root: first_root,
        fresh,
    };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    let first_exec = exec_root(first_root).id;
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "receipt leaf", &[]);
    let second_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "receipt second",
        &[leaf],
    );
    let second = Candidate {
        root: second_root,
        fresh,
    };
    let second_commit = CommitId::mint();
    let admitting = Publication {
        effects: &["receipt-effect"],
        ..simple(&exec, Some((1, first_exec)))
    };
    // The answer is lost: the parent knows only what it sent.
    let _lost = publish(
        tier,
        &owner,
        &admitting,
        Box::new(second.manifest(Some(exec_root(first_root).id), second_commit)),
    )
    .await;
    let second_exec = exec_root(second_root).id;
    ensure!(
        store.candidate_state(&exec).await? == Some((second_exec, second_commit)),
        "the execution row does not resolve the committed candidate"
    );
    ensure!(
        tier.catalog()
            .await
            .contains(&admission_node("receipt-effect").id),
        "the committed admission is not authority without the receipt"
    );
    let blobs = tier.stored().await;
    let roots = tier.catalog().await;
    let again = publish(
        tier,
        &owner,
        &admitting,
        Box::new(second.manifest(Some(exec_root(first_root).id), second_commit)),
    )
    .await;
    ensure!(again.is_err(), "a committed candidate was applied twice");
    ensure!(
        tier.stored().await == blobs && tier.catalog().await == roots,
        "a repeated candidate changed rows"
    );
    // A publication that did not commit leaves the base: retried exactly,
    // it commits.
    let mut fresh = BTreeMap::new();
    let third_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "receipt third",
        &[leaf],
    );
    let third = Candidate {
        root: third_root,
        fresh,
    };
    let third_commit = CommitId::mint();
    let next = simple(&exec, Some((2, second_exec)));
    let cut = publish(
        tier,
        &owner,
        &next,
        Box::new(
            third
                .manifest(Some(exec_root(second_root).id), third_commit)
                .cut(Cut::Nodes),
        ),
    )
    .await;
    ensure!(cut.is_err(), "a cut publication committed");
    ensure!(
        store.candidate_state(&exec).await? == Some((second_exec, second_commit)),
        "a publication that did not commit moved the execution row"
    );
    publish(
        tier,
        &owner,
        &next,
        Box::new(third.manifest(Some(exec_root(second_root).id), third_commit)),
    )
    .await?;
    ensure!(
        store.candidate_state(&exec).await? == Some((exec_root(third_root).id, third_commit)),
        "the retried candidate did not commit"
    );
    Ok(())
}

/// S6: every publication takes its blob locks in one ascending digest
/// order, its manifest read in ascending sweeps, so two candidates that
/// insert the same new pages and name the same committed ones commit side
/// by side.
///
/// # Errors
///
/// The first rule broken.
pub async fn publication_keeps_global_digest_lock_order(tier: &dyn PageTier) -> LawOutcome {
    let left = owned(tier, "pages-order-left").await?;
    let right = owned(tier, "pages-order-right").await?;
    let (left_exec, right_exec) = (left.exec("order"), right.exec("order"));
    let mut base = BTreeMap::new();
    let named: Vec<NodeId> = (0..16)
        .map(|leaf| {
            page(
                &mut base,
                NodeKind::PAYLOAD_LEAF,
                &format!("order named {leaf}"),
                &[],
            )
        })
        .collect();
    let base_root = page(&mut base, NodeKind::CHECKPOINT_ROOT, "order base", &named);
    publish(
        tier,
        &left,
        &simple(&left.exec("base"), None),
        Box::new(
            Candidate {
                root: base_root,
                fresh: base,
            }
            .manifest(None, CommitId::mint()),
        ),
    )
    .await?;
    let mut shared = BTreeMap::new();
    let both: Vec<NodeId> = (0..32)
        .map(|leaf| {
            let name = named[leaf % named.len()];
            page(
                &mut shared,
                NodeKind::KEYED_LEAF,
                &format!("order both {leaf}"),
                &[name],
            )
        })
        .collect();
    let candidate = |tag: &str| {
        let mut fresh = shared.clone();
        let root = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, tag, &both);
        Candidate { root, fresh }
    };
    let (left_candidate, right_candidate) = (candidate("order left"), candidate("order right"));
    let left_manifest = left_candidate.manifest(None, CommitId::mint());
    let right_manifest = right_candidate.manifest(None, CommitId::mint());
    let (left_reads, right_reads) = (left_manifest.reads(), right_manifest.reads());
    let (left_publication, right_publication) =
        (simple(&left_exec, None), simple(&right_exec, None));
    let (left_done, right_done) = futures_util::join!(
        publish(tier, &left, &left_publication, Box::new(left_manifest)),
        publish(tier, &right, &right_publication, Box::new(right_manifest)),
    );
    left_done.map_err(|error| LawBroken(format!("the left candidate: {error}")))?;
    right_done.map_err(|error| LawBroken(format!("the right candidate: {error}")))?;
    for (reads, candidate) in [
        (left_reads, &left_candidate),
        (right_reads, &right_candidate),
    ] {
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
        // A transaction retried for contention sweeps again from the start.
        let (sweeps, complete) = sweeps_ascend(&reads, &all)?;
        ensure!(
            complete >= 1 && sweeps >= complete,
            "a candidate was swept {sweeps} times, {complete} of them whole"
        );
    }
    let stored = tier.stored().await;
    ensure!(
        both.iter().all(|page| stored.contains(page)),
        "a page both candidates supplied is missing"
    );
    Ok(())
}

/// D-LEASE, D-SPOOL: an ownership transfer fences the old activation: its
/// lease no longer renews and its candidate commits nothing, while the
/// lease's row keeps its root until it expires and a step reaps it; the new
/// owner leases and publishes under its own epoch.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_ownership_transfer_fences_the_old_activation(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-transfer").await?;
    let exec = owner.exec("transfer");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "transfer leaf", &[]);
    let first_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "transfer first",
        &[leaf],
    );
    let first = Candidate {
        root: first_root,
        fresh,
    };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    let first_exec = exec_root(first_root).id;
    let old_lease = store
        .acquire_root_lease(lease_request(&owner, &exec, first_exec, 60_000))
        .await?;
    let old_tx = store.begin(&owner.actor, owner.epoch).await?;
    // The old node lets go; another claims the actor at a later epoch.
    store.release_node(&owner.node).await?;
    let successor = node(store, "pages-transfer-successor").await?;
    let claimed = store.claim(&successor, 1).await?;
    let epoch = claimed
        .first()
        .map(|claimed| claimed.epoch)
        .ok_or_else(|| LawBroken("the successor claimed nothing".into()))?;
    ensure!(epoch.0 > owner.epoch.0, "the transfer kept epoch {epoch}");
    ensure!(
        matches!(
            store.renew_root_lease(&old_lease).await,
            Err(DurableError::OwnershipLost(_))
        ),
        "the old activation's lease renewed after the transfer"
    );
    let blobs = tier.stored().await;
    let mut fresh = BTreeMap::new();
    let stale_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "transfer stale",
        &[leaf],
    );
    let stale = Candidate {
        root: stale_root,
        fresh,
    };
    let refused = publish_in(
        store,
        old_tx,
        &simple(&exec, Some((1, first_exec))),
        Box::new(stale.manifest(Some(exec_root(first_root).id), CommitId::mint())),
    )
    .await;
    ensure!(
        matches!(refused, Err(DurableError::OwnershipLost(_))),
        "the old activation's candidate answered {refused:?}"
    );
    ensure!(
        tier.stored().await == blobs,
        "a fenced candidate wrote nodes"
    );
    // The successor leases the same root and publishes under its epoch.
    let successor_owner = Owned {
        actor: owner.actor.clone(),
        epoch,
        node: successor,
        session: owner.session.clone(),
    };
    let lease = store
        .acquire_root_lease(lease_request(&successor_owner, &exec, first_exec, 60_000))
        .await?;
    let mut fresh = BTreeMap::new();
    let next_root = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, "transfer next", &[]);
    let next = Candidate {
        root: next_root,
        fresh,
    };
    publish(
        tier,
        &successor_owner,
        &simple(&exec, Some((1, first_exec))),
        Box::new(next.manifest(Some(exec_root(first_root).id), CommitId::mint())),
    )
    .await?;
    store.release_root_lease(lease.id).await?;
    drain(store, 64).await?;
    // The old lease, unexpired, still keeps the root it read.
    ensure!(
        tier.stored().await.contains(&leaf),
        "the old lease's root was reclaimed while its row stood"
    );
    store.read_pages(&old_lease, &[leaf]).await?;
    tier.advance(60_000);
    drain(store, 64).await?;
    let stored = tier.stored().await;
    ensure!(
        !stored.contains(&leaf) && !stored.contains(&first_exec),
        "the old lease's root outlived its reaping"
    );
    Ok(())
}

/// S5/S6: installing a candidate of `leaves` new pages holds at most
/// [`CandidateBuffers::bound`] manifest bytes at once, whatever its size.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_large_candidate_installs_within_bounded_buffers(
    tier: &dyn PageTier,
    leaves: u32,
) -> LawOutcome {
    #[derive(Debug, Default)]
    struct Work {
        passes: u64,
        calls: u64,
        entries: u64,
        bytes: u64,
        range_calls: u64,
        range_bytes: u64,
        generation_nanos: u128,
    }
    struct MeasuredManifest {
        inner: GeneratedManifest,
        work: Arc<Mutex<Work>>,
    }
    impl CandidateManifest for MeasuredManifest {
        fn summary(&self) -> ManifestSummary {
            self.inner.summary()
        }
        fn entries(
            &mut self,
            after: Option<NodeId>,
            limit: usize,
        ) -> Result<Vec<ManifestEntry>, StageFault> {
            let started = std::time::Instant::now();
            let result = self.inner.entries(after, limit);
            let mut work = self
                .work
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            work.calls += 1;
            work.passes += u64::from(after.is_none());
            work.generation_nanos += started.elapsed().as_nanos();
            if let Ok(entries) = &result {
                work.entries += entries.len() as u64;
                work.bytes += entries
                    .iter()
                    .map(|entry| match entry {
                        ManifestEntry::Fresh(node) => node.bytes.len() as u64,
                        ManifestEntry::External(_) => 0,
                    })
                    .sum::<u64>();
            }
            result
        }
        fn fresh_range(
            &mut self,
            id: &NodeId,
            offset: u64,
            len: usize,
        ) -> Result<Vec<u8>, StageFault> {
            let started = std::time::Instant::now();
            let result = self.inner.fresh_range(id, offset, len);
            let mut work = self
                .work
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            work.range_calls += 1;
            work.generation_nanos += started.elapsed().as_nanos();
            if let Ok(bytes) = &result {
                work.range_bytes += bytes.len() as u64;
            }
            result
        }
        fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
            self.inner.edge_log()
        }
    }
    let owner = owned(tier, "pages-bounded").await?;
    let exec = owner.exec("bounded");
    let generating = std::time::Instant::now();
    let big = GeneratedManifest::new("bounded", leaves, 1_000, CommitId::mint());
    let setup_ms = generating.elapsed().as_millis();
    let summary = big.summary();
    let work = Arc::new(Mutex::new(Work::default()));
    let manifest = MeasuredManifest {
        inner: big,
        work: Arc::clone(&work),
    };
    let resident_before = tier.resident_high_water();
    let started = std::time::Instant::now();
    let result = publish(tier, &owner, &simple(&exec, None), Box::new(manifest)).await;
    let elapsed = started.elapsed();
    let resident_after = tier.resident_high_water();
    let peak = tier.peak_buffered();
    let bound = CandidateBuffers::DEFAULT.bound();
    let evidence = format!(
        "bounded install: expected_nodes={} expected_edges={} work={:?} peak_buffered_bytes={peak} bound_bytes={bound} \
         vm_hwm_before_kib={resident_before:?} vm_hwm_after_kib={resident_after:?} setup_ms={setup_ms} elapsed_ms={}",
        summary.fresh_nodes,
        summary.fresh_edges,
        work.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        elapsed.as_millis()
    );
    eprintln!("{evidence}");
    result.map_err(|broken| LawBroken(format!("{broken}; {evidence}")))?;
    // The oracle lists the pages only once the install is measured.
    let nodes = GeneratedManifest::new("bounded", leaves, 1_000, CommitId::mint()).nodes();
    ensure!(
        peak > 0 && peak <= bound,
        "the install buffered {peak} bytes against a bound of {bound}"
    );
    let stored = tier.stored().await;
    ensure!(
        nodes.iter().all(|node| stored.contains(node)),
        "a generated page is missing"
    );
    Ok(())
}

#[path = "pages_retained.rs"]
mod retained;
pub use retained::lo_root_retained_delta_preserves_reachability;
