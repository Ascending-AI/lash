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
pub type LawOutcome = Result<(), LawBroken>;

/// Moves the store's clock forward.
pub type Advance<'a> = &'a (dyn Fn(Duration) + Sync);

mod claims;
mod formats;
pub mod graph;
mod hand_back;
mod model_calls;
pub mod node_wakes;
mod packages;
pub mod pages;
mod parks;
mod prompts;
mod session_close;
mod turn_namespaces;

pub use claims::{
    a_replaced_boot_adopts_none_of_its_successors_actors,
    a_retired_owner_claims_only_its_registry_callbacks,
    claims_take_the_oldest_eligible_actors_first,
    progress_before_an_empty_ready_release_resets_failed_activations,
};
pub use formats::{
    a_draining_node_claims_nothing_and_releases_ready,
    a_newer_format_is_not_written_while_an_older_node_is_live,
    a_node_claims_only_actors_whose_formats_it_decodes,
    actors_in_a_format_set_and_a_turns_cells_are_listed,
};
pub use hand_back::an_abandoned_activation_hands_back_only_its_original_grant;
pub use model_calls::{
    a_receipt_stands_beside_its_model_phase_until_the_next_phase,
    a_turn_counts_its_tool_admissions_atomically, a_turn_counts_the_model_calls_it_admitted,
};
pub use packages::{
    a_hard_record_lands_with_its_owner_commit_or_not_at_all,
    a_node_claims_only_actors_whose_hard_requirements_it_meets,
    a_store_keeps_artifacts_and_sets_by_digest,
    a_store_keeps_each_sets_first_surface_and_indexes_its_definitions,
};
pub use parks::a_boot_redrives_only_the_session_parks_it_names;
pub use prompts::{
    deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text,
    prompt_snapshot_roots_survive_phase_pruning_until_released,
    settling_a_turns_calls_releases_their_requests_and_keeps_their_snapshots,
};
pub use session_close::a_session_close_moves_one_step_at_a_time;
pub use turn_namespaces::a_runs_namespace_write_modes_keep_only_current_values_and_end_with_it;

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(LawBroken(format!($($message)+)));
        }
    };
}
use ensure;

const TTL_MILLIS: i64 = 15_000;
const TTL: Duration = Duration::from_millis(15_000);
const LABEL: CommitLabel = CommitLabel::new("law.write");

/// A small checkpoint graph node naming `tag`: a root a law's row can
/// publish, staged with it.
fn tagged_node(
    tag: &str,
) -> (
    lash_core_store::checkpoint_graph::NodeId,
    lash_core_store::checkpoint_graph::EncodedNode,
) {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    let mut out = TokenWriter::new();
    out.text(tag);
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let id = staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(out.as_bytes()))
        .unwrap_or_else(|error| panic!("a law's node encodes: {error}"));
    let node = staging
        .get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("a law's node is staged"));
    (id, node)
}

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
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: TTL_MILLIS,
        })
        .await?)
}

async fn create(store: &dyn DurableStore, actors: &[ActorKey]) -> LawOutcome {
    let mut tx = MailTx::new();
    for actor in actors {
        tx.create_actor(actor.clone(), formats());
    }
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await?;
    Ok(())
}

