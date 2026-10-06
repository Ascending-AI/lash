//! The fencing laws every [`DurableStore`] keeps, written once and run by
//! each dialect's tests over its own store.
//!
//! Each law takes a fresh store and `advance`, which moves the store's clock
//! forward. A law returns the first rule it saw broken.

use std::time::Duration;

use crate::{
    ActorKey, ActorState, ClaimCause, CommitLabel, DurableError, DurableInstant, DurableStore,
    Epoch, FormatSet, HeartbeatOutcome, MailKind, MailRefusal, MailTx, NodeId, NodeLease, NodeSpec,
    Release,
};

/// A broken law: what was expected, and what happened.
#[derive(Debug)]
pub struct LawBroken(pub String);

impl std::fmt::Display for LawBroken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<DurableError> for LawBroken {
    fn from(error: DurableError) -> Self {
        Self(format!("unexpected refusal: {error}"))
    }
}

/// The outcome of one law.
pub type LawResult = Result<(), LawBroken>;

/// Moves the store's clock forward.
pub type Advance<'a> = &'a (dyn Fn(Duration) + Sync);

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(LawBroken(format!($($message)+)));
        }
    };
}

const TTL_MILLIS: i64 = 15_000;
const TTL: Duration = Duration::from_millis(15_000);
const LABEL: CommitLabel = CommitLabel::new("law.write");

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn session(id: &str) -> Result<ActorKey, LawBroken> {
    ActorKey::session(id).map_err(|error| LawBroken(error.to_string()))
}

async fn node(store: &dyn DurableStore, name: &str) -> Result<NodeLease, LawBroken> {
    Ok(store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats()],
            ttl_millis: TTL_MILLIS,
        })
        .await?)
}

async fn create(store: &dyn DurableStore, actors: &[ActorKey]) -> LawResult {
    let mut tx = MailTx::new();
    for actor in actors {
        tx.create_actor(actor.clone(), formats());
    }
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await?;
    Ok(())
}

async fn append(store: &dyn DurableStore, actor: &ActorKey, body: &str) -> LawResult {
    let mut tx = MailTx::new();
    tx.append(actor.clone(), MailKind::new("law.note"), body);
    store.commit_mail(tx, CommitLabel::new("law.mail")).await?;
    Ok(())
}

async fn epoch_of(store: &dyn DurableStore, actor: &ActorKey) -> Result<Epoch, LawBroken> {
    match store.actor(actor).await? {
        Some(snapshot) => Ok(snapshot.epoch),
        None => Err(LawBroken(format!("{actor} vanished"))),
    }
}

/// Every epoch below `current` is refused by `begin`, and a transaction a
/// stale owner opened earlier is refused by `commit` and writes nothing.
async fn stale_epochs_are_refused(
    store: &dyn DurableStore,
    actor: &ActorKey,
    current: Epoch,
) -> LawResult {
    let before = store.actor(actor).await?;
    for held in 0..current.0 {
        match store.begin(actor, Epoch(held)).await {
            Err(DurableError::OwnershipLost(fenced)) => ensure!(
                fenced.held == Epoch(held) && fenced.current == Some(current),
                "a stale begin at {held} named the wrong epochs: {fenced}"
            ),
            other => {
                return Err(LawBroken(format!(
                    "begin at stale epoch {held} of {actor} (current {current}) answered {other:?}"
                )));
            }
        }
    }
    ensure!(
        store.actor(actor).await? == before,
        "a refused begin changed {actor}"
    );
    Ok(())
}

