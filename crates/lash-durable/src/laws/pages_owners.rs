//! The page laws of a candidate's owner write and of host reads over heap
//! pages (FIG-6193): a step outcome owns its candidate as an execution root
//! does, and a host graph read never descends into a heap page.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::stage::CandidateManifest;
use lash_core_store::checkpoint_graph::{CommitId, EncodedNode, GraphRefusal, NodeId, NodeKind};

use super::super::{LABEL, LawBroken, LawOutcome, ensure};
use super::{Candidate, PageTier, encoded, exec_root, owned, page, publish, refusal, simple};
use crate::DomainWrite;
use crate::domain::graph::{ExecRootWrite, RootOwner, RunFactKind, RunFactWrite};
use crate::domain::pages::{CandidateOwner, CandidatePublication, SemanticAdmission};
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
        let summary = manifest.summary();
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
                        admission: SemanticAdmission {
                            root: summary.root,
                            kernel: 1,
                            page_format: 1,
                            validator: "law-validator".into(),
                            commit: summary.commit,
                        },
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