async fn append(store: &dyn DurableStore, actor: &ActorKey, body: &str) -> LawOutcome {
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
) -> LawOutcome {
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
pub async fn one_writer_under_a_claim_race(store: &dyn DurableStore) -> LawOutcome {
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
) -> LawOutcome {
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
) -> LawOutcome {
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
) -> LawOutcome {
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
) -> LawOutcome {
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

/// FIG-5936: an owner commit reads its session's turn cancel under its
/// fence, before its writes, and reports it on its receipt: a request
/// accepted after the owner's open is seen by the commit, with no read of
/// its own, and a commit with none to see reports none.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_owner_commit_reports_a_turn_cancel_accepted_after_its_open(
    store: &dyn DurableStore,
) -> LawOutcome {
    use crate::domain::{DomainWrite, MailDomainWrite, TurnCancelRequest, TurnWrite};
    use lash_core_store::store::{AdmittedTurnRows, RunAdmissionRecord};
    use lash_protocol_types::{
        SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId,
    };

    let id = SessionId::try_from("late-cancel-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let run = TurnId::try_from("late-cancel-turn".to_owned())
        .map_err(|_| LawBroken("a constant turn id".into()))?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "late-cancel-owner").await?;
    let claimed = store.claim(&owner, 1).await?;
    ensure!(claimed.len() == 1, "the session was not claimed");
    let epoch = claimed[0].epoch;
    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: id.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one(
                    lash_protocol_types::InputId::from("late-cancel-batch"),
                ),
            },
            trace: None,
        },
        turn_deadline: None,
    }))
    .ack_seen();
    let admitted = store.commit(tx, LABEL).await?;
    ensure!(
        admitted.turn_cancel.is_none(),
        "a commit with no request reported {:?}",
        admitted.turn_cancel
    );

    let tx = store.begin(&actor, epoch).await?;
    ensure!(
        tx.turn_cancel().is_none(),
        "the open read a request none made"
    );
    let request = TurnCancelRequest {
        session: id.clone(),
        run: run.clone(),
        request_id: "late".to_owned(),
        origin: Some("law".to_owned()),
        reason: None,
        undelivered: TurnCancelUndeliveredInputPolicy::Defer,
        mode: TurnCancelMode::Immediate,
    };
    let mut ask = MailTx::new();
    ask.write(MailDomainWrite::RequestTurnCancel(request.clone()));
    store
        .commit_mail(ask, CommitLabel::new("law.cancel"))
        .await?;
    let committed = store.commit(tx, LABEL).await?;
    ensure!(
        committed.turn_cancel.as_ref() == Some(&request),
        "the commit after a late request reported {:?}",
        committed.turn_cancel
    );
    Ok(())
}

/// L3 (FIG-5172): a turn cancel request is a row on the turn plus a control
/// wake, written by a non-owner in one mailbox transaction. The first
/// request holds the undelivered-input policy; a request with that policy
/// and a stronger mode escalates it; any other repeat writes nothing; a turn
/// that is not unfinished answers `AlreadyEnded`. The owner reads the
/// accepted request on the turn's row, and with its open.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_turn_cancel_is_a_first_winner_row_with_a_wake(
    store: &dyn DurableStore,
) -> LawOutcome {
    use crate::domain::{
        DomainWrite, MailAnswer, MailDomainWrite, TurnCancelAnswer, TurnCancelRequest, TurnWrite,
    };
    use lash_core_store::store::{AdmittedTurnRows, RunAdmissionRecord, RunTerminalCause};
    use lash_protocol_types::{
        SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId,
    };

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
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one(
                    lash_protocol_types::InputId::from("cancelled-batch"),
                ),
            },
            trace: None,
        },
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
    // The escalating request's identity, origin, reason and mode, over the
    // first request's policy (ADR 0039).
    let stronger = TurnCancelRequest {
        origin: Some("escalation".to_owned()),
        reason: Some("now".to_owned()),
        ..request("stronger", &run, defer, TurnCancelMode::Immediate)
    };
    let mut escalated = first.clone();
    escalated.escalate_to(&stronger);
    let receipt = ask(stronger).await?;
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
    ensure!(
        tx.turn_cancel() == Some(&escalated),
        "the owner's open read the turn's cancel as {:?}",
        tx.turn_cancel()
    );
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: id.clone(),
        run: run.clone(),
        cause: Box::new(RunTerminalCause::OperatorCancelled {
            intent: lash_core_store::store::ControlIntentId::from_sequence(1),
        }),
        head_revision: None,
        transcript: None,
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

/// LO-FENCE: stale revisions refuse the entire transition, while mailbox
/// arrival alone remains revision-neutral. Takeover keeps its distinct cause.
pub async fn lo_fence_rejects_stale_epoch_and_revision(
    store: &dyn DurableStore,
    advance: Advance<'_>,
) -> LawOutcome {
    lo_fence_with_path(store, advance, false).await
}