/// Several nodes claim the same ready actors at once. Every actor goes to
/// exactly one of them, and only the epoch its claim answered commits.
pub async fn one_writer_under_a_claim_race(store: &dyn DurableStore) -> LawResult {
    let actors = (0..8)
        .map(|index| session(&format!("race-{index}")))
        .collect::<Result<Vec<_>, _>>()?;
    create(store, &actors).await?;
    let mut nodes = Vec::new();
    for index in 0..4 {
        nodes.push(node(store, &format!("racer-{index}")).await?);
    }
    let claims =
        futures_util::future::join_all(nodes.iter().map(|lease| store.claim(lease, actors.len())))
            .await;
    let mut owners: Vec<(ActorKey, Epoch, usize)> = Vec::new();
    for (index, claim) in claims.into_iter().enumerate() {
        for claimed in claim? {
            ensure!(
                !owners.iter().any(|(actor, ..)| *actor == claimed.actor),
                "{} was claimed twice",
                claimed.actor
            );
            ensure!(
                claimed.cause == ClaimCause::Ready,
                "a fresh actor was claimed as {:?}",
                claimed.cause
            );
            owners.push((claimed.actor, claimed.epoch, index));
        }
    }
    ensure!(
        owners.len() == actors.len(),
        "{} of {} ready actors were claimed",
        owners.len(),
        actors.len()
    );
    for (actor, epoch, index) in &owners {
        let snapshot = store
            .actor(actor)
            .await?
            .ok_or_else(|| LawBroken(format!("{actor} vanished")))?;
        ensure!(
            snapshot.state == ActorState::Owned
                && snapshot.owner.as_ref() == Some(&nodes[*index].owner)
                && snapshot.epoch == *epoch,
            "{actor} is not owned by its claimer at its claimed epoch: {snapshot:?}"
        );
        stale_epochs_are_refused(store, actor, *epoch).await?;
        let tx = store.begin(actor, *epoch).await?;
        store.commit(tx, LABEL).await?;
    }
    Ok(())
}

/// An owner whose node was reaped cannot commit what it opened before the
/// reap, cannot renew, claim or reap, and nothing it tried is visible.
pub async fn a_zombie_owner_past_reap_cannot_commit(
    store: &dyn DurableStore,
    advance: Advance<'_>,
) -> LawResult {
    let actor = session("zombie")?;
    create(store, std::slice::from_ref(&actor)).await?;
    let zombie = node(store, "zombie").await?;
    let claimed = store.claim(&zombie, 1).await?;
    ensure!(claimed.len() == 1, "the zombie claimed {claimed:?}");
    let held = claimed[0].epoch;
    append(store, &actor, "before the reap").await?;
    let mut open = store.begin(&actor, held).await?;
    open.ack_seen().give_up(Release::Idle);
    let before = store.actor(&actor).await?;

    advance(TTL + Duration::from_millis(1));
    let reaper = node(store, "reaper").await?;
    let reaped = store.reap(&reaper).await?;
    ensure!(
        reaped.len() == 1
            && reaped[0].actor == actor
            && reaped[0].from == zombie.owner
            && reaped[0].epoch > held,
        "the reap answered {reaped:?}"
    );
    let current = reaped[0].epoch;

    match store.commit(open, LABEL).await {
        Err(DurableError::OwnershipLost(fenced)) => ensure!(
            fenced.held == held && fenced.current == Some(current),
            "the zombie's refusal named the wrong epochs: {fenced}"
        ),
        other => return Err(LawBroken(format!("the zombie's commit answered {other:?}"))),
    }
    let after = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("the actor vanished".into()))?;
    ensure!(
        after.state == ActorState::Ready
            && after.has_mail
            && after.pending_mail == 1
            && Some(after.revision) == before.as_ref().map(|before| before.revision),
        "the zombie's refused commit left a trace: {after:?}"
    );
    ensure!(
        store.heartbeat(&zombie).await? == HeartbeatOutcome::Reaped,
        "a reaped node renewed its lease"
    );
    ensure!(
        matches!(
            store.claim(&zombie, 1).await,
            Err(DurableError::NodeLeaseLost { .. })
        ),
        "a reaped node claimed"
    );
    ensure!(
        matches!(
            store.reap(&zombie).await,
            Err(DurableError::NodeLeaseLost { .. })
        ),
        "a reaped node reaped"
    );
    stale_epochs_are_refused(store, &actor, current).await?;
    let reclaimed = store.claim(&reaper, 1).await?;
    ensure!(
        reclaimed.len() == 1 && reclaimed[0].epoch > current,
        "the survivor's claim answered {reclaimed:?}"
    );
    let tx = store.begin(&actor, reclaimed[0].epoch).await?;
    ensure!(
        tx.mail().len() == 1,
        "the survivor reads the mail the zombie never acknowledged"
    );
    Ok(())
}

