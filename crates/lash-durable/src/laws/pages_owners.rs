//! The page laws of a candidate's owner write and of host reads over heap
//! pages (FIG-6193): a step outcome owns its candidate as an execution root
//! does, a deleted session's heap retires in bounded steps, and a host
//! graph read never descends into a heap page.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::{CommitId, EncodedNode, GraphRefusal, NodeId, NodeKind};

use super::super::{LABEL, LawBroken, LawOutcome, ensure};
use super::{
    Candidate, GeneratedManifest, PageTier, drain, encoded, exec_root, owned, page, publish,
    refusal, simple,
};
use crate::DomainWrite;
use crate::domain::graph::{ExecRootWrite, RootOwner, RunFactKind, RunFactWrite};
use crate::domain::pages::{CandidateOwner, CandidatePublication};
use crate::domain::{ExecKey, Ordinal, OwnerKey, RunSeq};

/// A process engine step's outcome owns its candidate (FIG-6193, P1): the
/// run fact it appends has a record that reaches the candidate's root, and
/// the candidate installs with it under the same publication identity
/// check as an execution root's. A record that does not reach the root,
/// a manifest with a base, or a fact at another place is refused before
/// anything is written.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_step_outcome_owns_its_candidate(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-step").await?;
    let exec = owner.exec("step");
    let ExecKey::Cell(session, turn, cell) = exec else {
        return Err(LawBroken("a law cell is a cell".into()));
    };
    let facts = OwnerKey::Cell(session, turn, cell);
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "step leaf", &[]);
    let root = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, "step root", &[leaf]);
    let candidate = Candidate { root, fresh };
    let store = tier.pages();
    let publish = |record: EncodedNode, ordinal: u64, base: Option<NodeId>| {
        let manifest = candidate.manifest(base, CommitId([ordinal as u8 + 1; 32]));
        let facts = facts.clone();
        let actor = owner.actor.clone();
        let epoch = owner.epoch;
        async move {
            let mut tx = store.begin(&actor, epoch).await?;
            tx.stage([record.clone()]);
            tx.write(DomainWrite::RunFact(RunFactWrite::Append {
                owner: facts.clone(),
                run: RunSeq(0),
                ordinal: Ordinal(ordinal),
                kind: RunFactKind::XOutcome,
                call: None,
                record: record.id,
            }));
            store
                .commit_candidate(
                    tx,
                    CandidatePublication {
                        owner: CandidateOwner::RunFactAppend {
                            owner: facts,
                            run: RunSeq(0),
                            ordinal: Ordinal(ordinal),
                        },
                        manifest: Box::new(manifest),
                    },
                    LABEL,
                )
                .await
        }
    };
    let blobs = tier.stored().await;
    let stray = encoded(NodeKind::FROZEN_VALUE, "stray outcome", &[]);
    for (record, base) in [
        (stray, None),
        (
            encoded(NodeKind::FROZEN_VALUE, "based outcome", &[root]),
            Some(root),
        ),
    ] {
        ensure!(
            refusal(publish(record, 0, base).await) == Some(GraphRefusal::PublicationMismatch),
            "a step outcome that does not own its candidate was not refused"
        );
    }
    ensure!(
        tier.stored().await == blobs,
        "a refused step outcome left rows behind"
    );
    let record = encoded(NodeKind::FROZEN_VALUE, "step outcome", &[root]);
    publish(record.clone(), 0, None).await?;
    let stored = tier.stored().await;
    ensure!(
        stored.contains(&root) && stored.contains(&leaf),
        "the step outcome's candidate did not install"
    );
    ensure!(
        tier.catalog().await.contains(&record.id),
        "the step outcome's record is no root"
    );
    Ok(())
}

/// A pruned step outcome lets its candidate go (FIG-6193, P1): pruning
/// the run's facts drops the record's root, and the counted reclaim then
/// deletes exactly the record and the candidate pages no other owner
/// reaches; a page the execution root also reaches is kept.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_pruned_step_outcome_frees_exactly_its_unshared_pages(
    tier: &dyn PageTier,
) -> LawOutcome {
    let owner = owned(tier, "pages-prune").await?;
    let exec = owner.exec("prune");
    let ExecKey::Cell(session, turn, cell) = exec.clone() else {
        return Err(LawBroken("a law cell is a cell".into()));
    };
    let facts = OwnerKey::Cell(session, turn, cell);
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let shared = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "prune shared", &[]);
    let kept = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "prune kept",
        &[shared],
    );
    let kept = Candidate { root: kept, fresh };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(kept.manifest(None, CommitId::mint())),
    )
    .await?;
    drain(store, 64).await?;
    let before = tier.stored().await;
    let mut fresh = BTreeMap::new();
    let only = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "prune unshared", &[]);
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "prune step root",
        &[shared, only],
    );
    let candidate = Candidate { root, fresh };
    let manifest = candidate.manifest(None, CommitId::mint());
    let record = encoded(NodeKind::FROZEN_VALUE, "prune outcome", &[root]);
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.stage([record.clone()]);
    tx.write(DomainWrite::RunFact(RunFactWrite::Append {
        owner: facts.clone(),
        run: RunSeq(0),
        ordinal: Ordinal(0),
        kind: RunFactKind::XOutcome,
        call: None,
        record: record.id,
    }));
    store
        .commit_candidate(
            tx,
            CandidatePublication {
                owner: CandidateOwner::RunFactAppend {
                    owner: facts.clone(),
                    run: RunSeq(0),
                    ordinal: Ordinal(0),
                },
                manifest: Box::new(manifest),
            },
            LABEL,
        )
        .await?;
    let (_, deleted) = drain(store, 64).await?;
    ensure!(
        deleted == 0,
        "retirement deleted {deleted} nodes a live step outcome owns"
    );
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.write(DomainWrite::RunFact(RunFactWrite::Prune {
        owner: facts,
        before: RunSeq(1),
    }));
    store.commit(tx, LABEL).await?;
    let (_, deleted) = drain(store, 64).await?;
    let stored = tier.stored().await;
    ensure!(
        stored == before,
        "the pruned outcome left {:?} and took {:?}",
        stored.difference(&before).collect::<Vec<_>>(),
        before.difference(&stored).collect::<Vec<_>>()
    );
    ensure!(
        deleted == 3,
        "reclaiming the record, its root and its one unshared page deleted {deleted} nodes"
    );
    Ok(())
}

