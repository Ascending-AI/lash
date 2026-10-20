//! Boot authority, claim priority and same-claim progress (FIG-6130).

use super::{Advance, LABEL, LawBroken, LawOutcome, append, create, ensure, node, session};
use crate::{
    ActorSnapshot, ActorState, ClaimCause, DurableInstant, DurableStore, Epoch, FormatSet, Release,
};
use std::time::Duration;

async fn snapshot(
    store: &dyn DurableStore,
    actor: &crate::ActorKey,
) -> Result<ActorSnapshot, LawBroken> {
    store
        .actor(actor)
        .await?
        .ok_or_else(|| LawBroken(format!("{actor} vanished")))
}

/// Adoption is authority for one boot, never for its stable node name.
/// The replaced boot sees neither its successor's epoch nor a peer's grant.
pub async fn a_replaced_boot_adopts_none_of_its_successors_actors(
    store: &dyn DurableStore,
) -> LawOutcome {
    let actors = [
        session("adopt-a")?,
        session("adopt-b")?,
        session("adopt-released")?,
    ];
    create(store, &actors).await?;
    let old = node(store, "adopter").await?;
    let initial = store.claim(&old, 3).await?;
    ensure!(initial.len() == 3, "the initial boot claimed {initial:?}");
    let successor = node(store, "adopter").await?;
    ensure!(
        old.owner.boot != successor.owner.boot,
        "registration reused the boot"
    );
    let next = store.claim(&successor, 1).await?;
    let peer = node(store, "peer").await?;
    let other = store.claim(&peer, 1).await?;
    let released = store.claim(&successor, 1).await?;
    ensure!(
        next.len() == 1 && other.len() == 1 && released.len() == 1,
        "successor/peer claims: {next:?}, {other:?}, {released:?}"
    );
    for claim in next.iter().chain(&other).chain(&released) {
        let prior = initial
            .iter()
            .find(|prior| prior.actor == claim.actor)
            .ok_or_else(|| LawBroken("claimed an unknown actor".into()))?;
        ensure!(
            claim.epoch == Epoch(prior.epoch.0 + 2),
            "replacement and reclaim did not each bump the epoch: {prior:?}, {claim:?}"
        );
    }
    let mut tx = store.begin(&released[0].actor, released[0].epoch).await?;
    tx.give_up(Release::Idle);
    store.commit(tx, LABEL).await?;
    ensure!(
        store.owned(&old).await?.is_empty(),
        "the replaced boot adopted successor authority"
    );
    for (lease, claim) in [(&successor, &next[0]), (&peer, &other[0])] {
        let mut expected = claim.clone();
        expected.cause = ClaimCause::Adopted;
        let owned = store.owned(lease).await?;
        ensure!(
            owned == [expected],
            "{} adopted the wrong set or epochs: {owned:?}",
            lease.owner.node
        );
        let row = snapshot(store, &claim.actor).await?;
        ensure!(
            row.owner.as_ref() == Some(&lease.owner)
                && row.epoch == claim.epoch
                && row.state == ActorState::Owned,
            "adoption disagreed with stored ownership: {row:?}"
        );
    }
    let row = snapshot(store, &released[0].actor).await?;
    ensure!(
        row.state == ActorState::Idle
            && row.owner.is_none()
            && row.epoch == Epoch(released[0].epoch.0 + 1),
        "the released actor retained authority: {row:?}"
    );
    Ok(())
}

