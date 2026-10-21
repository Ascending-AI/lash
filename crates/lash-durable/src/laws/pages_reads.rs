//! The page laws of a store's narrow reads (FIG-7047): a page read is one
//! statement whose lease answers before its pages, and a candidate's check
//! fetches the bytes of only the nodes it supplies.

use std::collections::BTreeMap;

use lash_core_ids::operational_metrics::sql;
use lash_core_store::checkpoint_graph::{CommitId, GraphRefusal, NodeId, NodeKind};

use super::super::graph::{ForeignWrite, GraphTier};
use super::super::{LawBroken, LawOutcome, ensure};
use super::install::sized_leaf;
use super::{Candidate, PageTier, exec_root, lease_request, owned, page, publish, refusal, simple};
use crate::DurableError;
use crate::domain::pages::{RootLease, StoredPage};

/// D-LEASE, one snapshot: every page read is at most one SQL statement,
/// whatever it asks; it answers the requested pages in the requested order,
/// a duplicate as often as it is asked; it refuses an opaque page it read
/// before a page it missed; and once its lease is lost it refuses for the
/// lease, before an empty, missing or opaque page.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_page_read_is_one_statement_that_answers_its_lease_first<T>(tier: &T) -> LawOutcome
where
    T: PageTier + GraphTier,
{
    let owner = owned(tier, "pages-one-read").await?;
    let exec = owner.exec("one-read");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let first = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "one-read first", &[]);
    let second = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "one-read second", &[]);
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "one-read root",
        &[first, second],
    );
    let candidate = Candidate { root, fresh };
    publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(candidate.manifest(None, CommitId::mint())),
    )
    .await?;
    let opaque = NodeId([0x5a; 32]);
    tier.foreign(ForeignWrite::OpaqueBlob {
        hash: opaque.to_hex(),
        bytes: b"one-read opaque".to_vec(),
    })
    .await;
    let missing = NodeId([0xa5; 32]);
    let lease = store
        .acquire_root_lease(lease_request(&owner, &exec, exec_root(root).id, 1_000))
        .await?;
    let read = |pages: Vec<NodeId>| {
        let lease: RootLease = lease.clone();
        async move {
            let (answer, receipt) =
                sql::collect("pages.read", "law", store.read_pages(&lease, &pages)).await;
            ensure!(
                receipt.statements <= 1,
                "a read of {} pages took {} statements",
                pages.len(),
                receipt.statements
            );
            Ok::<Result<Vec<StoredPage>, DurableError>, LawBroken>(answer)
        }
    };

    ensure!(
        read(Vec::new()).await??.is_empty(),
        "an empty read answered pages"
    );
    let asked = vec![second, first, second];
    let answered = read(asked.clone()).await??;
    ensure!(
        answered.iter().map(|page| page.id).collect::<Vec<_>>() == asked,
        "a read answered other pages or another order"
    );
    for page in &answered {
        ensure!(
            page.bytes[..] == candidate.fresh[&page.id].bytes[..],
            "page {} was read with other bytes",
            page.id
        );
    }
    ensure!(
        refusal(read(vec![first, missing]).await?) == Some(GraphRefusal::MissingNode(missing)),
        "a read of a missing page did not miss it"
    );
    ensure!(
        refusal(read(vec![missing, opaque]).await?) == Some(GraphRefusal::EncodingConflict(opaque)),
        "a read that returned an opaque page refused another way"
    );

    tier.advance(1_000);
    for pages in [
        Vec::new(),
        vec![first],
        vec![missing],
        vec![opaque],
        vec![opaque, missing, first],
    ] {
        ensure!(
            refusal(read(pages.clone()).await?) == Some(GraphRefusal::LeaseLost),
            "an expired lease's read of {pages:?} did not refuse for its lease"
        );
    }
    Ok(())
}