/// The same law with root-lease work to select PostgreSQL's explicit path.
pub async fn lo_fence_with_path(
    store: &dyn DurableStore,
    advance: Advance<'_>,
    explicit: bool,
) -> LawOutcome {
    use crate::domain::{
        CellId, EffectAdmissionWrite, EffectKey, ExecKey, Ordinal, OwnerKey, RootOwner,
        RunFactKind, RunFactWrite, RunSeq,
    };
    use lash_core_store::checkpoint_graph::{DecodeLimits, GraphRefusal};
    use lash_protocol_types::{SessionId, TurnId};
    let actor = session("lo-fence")?;
    create(store, std::slice::from_ref(&actor)).await?;
    append(store, &actor, "seen before draft").await?;
    let owner = node(store, "lo-fence-owner").await?;
    let grant = store.claim(&owner, 1).await?;
    let epoch = grant[0].epoch;
    let sid =
        SessionId::try_from("lo-fence".to_owned()).map_err(|error| LawBroken(error.to_string()))?;
    let run = TurnId::try_from("lo-fence-run".to_owned())
        .map_err(|error| LawBroken(error.to_string()))?;
    let facts = OwnerKey::Cell(sid.clone(), run.clone(), CellId::new("stale"));
    let exec = ExecKey::Cell(sid, run, CellId::new("stale"));
    let effect = EffectKey(vec![7].into());
    let (root, encoded) = tagged_node("lo-fence-stale");
    let mut stale = store.begin(&actor, epoch).await?;
    let expected = stale.revision();
    stale.stage([encoded]);
    stale.write(crate::DomainWrite::RunFact(RunFactWrite::Append {
        owner: facts.clone(),
        run: RunSeq(0),
        ordinal: Ordinal(0),
        kind: RunFactKind::Admit,
        call: None,
        record: root,
    }));
    stale.write(crate::DomainWrite::EffectAdmission(
        EffectAdmissionWrite::Put {
            exec: exec.clone(),
            effect: effect.clone(),
            member: None,
            admission: root,
        },
    ));
    stale.ack_seen().give_up(Release::Idle);
    if explicit {
        stale.lease_root(
            RootOwner::EffectAdmission {
                exec: exec.clone(),
                effect: effect.clone(),
            },
            root,
            TTL_MILLIS,
        );
    }
    let mut parked = store.begin(&actor, epoch).await?;
    parked.write(crate::DomainWrite::ActorPark {
        reason_json: Some("stale-park".into()),
    });
    parked.ack_seen().give_up(Release::Parked);
    let winner = store.begin(&actor, epoch).await?;
    let acknowledged = store.commit(winner, LABEL).await?;
    let before = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("the fenced actor vanished".into()))?;
    let mut stale_effect_starts = 0;
    let refused = store.commit(stale, LABEL).await;
    if refused.is_ok() {
        stale_effect_starts += 1;
    }
    ensure!(
        matches!(refused, Err(DurableError::RevisionConflict {
        actor: ref refused_actor, epoch: held, expected: planned, actual
    }) if refused_actor == &actor && held == epoch && planned == expected && actual == acknowledged.revision),
        "same-epoch stale draft was not refused at its planning revision: {refused:?}"
    );
    ensure!(
        matches!(store.commit(parked, LABEL).await,
        Err(DurableError::RevisionConflict { expected: planned, actual, .. })
            if planned == expected && actual == acknowledged.revision),
        "a stale claimed park/release committed"
    );
    let after = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("the fenced actor vanished".into()))?;
    ensure!(
        after == before,
        "refused transition changed actor/ack/park/release: {after:?}"
    );
    let stale_domain_rows = store.run_facts(&facts).await?.rows.len();
    ensure!(
        stale_domain_rows == 0 && stale_effect_starts == 0,
        "stale_domain_rows={stale_domain_rows} stale_effect_starts={stale_effect_starts}"
    );
    for owner in [
        RootOwner::RunFact {
            owner: facts,
            run: RunSeq(0),
            ordinal: Ordinal(0),
        },
        RootOwner::EffectAdmission { exec, effect },
    ] {
        ensure!(
            matches!(
                store
                    .hydrate(owner, None, DecodeLimits::within_total(1024))
                    .await,
                Err(DurableError::Graph(GraphRefusal::RootRetired))
            ),
            "a stale root was installed"
        );
    }
    // Arrival changes mail_seq, never the actor's planning revision. The
    // old read can acknowledge only its seen prefix, leaving new mail owed.
    let mut tx = store.begin(&actor, epoch).await?;
    append(store, &actor, "unseen arrival").await?;
    tx.ack_seen();
    store.commit(tx, LABEL).await?;
    let after = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("the fenced actor vanished".into()))?;
    ensure!(
        after.has_mail && after.pending_mail == 1,
        "unseen mail was acknowledged"
    );
    let takeover = store.begin(&actor, epoch).await?;
    advance(TTL + Duration::from_millis(1));
    let successor = node(store, "lo-fence-successor").await?;
    store.reap(&successor).await?;
    let claimed = store.claim(&successor, 1).await?;
    ensure!(claimed.len() == 1, "the successor did not claim");
    ensure!(
        matches!(
            store.commit(takeover, LABEL).await,
            Err(DurableError::OwnershipLost(_))
        ),
        "takeover lost its distinct ownership cause"
    );
    eprintln!("LO-FENCE stale_domain_rows=0 stale_effect_starts=0");
    Ok(())
}

