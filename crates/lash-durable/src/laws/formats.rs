//! The format laws (ADR 0106 §1, §2; L11, FIG-5187): the claim filter, the
//! cancel of an actor no node decodes, the fleet-format gate and the
//! draining node.

use super::{LawBroken, LawResult};
use crate::domain::CANCEL_MAIL;
use crate::{
    ActorKey, ActorState, ClaimPurpose, CommitLabel, DurableError, DurableStore, FormatSet,
    MailKind, MailTx, NodeId, NodeLease, NodeSpec, Release, fleet_writable,
};

const TTL_MILLIS: i64 = 15_000;

fn old() -> FormatSet {
    FormatSet::new("law:old")
}

fn new() -> FormatSet {
    FormatSet::new("law:new")
}

fn key(actor: Result<ActorKey, crate::ActorKeyError>) -> Result<ActorKey, LawBroken> {
    actor.map_err(|error| LawBroken(error.to_string()))
}

async fn node(
    store: &dyn DurableStore,
    name: &str,
    decodes: Vec<FormatSet>,
) -> Result<NodeLease, LawBroken> {
    Ok(store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes,
            ttl_millis: TTL_MILLIS,
        })
        .await?)
}

async fn create(store: &dyn DurableStore, actor: &ActorKey, formats: FormatSet) -> LawResult {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats);
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await?;
    Ok(())
}

async fn state(store: &dyn DurableStore, actor: &ActorKey) -> Result<ActorState, LawBroken> {
    store
        .actor(actor)
        .await?
        .map(|snapshot| snapshot.state)
        .ok_or_else(|| LawBroken(format!("{actor} vanished")))
}

/// The claim filter: a node never claims an actor whose format set it does
/// not decode, and the actor stays visible, ready for a node that does. A
/// process actor in such a set with a cancel pending is claimed, marked
/// cancel-only, so its cancel ends it without decoding its state; a session
/// with the same mail is not. An owner that stamps its actor's state into a
/// newer set moves it out of an older node's reach.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_node_claims_only_actors_whose_formats_it_decodes(
    store: &dyn DurableStore,
) -> LawResult {
    let older = node(store, "older", vec![old()]).await?;
    let newer = node(store, "newer", vec![old(), new()]).await?;
    let session = key(ActorKey::session("in-new"))?;
    let process = key(ActorKey::process("in-new"))?;
    create(store, &session, new()).await?;
    create(store, &process, new()).await?;
    super::ensure!(
        store.claim(&older, 8).await?.is_empty(),
        "a node claimed an actor in a format set it does not decode"
    );
    for actor in [&session, &process] {
        let snapshot = store
            .actor(actor)
            .await?
            .ok_or_else(|| LawBroken(format!("{actor} vanished")))?;
        super::ensure!(
            snapshot.state == ActorState::Ready && snapshot.formats == new(),
            "an undecodable actor is not visible and ready in its set: {snapshot:?}"
        );
    }

    // A pending cancel lets the older node end the process, and only it.
    let mut cancel = MailTx::new();
    cancel.append(session.clone(), MailKind::new(CANCEL_MAIL), "{}");
    cancel.append(process.clone(), MailKind::new(CANCEL_MAIL), "{}");
    store
        .commit_mail(cancel, CommitLabel::new("law.cancel"))
        .await?;
    let claimed = store.claim(&older, 8).await?;
    super::ensure!(
        claimed.len() == 1
            && claimed[0].actor == process
            && claimed[0].purpose == ClaimPurpose::CancelOnly,
        "the older node's claim of cancelled undecodable actors was {claimed:?}"
    );
    let mut end = store.begin(&process, claimed[0].epoch).await?;
    end.ack_seen().give_up(Release::Terminal);
    store.commit(end, CommitLabel::new("law.end")).await?;
    super::ensure!(
        state(store, &process).await? == ActorState::Terminal,
        "the cancelled undecodable process did not end"
    );

    // The newer node decodes the session and claims it to run it; its
    // stamp into the newer set keeps the older node from it after release.
    let claimed = store.claim(&newer, 8).await?;
    super::ensure!(
        claimed.len() == 1
            && claimed[0].actor == session
            && claimed[0].purpose == ClaimPurpose::Run,
        "the newer node's claim was {claimed:?}"
    );
    let older_session = key(ActorKey::session("in-old"))?;
    create(store, &older_session, old()).await?;
    let claimed = store.claim(&newer, 8).await?;
    super::ensure!(
        claimed.len() == 1
            && claimed[0].actor == older_session
            && claimed[0].purpose == ClaimPurpose::Run,
        "the newer node did not claim the actor in the older set: {claimed:?}"
    );
    let mut stamp = store.begin(&older_session, claimed[0].epoch).await?;
    stamp.stamp_formats(new()).give_up(Release::Ready);
    store.commit(stamp, CommitLabel::new("law.stamp")).await?;
    let snapshot = store
        .actor(&older_session)
        .await?
        .ok_or_else(|| LawBroken(format!("{older_session} vanished")))?;
    super::ensure!(
        snapshot.formats == new() && snapshot.state == ActorState::Ready,
        "a stamped release left {snapshot:?}"
    );
    super::ensure!(
        store.claim(&older, 8).await?.is_empty(),
        "the older node claimed an actor stamped into a newer set"
    );
    Ok(())
}