/// Deleting a session retires its heap in bounded steps (FIG-6423): the
/// delete's transaction deletes no page and holds only the roots the
/// session's rows held, as many for a heap of `pages` pages as for one of
/// eight, and the retirement steps, each within its budget, then remove
/// every page the heap added, so nothing is left for the orphan audit.
///
/// # Errors
///
/// The first rule broken.
pub async fn deleting_a_session_retires_its_heap_in_bounded_steps(
    tier: &dyn PageTier,
    pages: u32,
) -> LawOutcome {
    // The standard node's step: what its retirement driver deletes per
    // transaction.
    let budget = crate::RetirementSettings::standard().batch_nodes;
    let store = tier.pages();
    let mut held = Vec::new();
    let mut delete_rows = Vec::new();
    for (session, leaves) in [("pages-delete-small", 8), ("pages-delete-large", pages)] {
        let owner = owned(tier, session).await?;
        let exec = owner.exec("heap");
        let before = tier.stored().await;
        let heap = GeneratedManifest::new(session, leaves, 1_000, CommitId::mint());
        // The heap's pages and the execution root over them.
        let added = heap.nodes().len() + 1;
        publish(tier, &owner, &simple(&exec, None), Box::new(heap)).await?;
        drain(store, budget).await?;
        let published = tier.stored().await;
        ensure!(
            published.len() == before.len() + added,
            "{session}'s heap stored {} nodes, not {added}",
            published.len() - before.len()
        );
        delete_rows.push(tier.delete_session(session).await);
        let after = tier.stored().await;
        ensure!(
            after == published,
            "deleting {session} deleted {} of its {added} heap nodes in its own transaction",
            published.len() - after.len()
        );
        held.push(store.retirement_backlog().await?.holds);
        let (_, deleted) = drain(store, budget).await?;
        let left = tier.stored().await;
        ensure!(
            left == before,
            "retiring {session}'s heap left {} nodes and took {} it did not add",
            left.difference(&before).count(),
            before.difference(&left).count()
        );
        ensure!(
            deleted == added as u64,
            "retiring {session}'s heap of {added} nodes deleted {deleted}"
        );
    }
    ensure!(
        held.first() == held.last() && held.first().is_some_and(|held| *held > 0),
        "the deletes held {held:?} roots for heaps of 8 and {pages} pages"
    );
    ensure!(
        delete_rows.first() == delete_rows.last()
            && delete_rows.first().is_some_and(|rows| *rows > 0),
        "the deletes affected {delete_rows:?} SQL rows for heaps of 8 and {pages} pages"
    );
    Ok(())
}

/// A host graph read takes a persistent-heap page as a leaf (FIG-6193):
/// hydrating an execution root reads its host nodes and its CheckpointRoot
/// header, and nothing the header reaches; a read asked to start at a heap
/// page is refused, typed.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_host_read_never_descends_into_a_heap_page(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "pages-hydrate").await?;
    let exec = owner.exec("hydrate");
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "hydrate leaf", &[]);
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "hydrate root",
        &[leaf],
    );
    let candidate = Candidate { root, fresh };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(candidate.manifest(None, CommitId::mint())),
    )
    .await?;
    let wrapper = exec_root(root).id;
    let store = tier.pages();
    let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20);
    let read = store
        .hydrate(RootOwner::Exec(exec.clone()), Some(wrapper), limits)
        .await?;
    let nodes: BTreeSet<NodeId> = read.nodes.keys().copied().collect();
    ensure!(
        nodes == [wrapper, root].into(),
        "hydrating the execution read {nodes:?}, not its wrapper and header alone"
    );
    // An owner whose root is a heap page is never hydrated.
    let other = owner.exec("hydrate-page");
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.stage([
        candidate.fresh[&leaf].clone(),
        candidate.fresh[&root].clone(),
    ]);
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: other.clone(),
        expected: None,
        expected_root: None,
        root,
        commit_id: CommitId::mint(),
    }));
    store.commit(tx, LABEL).await?;
    ensure!(
        refusal(
            store
                .hydrate(RootOwner::Exec(other), Some(root), limits)
                .await
        ) == Some(GraphRefusal::SemanticPage(root)),
        "a heap page was hydrated as a host graph"
    );
    Ok(())
}
