//! ENV-LIFETIME, ENV-PREPARE, ENV-REINCARNATE and the publication fence.
use super::graph::GraphTier;
use super::{LawBroken, LawOutcome, ensure};
use crate::domain::{CellId, ExecKey, ExecRootWrite};
use crate::environment::{self as env, EnvironmentWrite};
use crate::{ActorKey, CommitLabel, DomainWrite, DurableError, Epoch};
use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_core_store::{
    artifact_referrer::{ReferrerClaim, ReferrerGuard},
    environment::*,
};
use lash_vocabulary::identity::{SessionId, TurnId};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait EnvironmentTier: GraphTier {
    fn packages(&self) -> Arc<dyn crate::packages::PackageStore>;
    async fn relay(&self) -> Result<(), String>;
}
fn broken(error: impl std::fmt::Display) -> LawBroken {
    LawBroken(error.to_string())
}
fn write(value: EnvironmentWrite) -> DomainWrite {
    DomainWrite::Environment(value)
}
struct Owned {
    actor: ActorKey,
    epoch: Epoch,
    session: SessionId,
}
async fn owned(tier: &dyn EnvironmentTier, name: &str) -> Result<Owned, LawBroken> {
    let store = tier.store();
    let node = super::node(store, name).await?;
    let actor = super::session(name)?;
    super::create(store, std::slice::from_ref(&actor)).await?;
    let claims = store.claim(&node, 1).await?;
    Ok(Owned {
        actor,
        epoch: claims[0].epoch,
        session: SessionId::parse(name).map_err(broken)?,
    })
}
fn head(owner: &VersionOwner) -> EnvironmentHead {
    EnvironmentHead {
        version: owner.version().clone(),
        owner: owner.id().clone(),
        input_frontier: Default::default(),
    }
}
async fn prepare(tier: &dyn EnvironmentTier, owned: &Owned) -> Result<VersionOwner, LawBroken> {
    use lash_vocabulary::{names::*, package::*};
    let package = Package {
        path: PackagePath::parse("environment").map_err(broken)?,
        members: Default::default(),
    };
    let accepted = AcceptedAdvertisement {
        publication: PublicationRecord {
            context: PublicationContext {
                domain: PublicationDomain("environment".into()),
                scope: PublicationScope::Host,
                publisher: PublisherInstance("configured".into()),
                root: Segment::new("environment").map_err(broken)?,
            },
            path: package.path.clone(),
            content: PublishedContent::Package(package.identity()),
            code_links: Default::default(),
            imported_publications: vec![],
        },
        advertisement: PackageAdvertisement::Enumerated(package),
    };
    let body = assemble(
        None,
        [SourceSnapshot {
            source: SourceId("packages".into()),
            revision: env::decode(
                r#"{"incarnation":"00000000-0000-0000-0000-000000000000","counter":0}"#,
            )?,
            fragment: EnvironmentFragment {
                package_roots: vec![accepted],
                ..Default::default()
            },
        }],
    )
    .map_err(broken)?;
    let owner = VersionOwner::preparing(body.id().clone());
    let mut tx = tier.store().begin(&owned.actor, owned.epoch).await?;
    tx.write(write(EnvironmentWrite::prepare(owner.clone(), &body)?));
    tier.store()
        .commit(tx, CommitLabel::new("law.env.prepare"))
        .await?;
    Ok(owner)
}
async fn publish(
    tier: &dyn EnvironmentTier,
    owned: &Owned,
    owner: &VersionOwner,
) -> Result<(), DurableError> {
    let head = head(owner);
    let mut nodes = env::staging();
    env::stage_head(&mut nodes, &head)?;
    let mut tx = tier.store().begin(&owned.actor, owned.epoch).await?;
    tx.stage(nodes.into_nodes().into_values());
    tx.write(write(EnvironmentWrite::Publish {
        owner: owner.id().clone(),
    }));
    tx.write(write(EnvironmentWrite::Head {
        expected: None,
        head: Some(head),
    }));
    tier.store()
        .commit(tx, CommitLabel::new("law.env.publish"))
        .await?;
    Ok(())
}
fn exec(owned: &Owned, name: &str) -> ExecKey {
    ExecKey::Cell(
        owned.session.clone(),
        TurnId::from("environment-run"),
        CellId::new(name),
    )
}
async fn reader(
    tier: &dyn EnvironmentTier,
    owned: &Owned,
    owner: &VersionOwner,
    name: &str,
) -> Result<NodeId, DurableError> {
    let mut nodes = env::staging();
    let root = env::stage_round(&mut nodes, &head(owner).round())?;
    let mut tx = tier.store().begin(&owned.actor, owned.epoch).await?;
    tx.stage(nodes.into_nodes().into_values());
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec(owned, name),
        expected: None,
        expected_root: None,
        root,
        commit_id: CommitId::mint(),
    }));
    tier.store()
        .commit(tx, CommitLabel::new("law.env.acquire"))
        .await?;
    Ok(root)
}
async fn remove_reader(
    tier: &dyn EnvironmentTier,
    owned: &Owned,
    name: &str,
    root: NodeId,
) -> LawOutcome {
    let mut tx = tier.store().begin(&owned.actor, owned.epoch).await?;
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Retire {
        exec: exec(owned, name),
        expected: root,
    }));
    tier.store()
        .commit(tx, CommitLabel::new("law.env.release"))
        .await?;
    Ok(())
}
async fn clear(tier: &dyn EnvironmentTier, owned: &Owned, owner: &VersionOwner) -> LawOutcome {
    let mut tx = tier.store().begin(&owned.actor, owned.epoch).await?;
    tx.write(write(EnvironmentWrite::Head {
        expected: Some(head(owner)),
        head: None,
    }));
    tier.store()
        .commit(tx, CommitLabel::new("law.env.clear"))
        .await?;
    Ok(())
}
async fn retained_package(
    tier: &dyn EnvironmentTier,
    owner: &VersionOwner,
) -> Result<lash_vocabulary::package::PackageId, LawBroken> {
    use lash_vocabulary::package::PackageAdvertisement;
    let record = tier
        .store()
        .environments()
        .environment_owner(owner.id())
        .await?
        .ok_or_else(|| broken("missing preparation"))?;
    let accepted = record
        .body
        .as_ref()
        .and_then(|b| b.fragments().values().next())
        .and_then(|f| f.fragment().package_roots.first())
        .ok_or_else(|| broken("missing publication fixture"))?;
    let PackageAdvertisement::Enumerated(package) = &accepted.advertisement else {
        return Err(broken("expected enumerated package"));
    };
    let record = crate::packages::PackageRecord {
        id: package.identity(),
        package: package.clone(),
    };
    let claim =
        ReferrerClaim::guarded(ReferrerGuard::EnvironmentVersionRetired(owner.id().clone()));
    tier.packages().put_package(&claim, &record).await?;
    tier.packages()
        .publish(&claim, &accepted.publication)
        .await?;
    Ok(record.id)
}
async fn retired(tier: &dyn EnvironmentTier, owner: &VersionOwner) -> Result<bool, LawBroken> {
    Ok(tier
        .store()
        .environments()
        .environment_owner(owner.id())
        .await?
        .ok_or_else(|| broken("missing permanent owner"))?
        .owner
        .state()
        == VersionOwnerState::Retired)
}

