//! Retained graph roots obey the same durable reachability rule.
use super::*;

/// LO-ROOT: a retained delta is only a reference hint. Held diamonds survive
/// bounded retirement; a missing unheld root refuses the complete publication.
///
/// # Errors
///
/// The first reachability or atomicity rule broken.
pub async fn lo_root_retained_delta_preserves_reachability(tier: &dyn PageTier) -> LawOutcome {
    let owner = owned(tier, "lo-root").await?;
    let exec = owner.exec("delta");
    let store = tier.pages();
    let mut fresh = BTreeMap::new();
    let shared = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "diamond leaf", &[]);
    let left = page(
        &mut fresh,
        NodeKind::KEYED_INDEX,
        "diamond left",
        &[shared, shared],
    );
    let right = page(
        &mut fresh,
        NodeKind::KEYED_INDEX,
        "diamond right",
        &[shared],
    );
    let old_root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "diamond root",
        &[left, right],
    );
    let first = Candidate {
        root: old_root,
        fresh,
    };
    let receipt = publish(
        tier,
        &owner,
        &simple(&exec, None),
        Box::new(first.manifest(None, CommitId::mint())),
    )
    .await?;
    ensure!(
        receipt.root == old_root,
        "the cold reference installed another root"
    );
    let old_exec = exec_root(old_root).id;
    let held = store
        .acquire_root_lease(lease_request(&owner, &exec, old_exec, 600_000))
        .await?;
    let mut fresh = BTreeMap::new();
    let added = page(&mut fresh, NodeKind::PAYLOAD_LEAF, "new leaf", &[]);
    let current = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "retained delta",
        &[shared, added],
    );
    let next = Candidate {
        root: current,
        fresh,
    };
    let before = tier.stored().await;
    publish(
        tier,
        &owner,
        &simple(&exec, Some((1, old_exec))),
        Box::new(next.manifest(Some(old_exec), CommitId::mint())),
    )
    .await?;
    let installed: BTreeSet<_> = tier.stored().await.difference(&before).copied().collect();
    ensure!(
        installed == BTreeSet::from([added, current, exec_root(current).id]),
        "the delta installed old pages: {installed:?}"
    );
    // One-node batches leave a partially processed frontier. The concurrent
    // read's lease keeps the entire old diamond rooted throughout.
    let leased: Vec<_> = first.fresh.keys().copied().chain([old_exec]).collect();
    let (retired, read) = futures_util::join!(drain(store, 1), store.read_pages(&held, &leased));
    retired?;
    let read = read?;
    ensure!(
        read.iter().map(|page| page.id).collect::<Vec<_>>() == leased,
        "retirement damaged a held diamond"
    );
    for body in read {
        let bytes = first
            .fresh
            .get(&body.id)
            .map_or_else(|| exec_root(old_root).bytes, |node| node.bytes.clone());
        ensure!(body.bytes == bytes, "a retained page changed bytes");
    }
    store.release_root_lease(held.id).await?;
    drain(store, 1).await?;
    let stored = tier.stored().await;
    ensure!(
        [shared, added, current, exec_root(current).id]
            .iter()
            .all(|id| stored.contains(id)),
        "the current root lost a shared descendant"
    );
    ensure!(
        [left, right, old_root, old_exec]
            .iter()
            .all(|id| !stored.contains(id)),
        "an unheld old path leaked"
    );
    let roots = tier.catalog().await;
    let edges = tier.edge_rows().await;
    let mut fresh = BTreeMap::new();
    let missing = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        "remembered missing digest",
        &[old_root],
    );
    let refused = Candidate {
        root: missing,
        fresh,
    };
    let error = refusal(
        publish(
            tier,
            &owner,
            &simple(&exec, Some((2, exec_root(current).id))),
            Box::new(refused.manifest(Some(exec_root(current).id), CommitId::mint())),
        )
        .await,
    );
    ensure!(
        error == Some(GraphRefusal::MissingNode(old_root)),
        "a memory-only digest resurrected content: {error:?}"
    );
    ensure!(
        tier.stored().await == stored
            && tier.catalog().await == roots
            && tier.edge_rows().await == edges,
        "a refused delta partially published"
    );
    Ok(())
}