/// An empty Ready release does not erase earlier progress in that claim.
/// Empty releases in claims with no progress still increment the streak.
pub async fn progress_before_an_empty_ready_release_resets_failed_activations(
    store: &dyn DurableStore,
) -> LawOutcome {
    let actor = session("progress-then-hand-back")?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "progress-owner").await?;
    let first = store.claim(&owner, 1).await?;
    ensure!(first.len() == 1, "the fresh actor was not claimed");
    let mut tx = store.begin(&actor, first[0].epoch).await?;
    tx.give_up(Release::Ready);
    store.commit(tx, LABEL).await?;
    let second = store.claim(&owner, 1).await?;
    ensure!(second.len() == 1, "the control release was not reclaimed");
    let before = snapshot(store, &actor).await?;
    ensure!(
        before.failed_activations == 1,
        "the no-progress control did not increment: {before:?}"
    );
    append(store, &actor, "committed progress").await?;
    let mut tx = store.begin(&actor, second[0].epoch).await?;
    ensure!(
        tx.mail().len() == 1 && tx.mail()[0].body == "committed progress",
        "the progress mail was not read"
    );
    tx.ack_seen();
    let consumed = store.commit(tx, LABEL).await?;
    ensure!(
        consumed.revision.0 == before.revision.0 + 1 && consumed.state == ActorState::Owned,
        "consumption changed the grant: {consumed:?}"
    );
    let mut tx = store.begin(&actor, second[0].epoch).await?;
    ensure!(
        tx.mail().is_empty() && !tx.advances(),
        "the hand-back was not empty"
    );
    tx.give_up(Release::Ready);
    let released = store.commit(tx, LABEL).await?;
    ensure!(
        released.revision.0 == before.revision.0 + 2,
        "the two commits did not each advance revision"
    );
    let third = store.claim(&owner, 1).await?;
    ensure!(third.len() == 1, "the progressed actor was not reclaimed");
    let after = snapshot(store, &actor).await?;
    ensure!(
        after.failed_activations == 0
            && after.pending_mail == 0
            && !after.has_mail
            && after.revision == released.revision,
        "the empty hand-back erased progress or consumption: {after:?}"
    );
    let mut tx = store.begin(&actor, third[0].epoch).await?;
    ensure!(
        tx.mail().is_empty(),
        "acknowledged mail returned on reclaim"
    );
    tx.give_up(Release::Ready);
    store.commit(tx, LABEL).await?;
    let fourth = store.claim(&owner, 1).await?;
    ensure!(
        fourth.len() == 1 && snapshot(store, &actor).await?.failed_activations == 1,
        "a subsequent empty claim did not restart the streak"
    );
    Ok(())
}

/// A finite eligibility table: due and Ready compete by age, ties by key,
/// with exact page membership. Unowned ineligible states, future deadlines,
/// unsupported formats/requirements and an already-owned actor are excluded.
pub async fn claims_take_the_oldest_eligible_actors_first(
    store: &dyn DurableStore,
    advance: Advance<'_>,
) -> LawOutcome {
    let names = [
        "z-old-due",
        "a-tie-due",
        "b-tie-ready",
        "a-new-ready",
        "boundary-due",
        "future-due",
        "idle",
        "parked",
        "terminal",
        "unsupported",
        "unserved",
        "held",
    ];
    let actors = names
        .iter()
        .map(|name| session(name))
        .collect::<Result<Vec<_>, _>>()?;
    create(store, &actors).await?;
    let setup = node(store, "priority-setup").await?;
    let grants = store.claim(&setup, actors.len()).await?;
    ensure!(
        grants.len() == actors.len(),
        "the eligibility table was not populated"
    );
    let base = store.now().await?.0;
    advance(Duration::from_millis(20));
    for (index, actor) in actors.iter().enumerate() {
        if index == 3 || index == 11 {
            continue;
        }
        let grant = grants
            .iter()
            .find(|claim| &claim.actor == actor)
            .ok_or_else(|| LawBroken("missing setup grant".into()))?;
        let mut tx = store.begin(actor, grant.epoch).await?;
        let release = match index {
            0 => Release::Waiting {
                next_due: Some(DurableInstant(base + 10)),
            },
            1 => Release::Waiting {
                next_due: Some(DurableInstant(base + 20)),
            },
            2 | 9 | 10 => Release::Ready,
            4 => Release::Waiting {
                next_due: Some(DurableInstant(base + 40)),
            },
            5 => Release::Waiting {
                next_due: Some(DurableInstant(base + 41)),
            },
            6 => Release::Idle,
            7 => Release::Parked,
            8 => Release::Terminal,
            _ => unreachable!(),
        };
        if index == 7 {
            tx.write(crate::DomainWrite::ActorPark {
                reason_json: Some("\"law park\"".into()),
            });
        }
        if index == 9 {
            tx.stamp_formats(FormatSet::new("unsupported"));
        }
        if index == 10 {
            tx.set_requires(vec!["unserved".into()]);
        }
        tx.give_up(release);
        store.commit(tx, LABEL).await?;
    }
    advance(Duration::from_millis(10));
    let grant = grants
        .iter()
        .find(|claim| claim.actor == actors[3])
        .ok_or_else(|| LawBroken("missing new-ready grant".into()))?;
    let mut tx = store.begin(&actors[3], grant.epoch).await?;
    tx.give_up(Release::Ready);
    store.commit(tx, LABEL).await?;
    advance(Duration::from_millis(10));
    let chooser = node(store, "priority-chooser").await?;
    ensure!(
        store.claim(&chooser, 0).await?.is_empty(),
        "zero capacity claimed actors"
    );
    // Result row order is unspecified; page selection is oldest-first.
    for (limit, expected) in [(1, vec![0]), (1, vec![1]), (2, vec![2, 3]), (8, vec![4])] {
        let claims = store.claim(&chooser, limit).await?;
        let mut actual = claims
            .iter()
            .map(|claim| claim.actor.clone())
            .collect::<Vec<_>>();
        let mut wanted = expected
            .iter()
            .map(|index| actors[*index].clone())
            .collect::<Vec<_>>();
        actual.sort();
        wanted.sort();
        ensure!(
            actual == wanted,
            "oldest eligible page at limit {limit}: expected {wanted:?}, got {claims:?}"
        );
        for claim in &claims {
            let index = actors
                .iter()
                .position(|actor| actor == &claim.actor)
                .ok_or_else(|| LawBroken("unknown claim".into()))?;
            let cause = if [0, 1, 4].contains(&index) {
                ClaimCause::Due
            } else {
                ClaimCause::Ready
            };
            ensure!(
                claim.cause == cause && claim.purpose == crate::ClaimPurpose::Run,
                "wrong eligibility cause: {claim:?}"
            );
        }
    }
    ensure!(
        store.claim(&chooser, 8).await?.is_empty(),
        "an ineligible actor was claimed"
    );
    Ok(())
}

