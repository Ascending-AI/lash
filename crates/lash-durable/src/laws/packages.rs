//! The package laws (FIG-6918, S1-F2, FIG-6991, FIG-6993): what a store
//! keeps of package artifacts and sets and their surfaces, what a node
//! advertises it offers, and CLAIM-BY-REQUIREMENTS.

use std::collections::BTreeSet;

use super::{LawBroken, LawOutcome};
use crate::domain::CANCEL_MAIL;
use crate::packages::{ArtifactInstall, PackageArtifact, SetInstall};
use crate::packages::{EffectName, FunctionId};
use crate::{
    ActorKey, ArtifactDigest, ClaimPurpose, CommitLabel, DurableError, DurableStore,
    ExecutionRequirements, FormatSet, MailKind, MailTx, NodeCapabilities, NodeId, NodeLease,
    NodeSpec, PackageSetDigest, Release,
};

fn artifact(path: &str, byte: u8) -> PackageArtifact {
    PackageArtifact {
        digest: ArtifactDigest::from_bytes([byte; 32]),
        path: path.to_owned(),
    }
}

fn install(artifact: &PackageArtifact) -> ArtifactInstall {
    ArtifactInstall {
        artifact: artifact.clone(),
        header: format!("{{\"path\":\"{}\"}}", artifact.path),
        definitions: BTreeSet::new(),
        internal: BTreeSet::new(),
        labels: format!("{{\"release\":\"{}\"}}", artifact.path),
    }
}

fn set(byte: u8, members: &[&PackageArtifact]) -> SetInstall {
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
    capabilities: NodeCapabilities,
) -> Result<NodeLease, LawBroken> {
    Ok(store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![FormatSet::new("law:packages")],
            serves: Vec::new(),
            capabilities,
            ttl_millis: 15_000,
        })
        .await?)
}

/// A store keeps each artifact and set once, by digest: an artifact takes
/// the next ordinal on its package line, a set the next install sequence,
/// and installing either again changes nothing. A set naming an artifact
/// the store does not hold is refused, whole.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_store_keeps_artifacts_and_sets_by_digest(store: &dyn DurableStore) -> LawOutcome {
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
    let sets: Vec<(PackageSetDigest, u64, BTreeSet<PackageArtifact>)> = catalog
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
    Ok(())
}