/// ENV-LIFETIME: current head and two retained round/cell roots are independent.
/// Retiring the first reader keeps bytes; final graph peel and relay reclaim.
pub async fn env_lifetime(tier: &dyn EnvironmentTier) -> LawOutcome {
    let owned = owned(tier, "env-lifetime").await?;
    let owner = prepare(tier, &owned).await?;
    ensure!(
        matches!(
            publish(tier, &owned, &owner).await,
            Err(DurableError::Environment(env::EnvironmentRefusal::Content))
        ),
        "unheld publication was exposed"
    );
    let package = retained_package(tier, &owner).await?;
    publish(tier, &owned, &owner).await?;
    let first = reader(tier, &owned, &owner, "first").await?;
    let hydrated = tier
        .store()
        .hydrate(
            crate::domain::RootOwner::Exec(exec(&owned, "first")),
            Some(first),
            crate::domain::graph::OWNER_READ_LIMITS,
        )
        .await?;
    {
        let mut decoder = lash_core_store::checkpoint_graph::GraphDecoder::new(
            &hydrated,
            crate::domain::graph::OWNER_READ_LIMITS,
        );
        ensure!(
            env::decode_round(&mut decoder, first)? == head(&owner).round(),
            "retained round lost its typed owner child"
        );
    }
    let last = reader(tier, &owned, &owner, "last").await?;
    clear(tier, &owned, &owner).await?;
    remove_reader(tier, &owned, "first", first).await?;
    tier.retire().await?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        !retired(tier, &owner).await? && tier.packages().package(&package).await?.is_some(),
        "one retained reader lost its closure"
    );
    remove_reader(tier, &owned, "last", last).await?;
    tier.retire().await?;
    ensure!(
        retired(tier, &owner).await?,
        "last reader did not retire its version"
    );
    tier.relay().await.map_err(broken)?;
    ensure!(
        tier.packages().package(&package).await?.is_none(),
        "ENV-LIFETIME: superseded closure survived its last reader"
    );
    Ok(())
}