/// A node that stops renewing keeps its actors until it is reaped: an
/// expired lease nobody reaped is still the only owner. A reap then bumps
/// the epoch of everything it owned, and its heartbeat answers `Reaped`. A
/// node that registers again under its name fences its earlier boot at once.
pub async fn a_lapsed_heartbeat_is_reaped_with_an_epoch_bump(
    store: &dyn DurableStore,
    advance: Advance<'_>,
) -> LawResult {
    let actors = [session("lapsed-a")?, session("lapsed-b")?];
    create(store, &actors).await?;
    let lapsing = node(store, "lapsing").await?;
    let watcher = node(store, "watcher").await?;
    let claimed = store.claim(&lapsing, 2).await?;
    ensure!(claimed.len() == 2, "the node claimed {claimed:?}");

    advance(TTL / 2);
    let renewed = store.heartbeat(&lapsing).await?;
    ensure!(
        matches!(renewed, HeartbeatOutcome::Renewed { expires_at } if expires_at > lapsing.expires_at),
        "a live heartbeat answered {renewed:?}"
    );
    store.heartbeat(&watcher).await?;
    advance(TTL / 2 + Duration::from_millis(1));
    ensure!(
        store.reap(&watcher).await?.is_empty(),
        "a renewed lease was reaped"
    );

    advance(TTL);
    let tx = store.begin(&claimed[0].actor, claimed[0].epoch).await?;
    store.commit(tx, LABEL).await?;
    store.heartbeat(&watcher).await?;
    let mut reaped = store.reap(&watcher).await?;
    reaped.sort_by(|left, right| left.actor.cmp(&right.actor));
    ensure!(
        reaped.len() == 2
            && reaped
                .iter()
                .all(|reaped| reaped.from == lapsing.owner && reaped.epoch.0 > 0),
        "the reap answered {reaped:?}"
    );
    for (claim, reaped) in claimed.iter().zip(&reaped) {
        let epoch = epoch_of(store, &claim.actor).await?;
        ensure!(
            epoch > claim.epoch,
            "the reap left {} at {epoch}",
            claim.actor
        );
        ensure!(
            reaped.epoch == epoch,
            "the reap misreported {}",
            claim.actor
        );
    }
    ensure!(
        store.heartbeat(&lapsing).await? == HeartbeatOutcome::Reaped,
        "a reaped node renewed its lease"
    );

    let first = store.claim(&watcher, 2).await?;
    ensure!(first.len() == 2, "the watcher claimed {first:?}");
    let restarted = node(store, "watcher").await?;
    ensure!(
        restarted.owner.boot != watcher.owner.boot,
        "a second registration reused its boot"
    );
    for claim in &first {
        let epoch = epoch_of(store, &claim.actor).await?;
        ensure!(
            epoch > claim.epoch,
            "registering again left {} at {epoch}",
            claim.actor
        );
    }
    ensure!(
        store.heartbeat(&watcher).await? == HeartbeatOutcome::Reaped,
        "an earlier boot renewed after its node registered again"
    );
    ensure!(
        store.claim(&restarted, 2).await?.len() == 2,
        "the new boot could not claim what its earlier boot held"
    );
    Ok(())
}