/// A candidate's check reads each node it only names by its encoding and
/// never its bytes, and still compares each node it supplies byte for byte:
/// a stored copy that differs refuses the publication.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_candidate_check_fetches_only_the_bytes_it_compares<T>(tier: &T) -> LawOutcome
where
    T: PageTier + GraphTier,
{
    const NAMED: usize = 8;
    const NAMED_BYTES: usize = 16 * 1024;
    let owner = owned(tier, "pages-narrow-check").await?;
    let mut base = BTreeMap::new();
    let named: Vec<NodeId> = (0..NAMED)
        .map(|at| sized_leaf(&mut base, &format!("narrow named {at}"), NAMED_BYTES))
        .collect();
    let base_root = page(&mut base, NodeKind::CHECKPOINT_ROOT, "narrow base", &named);
    publish(
        tier,
        &owner,
        &simple(&owner.exec("narrow-base"), None),
        Box::new(
            Candidate {
                root: base_root,
                fresh: base,
            }
            .manifest(None, CommitId::mint()),
        ),
    )
    .await?;

    // A candidate naming every large page and supplying small ones.
    let mut fresh = BTreeMap::new();
    let kept = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "narrow kept", &[]);
    let keyed = page(&mut fresh, NodeKind::KEYED_LEAF, "narrow keyed", &named);
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "narrow root",
        &[keyed, kept],
    );
    let naming = Candidate { root, fresh };
    let (published, receipt) = sql::collect(
        "pages.narrow-check",
        "law",
        publish(
            tier,
            &owner,
            &simple(&owner.exec("narrow-naming"), None),
            Box::new(naming.manifest(None, CommitId::mint())),
        ),
    )
    .await;
    published?;
    ensure!(
        receipt.result_bytes < NAMED_BYTES as u64,
        "a candidate naming {NAMED} pages of {NAMED_BYTES} bytes read {} result bytes",
        receipt.result_bytes
    );

    // Another candidate supplies the kept page again, whose stored copy
    // changed under its digest.
    let mut changed = naming.fresh[&kept].bytes.to_vec();
    if let Some(last) = changed.last_mut() {
        *last ^= 1;
    }
    tier.foreign(ForeignWrite::OverwriteBlob {
        node: kept,
        bytes: changed,
    })
    .await;
    let mut fresh = BTreeMap::new();
    fresh.insert(kept, naming.fresh[&kept].clone());
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "narrow again",
        &[kept],
    );
    let again = publish(
        tier,
        &owner,
        &simple(&owner.exec("narrow-again"), None),
        Box::new(Candidate { root, fresh }.manifest(None, CommitId::mint())),
    )
    .await;
    ensure!(
        refusal(again) == Some(GraphRefusal::ContentConflict(kept)),
        "a supplied page whose stored copy differs was not refused"
    );
    Ok(())
}

/// D-LEASE / FIG-7084: publishing a checkpoint and leasing its exact root
/// are atomic. A refused or abandoned replacement releases nothing; an
/// acknowledged replacement transfers the old lease's retirement to its
/// transaction and hands the reader the new lease.
///
/// # Errors
///
/// The first rule broken.
pub async fn checkpoint_lease_handoffs_are_atomic(tier: &dyn PageTier) -> LawOutcome {
    use super::{drain, publish_in};
    use crate::domain::RootOwner;
    let owner = owned(tier, "pages-lease-handoff").await?;
    let exec = owner.exec("handoff");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let leaf = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "handoff leaf", &[]);
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "handoff root",
        &[leaf],
    );
    let first = Candidate { root, fresh };
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.lease_root(RootOwner::Exec(exec.clone()), exec_root(root).id, 60_000);
    let receipt = publish_in(
        store,
        tx,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    ensure!(
        receipt.actor.root_leases.len() == 1,
        "publication did not hand on its lease"
    );
    let lease = receipt.actor.root_leases[0].clone();
    ensure!(lease.root == exec_root(root).id, "leased another root");
    store.read_pages(&lease, &[leaf]).await?;

    let mut abandoned = store.begin(&owner.actor, owner.epoch).await?;
    abandoned.release_root_lease(lease.id);
    drop(abandoned);
    store.read_pages(&lease, &[leaf]).await?;
    let mut refused = store.begin(&owner.actor, owner.epoch).await?;
    refused.release_root_lease(lease.id);
    refused.lease_root(RootOwner::Exec(exec.clone()), NodeId([0xa9; 32]), 60_000);
    ensure!(
        super::refusal(store.commit(refused, super::super::LABEL).await)
            == Some(GraphRefusal::RevisionConflict),
        "a changed root was leased"
    );
    store.read_pages(&lease, &[leaf]).await?;

    let mut fresh = BTreeMap::new();
    let next = page(&mut fresh, NodeKind::CHECKPOINT_ROOT, "handoff next", &[]);
    let second = Candidate { root: next, fresh };
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.lease_root(RootOwner::Exec(exec.clone()), exec_root(next).id, 60_000);
    tx.release_root_lease(lease.id);
    let receipt = publish_in(
        store,
        tx,
        &simple(&exec, Some((1, exec_root(root).id))),
        Box::new(second.manifest(Some(exec_root(root).id), CommitId::mint())),
    )
    .await?;
    ensure!(
        receipt.actor.root_leases.len() == 1,
        "replacement did not hand on its lease"
    );
    store
        .read_pages(&receipt.actor.root_leases[0], &[next])
        .await?;
    ensure!(
        super::refusal(store.read_pages(&lease, &[leaf]).await) == Some(GraphRefusal::LeaseLost),
        "the replaced lease still reads"
    );
    drain(store, 10).await?;
    ensure!(
        !tier.stored().await.contains(&leaf),
        "the released root's pages were not retired"
    );
    Ok(())
}
