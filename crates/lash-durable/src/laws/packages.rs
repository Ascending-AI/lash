//! Claim requirements are independent of package content.
use super::{LawBroken, LawOutcome};
use crate::domain::CANCEL_MAIL;
use crate::{
    ActorKey, ClaimPurpose, ClaimRequirements, CommitLabel, DurableError, DurableStore, FormatSet,
    MailKind, MailTx, NodeCapabilities, NodeId, NodeLease, NodeSpec, Release,
};
use lash_vocabulary::claims::NodeRequirements;
use lash_vocabulary::package::ProviderName;
use lash_vocabulary::providers::FormatVersion;
fn provider() -> ProviderName {
    ProviderName {
        owner: "host".into(),
        key: "inbox".into(),
    }
}
fn format_two() -> FormatVersion {
    FormatVersion::new(2).unwrap_or_else(|| unreachable!("nonzero literal"))
}
fn function(n: u8) -> lash_vocabulary::names::FunctionId {
    lash_vocabulary::names::FunctionId::from_bytes([n; 32])
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
    use lash_vocabulary::claims::ObligationToken;
    let reached = function(41);
    let record = ClaimRequirements {
        machine: NodeRequirements {
            natives: [reached].into(),
            kernels: [1].into(),
        },
        providers: [provider()].into(),
        codecs: [(provider(), format_two())].into(),
        ..ClaimRequirements::default()
    };
    let mut requires: Vec<String> = ObligationToken::for_requirements(&record)
        .map(String::from)
        .collect();
    // The store answers a record's tokens in their order.
    requires.sort();
    let lacking = NodeCapabilities {
        machine: NodeRequirements {
            kernels: [1].into(),
            ..NodeRequirements::default()
        },
        providers: [provider()].into(),
        codecs: [(provider(), FormatVersion::ONE)].into(),
        ..NodeCapabilities::default()
    };
    let capable = NodeCapabilities {
        machine: NodeRequirements {
            natives: [reached, function(42)].into(),
            ..lacking.machine.clone()
        },
        codecs: [(provider(), FormatVersion::ONE), (provider(), format_two())]
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
    let expected = ClaimRequirements {
        machine: NodeRequirements {
            natives: [reached].into(),
            ..NodeRequirements::default()
        },
        codecs: [(provider(), format_two())].into(),
        ..ClaimRequirements::default()
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
            ObligationToken::for_requirements(&ClaimRequirements {
                machine: NodeRequirements {
                    kernels: [1].into(),
                    ..NodeRequirements::default()
                },
                ..ClaimRequirements::default()
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
    use lash_vocabulary::claims::ObligationToken;
    let tokens = |record: &ClaimRequirements| -> Vec<String> {
        let mut tokens: Vec<String> = ObligationToken::for_requirements(record)
            .map(String::from)
            .collect();
        tokens.sort();
        tokens
    };
    let kernel_only = ClaimRequirements {
        machine: NodeRequirements {
            kernels: [1].into(),
            ..NodeRequirements::default()
        },
        ..ClaimRequirements::default()
    };
    let with_native = ClaimRequirements {
        machine: NodeRequirements {
            natives: [function(51)].into(),
            ..kernel_only.machine.clone()
        },
        ..kernel_only.clone()
    };
    let writer = node(
        store,
        "kernel-only-writer",
        NodeCapabilities {
            machine: NodeRequirements {
                kernels: [1].into(),
                ..NodeRequirements::default()
            },
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
        lash_vocabulary::identity::ProcessId::parse(id)
            .map_err(|error| LawBroken(error.to_string()))?,
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

/// L-DEFERRED-PIN: first resolution and target debt land once under the
/// actor revision. Conflicts, foreign uses and stale owners write nothing;
/// distinct executions have independent immutable negative outcomes.
pub async fn a_resolution_winner_and_its_claim_debt_commit_atomically(
    store: &dyn DurableStore,
    resolutions: &dyn crate::resolutions::ExecutionResolutions,
) -> LawOutcome {
    use crate::resolutions::*;
    use lash_core_store::artifact_referrer::{ArtifactReferrer, HostArtifactPin};
    use lash_core_store::tool_surface::{OperationRoute, RouteSource};
    use lash_vocabulary::{
        ToolDefinition,
        claims::{ClaimRequirements, RegistryKind},
        names::{ImportSlot, OperationUse, QualifiedName},
        package::{LinkId, OperationMode, ProviderName},
        providers::{ToolProviderHandle, ToolProviderKey},
    };
    let writer = node(store, "resolution-owner", NodeCapabilities::default()).await?;
    let actor =
        ActorKey::process("resolution-owner").map_err(|error| LawBroken(error.to_string()))?;
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), FormatSet::new("law:packages"));
    store
        .commit_mail(mail, CommitLabel::new("law.resolution.create"))
        .await?;
    let claimed = store.claim(&writer, 1).await?;
    let epoch = claimed[0].epoch;
    let use_ = OperationUse {
        link: LinkId::from_bytes([71; 32]),
        slot: ImportSlot(3),
    };
    let requested = QualifiedName {
        package: lash_vocabulary::names::PackagePath::parse("wip")
            .map_err(|error| LawBroken(format!("{error}")))?,
        member: lash_vocabulary::names::Segment::new("lookup")
            .map_err(|error| LawBroken(format!("{error}")))?,
        type_member: None,
    };
    let execution = ExecutionId("process:resolution-owner".to_owned());
    let shared = ClaimRequirements {
        providers: [ProviderName {
            owner: "shared".into(),
            key: "tools".into(),
        }]
        .into(),
        ..Default::default()
    };
    let mut discovery = shared.clone();
    discovery.providers.insert(ProviderName {
        owner: "discovery".into(),
        key: "resolver".into(),
    });
    let registration = ExecutionRegistration {
        execution: execution.clone(),
        uses: [(
            use_,
            crate::resolutions::DeferredUse {
                requested: requested.clone(),
                discovery: discovery.clone(),
            },
        )]
        .into(),
        referrer: ArtifactReferrer::HostPin(HostArtifactPin::mint()),
        requirements: ClaimRequirements::default(),
    };
    let mut tx = store.begin(&actor, epoch).await?;
    tx.register_execution(registration.clone())
        .set_requires(shared.tokens().collect());
    store
        .commit(tx, CommitLabel::new("law.resolution.register"))
        .await?;
    super::ensure!(
        resolutions.requirements(&actor).await? == discovery,
        "unresolved use lost discovery debt"
    );
    let revision = store.begin(&actor, epoch).await?.revision();
    let definition = ToolDefinition::raw(
        "tool:resolved",
        "wip.lookup",
        "lookup",
        serde_json::json!({}),
        serde_json::json!({}),
    )
    .map_err(|error| LawBroken(error.to_string()))?
    .with_execution(std::time::Duration::from_secs(1));
    let target = ClaimRequirements {
        providers: [ProviderName {
            owner: "target".to_owned(),
            key: "tools".to_owned(),
        }]
        .into(),
        ..ClaimRequirements::default()
    };
    let route = OperationRoute {
        tool: definition.manifest.id.clone(),
        provider: ToolProviderHandle::new(
            RegistryKind::Process,
            "target",
            ToolProviderKey::new("tools").map_err(|error| LawBroken(format!("{error:?}")))?,
        ),
        policy: definition.manifest.execution_policy,
        projection: definition.manifest.argument_projection.clone(),
        payload: None,
        source: RouteSource::Resolved {
            definition: Box::new(definition.clone()),
        },
    };
    let candidate = ResolutionRecord {
        key: ResolutionKey { execution, use_ },
        requested,
        entry: ResolutionEntry::Resolved {
            contract: definition.contract_id(),
            definition: Box::new(definition),
            route: Box::new(route),
            mode: OperationMode::Waited,
            requirements: target.clone(),
        },
    };
    let stale_revision = ResolutionOwner {
        actor: actor.clone(),
        epoch,
        expected_revision: crate::StateRevision(revision.0 - 1),
    };
    let refused = resolutions.commit(&stale_revision, candidate.clone()).await;
    super::ensure!(
        matches!(
            refused,
            Err(DurableError::ResolutionRevisionConflict { .. })
        ),
        "stale revision answered {refused:?}"
    );
    super::ensure!(
        resolutions.load(&candidate.key).await?.is_none(),
        "revision refusal stored an outcome"
    );
    let owner = ResolutionOwner {
        actor: actor.clone(),
        epoch,
        expected_revision: revision,
    };
    let mut foreign = candidate.clone();
    foreign.key.use_.slot = ImportSlot(4);
    let refused = resolutions.commit(&owner, foreign).await;
    super::ensure!(
        matches!(
            refused,
            Err(DurableError::Package(
                crate::packages::PackageRefusal::ExecutionUse
            ))
        ),
        "foreign use answered {refused:?}"
    );
    let first = resolutions.commit(&owner, candidate.clone());
    let second = resolutions.commit(&owner, candidate.clone());
    let (first, second) = tokio::join!(first, second);
    let first = first?;
    let second = second?;
    let (winner, loser) = match (first, second) {
        (CommitResolution::Inserted(winner), CommitResolution::Existing(loser))
        | (CommitResolution::Existing(loser), CommitResolution::Inserted(winner)) => {
            (winner, loser)
        }
        other => {
            return Err(LawBroken(format!(
                "racing first resolutions answered {other:?}"
            )));
        }
    };
    super::ensure!(
        winner == loser && resolutions.load(&candidate.key).await? == Some(winner.clone()),
        "callers did not share one winner: winner={winner:?} loser={loser:?} stored={:?}",
        resolutions.load(&candidate.key).await?
    );
    let debt = store.actor_requirements(None, 8).await?;
    let mut effective = shared.clone();
    effective.extend(&target);
    let expected: Vec<_> = effective.tokens().collect();
    let actual = debt
        .into_iter()
        .find(|(key, _)| key == &actor)
        .map(|(_, debt)| debt)
        .unwrap_or_default();
    super::ensure!(
        actual == expected,
        "winning target debt differs: {actual:?} != {expected:?}"
    );
    let after = store.begin(&actor, epoch).await?.revision();
    super::ensure!(
        after.0 == revision.0 + 1,
        "more than one resolution committed: {after:?}"
    );
    // Normal owner record replacement cannot erase a live resolved target.
    let mut replace = store.begin(&actor, epoch).await?;
    replace.set_requires(Vec::new());
    store
        .commit(replace, CommitLabel::new("law.resolution.protected"))
        .await?;
    super::ensure!(
        resolutions.requirements(&actor).await? == target,
        "the live source debt differs"
    );
    let expected: Vec<_> = target.tokens().collect();
    let stored = store.actor_requirements(None, 8).await?;
    super::ensure!(
        stored
            .iter()
            .any(|(key, debt)| key == &actor && *debt == expected),
        "an ordinary owner commit erased live target debt"
    );
    let mut successor = registration.clone();
    successor.uses.clear();
    let successor_use = OperationUse {
        slot: ImportSlot(5),
        ..use_
    };
    successor.uses.insert(
        successor_use,
        crate::resolutions::DeferredUse {
            requested: candidate.requested.clone(),
            discovery: ClaimRequirements::default(),
        },
    );
    let mut carried = candidate.clone();
    carried.key.use_ = successor_use;
    let mut adoption = store.begin(&actor, epoch).await?;
    adoption
        .adopt_execution(successor)
        .seed_resolution(carried.clone());
    store
        .commit(adoption, CommitLabel::new("law.resolution.adopt"))
        .await?;
    super::ensure!(
        resolutions.load(&carried.key).await? == Some(carried.clone()),
        "adoption lost the inherited outcome"
    );
    let mut conflicting = carried.clone();
    conflicting.entry = ResolutionEntry::Refused {
        note: "replacement".into(),
    };
    let mut refused_seed = store.begin(&actor, epoch).await?;
    let before_seed = refused_seed.revision();
    refused_seed
        .seed_resolution(conflicting)
        .set_requires(Vec::new());
    let refused = store
        .commit(
            refused_seed,
            CommitLabel::new("law.resolution.conflicting_seed"),
        )
        .await;
    super::ensure!(
        matches!(
            refused,
            Err(DurableError::Package(
                crate::packages::PackageRefusal::Immutable(_)
            ))
        ),
        "conflicting inherited pin answered {refused:?}"
    );
    super::ensure!(
        store.begin(&actor, epoch).await?.revision() == before_seed
            && resolutions.load(&carried.key).await? == Some(carried),
        "refused seed changed the actor or winner"
    );
    let mut no_live_uses = registration.clone();
    no_live_uses.uses.clear();
    let mut prune = store.begin(&actor, epoch).await?;
    prune.adopt_execution(no_live_uses);
    store
        .commit(prune, CommitLabel::new("law.resolution.dead_use"))
        .await?;
    super::ensure!(
        resolutions.requirements(&actor).await? == ClaimRequirements::default(),
        "dead use retained target debt"
    );
    super::ensure!(
        store
            .actor_requirements(None, 8)
            .await?
            .iter()
            .all(|(key, debt)| key != &actor || debt.is_empty()),
        "effective actor projection retained dead target debt"
    );
    let mut ended = store.begin(&actor, epoch).await?;
    ended
        .end_execution(registration.execution.clone())
        .set_requires(Vec::new());
    store
        .commit(ended, CommitLabel::new("law.resolution.end"))
        .await?;
    super::ensure!(
        resolutions.requirements(&actor).await? == ClaimRequirements::default(),
        "ended execution still owes debt"
    );
    super::ensure!(
        resolutions.load(&winner.key).await? == Some(winner),
        "ending debt removed resolution history"
    );
    let mut fresh = registration.clone();
    fresh.execution = ExecutionId("process:resolution-owner:fresh".into());
    fresh.uses.insert(successor_use, fresh.uses[&use_].clone());
    let mut admit = store.begin(&actor, epoch).await?;
    admit.register_execution(fresh.clone());
    store
        .commit(admit, CommitLabel::new("law.resolution.fresh"))
        .await?;
    let fresh_owner = ResolutionOwner {
        actor: actor.clone(),
        epoch,
        expected_revision: store.begin(&actor, epoch).await?.revision(),
    };
    let mut unavailable = candidate.clone();
    unavailable.key.execution = fresh.execution.clone();
    unavailable.entry = ResolutionEntry::Unavailable {
        note: Some("missing".into()),
    };
    super::ensure!(
        matches!(
            resolutions
                .commit(&fresh_owner, unavailable.clone())
                .await?,
            CommitResolution::Inserted(_)
        ),
        "fresh execution inherited old resolution"
    );
    let mut changed = unavailable.clone();
    changed.entry = ResolutionEntry::Refused {
        note: "later answer".into(),
    };
    super::ensure!(
        matches!(resolutions.commit(&fresh_owner, changed).await?, CommitResolution::Existing(record) if record == unavailable),
        "a negative outcome was not pinned"
    );
    super::ensure!(
        resolutions.requirements(&actor).await? == discovery,
        "one negative winner erased another unresolved use's overlapping discovery debt"
    );
    fresh.uses.remove(&successor_use);
    let mut prune = store.begin(&actor, epoch).await?;
    prune.adopt_execution(fresh);
    store
        .commit(prune, CommitLabel::new("law.resolution.prune_discovery"))
        .await?;
    super::ensure!(
        resolutions.requirements(&actor).await? == ClaimRequirements::default(),
        "negative winner or dead use still owes discovery debt"
    );
    super::ensure!(
        store
            .actor_requirements(None, 8)
            .await?
            .iter()
            .all(|(key, debt)| key != &actor || debt.is_empty()),
        "last discovery debt remained in the materialized claim filter"
    );
    let mut release = store.begin(&actor, epoch).await?;
    release.give_up(Release::Ready);
    store
        .commit(release, CommitLabel::new("law.resolution.release"))
        .await?;
    let refused = resolutions.commit(&owner, candidate).await;
    super::ensure!(
        matches!(refused, Err(DurableError::OwnershipLost(_))),
        "stale owner used a stored winner: {refused:?}"
    );
    Ok(())
}

/// L-PIN: a publication acquired by a workflow/process keeps the complete
/// Package/Link/Document DAG after its configured root ends. Carries preserve
/// that same closure; the final end reclaims it and fences late resurrection.
pub async fn a_publication_retains_its_complete_content_dag(
    packages: &dyn crate::packages::PackageStore,
    storage: &dyn crate::packages::PackageStorage,
    document: lash_vocabulary::names::DocumentId,
    configured: lash_core_store::artifact_referrer::ReferrerClaim,
) -> LawOutcome {
    use lash_core_store::artifact_referrer::{
        ArtifactCarry, ArtifactName, ArtifactReferrer, ArtifactStoreId, HostArtifactPin,
        ReferrerClaim, ResolvedArtifactCleanup,
    };
    use lash_core_store::packages::*;
    use lash_vocabulary::{names::*, package::*};
    let link = Link {
        document,
        fills: Default::default(),
    };
    let link = LinkRecord {
        id: link.identity(),
        link,
    };
    packages.put_link(&configured, &link, &[]).await?;
    let path: PackagePath = lash_vocabulary::names::PackagePath::parse("law")
        .map_err(|error| LawBroken(format!("{error}")))?;
    let name: Segment = lash_vocabulary::names::Segment::new("run")
        .map_err(|error| LawBroken(format!("{error}")))?;
    let inner_package = Package {
        path: lash_vocabulary::names::PackagePath::parse("law.inner")
            .map_err(|error| LawBroken(format!("{error}")))?,
        members: [(
            name.clone(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link: link.id,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            },
        )]
        .into(),
    };
    let inner_package = PackageRecord {
        id: inner_package.identity(),
        package: inner_package,
    };
    packages.put_package(&configured, &inner_package).await?;
    let inner_publication = PublicationRecord {
        context: PublicationContext {
            domain: PublicationDomain("law".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("configured".into()),
            root: lash_vocabulary::names::Segment::new("law")
                .map_err(|error| LawBroken(format!("{error}")))?,
        },
        path: inner_package.package.path.clone(),
        content: PublishedContent::Package(inner_package.id),
        code_links: [link.id].into(),
        imported_publications: Vec::new(),
    };
    let inner_key = packages.publish(&configured, &inner_publication).await?;
    let outer = Link {
        document,
        fills: [(
            ImportSlot(0),
            Fill::Export {
                link: link.id,
                export: ExportName {
                    name: Name::new("run"),
                    member: None,
                },
            },
        )]
        .into(),
    };
    let outer = LinkRecord {
        id: outer.identity(),
        link: outer,
    };
    packages.put_link(&configured, &outer, &[]).await?;
    let package = Package {
        path: path.clone(),
        members: [(
            name,
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link: outer.id,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            },
        )]
        .into(),
    };
    let package = PackageRecord {
        id: package.identity(),
        package,
    };
    packages.put_package(&configured, &package).await?;
    // L-LABEL-SIDECAR: labels do not enter Package identity, deduplicate
    // literal payloads, and survive exactly as long as their Package.
    let labels = [r#"{"description":"first"}"#, r#"{"description":"second"}"#];
    for json in [labels[0], labels[1], labels[0]] {
        packages.put_labels(&configured, &package.id, json).await?;
    }
    let mut expected_labels: Vec<_> = labels.into_iter().map(str::to_owned).collect();
    expected_labels.sort_by_key(|json| *blake3::hash(json.as_bytes()).as_bytes());
    super::ensure!(
        packages.labels(&package.id).await? == expected_labels,
        "label history duplicated or lost a sidecar"
    );
    super::ensure!(
        packages.package(&package.id).await? == Some(package.clone()),
        "labels changed Package content"
    );
    let publication = PublicationRecord {
        context: PublicationContext {
            domain: PublicationDomain("law".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("configured".into()),
            root: lash_vocabulary::names::Segment::new("law")
                .map_err(|error| LawBroken(format!("{error}")))?,
        },
        path,
        content: PublishedContent::Package(package.id),
        code_links: [outer.id].into(),
        imported_publications: vec![inner_key.clone()],
    };
    let key = packages.publish(&configured, &publication).await?;
    let process = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let process_claim =
        ReferrerClaim::unguarded(process.clone()).map_err(|error| LawBroken(error.to_string()))?;
    packages.acquire_publication(&process_claim, &key).await?;
    packages
        .end_package_referrer(&ResolvedArtifactCleanup {
            referrer: configured.referrer(),
            carries: Vec::new(),
        })
        .await?;
    super::ensure!(
        packages.publication(&key).await? == Some(publication),
        "configured-root end lost the recorded publication"
    );
    super::ensure!(
        packages.link(&link.id).await? == Some(link.clone())
            && packages.package(&package.id).await? == Some(package.clone()),
        "publication lost its reached content"
    );
    super::ensure!(
        packages.link(&outer.id).await?.is_some()
            && packages.publication(&inner_key).await?.is_some()
            && packages.package(&inner_package.id).await?.is_some(),
        "transitive Link or publication edge was not retained"
    );
    super::ensure!(
        packages.labels(&package.id).await? == expected_labels,
        "ending configured root lost live Package labels"
    );
    let refused = packages
        .put_labels(&configured, &package.id, labels[0])
        .await;
    super::ensure!(refused.is_err(), "ended label writer bypassed the fence");
    let successor = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let cleanup = ResolvedArtifactCleanup {
        referrer: process,
        carries: vec![ArtifactCarry {
            artifact: ArtifactName {
                store: ArtifactStoreId::KernelDocument,
                artifact_ref: crate::packages::publication_reference(&key),
            },
            to: successor.clone(),
        }],
    };
    packages.end_package_referrer(&cleanup).await?;
    packages.end_package_referrer(&cleanup).await?;
    super::ensure!(
        packages.link(&link.id).await?.is_some()
            && storage
                .load_package_record(&document.to_string())
                .await?
                .is_some(),
        "a publication carry lost its Link/Document closure"
    );
    packages
        .end_package_referrer(&ResolvedArtifactCleanup {
            referrer: successor,
            carries: Vec::new(),
        })
        .await?;
    super::ensure!(
        packages.link(&link.id).await?.is_none()
            && packages.link(&outer.id).await?.is_none()
            && packages.publication(&inner_key).await?.is_none()
            && packages.package(&inner_package.id).await?.is_none()
            && packages.package(&package.id).await?.is_none()
            && packages.publication(&key).await?.is_none()
            && storage
                .load_package_record(&document.to_string())
                .await?
                .is_none(),
        "last-referrer end failed to reclaim content"
    );
    super::ensure!(
        packages.labels(&package.id).await?.is_empty(),
        "reclaimed Package retained label sidecars"
    );
    let refused = packages.put_package(&process_claim, &package).await;
    super::ensure!(
        matches!(
            refused,
            Err(DurableError::Package(
                crate::packages::PackageRefusal::Missing(_)
            )) | Err(DurableError::Package(
                crate::packages::PackageRefusal::ReferrerEnded(_)
            ))
        ),
        "ended referrer resurrected content: {refused:?}"
    );
    Ok(())
}