/// Mail from non-owners wakes the actor: an idle or waiting actor becomes
/// ready, an owned one stays its owner's with the mail announced, and a wake
/// that lands between an owner's read and its release makes the actor ready
/// again. A due time wakes a waiting actor with no mail. Mail to an unknown
/// or ended actor is refused, and a refused mailbox transaction writes
/// nothing.
pub async fn mail_from_non_owners_wakes_the_actor(
    store: &dyn DurableStore,
    advance: Advance<'_>,
) -> LawResult {
    let actor = session("mailbox")?;
    let other = session("bystander")?;
    create(store, &[actor.clone(), other.clone()]).await?;
    let owner = node(store, "mail-owner").await?;
    let claimed = store.claim(&owner, 2).await?;
    let epoch_of_claim = |key: &ActorKey| {
        claimed
            .iter()
            .find(|claimed| claimed.actor == *key)
            .map(|claimed| claimed.epoch)
            .ok_or_else(|| LawBroken(format!("{key} was not claimed")))
    };
    let mut tx = store.begin(&actor, epoch_of_claim(&actor)?).await?;
    tx.give_up(Release::Idle);
    let released = store.commit(tx, LABEL).await?;
    ensure!(
        released.state == ActorState::Idle,
        "a release with no mail left {:?}",
        released.state
    );
    ensure!(
        store.claim(&owner, 1).await?.is_empty(),
        "an idle actor was claimed"
    );

    let mut mail = MailTx::new();
    mail.append(actor.clone(), MailKind::new("law.note"), "first");
    let receipt = store
        .commit_mail(mail, CommitLabel::new("law.mail"))
        .await?;
    ensure!(
        receipt.woken.len() == 1
            && receipt.woken[0].state == ActorState::Ready
            && receipt.woken[0].owner.is_none(),
        "mail to an idle actor answered {receipt:?}"
    );
    let claimed = store.claim(&owner, 1).await?;
    ensure!(
        claimed.len() == 1 && claimed[0].actor == actor && claimed[0].cause == ClaimCause::Ready,
        "the woken actor was claimed as {claimed:?}"
    );
    let epoch = claimed[0].epoch;

    let mut mail = MailTx::new();
    mail.append(actor.clone(), MailKind::new("law.note"), "second");
    let receipt = store
        .commit_mail(mail, CommitLabel::new("law.mail"))
        .await?;
    ensure!(
        receipt.woken.len() == 1
            && receipt.woken[0].state == ActorState::Owned
            && receipt.woken[0].owner.as_ref() == Some(&owner.owner),
        "mail to an owned actor answered {receipt:?}"
    );
    let mut tx = store.begin(&actor, epoch).await?;
    let bodies: Vec<&str> = tx.mail().iter().map(|mail| mail.body.as_str()).collect();
    ensure!(
        bodies == ["first", "second"] && tx.woken(),
        "the owner read {bodies:?}"
    );
    let mut wake = MailTx::new();
    wake.wake(actor.clone());
    store
        .commit_mail(wake, CommitLabel::new("law.wake"))
        .await?;
    tx.ack_seen().give_up(Release::Idle);
    let released = store.commit(tx, LABEL).await?;
    ensure!(
        released.state == ActorState::Ready,
        "a wake after the owner's read was lost: the release left {:?}",
        released.state
    );
    let claimed = store.claim(&owner, 1).await?;
    ensure!(claimed.len() == 1, "the re-woken actor was not claimed");
    let mut tx = store.begin(&actor, claimed[0].epoch).await?;
    ensure!(
        tx.woken() && tx.mail().is_empty(),
        "a wake without mail read {:?}",
        tx.mail()
    );
    let now = store.now().await?;
    let due = DurableInstant(now.0 + 1_000);
    tx.ack_seen().give_up(Release::Waiting {
        next_due: Some(due),
    });
    let released = store.commit(tx, LABEL).await?;
    ensure!(
        released.state == ActorState::Waiting,
        "a release to a timer left {:?}",
        released.state
    );
    ensure!(
        store.claim(&owner, 1).await?.is_empty(),
        "a waiting actor was claimed before its due time"
    );
    advance(Duration::from_millis(1_000));
    let claimed = store.claim(&owner, 1).await?;
    ensure!(
        claimed.len() == 1 && claimed[0].cause == ClaimCause::Due,
        "a due actor was claimed as {claimed:?}"
    );

    let mut tx = store.begin(&actor, claimed[0].epoch).await?;
    tx.give_up(Release::Terminal);
    store.commit(tx, LABEL).await?;
    let mut mail = MailTx::new();
    mail.append(other.clone(), MailKind::new("law.note"), "never")
        .append(actor.clone(), MailKind::new("law.note"), "too late");
    ensure!(
        store.commit_mail(mail, CommitLabel::new("law.mail")).await
            == Err(DurableError::MailRefused(MailRefusal::ActorTerminal(
                actor.clone()
            ))),
        "mail to a terminal actor was not refused"
    );
    let mut mail = MailTx::new();
    mail.append(session("nobody")?, MailKind::new("law.note"), "lost");
    ensure!(
        matches!(
            store.commit_mail(mail, CommitLabel::new("law.mail")).await,
            Err(DurableError::MailRefused(MailRefusal::UnknownActor(_)))
        ),
        "mail to an unknown actor was not refused"
    );
    let mut mail = MailTx::new();
    mail.append(other.clone(), MailKind::new("law.note"), "never")
        .create_actor(actor.clone(), formats());
    ensure!(
        store.commit_mail(mail, CommitLabel::new("law.mail")).await
            == Err(DurableError::MailRefused(MailRefusal::ActorExists(
                actor.clone()
            ))),
        "creating an existing actor was not refused"
    );
    let bystander = store
        .actor(&other)
        .await?
        .ok_or_else(|| LawBroken("the bystander vanished".into()))?;
    ensure!(
        bystander.pending_mail == 0 && !bystander.has_mail,
        "a refused mailbox transaction delivered mail: {bystander:?}"
    );
    Ok(())
}