/// ENV-PREPARE: cleanup exists before holds, and a lost preparation epoch
/// releases its partial holds. A lost publish acknowledgement cannot abort it.
pub async fn env_prepare(tier: &dyn EnvironmentTier) -> LawOutcome {
    let owned = owned(tier, "env-prepare").await?;
    let body = assemble(None, []).map_err(broken)?;
    let refused = VersionOwner::preparing(body.id().clone());
    let mut too_small = tier.store().begin(&owned.actor, owned.epoch).await?;
    too_small.write(write(EnvironmentWrite::prepare(refused.clone(), &body)?));
    too_small.within(lash_core_store::checkpoint_graph::GraphCommitBudget {
        encoded_bytes: lash_core_store::checkpoint_graph::ExplicitLimit::new(1),
        mutation_rows: lash_core_store::checkpoint_graph::ExplicitLimit::new(64),
        encode_memory: lash_core_store::checkpoint_graph::ExplicitLimit::new(64 * 1024),
        failure_reserve_bytes: 0,
    });
    ensure!(
        matches!(
            tier.store()
                .commit(too_small, CommitLabel::new("law.env.prepare_budget"))
                .await,
            Err(DurableError::Graph(
                lash_core_store::checkpoint_graph::GraphRefusal::CommitTooLarge { .. }
            ))
        ),
        "preparation body escaped the owner commit budget"
    );
    ensure!(
        tier.store()
            .environments()
            .environment_owner(refused.id())
            .await?
            .is_none(),
        "refused preparation left an owner/cleanup claim"
    );
    let aborted = prepare(tier, &owned).await?;
    let package = retained_package(tier, &aborted).await?;
    let mut abort = tier.store().begin(&owned.actor, owned.epoch).await?;
    abort.write(write(EnvironmentWrite::Abort {
        owner: aborted.id().clone(),
    }));
    tier.store()
        .commit(abort, CommitLabel::new("law.env.abort"))
        .await?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        retired(tier, &aborted).await? && tier.packages().package(&package).await?.is_none(),
        "explicitly aborted preparation leaked holds"
    );
    let owner = prepare(tier, &owned).await?;
    let package = retained_package(tier, &owner).await?;
    ensure!(
        !tier
            .store()
            .environments()
            .retire_abandoned_environment(owner.id())
            .await?,
        "live preparation retired"
    );
    let mut tx = tier.store().begin(&owned.actor, owned.epoch).await?;
    tx.give_up(crate::Release::Ready);
    tier.store()
        .commit(tx, CommitLabel::new("law.env.crash_prepare"))
        .await?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        retired(tier, &owner).await? && tier.packages().package(&package).await?.is_none(),
        "abandoned preparation leaked its holds"
    );
    ensure!(
        matches!(
            publish(tier, &owned, &owner).await,
            Err(DurableError::OwnershipLost(_))
        ),
        "stale epoch published"
    );
    Ok(())
}

/// ENV-REINCARNATE: equal content after permanent retirement needs a new
/// lifetime owner, whose package acquisition is independent of the old fence.
pub async fn env_reincarnate(tier: &dyn EnvironmentTier) -> LawOutcome {
    let owned = owned(tier, "env-reincarnate").await?;
    let first = prepare(tier, &owned).await?;
    let package = retained_package(tier, &first).await?;
    publish(tier, &owned, &first).await?;
    clear(tier, &owned, &first).await?;
    tier.retire().await?;
    tier.relay().await.map_err(broken)?;
    let next = prepare(tier, &owned).await?;
    ensure!(
        first.version() == next.version() && first.id() != next.id(),
        "content identity was reused as lifetime identity"
    );
    let claim =
        ReferrerClaim::guarded(ReferrerGuard::EnvironmentVersionRetired(first.id().clone()));
    ensure!(
        tier.packages()
            .unheld_publications(&claim, &Default::default())
            .await
            .is_err(),
        "retired owner acquired again"
    );
    retained_package(tier, &next).await?;
    publish(tier, &owned, &next).await?;
    ensure!(
        tier.packages().package(&package).await?.is_some() && retired(tier, &first).await?,
        "reincarnation lost the new hold or old fence"
    );
    Ok(())
}

/// Acquire-versus-retire has one winner. Simultaneous publication, a new
/// reader and a peel cannot expose a retired owner or collect a live reader.
pub async fn env_acquire_retire(tier: &dyn EnvironmentTier) -> LawOutcome {
    let owned = owned(tier, "env-race").await?;
    let owner = prepare(tier, &owned).await?;
    retained_package(tier, &owner).await?;
    publish(tier, &owned, &owner).await?;
    clear(tier, &owned, &owner).await?;
    let (acquired, peeled) = tokio::join!(reader(tier, &owned, &owner, "racer"), tier.retire());
    peeled?;
    if let Ok(root) = acquired {
        ensure!(
            !retired(tier, &owner).await?,
            "retirement won after acquisition committed"
        );
        remove_reader(tier, &owned, "racer", root).await?;
        tier.retire().await?;
    } else {
        ensure!(
            retired(tier, &owner).await?,
            "neither acquisition nor retirement won"
        );
    }
    ensure!(retired(tier, &owner).await?, "final reader did not retire");
    Ok(())
}

