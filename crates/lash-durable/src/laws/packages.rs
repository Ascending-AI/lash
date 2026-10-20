//! The package laws (FIG-6918, S1-F2): what a store keeps of package
//! artifacts and sets, what a node advertises it holds, and CLAIM-HOLDS.

use std::collections::BTreeSet;

use super::{LawBroken, LawOutcome};
use crate::domain::CANCEL_MAIL;
use crate::packages::{ArtifactInstall, HeldArtifact, SetInstall, held_sets};
use crate::{
    ActorKey, ArtifactDigest, ClaimPurpose, CommitLabel, DurableError, DurableStore, FormatSet,
    MailKind, MailTx, NodeId, NodeLease, NodeSpec, PackageSetDigest, Release,
};

fn artifact(path: &str, byte: u8) -> HeldArtifact {
    HeldArtifact {
        digest: ArtifactDigest::from_bytes([byte; 32]),
        path: path.to_owned(),
    }
}

fn install(artifact: &HeldArtifact) -> ArtifactInstall {
    ArtifactInstall {
        artifact: artifact.clone(),
        header: format!("{{\"path\":\"{}\"}}", artifact.path),
    }
}

fn set(byte: u8, members: &[&HeldArtifact]) -> SetInstall {
    SetInstall {
        digest: PackageSetDigest::from_bytes([byte; 32]),
        members: members.iter().map(|member| (*member).clone()).collect(),
    }
}

async fn node(
    store: &dyn DurableStore,
    name: &str,
    held: &[&HeldArtifact],
) -> Result<NodeLease, LawBroken> {
    Ok(store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![FormatSet::new("law:packages")],
            serves: Vec::new(),
            held: held.iter().map(|artifact| (*artifact).clone()).collect(),
            ttl_millis: 15_000,
        })
        .await?)
}

/// A store keeps each artifact and set once, by digest: an artifact takes
/// the next ordinal on its package line, a set the next install sequence,
/// and installing either again changes nothing. A set naming an artifact
/// the store does not hold is refused, whole. A live node's held artifacts
/// are what it registered, and the sets it holds are derived from them,
/// newest first, with no set row naming the node.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_store_keeps_artifacts_and_sets_by_digest_and_derives_what_a_node_holds(
    store: &dyn DurableStore,
) -> LawOutcome {
    let p1 = artifact("p", 1);
    let q1 = artifact("q", 2);
    let q2 = artifact("q", 3);
    let p2 = artifact("p", 4);
    let old = set(10, &[&p1, &q1]);
    let new = set(11, &[&p2, &q2]);
    store
        .install_packages(&[install(&p1), install(&q1)], std::slice::from_ref(&old))
        .await?;
    store
        .install_packages(
            &[install(&q2), install(&p2), install(&p1)],
            &[new.clone(), old.clone()],
        )
        .await?;
    let unknown = set(12, &[&p1, &artifact("r", 5)]);
    let refused = store
        .install_packages(
            &[install(&artifact("s", 6))],
            std::slice::from_ref(&unknown),
        )
        .await;
    super::ensure!(
        matches!(refused, Err(DurableError::UnknownArtifact { .. })),
        "a set naming an uninstalled artifact answered {refused:?}"
    );
    let catalog = store.package_catalog().await?;
    let ordinals: Vec<(String, u8, u32)> = catalog
        .artifacts
        .iter()
        .map(|stored| {
            (
                stored.artifact.path.clone(),
                stored.artifact.digest.as_bytes()[0],
                stored.ordinal,
            )
        })
        .collect();
    super::ensure!(
        ordinals
            == vec![
                ("p".to_owned(), 1, 1),
                ("p".to_owned(), 4, 2),
                ("q".to_owned(), 2, 1),
                ("q".to_owned(), 3, 2),
            ],
        "the stored artifacts and their ordinals are {ordinals:?}"
    );
    let sets: Vec<(PackageSetDigest, u64, BTreeSet<HeldArtifact>)> = catalog
        .sets
        .iter()
        .map(|stored| (stored.digest, stored.sequence, stored.members.clone()))
        .collect();
    super::ensure!(
        sets == vec![
            (old.digest, 1, old.members.clone()),
            (new.digest, 2, new.members.clone())
        ],
        "the stored sets are {sets:?}"
    );

    node(store, "holds-both", &[&p1, &q1, &p2, &q2]).await?;
    node(store, "holds-old", &[&p1, &q1]).await?;
    let live = store.live_nodes().await?;
    super::ensure!(
        live.len() == 2,
        "live_nodes answered {} nodes, not 2",
        live.len()
    );
    let held: Vec<Vec<PackageSetDigest>> = live
        .iter()
        .map(|node| {
            held_sets(&catalog, node)
                .iter()
                .map(|set| set.digest)
                .collect()
        })
        .collect();
    super::ensure!(
        held == vec![vec![new.digest, old.digest], vec![old.digest]],
        "the nodes hold the sets {held:?}"
    );
    Ok(())
}