/// No write goes through a stale epoch, ever: across claims, releases,
/// reaps and re-registrations, every epoch but the current one is refused by
/// `begin` and `commit`, an owner cannot acknowledge mail it never read, and
/// the current owner's commit is the only one that lands.
pub async fn no_write_through_a_stale_epoch(
    store: &dyn DurableStore,
    advance: Advance<'_>,
) -> LawResult {
    let actor = session("epochs")?;
    create(store, std::slice::from_ref(&actor)).await?;
    let first = node(store, "epoch-first").await?;
    let second = node(store, "epoch-second").await?;
    let mut opened = Vec::new();

    let claimed = store.claim(&first, 1).await?;
    opened.push(store.begin(&actor, claimed[0].epoch).await?);
    let mut tx = store.begin(&actor, claimed[0].epoch).await?;
    tx.give_up(Release::Idle);
    store.commit(tx, LABEL).await?;
    append(store, &actor, "wake").await?;

    let claimed = store.claim(&second, 1).await?;
    opened.push(store.begin(&actor, claimed[0].epoch).await?);
    let mut beyond = store.begin(&actor, claimed[0].epoch).await?;
    beyond.ack_through(crate::MailSeq(beyond.seen().0 + 1));
    ensure!(
        matches!(
            store.commit(beyond, LABEL).await,
            Err(DurableError::AckBeyondRead { .. })
        ),
        "an owner acknowledged mail it never read"
    );
    advance(TTL + Duration::from_millis(1));
    store.heartbeat(&first).await?;
    store.reap(&first).await?;

    let claimed = store.claim(&first, 1).await?;
    ensure!(claimed.len() == 1, "the reaped actor was not claimable");
    opened.push(store.begin(&actor, claimed[0].epoch).await?);
    node(store, "epoch-first").await?;
    let current = epoch_of(store, &actor).await?;
    ensure!(
        store.actor(&actor).await?.map(|snapshot| snapshot.state) == Some(ActorState::Ready),
        "registering again did not release the earlier boot's actor"
    );

    let revision = store.actor(&actor).await?.map(|snapshot| snapshot.revision);
    for tx in opened {
        let held = tx.epoch();
        match store.commit(tx, LABEL).await {
            Err(DurableError::OwnershipLost(fenced)) => ensure!(
                fenced.held == held && fenced.current == Some(current),
                "a stale commit named the wrong epochs: {fenced}"
            ),
            other => {
                return Err(LawBroken(format!(
                    "a commit at stale epoch {held} answered {other:?}"
                )));
            }
        }
    }
    ensure!(
        store.actor(&actor).await?.map(|snapshot| snapshot.revision) == revision,
        "a stale commit moved the state revision"
    );
    stale_epochs_are_refused(store, &actor, current).await?;
    ensure!(
        matches!(
            store.begin(&session("absent")?, Epoch(0)).await,
            Err(DurableError::OwnershipLost(fenced)) if fenced.current.is_none()
        ),
        "an unknown actor was opened"
    );
    Ok(())
}