/// Crash cuts at prepare, publish, retire and relay: committed ownership is
/// re-read, guards cannot revoke Published, and repeated delivery is idempotent.
pub async fn env_crash_cuts_and_relay(tier: &dyn EnvironmentTier) -> LawOutcome {
    let owned = owned(tier, "env-crash").await?;
    let owner = prepare(tier, &owned).await?;
    let package = retained_package(tier, &owner).await?;
    ensure!(
        tier.store()
            .environments()
            .environment_owner(owner.id())
            .await?
            .is_some(),
        "prepare was not durable before hold"
    );
    publish(tier, &owned, &owner).await?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        !tier
            .store()
            .environments()
            .retire_abandoned_environment(owner.id())
            .await?,
        "published owner was mistaken for an abandoned preparation"
    );
    ensure!(
        tier.store()
            .environments()
            .environment_head(&owned.session)
            .await?
            == Some(head(&owner)),
        "publication lost its head"
    );
    clear(tier, &owned, &owner).await?;
    tier.retire().await?;
    ensure!(
        retired(tier, &owner).await? && tier.packages().package(&package).await?.is_some(),
        "retire was not enqueue-only"
    );
    // Crash after one store applied cleanup, before the relay acknowledged it.
    tier.packages()
        .end_package_referrer(
            &lash_core_store::artifact_referrer::ResolvedArtifactCleanup {
                referrer: lash_core_store::artifact_referrer::ArtifactReferrer::EnvironmentVersion(
                    owner.id().clone(),
                ),
                carries: vec![],
            },
        )
        .await?;
    tier.relay().await.map_err(broken)?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        tier.packages().package(&package).await?.is_none(),
        "relay did not reclaim final-referrer bytes"
    );
    Ok(())
}

/// A PostgreSQL law's deliberately held version row.
#[async_trait::async_trait]
pub trait EnvironmentOwnerProbe: Send + Sync {
    async fn retire_once(&self) -> Result<(), DurableError>;
    async fn hold_owner(&self, id: &VersionOwnerId) -> Box<dyn super::graph::LockHold>;
}

/// ENV-LOCK-ORDER: while a reader waits on a version row, a peel skips it
/// and an independent publication commits. Dropping the blocked reader
/// rolls back its actor fence; its replacement acquires before final retirement.
pub async fn env_publish_retire_acquire(
    tier: &dyn EnvironmentTier,
    probe: &dyn EnvironmentOwnerProbe,
) -> LawOutcome {
    let original = owned(tier, "env-lock-original").await?;
    let old = prepare(tier, &original).await?;
    let package = retained_package(tier, &old).await?;
    publish(tier, &original, &old).await?;
    clear(tier, &original, &old).await?;
    let publisher = owned(tier, "env-lock-publisher").await?;
    let next = prepare(tier, &publisher).await?;
    retained_package(tier, &next).await?;
    let acquiring_owner = owned(tier, "env-lock-reader").await?;
    let mut hold = probe.hold_owner(old.id()).await;
    let mut acquisition = Box::pin(reader(tier, &acquiring_owner, &old, "cancelled"));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result=&mut acquisition=>Err(broken(format!("reader escaped held owner: {result:?}"))),
            ()=hold.wait_for_blocked()=>Ok(())
        }
    })
    .await
    .map_err(broken)??;
    drop(acquisition);
    let mut acquisition = Box::pin(reader(tier, &acquiring_owner, &old, "winner"));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result=&mut acquisition=>Err(broken(format!("reader escaped held owner: {result:?}"))),
            ()=hold.wait_for_blocked()=>Ok(())
        }
    })
    .await
    .map_err(broken)??;
    let (peel, publication) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(probe.retire_once(), publish(tier, &publisher, &next))
    })
    .await
    .map_err(broken)?;
    peel?;
    publication?;
    ensure!(!retired(tier, &old).await?, "peel retired a locked version");
    hold.release().await;
    let root = acquisition.await?;
    tier.retire().await?;
    ensure!(
        !retired(tier, &old).await?,
        "acquired reader lost its version"
    );
    remove_reader(tier, &acquiring_owner, "winner", root).await?;
    tier.retire().await?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        retired(tier, &old).await? && tier.packages().package(&package).await?.is_some(),
        "old retirement revoked independent publication"
    );
    clear(tier, &publisher, &next).await?;
    tier.retire().await?;
    tier.relay().await.map_err(broken)?;
    ensure!(
        tier.packages().package(&package).await?.is_none(),
        "last version leaked shared closure"
    );
    Ok(())
}