/// A held outcome cannot acquire the newer basis of its publishing boundary.
pub async fn lo_fence_preserves_a_composed_drafts_basis(store: &dyn DurableStore) -> LawOutcome {
    use crate::domain::{CellId, EffectAdmissionWrite, EffectKey, ExecKey, RootOwner};
    use lash_core_store::checkpoint_graph::{DecodeLimits, GraphRefusal};
    use lash_protocol_types::{SessionId, TurnId};

    let actor = session("lo-composed")?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "lo-composed-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let exec = ExecKey::Cell(
        SessionId::from("lo-composed"),
        TurnId::from("run"),
        CellId::new("held"),
    );
    let effect = EffectKey(vec![7].into());
    let (root, encoded) = tagged_node("held-admission");
    let mut held = store.begin(&actor, epoch).await?;
    let expected = held.revision();
    held.stage([encoded]);
    held.write(crate::DomainWrite::EffectAdmission(
        EffectAdmissionWrite::Put {
            exec: exec.clone(),
            effect: effect.clone(),
            member: None,
            admission: root,
        },
    ));
    let winner = store.begin(&actor, epoch).await?;
    let committed = store.commit(winner, LABEL).await?;
    let before = store.actor(&actor).await?;
    // OutputBoundary and the first cell save open anew for fresh mail/cancel
    // facts before appending the response/outcome they held across preparation.
    append(store, &actor, "fresh control frontier").await?;
    let mut boundary = store.begin(&actor, epoch).await?;
    let result = match boundary.append(held) {
        Ok(_) => store.commit(boundary, LABEL).await,
        Err(error) => {
            ensure!(
                boundary.domain().is_empty() && boundary.graph().is_empty(),
                "refused composition partially added held writes"
            );
            Err(error)
        }
    };
    let stale_effect_starts = usize::from(result.is_ok());
    ensure!(
        matches!(result, Err(DurableError::RevisionConflict {
        actor: ref refused, epoch: held_epoch, expected: planned, actual,
    }) if refused == &actor && held_epoch == epoch && planned == expected && actual == committed.revision),
        "composition upgraded a held draft's basis: {result:?}"
    );
    let after = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("actor vanished".into()))?;
    ensure!(
        Some(after.revision) == before.map(|row| row.revision),
        "stale composition advanced revision"
    );
    ensure!(
        after.pending_mail == 1,
        "stale composition acknowledged fresh mail"
    );
    ensure!(
        matches!(
            store
                .hydrate(
                    RootOwner::EffectAdmission { exec, effect },
                    None,
                    DecodeLimits::within_total(1024)
                )
                .await,
            Err(DurableError::Graph(GraphRefusal::RootRetired))
        ),
        "stale admission root was published"
    );
    ensure!(
        stale_effect_starts == 0,
        "stale_effect_starts={stale_effect_starts}"
    );
    Ok(())
}