/// L3 (FIG-5172): a turn cancel request is a row on the turn plus a control
/// wake, written by a non-owner in one mailbox transaction. The first
/// request holds the undelivered-input policy; a request with that policy
/// and a stronger mode escalates it; any other repeat writes nothing; a turn
/// that is not unfinished answers `AlreadyEnded`. The owner reads the
/// accepted request on the turn's row.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_turn_cancel_is_a_first_winner_row_with_a_wake(
    store: &dyn DurableStore,
) -> LawResult {
    use crate::domain::{
        DomainWrite, MailAnswer, MailDomainWrite, TurnCancelAnswer, TurnCancelRequest,
        TurnTerminal, TurnWrite,
    };
    use lash_sansio::{SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId};

    let id = SessionId::try_from("cancelled-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let turn = |name: &str| {
        TurnId::try_from(name.to_owned()).map_err(|_| LawBroken(format!("turn id {name}")))
    };
    let run = turn("cancelled-turn")?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "cancel-owner").await?;
    let claimed = store.claim(&owner, 1).await?;
    ensure!(claimed.len() == 1, "the session was not claimed");
    let mut tx = store.begin(&actor, claimed[0].epoch).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: id.clone(),
        run: run.clone(),
        admission_json: "{}".to_owned(),
        turn_deadline: None,
    }))
    .ack_seen()
    .give_up(Release::Idle);
    store.commit(tx, LABEL).await?;

    let request = |request_id: &str,
                   run: &TurnId,
                   undelivered: TurnCancelUndeliveredInputPolicy,
                   mode: TurnCancelMode| TurnCancelRequest {
        session: id.clone(),
        run: run.clone(),
        request_id: request_id.to_owned(),
        origin: Some("law".to_owned()),
        reason: None,
        undelivered,
        mode,
    };
    let ask = |request: TurnCancelRequest| async move {
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::RequestTurnCancel(request));
        store.commit_mail(tx, CommitLabel::new("law.cancel")).await
    };
    let defer = TurnCancelUndeliveredInputPolicy::Defer;
    let first = request("first", &run, defer, TurnCancelMode::AfterStep);

    let receipt = ask(first.clone()).await?;
    ensure!(
        receipt.answers == [MailAnswer::RequestTurnCancel(TurnCancelAnswer::Requested)]
            && receipt.woken.len() == 1
            && receipt.woken[0].actor == actor
            && receipt.woken[0].state == ActorState::Ready,
        "the first request answered {receipt:?}"
    );
    let row = store
        .turn(&id)
        .await?
        .ok_or_else(|| LawBroken("the admitted turn vanished".into()))?;
    ensure!(
        row.cancel.as_ref() == Some(&first),
        "the owner read the turn's cancel as {:?}",
        row.cancel
    );

    let conflicting = request(
        "conflicting",
        &run,
        TurnCancelUndeliveredInputPolicy::Drop,
        TurnCancelMode::Immediate,
    );
    let receipt = ask(conflicting).await?;
    ensure!(
        receipt.answers
            == [MailAnswer::RequestTurnCancel(
                TurnCancelAnswer::PolicyConflict {
                    accepted: first.clone()
                }
            )]
            && receipt.woken.is_empty(),
        "a request with another policy answered {receipt:?}"
    );
    let receipt = ask(request("repeat", &run, defer, TurnCancelMode::AfterStep)).await?;
    ensure!(
        receipt.answers
            == [MailAnswer::RequestTurnCancel(
                TurnCancelAnswer::AlreadyRequested {
                    accepted: first.clone()
                }
            )]
            && receipt.woken.is_empty(),
        "a repeat answered {receipt:?}"
    );
    let mut escalated = first.clone();
    escalated.mode = TurnCancelMode::Immediate;
    let receipt = ask(request("stronger", &run, defer, TurnCancelMode::Immediate)).await?;
    ensure!(
        receipt.answers
            == [MailAnswer::RequestTurnCancel(TurnCancelAnswer::Escalated {
                accepted: escalated.clone()
            })]
            && receipt.woken.len() == 1,
        "a stronger mode answered {receipt:?}"
    );
    let row = store.turn(&id).await?;
    ensure!(
        row.as_ref().and_then(|row| row.cancel.as_ref()) == Some(&escalated),
        "the escalated request reads back as {row:?}"
    );

    let unknown = turn("never-admitted")?;
    let receipt = ask(request(
        "unknown",
        &unknown,
        defer,
        TurnCancelMode::Immediate,
    ))
    .await?;
    ensure!(
        receipt.answers
            == [MailAnswer::RequestTurnCancel(
                TurnCancelAnswer::AlreadyEnded
            )]
            && receipt.woken.is_empty(),
        "a request for a turn never admitted answered {receipt:?}"
    );

    let claimed = store.claim(&owner, 1).await?;
    ensure!(claimed.len() == 1, "the woken session was not claimed");
    let mut tx = store.begin(&actor, claimed[0].epoch).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: id.clone(),
        run: run.clone(),
        terminal: TurnTerminal::Cancelled,
        cause_json: Some("{}".to_owned()),
        head_revision: None,
    }))
    .ack_seen()
    .give_up(Release::Idle);
    store.commit(tx, LABEL).await?;
    let receipt = ask(request("late", &run, defer, TurnCancelMode::Immediate)).await?;
    ensure!(
        receipt.answers
            == [MailAnswer::RequestTurnCancel(
                TurnCancelAnswer::AlreadyEnded
            )]
            && receipt.woken.is_empty(),
        "a request for an ended turn answered {receipt:?}"
    );
    let snapshot = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("the session actor vanished".into()))?;
    ensure!(
        snapshot.state == ActorState::Idle,
        "a request for an ended turn woke the session: {snapshot:?}"
    );
    Ok(())
}