/// The fleet-format gate: while a node that decodes only the older set
/// serves, a writer that knows both writes the older one; once that node is
/// gone, the newer.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_newer_format_is_not_written_while_an_older_node_is_live(
    store: &dyn DurableStore,
) -> LawResult {
    let candidates = [new(), old()];
    let older = node(store, "older", vec![old()]).await?;
    node(store, "newer", vec![old(), new()]).await?;
    node(store, "elsewhere", vec![FormatSet::new("law:other")]).await?;
    let live = store.live_decodes().await?;
    super::ensure!(
        live.len() == 3,
        "live_decodes answered {} nodes, not 3",
        live.len()
    );
    super::ensure!(
        fleet_writable(&candidates, &live) == Some(&old()),
        "a newer format was writable while an older node is live: {live:?}"
    );
    store.release_node(&older).await?;
    let live = store.live_decodes().await?;
    super::ensure!(
        fleet_writable(&candidates, &live) == Some(&new()),
        "the newer format was not writable once the older node left: {live:?}"
    );
    Ok(())
}

/// A draining node claims nothing, and an owner's `ready` release under
/// `drain.release` hands its actor to the next claimer at once.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_draining_node_claims_nothing_and_releases_ready(
    store: &dyn DurableStore,
) -> LawResult {
    let draining = node(store, "draining", vec![old()]).await?;
    let first = key(ActorKey::session("held"))?;
    create(store, &first, old()).await?;
    let claimed = store.claim(&draining, 8).await?;
    super::ensure!(
        claimed.len() == 1,
        "the node did not claim its actor: {claimed:?}"
    );
    store.mark_draining(&draining).await?;
    let second = key(ActorKey::session("after"))?;
    create(store, &second, old()).await?;
    super::ensure!(
        store.claim(&draining, 8).await?.is_empty(),
        "a draining node claimed"
    );
    let mut release = store.begin(&first, claimed[0].epoch).await?;
    release.give_up(Release::Ready);
    store.commit(release, CommitLabel::DRAIN_RELEASE).await?;
    super::ensure!(
        state(store, &first).await? == ActorState::Ready,
        "a drain release did not leave its actor ready"
    );
    let next = node(store, "next", vec![old()]).await?;
    let claimed = store.claim(&next, 8).await?;
    super::ensure!(
        claimed.len() == 2,
        "the next node did not claim both actors: {claimed:?}"
    );
    store.release_node(&draining).await?;
    match store.mark_draining(&draining).await {
        Err(DurableError::NodeLeaseLost { .. }) => Ok(()),
        other => Err(LawBroken(format!(
            "marking a released node draining answered {other:?}"
        ))),
    }
}