/// LO-POSTIMAGE: every head write returns the exact result constructed in
/// its transaction. Adopting it equals cold reopen; a later refusal rolls
/// back the head and exposes no adoptable result. Receipt replay preserves
/// the first result even after another head advances.
///
/// # Errors
/// The first broken acknowledgement or atomicity rule.
pub async fn lo_postimage_receipt_matches_reopen(
    store: &dyn DurableStore,
    sessions: &dyn lash_core_store::store::RuntimeStore,
    explicit: bool,
) -> LawOutcome {
    use crate::DomainWrite;
    use crate::domain::{SessionCommitWrite, TurnWrite};
    use lash_core_store::session_state::{
        RuntimeSessionAuthority, RuntimeSessionState, adopt_durable_head,
    };
    use lash_core_store::session_store_factory_types::{
        SessionCreationHead, SessionStoreCreateRequest,
    };
    use lash_core_store::store::{RuntimeCommit, WindowSelector};
    let stored = |error: lash_core_store::store::StoreError| LawBroken(error.to_string());
    let policy = lash_core_store::session_policy::SessionPolicy::new(
        lash_protocol_types::TurnBudget::Unbounded,
        lash_protocol_types::MaxToolCalls::new(8),
        lash_protocol_types::NoProgressBudget::bounded(12),
    );
    let id = lash_protocol_types::SessionId::try_from("lo-postimage-receipt".to_owned())
        .map_err(|error| LawBroken(error.to_string()))?;
    let mut state = RuntimeSessionState::new(
        policy,
        RuntimeSessionAuthority::new(Default::default(), Default::default()),
    );
    state.session_id = id.clone();
    sessions
        .admit_session(&SessionStoreCreateRequest {
            session_id: id.clone(),
            relation: lash_core_store::session_identity::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            config: lash_core_store::store::persisted_session_config_from_state(&state),
            head: SessionCreationHead::Config,
            owning_process_id: None,
            retention: lash_core_store::store::Retention::UntilGc,
        })
        .await
        .map_err(stored)?;
    let actor = session(id.as_str())?;
    if store.actor(&actor).await?.is_none() {
        create(store, std::slice::from_ref(&actor)).await?;
    }
    append(store, &actor, "receipt-law").await?;
    let formats = store
        .actor(&actor)
        .await?
        .ok_or_else(|| LawBroken("session actor absent".into()))?
        .formats;
    let lease = store
        .register_node(&NodeSpec {
            node: NodeId::new("postimage-owner"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: TTL_MILLIS,
        })
        .await?;
    let epoch = store
        .claim(&lease, 1)
        .await?
        .first()
        .ok_or_else(|| LawBroken("session not claimed".into()))?
        .epoch;
    let fleet = sessions.fleet_format();
    let mut first = None;
    for ordinal in 0..3 {
        state.ensure_agent_frame_initialized().map_err(stored)?;
        state
            .session_graph
            .append_plugin("postimage-law", serde_json::json!({"ordinal": ordinal}))
            .map_err(stored)?;
        state.policy.generation.seed = Some(ordinal);
        let operation = lash_core_store::store::OperationId::new(
            lash_protocol_types::ExecutionScope::runtime_operation(format!("postimage-{ordinal}")),
            "commit",
        );
        let (commit, _) = RuntimeCommit::persisted_state_with_operation_and_budget(
            &mut state,
            operation,
            lash_core_store::store::CommitBudget::bounded(1 << 20, 512),
            fleet,
        )
        .map_err(stored)?;
        let encoded = lash_core_store::store::encode_session_commit(&commit).map_err(stored)?;
        let mut tx = store.begin(&actor, epoch).await?;
        tx.stage(encoded.namespace_nodes.clone().into_values());
        tx.stage(encoded.descriptor.clone());
        if explicit {
            tx.release_root_lease(crate::domain::pages::RootLeaseId(9_000_000));
        }
        let write = DomainWrite::SessionCommit(SessionCommitWrite {
            session: id.clone(),
            expected_head: commit.expected_head_revision,
            commit_json: encoded.json,
            execution: encoded.execution,
        });
        tx.write(write.clone());
        let receipt = store.commit(tx, LABEL).await?;
        let [head] = receipt.session_heads.as_slice() else {
            return Err(LawBroken(
                "head write did not acknowledge exactly one result".into(),
            ));
        };
        ensure!(head.session == id, "receipt names another session");
        state
            .adopt_retained_commit_result(&head.receipt)
            .map_err(stored)?;
        let read = sessions
            .load_session_window(&id, WindowSelector::Current)
            .await
            .map_err(stored)?
            .ok_or_else(|| LawBroken("committed head absent".into()))?;
        let mut cold = state.clone();
        adopt_durable_head(&mut cold, read, fleet).map_err(stored)?;
        ensure!(
            state.head_revision == cold.head_revision
                && state.checkpoint_ref == cold.checkpoint_ref,
            "receipt head/checkpoint differs from reopen"
        );
        ensure!(
            serde_json::to_value(state.to_snapshot())
                .map_err(|error| LawBroken(error.to_string()))?
                == serde_json::to_value(cold.to_snapshot())
                    .map_err(|error| LawBroken(error.to_string()))?,
            "prepared state plus receipt differs from cold reopen at cut {ordinal}"
        );
        if ordinal == 0 {
            first = Some((write, head.clone()));
        }
    }
    let mut empty = store.begin(&actor, epoch).await?;
    empty.ack_seen();
    ensure!(
        store.commit(empty, LABEL).await?.session_heads.is_empty(),
        "metadata-only commit fabricated a head result"
    );
    let (write, first) = first.ok_or_else(|| LawBroken("first receipt absent".into()))?;
    let mut replay = store.begin(&actor, epoch).await?;
    replay.write(write);
    let replay = store.commit(replay, LABEL).await?;
    let [replayed] = replay.session_heads.as_slice() else {
        return Err(LawBroken("replay lost its head result".into()));
    };
    let mut replayed = replayed.clone();
    ensure!(
        replayed.receipt.receipt_replayed,
        "replay did not preserve its decision"
    );
    replayed.receipt.receipt_replayed = first.receipt.receipt_replayed;
    ensure!(replayed == first, "replay changed the original receipt");
    let before = store.actor(&actor).await?;
    let operation = lash_core_store::store::OperationId::new(
        lash_protocol_types::ExecutionScope::runtime_operation("refused-postimage"),
        "commit",
    );
    let (commit, _) = RuntimeCommit::persisted_state_with_operation_and_budget(
        &mut state,
        operation,
        lash_core_store::store::CommitBudget::bounded(1 << 20, 512),
        fleet,
    )
    .map_err(stored)?;
    let encoded = lash_core_store::store::encode_session_commit(&commit).map_err(stored)?;
    let mut refused = store.begin(&actor, epoch).await?;
    refused.stage(encoded.namespace_nodes.into_values());
    refused.stage(encoded.descriptor);
    refused.write(DomainWrite::SessionCommit(SessionCommitWrite {
        session: id.clone(),
        expected_head: state.head_revision,
        commit_json: encoded.json,
        execution: encoded.execution,
    }));
    refused.write(DomainWrite::Turn(TurnWrite::Advance {
        session: id.clone(),
        run: lash_protocol_types::TurnId::try_from("absent-run".to_owned())
            .map_err(|error| LawBroken(error.to_string()))?,
        phase: crate::domain::PhaseAdvance::Tools {
            run: crate::domain::RunSeq(1),
            checkpoint: tagged_node("absent").0,
        },
        iteration: 0,
    }));
    ensure!(
        store.commit(refused, LABEL).await.is_err(),
        "a refused member returned an adoptable result"
    );
    ensure!(
        store.actor(&actor).await? == before,
        "refusal advanced actor authority"
    );
    ensure!(
        sessions
            .load_session_head_meta(&id)
            .await
            .map_err(stored)?
            .is_some_and(|head| head.head_revision == state.head_revision),
        "refusal left a partial head"
    );
    Ok(())
}
