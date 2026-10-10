//! The format laws (ADR 0106 §1, §2; L11, FIG-5187): the claim filter, the
//! cancel of an actor no node decodes, the fleet-format gate, the draining
//! node, and the listings an operator's kernel migration survey reads
//! (FIG-5787).

use lash_sansio::{SessionId, TurnId};

use super::{LawBroken, LawOutcome};
use crate::domain::{CANCEL_MAIL, CellId, DomainWrite, ExecKey, SnapshotWrite};
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

async fn create(store: &dyn DurableStore, actor: &ActorKey, formats: FormatSet) -> LawOutcome {
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
) -> LawOutcome {
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
) -> LawOutcome {
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
) -> LawOutcome {
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

/// The listings an operator's survey and sweep read (FIG-5787): the actors
/// in one format set that have not ended, by key and a page at a time; and
/// the cell snapshots of one turn, which no other turn's cell, no turn of
/// another session and no process joins.
///
/// # Errors
///
/// The first rule broken.
pub async fn actors_in_a_format_set_and_a_turns_cells_are_listed(
    store: &dyn DurableStore,
) -> LawOutcome {
    let lister = node(store, "lister", vec![old()]).await?;
    let first = key(ActorKey::session("listed-a"))?;
    let second = key(ActorKey::process("listed-b"))?;
    let ended = key(ActorKey::session("listed-c"))?;
    let elsewhere = key(ActorKey::session("listed-d"))?;
    for actor in [&first, &second, &ended] {
        create(store, actor, old()).await?;
    }
    create(store, &elsewhere, new()).await?;
    let claimed = store.claim(&lister, 8).await?;
    super::ensure!(
        claimed.len() == 3,
        "the lister did not claim the three actors in its set: {claimed:?}"
    );
    let session = SessionId::try_from("listed-a".to_owned())
        .map_err(|_| LawBroken("session id listed-a".to_owned()))?;
    let other_session = SessionId::try_from("listed-a2".to_owned())
        .map_err(|_| LawBroken("session id listed-a2".to_owned()))?;
    let turn = |name: &str| {
        TurnId::try_from(name.to_owned()).map_err(|_| LawBroken(format!("turn id {name}")))
    };
    let (run, later_run) = (turn("run-1")?, turn("run-10")?);
    let cell = |session: &SessionId, run: &TurnId, cell: &str| {
        ExecKey::Cell(session.clone(), run.clone(), CellId::new(cell))
    };
    let process = lash_sansio::ProcessId::fixture("listed-b");
    for claim in &claimed {
        let mut tx = store.begin(&claim.actor, claim.epoch).await?;
        if claim.actor == first {
            for exec in [
                cell(&session, &run, "cell-2"),
                cell(&session, &run, "cell-1"),
                cell(&session, &later_run, "cell-1"),
                cell(&other_session, &run, "cell-1"),
                ExecKey::Process(process.clone()),
            ] {
                tx.write(DomainWrite::Snapshot(SnapshotWrite::Put {
                    snapshot_ref: exec.stored(),
                    exec,
                    expected: None,
                    executable_identity: "law".to_owned(),
                    format_version: 1,
                }));
            }
        }
        let release = if claim.actor == ended {
            Release::Terminal
        } else {
            Release::Idle
        };
        tx.ack_seen().give_up(release);
        store.commit(tx, CommitLabel::new("law.release")).await?;
    }

    // By key: a process's key sorts before a session's.
    let listed = store.actors_in(&old(), None, 8).await?;
    super::ensure!(
        listed == [second.clone(), first.clone()],
        "the actors listed in the older set were {listed:?}, not the two that have not ended"
    );
    let page = store.actors_in(&old(), None, 1).await?;
    super::ensure!(page == [second.clone()], "the first page was {page:?}");
    let page = store.actors_in(&old(), Some(&second), 1).await?;
    super::ensure!(page == [first.clone()], "the second page was {page:?}");
    let page = store.actors_in(&old(), Some(&first), 1).await?;
    super::ensure!(page.is_empty(), "the page after the last was {page:?}");
    let listed = store.actors_in(&new(), None, 8).await?;
    super::ensure!(
        listed == [elsewhere.clone()],
        "the actors listed in the newer set were {listed:?}"
    );

    let cells = store.cell_snapshots(&session, &run).await?;
    let keys: Vec<ExecKey> = cells.iter().map(|row| row.exec.clone()).collect();
    super::ensure!(
        keys == [
            cell(&session, &run, "cell-1"),
            cell(&session, &run, "cell-2")
        ],
        "the cells listed of one turn were {keys:?}"
    );
    super::ensure!(
        cells
            .iter()
            .all(|row| row.snapshot_ref == row.exec.stored() && row.format_version == 1),
        "a listed cell's snapshot is not the one written: {cells:?}"
    );
    let none = store
        .cell_snapshots(&session, &turn("run-unknown")?)
        .await?;
    super::ensure!(none.is_empty(), "a turn with no cell listed {none:?}");
    Ok(())
}