/// CLAIM-HOLDS (S1-F2): a node claims an actor to run it only when it
/// decodes the actor's format set and holds the package set its code was
/// linked under. A node that decodes the set but lacks a member does not
/// run it, and claims it only for a pending cancel; an actor stamped as
/// pinning no code is claimed as its formats alone allow.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_node_claims_only_actors_whose_package_set_it_holds(
    store: &dyn DurableStore,
) -> LawOutcome {
    let base = artifact("base", 21);
    let s1 = artifact("lib", 22);
    let s2 = artifact("lib", 23);
    let first = set(30, &[&base, &s1]);
    let second = set(31, &[&base, &s2]);
    store
        .install_packages(
            &[install(&base), install(&s1), install(&s2)],
            &[first.clone(), second.clone()],
        )
        .await?;
    let newer = node(store, "holds-s2", &[&base, &s1, &s2]).await?;
    let process =
        ActorKey::process("pinned-to-s2").map_err(|error| LawBroken(error.to_string()))?;
    let mut create = MailTx::new();
    create.create_actor(process.clone(), FormatSet::new("law:packages"));
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await?;
    let claimed = store.claim(&newer, 8).await?;
    super::ensure!(
        claimed.len() == 1 && claimed[0].purpose == ClaimPurpose::Run,
        "the holding node's first claim was {claimed:?}"
    );
    let mut stamp = store.begin(&process, claimed[0].epoch).await?;
    stamp
        .stamp_packages(Some(second.digest))
        .give_up(Release::Ready);
    store.commit(stamp, CommitLabel::new("law.stamp")).await?;

    let older = node(store, "holds-s1", &[&base, &s1]).await?;
    super::ensure!(
        store.claim(&older, 8).await?.is_empty(),
        "a node lacking the actor's package set claimed it, though it decodes its formats"
    );
    let mut cancel = MailTx::new();
    cancel.append(process.clone(), MailKind::new(CANCEL_MAIL), "{}");
    store
        .commit_mail(cancel, CommitLabel::new("law.cancel"))
        .await?;
    let claimed = store.claim(&older, 8).await?;
    super::ensure!(
        claimed.len() == 1 && claimed[0].purpose == ClaimPurpose::CancelOnly,
        "the lacking node's claim of the cancelled actor was {claimed:?}"
    );
    let mut back = store.begin(&process, claimed[0].epoch).await?;
    back.stamp_packages(None).give_up(Release::Ready);
    store.commit(back, CommitLabel::new("law.unpin")).await?;
    let claimed = store.claim(&older, 8).await?;
    super::ensure!(
        claimed.len() == 1 && claimed[0].purpose == ClaimPurpose::Run,
        "an actor pinning no code was not claimed to run: {claimed:?}"
    );
    let adopted = store.owned(&older).await?;
    super::ensure!(
        adopted.len() == 1 && adopted[0].purpose == ClaimPurpose::Run,
        "the owner's adoption read {adopted:?}"
    );
    let mut repin = store.begin(&process, claimed[0].epoch).await?;
    repin.stamp_packages(Some(second.digest));
    store.commit(repin, CommitLabel::new("law.repin")).await?;
    let adopted = store.owned(&older).await?;
    super::ensure!(
        adopted.len() == 1 && adopted[0].purpose == ClaimPurpose::CancelOnly,
        "an owner lacking the set its actor now pins adopted it as {adopted:?}"
    );
    Ok(())
}