/// CLAIM-BY-REQUIREMENTS (FIG-6993): a node claims an actor to run it only
/// when its capabilities serve every hard requirement the actor's record
/// names (a native it reached, a kernel version, a linked provider, a
/// codec version), whatever package set its code was linked under. A node
/// that lacks one claims it only for a pending cancel, and the strand read
/// names what the closest live node lacks; an owned recovery judges by the
/// same predicate, so an owner whose actor's record grew past its
/// capabilities adopts it only to cancel; and once the actor's record no
/// longer names what the node lacks, the node runs it.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_node_claims_only_actors_whose_hard_requirements_it_meets(
    store: &dyn DurableStore,
) -> LawOutcome {
    use lash_core_store::obligations::ObligationToken;
    let reached = function(41);
    let record = ExecutionRequirements {
        natives: [reached].into_iter().collect(),
        kernels: [1].into_iter().collect(),
        providers: ["host/inbox".to_owned()].into_iter().collect(),
        codecs: [("host/inbox".to_owned(), 2)].into_iter().collect(),
    };
    let mut requires: Vec<String> = ObligationToken::for_requirements(&record)
        .map(String::from)
        .collect();
    // The store answers a record's tokens in their order.
    requires.sort();
    let lacking = NodeCapabilities {
        natives: BTreeSet::new(),
        kernels: [1].into_iter().collect(),
        providers: ["host/inbox".to_owned()].into_iter().collect(),
        codecs: [("host/inbox".to_owned(), 1)].into_iter().collect(),
        surface: None,
    };
    let capable = NodeCapabilities {
        natives: [reached, function(42)].into_iter().collect(),
        codecs: [("host/inbox".to_owned(), 1), ("host/inbox".to_owned(), 2)]
            .into_iter()
            .collect(),
        ..lacking.clone()
    };
    let older = node(store, "lacks-native-and-codec", lacking.clone()).await?;
    let process =
        ActorKey::process("requires-a-native").map_err(|error| LawBroken(error.to_string()))?;
    let mut create = MailTx::new();
    create.create_actor(process.clone(), FormatSet::new("law:packages"));
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await?;
    let claimed = store.claim(&older, 8).await?;
    super::ensure!(
        claimed.len() == 1 && claimed[0].purpose == ClaimPurpose::Run,
        "an actor requiring nothing was not claimed to run: {claimed:?}"
    );
    let mut record_write = store.begin(&process, claimed[0].epoch).await?;
    record_write
        .set_requires(requires.clone())
        .give_up(Release::Ready);
    store
        .commit(record_write, CommitLabel::new("law.record"))
        .await?;

    super::ensure!(
        store.claim(&older, 8).await?.is_empty(),
        "a node lacking a native and a codec version the record names claimed it"
    );
    let stranded = store.actor_requirements(None, 8).await?;
    let unmet: Vec<_> = stranded
        .iter()
        .filter_map(|(actor, requires)| {
            crate::packages::strand(requires, &store_nodes(&[&lacking]))
                .map(|unmet| (actor.clone(), unmet))
        })
        .collect();
    let expected = ExecutionRequirements {
        natives: [reached].into_iter().collect(),
        codecs: [("host/inbox".to_owned(), 2)].into_iter().collect(),
        ..ExecutionRequirements::default()
    };
    super::ensure!(
        unmet == vec![(process.clone(), expected)],
        "the strand read named {unmet:?}"
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
    let adopted = store.owned(&older).await?;
    super::ensure!(
        adopted.len() == 1 && adopted[0].purpose == ClaimPurpose::CancelOnly,
        "the lacking owner's recovery read {adopted:?}"
    );
    let mut back = store.begin(&process, claimed[0].epoch).await?;
    back.ack_seen().give_up(Release::Ready);
    store.commit(back, CommitLabel::new("law.back")).await?;

    let newer = node(store, "meets-the-record", capable).await?;
    let claimed = store.claim(&newer, 8).await?;
    super::ensure!(
        claimed.len() == 1
            && claimed[0].purpose == ClaimPurpose::Run
            && claimed[0].requires == requires,
        "the capable node's claim was {claimed:?}"
    );
    let adopted = store.owned(&newer).await?;
    super::ensure!(
        adopted.len() == 1 && adopted[0].purpose == ClaimPurpose::Run,
        "the capable owner's recovery read {adopted:?}"
    );
    // The record shrinks once the work that reached the native settles:
    // the lacking node runs it again.
    let mut settle = store.begin(&process, claimed[0].epoch).await?;
    settle
        .set_requires(
            ObligationToken::for_requirements(&ExecutionRequirements {
                kernels: [1].into_iter().collect(),
                ..ExecutionRequirements::default()
            })
            .map(String::from)
            .collect(),
        )
        .give_up(Release::Ready);
    store.commit(settle, CommitLabel::new("law.settle")).await?;
    store.release_node(&newer).await?;
    let claimed = store.claim(&older, 8).await?;
    super::ensure!(
        claimed.len() == 1 && claimed[0].purpose == ClaimPurpose::Run,
        "once its record names nothing the node lacks, the actor was claimed as {claimed:?}"
    );
    Ok(())
}

fn store_nodes(capabilities: &[&NodeCapabilities]) -> Vec<crate::NodeCapability> {
    capabilities
        .iter()
        .map(|capabilities| crate::NodeCapability {
            decodes: vec![FormatSet::new("law:packages")],
            capabilities: (*capabilities).clone(),
        })
        .collect()
}

/// CRASH-ATOMIC (FIG-6993): an actor's hard record is written by the owner
/// commit that changes the work it describes, inside that commit's
/// transaction. A commit the store refuses after its record and its other
/// writes were staged, here by a stale execution root as its last write,
/// leaves the stored record as it stood: a node lacking the native the
/// refused record names still claims the actor to run, and the strand read
/// still answers the old record. The commit that lands writes both.
pub async fn a_hard_record_lands_with_its_owner_commit_or_not_at_all(
    store: &dyn DurableStore,
) -> LawOutcome {
    use crate::domain::graph::ExecRootWrite;
    use crate::domain::snapshots::SnapshotRev;
    use crate::domain::{DomainRefusal, DomainWrite, ExecKey};
    use lash_core_store::obligations::ObligationToken;
    let tokens = |record: &ExecutionRequirements| -> Vec<String> {
        let mut tokens: Vec<String> = ObligationToken::for_requirements(record)
            .map(String::from)
            .collect();
        tokens.sort();
        tokens
    };
    let kernel_only = ExecutionRequirements {
        kernels: [1].into_iter().collect(),
        ..ExecutionRequirements::default()
    };
    let with_native = ExecutionRequirements {
        natives: [function(51)].into_iter().collect(),
        ..kernel_only.clone()
    };
    let writer = node(
        store,
        "kernel-only-writer",
        NodeCapabilities {
            kernels: [1].into_iter().collect(),
            ..NodeCapabilities::default()
        },
    )
    .await?;
    let id = "p_000000000000700080000000000000a1";
    let process = ActorKey::process(id).map_err(|error| LawBroken(error.to_string()))?;
    let mut create = MailTx::new();
    create.create_actor(process.clone(), FormatSet::new("law:packages"));
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await?;
    let claimed = store.claim(&writer, 8).await?;
    super::ensure!(
        claimed.len() == 1,
        "the new actor was claimed as {claimed:?}"
    );
    let mut first = store.begin(&process, claimed[0].epoch).await?;
    first
        .set_requires(tokens(&kernel_only))
        .give_up(Release::Ready);
    store.commit(first, CommitLabel::new("law.first")).await?;

    let claimed = store.claim(&writer, 8).await?;
    super::ensure!(
        claimed.len() == 1 && claimed[0].purpose == ClaimPurpose::Run,
        "the kernel-only record was claimed as {claimed:?}"
    );
    let exec = ExecKey::Process(
        lash_protocol_types::ProcessId::parse(id).map_err(|error| LawBroken(error.to_string()))?,
    );
    let (root, staged) = super::tagged_node(&exec.stored());
    let (stale, _) = super::tagged_node("never-put");
    let mut refused = store.begin(&process, claimed[0].epoch).await?;
    refused.set_requires(tokens(&with_native)).stage([staged]);
    refused.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec.clone(),
        expected: Some(SnapshotRev(7)),
        expected_root: Some(stale),
        root,
        commit_id: lash_core_store::checkpoint_graph::CommitId([5; 32]),
    }));
    let answer = store.commit(refused, CommitLabel::new("law.refused")).await;
    super::ensure!(
        matches!(
            answer,
            Err(DurableError::Domain(
                DomainRefusal::SnapshotRevConflict { .. }
            ))
        ),
        "the stale root's commit answered {answer:?}"
    );
    let stored = store.actor_requirements(None, 8).await?;
    super::ensure!(
        stored == vec![(process.clone(), tokens(&kernel_only))],
        "a refused commit moved the record: {stored:?}"
    );
    let mut release = store.begin(&process, claimed[0].epoch).await?;
    release.give_up(Release::Ready);
    store
        .commit(release, CommitLabel::new("law.release"))
        .await?;
    let claimed = store.claim(&writer, 8).await?;
    super::ensure!(
        claimed.len() == 1
            && claimed[0].purpose == ClaimPurpose::Run
            && claimed[0].requires == tokens(&kernel_only),
        "after the refused commit the actor was claimed as {claimed:?}"
    );

    let (root, staged) = super::tagged_node(&exec.stored());
    let mut landed = store.begin(&process, claimed[0].epoch).await?;
    landed.set_requires(tokens(&with_native)).stage([staged]);
    landed.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec,
        expected: None,
        expected_root: None,
        root,
        commit_id: lash_core_store::checkpoint_graph::CommitId([6; 32]),
    }));
    landed.give_up(Release::Ready);
    store.commit(landed, CommitLabel::new("law.landed")).await?;
    let stored = store.actor_requirements(None, 8).await?;
    super::ensure!(
        stored == vec![(process, tokens(&with_native))],
        "the landed commit's record is {stored:?}"
    );
    super::ensure!(
        store.claim(&writer, 8).await?.is_empty(),
        "a node lacking the landed record's native claimed it"
    );
    Ok(())
}