/// RETIRED-REGISTRY-CLAIM (FIG-6335): retiring an owner allows its
/// callbacks to be claimed for refusal, without serving another owner,
/// registry or opaque requirement. The claim and adoption agree.
pub async fn a_retired_owner_claims_only_its_registry_callbacks(
    store: &dyn DurableStore,
) -> LawOutcome {
    use super::{TTL_MILLIS, formats};
    use crate::{NodeId, NodeSpec};
    use lash_core_store::obligations::{ObligationToken, RegistryKind};
    use lash_core_store::store::plugin_writers::PluginCallbackIdentity;
    let callback = |kind, owner: &str| {
        ObligationToken::for_callback(kind, &PluginCallbackIdentity::tool_provider(owner, "tools"))
            .into_string()
    };
    let requirements = [
        callback(RegistryKind::Session, "retired/nested"),
        callback(RegistryKind::Session, "retired/nested-other"),
        callback(RegistryKind::Process, "retired/nested"),
        callback(RegistryKind::Session, "retired"),
        "opaque/requirement".to_owned(),
    ];
    let actors = (0..requirements.len())
        .map(|index| session(&format!("retirement-{index}")))
        .collect::<Result<Vec<_>, _>>()?;
    create(store, &actors).await?;
    let setup = node(store, "retirement-setup").await?;
    let grants = store.claim(&setup, actors.len()).await?;
    for (actor, required) in actors.iter().zip(requirements) {
        let grant = grants
            .iter()
            .find(|claim| &claim.actor == actor)
            .ok_or_else(|| LawBroken("missing setup grant".into()))?;
        let mut tx = store.begin(actor, grant.epoch).await?;
        tx.set_requires(vec![required]);
        tx.give_up(Release::Ready);
        store.commit(tx, LABEL).await?;
    }
    let retired = store
        .register_node(&NodeSpec {
            node: NodeId::new("retirement-claimant"),
            decodes: vec![formats()],
            serves: vec![
                ObligationToken::for_retired_owner(RegistryKind::Session, "retired/nested")
                    .into_string(),
            ],
            held: Default::default(),
            ttl_millis: TTL_MILLIS,
        })
        .await?;
    let claimed = store.claim(&retired, actors.len()).await?;
    ensure!(
        claimed.len() == 1
            && claimed[0].actor == actors[0]
            && claimed[0].purpose == crate::ClaimPurpose::Run,
        "retirement claim crossed its registry/owner boundary: {claimed:?}"
    );
    let adopted = store.owned(&retired).await?;
    ensure!(
        adopted.len() == 1 && adopted[0].purpose == crate::ClaimPurpose::Run,
        "retirement adoption and claim disagree: {adopted:?}"
    );
    Ok(())
}
