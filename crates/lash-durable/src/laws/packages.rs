//! The package laws (FIG-6918, S1-F2, FIG-6991): what a store keeps of
//! package artifacts and sets and their surfaces, what a node advertises
//! it holds and offers, and CLAIM-HOLDS.

use std::collections::BTreeSet;

use super::{LawBroken, LawOutcome};
use crate::domain::CANCEL_MAIL;
use crate::packages::{ArtifactInstall, HeldArtifact, SetInstall, held_sets};
use crate::packages::{EffectName, FunctionId};
use crate::{
    ActorKey, ArtifactDigest, ClaimPurpose, CommitLabel, DurableError, DurableStore,
    ExecutionRequirements, FormatSet, MailKind, MailTx, NodeCapabilities, NodeId, NodeLease,
    NodeSpec, PackageSetDigest, Release,
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
        definitions: BTreeSet::new(),
        internal: BTreeSet::new(),
        labels: format!("{{\"release\":\"{}\"}}", artifact.path),
    }
}

fn set(byte: u8, members: &[&HeldArtifact]) -> SetInstall {
    SetInstall {
        digest: PackageSetDigest::from_bytes([byte; 32]),
        members: members.iter().map(|member| (*member).clone()).collect(),
        surface: vec![byte],
        requirements: ExecutionRequirements::default(),
    }
}

fn function(byte: u8) -> FunctionId {
    FunctionId::from_bytes([byte; 32])
}

/// PROMOTE-BY-CAPABILITY's store facts (FIG-6991): a set's surface and
/// requirements are what its first install wrote, and a later install of
/// the set, with other bytes, changes neither. The definition index answers
/// every package path a stored definition is one of, helpers included, and
/// the internal index every path that declares an operation internal;
/// neither answers what no install wrote. An artifact keeps the labels of
/// every install, in install order and each once, though its content and
/// so its set are one. A live node's capabilities, its own set among them,
/// are what it registered, and are gone with its lease.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_store_keeps_each_sets_first_surface_and_indexes_its_definitions(
    store: &dyn DurableStore,
) -> LawOutcome {
    let lib = artifact("lib", 41);
    let mut first = ArtifactInstall {
        definitions: [function(1), function(2)].into_iter().collect(),
        internal: [EffectName::new("lib.explain").map_err(|error| LawBroken(error.to_string()))?]
            .into_iter()
            .collect(),
        ..install(&lib)
    };
    let requirements = ExecutionRequirements {
        natives: [function(2)].into_iter().collect(),
        kernels: [1].into_iter().collect(),
        ..ExecutionRequirements::default()
    };
    let mut stored = set(42, &[&lib]);
    stored.surface = b"the first surface".to_vec();
    stored.requirements = requirements.clone();
    store
        .install_packages(std::slice::from_ref(&first), std::slice::from_ref(&stored))
        .await?;
    let mut again = stored.clone();
    again.surface = b"another surface".to_vec();
    again.requirements = ExecutionRequirements::default();
    let first_labels = first.labels.clone();
    first.definitions.insert(function(3));
    first.labels = "{\"release\":\"1.0.1\"}".to_owned();
    store
        .install_packages(std::slice::from_ref(&first), &[again])
        .await?;
    store
        .install_packages(std::slice::from_ref(&first), &[])
        .await?;
    let labels = store
        .package_labels(&[lib.digest, artifact("none", 45).digest])
        .await?;
    let expected: std::collections::BTreeMap<ArtifactDigest, Vec<String>> =
        [(lib.digest, vec![first_labels, first.labels.clone()])].into();
    super::ensure!(labels == expected, "the stored labels are {labels:?}");
    let surface = store.package_surface(&stored.digest).await?;
    super::ensure!(
        surface.as_deref() == Some(&b"the first surface"[..]),
        "the stored surface is {surface:?}"
    );
    let absent = store
        .package_surface(&PackageSetDigest::from_bytes([43; 32]))
        .await?;
    super::ensure!(
        absent.is_none(),
        "an uninstalled set's surface is {absent:?}"
    );
    let catalog = store.package_catalog().await?;
    let kept: Vec<&ExecutionRequirements> = catalog
        .sets
        .iter()
        .filter(|set| set.digest == stored.digest)
        .map(|set| &set.requirements)
        .collect();
    super::ensure!(
        kept == vec![&requirements],
        "the stored requirements are {kept:?}"
    );
    let other = ArtifactInstall {
        definitions: [function(1)].into_iter().collect(),
        ..install(&artifact("other", 44))
    };
    store.install_packages(&[other], &[]).await?;
    let paths = store
        .package_paths(&[function(1), function(3), function(9)])
        .await?;
    let expected: std::collections::BTreeMap<FunctionId, BTreeSet<String>> = [
        (function(1), ["lib".to_owned(), "other".to_owned()].into()),
        (function(3), ["lib".to_owned()].into()),
    ]
    .into();
    super::ensure!(paths == expected, "the definition index answered {paths:?}");
    let explain = EffectName::new("lib.explain").map_err(|error| LawBroken(error.to_string()))?;
    let run = EffectName::new("lib.run").map_err(|error| LawBroken(error.to_string()))?;
    let internal = store.internal_operations(&[explain.clone(), run]).await?;
    let expected: std::collections::BTreeMap<EffectName, BTreeSet<String>> =
        [(explain, ["lib".to_owned()].into())].into();
    super::ensure!(
        internal == expected,
        "the internal index answered {internal:?}"
    );

    let capabilities = NodeCapabilities {
        natives: [function(2)].into_iter().collect(),
        kernels: [1].into_iter().collect(),
        providers: ["tools.law".to_owned()].into_iter().collect(),
        codecs: [("tools.law".to_owned(), 2)].into_iter().collect(),
        surface: Some(stored.digest),
    };
    let lease = store
        .register_node(&NodeSpec {
            node: NodeId::new("offers"),
            decodes: vec![FormatSet::new("law:packages")],
            serves: Vec::new(),
            held: [lib].into_iter().collect(),
            capabilities: capabilities.clone(),
            ttl_millis: 15_000,
        })
        .await?;
    let live = store.live_nodes().await?;
    let advertised: Vec<&NodeCapabilities> = live.iter().map(|node| &node.capabilities).collect();
    super::ensure!(
        advertised == vec![&capabilities],
        "the live node advertises {advertised:?}"
    );
    super::ensure!(
        requirements.met_by(&live[0].capabilities),
        "the node does not meet the requirements it offers"
    );
    store.release_node(&lease).await?;
    let live = store.live_nodes().await?;
    super::ensure!(
        live.is_empty(),
        "a released node still advertises {:?}",
        live.iter()
            .map(|node| &node.capabilities)
            .collect::<Vec<_>>()
    );
    Ok(())
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
            capabilities: Default::default(),
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
